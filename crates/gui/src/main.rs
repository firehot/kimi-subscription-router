#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! kimi-switch：Kimi Code 多账号管理的简易图形界面（eframe/egui）。
//!
//! 功能：账号卡片列表 + 5h/7d 彩色额度条、添加账号（浏览器设备码授权）、
//! 导入当前本机账号、切换（原子写 + 快照回滚）、重命名、删除（确认弹窗）。
//! 直接调用 kimi-switch-core / kimi-switch-provider-kimi 的公开 API，不 shell 调子进程。
//!
//! 异步方案：egui 是同步的，网络/文件操作放在后台 worker 线程（内置 tokio runtime，
//! `block_on` 驱动 async API），UI 与 worker 之间用 `std::sync::mpsc` 传消息，
//! worker 完成后通过 `egui::Context::request_repaint` 唤醒界面。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::{
    load_router_status, settings, AccountId, AccountRegistry, AuditEvent, AuditLog,
    CredentialStore, FileStore, KeyringStore, Provider, Quota, QuotaCache, QuotaWindow,
    RemovedAccounts, RouterStatusSnapshot,
};
use kimi_switch_kimi::{device_flow, KimiProvider};

mod control;

const AUTO_REFRESH_INTERVALS: &[(u64, &str)] = &[
    (0, "关闭"),
    (2 * 60 * 1000, "每 2 分钟"),
    (5 * 60 * 1000, "每 5 分钟"),
    (15 * 60 * 1000, "每 15 分钟"),
    (30 * 60 * 1000, "每 30 分钟"),
    (60 * 60 * 1000, "每 60 分钟"),
];

#[cfg(target_os = "macos")]
mod macos_app {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    /// 主窗口隐藏时切为状态栏应用；恢复窗口时重新显示程序坞图标。
    pub fn set_dock_visible(visible: bool) {
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(main_thread);
        let policy = if visible {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        if app.setActivationPolicy(policy) && visible {
            app.activate();
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop_startup {
    use auto_launch::{AutoLaunch, AutoLaunchBuilder};

    fn manager() -> anyhow::Result<AutoLaunch> {
        let executable = std::env::current_exe()?;
        let executable = executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("程序路径不是有效 UTF-8"))?;
        let mut builder = AutoLaunchBuilder::new();
        builder
            .set_app_name("io.github.firehot.kimi-subscription-router")
            .set_app_path(executable)
            .set_args(&["--hidden"]);
        #[cfg(target_os = "macos")]
        builder
            .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent)
            .set_bundle_identifiers(&["io.github.firehot.kimi-subscription-router"]);
        #[cfg(target_os = "windows")]
        builder.set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser);
        Ok(builder.build()?)
    }

    pub fn is_enabled() -> anyhow::Result<bool> {
        Ok(manager()?.is_enabled()?)
    }

    pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
        let manager = manager()?;
        if enabled {
            manager.enable()?;
        } else {
            manager.disable()?;
        }
        Ok(())
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop_tray {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Arc;

    use eframe::egui;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    pub enum TrayAction {
        Show,
        SetLaunchAtLogin(bool),
        SetRefreshInterval(u64),
        Exit,
    }

    /// 桌面状态栏图标及其事件通道；字段必须存活到程序退出。
    pub struct DesktopTray {
        _icon: TrayIcon,
        actions: Receiver<TrayAction>,
        launch_at_login: CheckMenuItem,
        launch_state: Arc<AtomicBool>,
        refresh_intervals: Vec<(u64, CheckMenuItem)>,
    }

    impl DesktopTray {
        pub fn new(
            ctx: egui::Context,
            icon_data: &egui::IconData,
            launch_enabled: bool,
            refresh_interval_ms: u64,
        ) -> anyhow::Result<Self> {
            let show_item = MenuItem::with_id("show-window", "显示主窗口", true, None);
            let launch_at_login =
                CheckMenuItem::with_id("launch-at-login", "开机启动", true, launch_enabled, None);
            let refresh_menu = Submenu::with_id("auto-refresh", "自动刷新", true);
            let refresh_intervals = super::AUTO_REFRESH_INTERVALS
                .iter()
                .map(|(interval, label)| {
                    let item = CheckMenuItem::with_id(
                        format!("refresh-{interval}"),
                        *label,
                        true,
                        *interval == refresh_interval_ms,
                        None,
                    );
                    (*interval, item)
                })
                .collect::<Vec<_>>();
            for (_, item) in &refresh_intervals {
                refresh_menu.append(item)?;
            }
            let settings_menu = Submenu::with_id("settings", "设置", true);
            settings_menu.append(&launch_at_login)?;
            settings_menu.append(&refresh_menu)?;
            let separator = PredefinedMenuItem::separator();
            let exit_item = MenuItem::with_id("exit-app", "退出", true, None);
            let menu = Menu::with_items(&[&show_item, &settings_menu, &separator, &exit_item])?;
            let icon = Icon::from_rgba(icon_data.rgba.clone(), icon_data.width, icon_data.height)?;

            let tray_builder = TrayIconBuilder::new()
                .with_tooltip("Kimi Subscription Router（非官方）")
                .with_icon(icon)
                .with_menu(Box::new(menu));
            #[cfg(target_os = "windows")]
            let tray_builder = tray_builder.with_menu_on_left_click(false);
            let tray_icon = tray_builder.build()?;

            let (action_tx, actions) = channel();
            let show_id = show_item.id().clone();
            let launch_id = launch_at_login.id().clone();
            let exit_id = exit_item.id().clone();
            let refresh_ids = refresh_intervals
                .iter()
                .map(|(interval, item)| (item.id().clone(), *interval))
                .collect::<Vec<_>>();
            let launch_state = Arc::new(AtomicBool::new(launch_enabled));
            let menu_launch_state = launch_state.clone();
            let menu_action_tx = action_tx.clone();
            let menu_ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let action = if event.id == show_id {
                    Some(TrayAction::Show)
                } else if event.id == launch_id {
                    Some(TrayAction::SetLaunchAtLogin(
                        !menu_launch_state.load(Ordering::Relaxed),
                    ))
                } else if event.id == exit_id {
                    Some(TrayAction::Exit)
                } else {
                    refresh_ids
                        .iter()
                        .find(|(id, _)| event.id == *id)
                        .map(|(_, interval)| TrayAction::SetRefreshInterval(*interval))
                };
                if let Some(action) = action {
                    let _ = menu_action_tx.send(action);
                    menu_ctx.request_repaint();
                }
            }));

            TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
                let should_show = matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    }
                );
                if should_show {
                    let _ = action_tx.send(TrayAction::Show);
                    ctx.request_repaint();
                }
            }));

            Ok(Self {
                _icon: tray_icon,
                actions,
                launch_at_login,
                launch_state,
                refresh_intervals,
            })
        }

        pub fn try_recv(&self) -> Option<TrayAction> {
            self.actions.try_recv().ok()
        }

        pub fn set_launch_at_login(&self, enabled: bool) {
            self.launch_state.store(enabled, Ordering::Relaxed);
            self.launch_at_login.set_checked(enabled);
        }

        pub fn set_refresh_interval(&self, interval_ms: u64) {
            for (interval, item) in &self.refresh_intervals {
                item.set_checked(*interval == interval_ms);
            }
        }
    }
}

/// 随程序嵌入中文字形，避免依赖目标系统的字体安装情况。
const CJK_FONT: &[u8] = include_bytes!("../assets/NotoSansCJKsc-Regular.otf");

const COLOR_OK: egui::Color32 = egui::Color32::from_rgb(63, 185, 80);
const COLOR_FULL: egui::Color32 = egui::Color32::from_rgb(248, 81, 73);
const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(88, 166, 255);
const EXTRA_NICKNAME: &str = "nickname";
const EXTRA_EMAIL: &str = "email";
const EXTRA_PROFILE_LOADED: &str = "profile_loaded";
const EXTRA_SUBSCRIPTION_EXPIRES_ON: &str = "subscription_expires_on";
const EXTRA_ROUTING_ENABLED: &str = "routing_enabled";

fn usage_progress(ui: &mut egui::Ui, used_ratio: f32) {
    let used_ratio = used_ratio.clamp(0.0, 1.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
    let (track, fill) = if ui.visuals().dark_mode {
        (
            egui::Color32::from_rgb(74, 74, 74),
            egui::Color32::from_rgb(88, 166, 255),
        )
    } else {
        (
            egui::Color32::from_rgb(225, 225, 225),
            egui::Color32::from_rgb(37, 99, 235),
        )
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(5), track);
    if used_ratio > 0.0 {
        let fill_rect = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * used_ratio, rect.height()),
        );
        ui.painter()
            .rect_filled(fill_rect, egui::CornerRadius::same(5), fill);
    }
}

fn auto_refresh_status(interval_ms: u64) -> String {
    if interval_ms == 0 {
        return "已关闭自动刷新".to_string();
    }
    let label = AUTO_REFRESH_INTERVALS
        .iter()
        .find_map(|(interval, label)| (*interval == interval_ms).then_some(*label))
        .unwrap_or("自定义间隔");
    format!("自动刷新已设置为{label}")
}

fn account_matches_filter(row: &AccountRow, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty()
        || row.label.to_lowercase().contains(&filter)
        || row.id.to_lowercase().contains(&filter)
        || row
            .masked_email
            .as_deref()
            .is_some_and(|email| email.to_lowercase().contains(&filter))
}

fn account_remaining(row: &AccountRow) -> Option<f32> {
    row.quotas
        .iter()
        .find(|quota| quota.window == "7天")
        .and_then(|quota| quota.ratio)
        .map(|ratio| (1.0 - ratio).clamp(0.0, 1.0) * 100.0)
}

fn toggled_account_selection(current: Option<&str>, clicked: &str) -> Option<String> {
    (current != Some(clicked)).then(|| clicked.to_string())
}

fn reset_display_text(reset: &str) -> String {
    let reset = reset.trim();
    if reset.starts_with("重置") {
        reset.to_string()
    } else {
        format!("重置 {reset}")
    }
}

#[derive(Default)]
struct ToolbarActions {
    toggle_theme: bool,
    refresh: bool,
    import: bool,
    add: bool,
}

fn toolbar_action_buttons(
    ui: &mut egui::Ui,
    busy: bool,
    dark_mode: bool,
    actions: &mut ToolbarActions,
) {
    let (primary_fill, primary_text) = if dark_mode {
        (
            egui::Color32::from_rgb(242, 242, 242),
            egui::Color32::from_rgb(24, 24, 24),
        )
    } else {
        (egui::Color32::from_rgb(24, 24, 24), egui::Color32::WHITE)
    };
    if ui
        .add_enabled(
            !busy,
            egui::Button::new(egui::RichText::new("＋ 添加账号").color(primary_text))
                .fill(primary_fill),
        )
        .clicked()
    {
        actions.add = true;
    }
    if ui
        .add_enabled(!busy, egui::Button::new("导入当前账号"))
        .clicked()
    {
        actions.import = true;
    }
    if ui
        .add_enabled(
            !busy,
            egui::Button::new(egui::RichText::new("↻").size(18.0)),
        )
        .on_hover_text("刷新额度")
        .clicked()
    {
        actions.refresh = true;
    }
    let theme_icon = if dark_mode { "☀" } else { "☾" };
    if ui
        .add_sized(
            [30.0, 28.0],
            egui::Button::new(egui::RichText::new(theme_icon).size(16.0)),
        )
        .on_hover_text(if dark_mode {
            "切换为浅色主题"
        } else {
            "切换为深色主题"
        })
        .clicked()
    {
        actions.toggle_theme = true;
    }
}

/// 官方品牌页 K 标志；本工具为非官方、非商业项目。
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/kimi-brand-icon.png"))
        .expect("embedded Kimi brand icon must be a valid PNG")
}

fn main() -> eframe::Result<()> {
    let icon = app_icon();
    let start_hidden = std::env::args_os().any(|arg| arg == "--hidden");
    let configured_dark = settings::reload_from_file()
        .unwrap_or_else(|_| settings::current())
        .gui
        .dark_mode;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([520.0, 420.0])
            .with_visible(!start_hidden)
            .with_icon(icon.clone()),
        ..Default::default()
    };
    // 显式环境变量优先，否则恢复上次保存的主题。
    let dark = match std::env::var("KIMI_SWITCH_THEME").as_deref() {
        Ok("light") => false,
        Ok("dark") => true,
        _ => configured_dark,
    };
    eframe::run_native(
        "Kimi Subscription Router",
        options,
        Box::new(move |cc| {
            load_cjk_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, dark);
            Ok(Box::new(GuiApp::new(
                cc.egui_ctx.clone(),
                dark,
                &icon,
                start_hidden,
            )))
        }),
    )
}

/// 优先使用系统界面字体，中文由内置 CJK 字体补齐，保证跨平台显示一致。
fn load_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    #[cfg(target_os = "macos")]
    let system_fonts = [
        ("system-sans", "/System/Library/Fonts/SFNS.ttf", false),
        ("system-mono", "/System/Library/Fonts/SFNSMono.ttf", true),
    ];
    #[cfg(target_os = "windows")]
    let system_fonts = [
        ("system-sans", r"C:\Windows\Fonts\segoeui.ttf", false),
        ("system-mono", r"C:\Windows\Fonts\consola.ttf", true),
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let system_fonts: [(&str, &str, bool); 0] = [];

    for (name, path, monospace) in system_fonts {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
            let family = if monospace {
                egui::FontFamily::Monospace
            } else {
                egui::FontFamily::Proportional
            };
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, name.to_owned());
        }
    }
    fonts.font_data.insert(
        "cjk".to_owned(),
        egui::FontData::from_static(CJK_FONT).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// 深色主题：中性黑灰层级，接近 Codex 原生界面的克制观感。
fn dark_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = Color32::from_rgb(25, 25, 25);
    visuals.panel_fill = Color32::from_rgb(25, 25, 25);
    visuals.faint_bg_color = Color32::from_rgb(35, 35, 35);
    visuals.extreme_bg_color = Color32::from_rgb(31, 31, 31);
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(65, 65, 65));
    visuals.hyperlink_color = COLOR_ACCENT;
    visuals.selection.bg_fill = COLOR_ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    let text = Color32::from_rgb(235, 235, 235);
    let border = Color32::from_rgb(65, 65, 65);
    // 非交互控件（标签、进度条槽）。
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(35, 35, 35);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(35, 35, 35);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    // 按钮三态。
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(47, 47, 47);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(47, 47, 47);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(58, 58, 58);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 58, 58);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(105, 105, 105));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.bg_fill = Color32::from_rgb(70, 70, 70);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(70, 70, 70);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(135, 135, 135));
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    // 输入框光标/选中。
    visuals.widgets.open = visuals.widgets.inactive;
    visuals
}

/// 浅色主题：白色内容面与浅灰控件层级。
fn light_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut visuals = egui::Visuals::light();
    visuals.window_fill = Color32::from_rgb(250, 250, 250);
    visuals.panel_fill = Color32::from_rgb(250, 250, 250);
    visuals.faint_bg_color = Color32::WHITE;
    visuals.extreme_bg_color = Color32::from_rgb(235, 235, 235);
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(210, 210, 210));
    visuals.hyperlink_color = Color32::from_rgb(9, 105, 218);
    visuals.selection.bg_fill = COLOR_ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    let text = Color32::from_rgb(31, 35, 40);
    let border = Color32::from_rgb(210, 210, 210);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(242, 242, 242);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(242, 242, 242);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(242, 242, 242);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(242, 242, 242);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(232, 232, 232);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(232, 232, 232);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(165, 165, 165));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.active.bg_fill = Color32::from_rgb(222, 222, 222);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(222, 222, 222);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(140, 140, 140));
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.open = visuals.widgets.inactive;
    visuals
}

/// 应用主题 + 全局观感（圆角、间距）。
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.options_mut(|o| {
        o.theme_preference = if dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        };
    });
    ctx.set_visuals(if dark {
        dark_visuals()
    } else {
        light_visuals()
    });
    ctx.style_mut(|style| {
        use egui::{FontFamily, FontId, TextStyle};

        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        );
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        let corner = egui::CornerRadius::same(6);
        style.visuals.widgets.noninteractive.corner_radius = corner;
        style.visuals.widgets.inactive.corner_radius = corner;
        style.visuals.widgets.hovered.corner_radius = corner;
        style.visuals.widgets.active.corner_radius = corner;
        style.visuals.selection.bg_fill = COLOR_ACCENT;
    });
}

// ---------------------------------------------------------------------------
// UI ↔ worker 消息
// ---------------------------------------------------------------------------

enum Request {
    /// 重新拉额度并刷新列表。
    Refresh,
    /// 添加账号：设备码授权（先取链接给用户，再轮询等授权完成）。
    StartDeviceAuth(Arc<AtomicBool>),
    /// 导入当前本机 Kimi Code 已登录账号（等价 `kimi-switch login kimi`）。
    Import,
    /// 切换到指定账号 id（原子写 + 快照回滚）。
    Swap(String),
    /// 删除指定账号 id。
    Remove(String),
    /// 给账号起别名（只改本地展示，不影响凭证）。
    Rename { id: String, label: String },
    /// 设置月订阅到期日；空字符串表示清除备注。
    SetSubscriptionExpiry { id: String, expires_on: String },
    /// 设置账号是否参与 ACP 自动路由。
    SetRoutingEnabled { id: String, enabled: bool },
    /// 来自仅绑定回环地址的本地控制接口。
    Control(control::Command),
}

/// 单个额度窗口的结构化数据（UI 用来画彩色进度条）。
#[derive(Clone)]
struct QuotaView {
    window: String,
    ratio: Option<f32>,
    text: String,
    reset: Option<String>,
}

#[derive(Clone)]
struct AccountRow {
    id: String,
    /// 展示名：用户别名优先，否则使用 Kimi 账号昵称，最后回退到 id。
    label: String,
    /// 官方 `/me` 返回邮箱的掩码，只用于展示和本机 API。
    masked_email: Option<String>,
    /// 用户手动备注的月订阅到期日（YYYY-MM-DD）。
    subscription_expires_on: Option<String>,
    routing_enabled: bool,
    priority: i32,
    session_count: usize,
    active: bool,
    /// 会员等级（接口 user.membership.level，已美化）。
    membership: Option<String>,
    quotas: Vec<QuotaView>,
    error: Option<String>,
}

/// 状态消息的级别（决定状态栏颜色）。
#[derive(Clone, Copy)]
enum Tone {
    Info,
    Ok,
    Err,
}

enum Response {
    /// 一次操作完成：账号列表 + 可选状态消息。
    List {
        rows: Vec<AccountRow>,
        router_status: RouterStatusSnapshot,
        message: Option<(String, Tone)>,
    },
    /// 设备码已拿到：弹授权对话框展示链接与授权码。
    AuthLink { url: String, user_code: String },
    /// 初始化失败等致命错误。
    Fatal(String),
}

// ---------------------------------------------------------------------------
// 后台 worker：持有 store/registry/kimi provider 与 tokio runtime
// ---------------------------------------------------------------------------

struct Backend {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    kimi: Arc<KimiProvider>,
    audit: AuditLog,
    runtime: tokio::runtime::Runtime,
}

impl Backend {
    fn new() -> anyhow::Result<Self> {
        let paths = AppPaths::resolve()?;
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::with_legacy_keyring(
            paths.credentials_file(),
            KeyringStore::new(),
        ));
        let registry = Arc::new(AccountRegistry::from_default_paths()?);
        let kimi = Arc::new(kimi_switch_kimi::new(store.clone(), registry.clone()));
        let audit = AuditLog::from_default_paths()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?;
        Ok(Self {
            store,
            registry,
            kimi,
            audit,
            runtime,
        })
    }

    fn load_removed() -> RemovedAccounts {
        AppPaths::resolve()
            .map(|p| RemovedAccounts::load(&p.removed_file()))
            .unwrap_or_else(|_| RemovedAccounts::load(std::path::Path::new("removed-missing.json")))
    }

    /// 扫本地 `~/.kimi-code`；当前激活账号没记录过就 import（`rm` 过的有墓碑，跳过）。
    fn sync_local_active(&self) {
        let removed = Self::load_removed();
        let Ok(id) = self.kimi.live_account_id() else {
            return;
        };
        if removed.contains("kimi", &id.0) {
            return;
        }
        if let Ok(account) = self.kimi.sync_active_metadata(None) {
            let _ = self.registry.set_active("kimi", &account.id);
        }
    }

    /// 列出账号（激活的排前面）并逐个查询额度（失败只影响该行的展示）。
    fn load_view(&self) -> (Vec<AccountRow>, RouterStatusSnapshot) {
        let router_status = AppPaths::resolve()
            .and_then(|paths| load_router_status(&paths))
            .unwrap_or_default();
        let rows = self.load_rows(&router_status);
        (rows, router_status)
    }

    fn load_rows(&self, router_status: &RouterStatusSnapshot) -> Vec<AccountRow> {
        // GUI 可长期驻留；每轮刷新都重新对齐官方 Kimi 当前账号，避免外部切号后
        // 旧 registry active 标记让停放账号误走 active 401 恢复路径。
        self.sync_local_active();
        let mut accounts = self.registry.list_by_provider("kimi").unwrap_or_default();
        accounts.sort_by_key(|a| !a.active);
        accounts
            .into_iter()
            .map(|mut account| {
                let (membership, quotas, error) =
                    match self
                        .runtime
                        .block_on(kimi_switch_core::query_quota_with_retry(
                            self.kimi.as_ref(),
                            &account.id,
                        )) {
                        Ok(quotas) => {
                            let membership = quotas
                                .iter()
                                .find_map(|q| q.note.as_deref())
                                .map(prettify_membership);
                            (membership, quota_views(&quotas), None)
                        }
                        Err(e) => (None, Vec::new(), Some(compact_error(&e.to_string()))),
                    };
                let mut nickname = account
                    .extra
                    .get(EXTRA_NICKNAME)
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                let mut email = account
                    .extra
                    .get(EXTRA_EMAIL)
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                let profile_loaded = account
                    .extra
                    .get(EXTRA_PROFILE_LOADED)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !profile_loaded {
                    if let Ok(profile) = self
                        .runtime
                        .block_on(self.kimi.query_account_profile(&account.id))
                    {
                        if let Some(value) = profile.display_label {
                            account.extra.insert(
                                EXTRA_NICKNAME.into(),
                                serde_json::Value::String(value.clone()),
                            );
                            nickname = Some(value);
                        }
                        if let Some(value) = profile.email {
                            account.extra.insert(
                                EXTRA_EMAIL.into(),
                                serde_json::Value::String(value.clone()),
                            );
                            email = Some(value);
                        }
                        account
                            .extra
                            .insert(EXTRA_PROFILE_LOADED.into(), true.into());
                        let _ = self.registry.upsert(account.clone());
                    }
                }
                let label = if account.label == account.id.0 {
                    nickname.unwrap_or_else(|| account.id.0.clone())
                } else {
                    account.label.clone()
                };
                let subscription_expires_on = account
                    .extra
                    .get(EXTRA_SUBSCRIPTION_EXPIRES_ON)
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
                let routing_enabled = account
                    .extra
                    .get(EXTRA_ROUTING_ENABLED)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
                    && !account.manual_only();
                AccountRow {
                    label,
                    masked_email: email.as_deref().map(mask_email),
                    id: account.id.0.clone(),
                    subscription_expires_on,
                    routing_enabled,
                    priority: account.priority,
                    session_count: router_status
                        .account_session_counts
                        .get(&account.id.0)
                        .copied()
                        .unwrap_or_default(),
                    active: account.active,
                    membership,
                    quotas,
                    error,
                }
            })
            .collect()
    }

    /// 导入当前本机已登录账号，返回状态消息。
    fn import(&self) -> anyhow::Result<String> {
        let account = self
            .kimi
            .import_active(None)
            .map_err(anyhow::Error::from)
            .context("导入失败：请先在 Kimi Code 里登录账号")?;
        self.registry.set_active("kimi", &account.id)?;
        if let Ok(mut removed) =
            AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
        {
            let _ = removed.clear("kimi", account.id.0.as_str());
        }
        self.audit
            .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
        Ok(format!("已导入账号 {}", account.id))
    }

    /// 切换激活账号（原子写 + 快照回滚在 provider 内部），返回状态消息。
    fn swap(&self, id: &str) -> anyhow::Result<String> {
        let id = AccountId(id.to_string());
        self.runtime
            .block_on(self.kimi.activate(&id))
            .map_err(anyhow::Error::from)
            .with_context(|| format!("切换到 {id} 失败"))?;
        self.audit
            .append(AuditEvent::ok("activate", "kimi", Some(id.0.as_str())));
        Ok(format!("已切换到 {id}"))
    }

    /// 删除账号：registry + 凭证仓库 + 墓碑 + quota 缓存。
    fn remove(&self, id: &str) -> anyhow::Result<()> {
        let id = AccountId(id.to_string());
        self.registry.remove("kimi", &id)?;
        if let Ok(mut removed) =
            AppPaths::resolve().map(|p| RemovedAccounts::load(&p.removed_file()))
        {
            removed.add("kimi", id.0.as_str())?;
        }
        let credential_delete_error = self.store.delete("kimi", id.0.as_str(), "blob").err();
        let paths = AppPaths::resolve().context("解析路由器数据目录失败")?;
        let isolated_credential = paths
            .router_account_home(&id.0)
            .join("credentials")
            .join("kimi-code.json");
        if let Err(error) = std::fs::remove_file(&isolated_credential) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).with_context(|| {
                    format!("删除隔离凭证失败: {}", isolated_credential.display())
                });
            }
        }
        let mut cache = QuotaCache::load(&paths.quota_cache_file());
        cache.remove("kimi", &id.0);
        cache.save(&paths.quota_cache_file());
        if let Some(error) = credential_delete_error {
            anyhow::bail!("账号记录已移除，但删除凭证失败: {error}");
        }
        self.audit
            .append(AuditEvent::ok("rm", "kimi", Some(id.0.as_str())));
        Ok(())
    }

    /// 给账号起别名（只改 registry 里的 label）。
    fn rename(&self, id: &str, label: &str) -> anyhow::Result<String> {
        let id = AccountId(id.to_string());
        let mut account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        let label = label.trim();
        if label.is_empty() || label == account.label {
            return Ok("别名未变化".to_string());
        }
        account.label = label.to_string();
        self.registry.upsert(account)?;
        Ok(format!("已把 {id} 重命名为「{label}」"))
    }

    /// 设置或清除账号的月订阅到期日备注。
    fn set_subscription_expiry(&self, id: &str, expires_on: &str) -> anyhow::Result<String> {
        let id = AccountId(id.to_string());
        let mut account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        match normalize_subscription_expiry(expires_on)? {
            Some(value) => {
                account.extra.insert(
                    EXTRA_SUBSCRIPTION_EXPIRES_ON.into(),
                    serde_json::Value::String(value.clone()),
                );
                self.registry.upsert(account)?;
                Ok(format!("已记录 {id} 的订阅到期日：{value}"))
            }
            None => {
                account.extra.remove(EXTRA_SUBSCRIPTION_EXPIRES_ON);
                self.registry.upsert(account)?;
                Ok(format!("已清除 {id} 的订阅到期日"))
            }
        }
    }

    /// 设置账号是否进入新会话与故障转移候选池。
    fn set_routing_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<String> {
        let id = AccountId(id.to_string());
        let mut account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        account
            .extra
            .insert(EXTRA_ROUTING_ENABLED.into(), enabled.into());
        self.registry.upsert(account)?;
        Ok(if enabled {
            format!("{id} 已加入自动路由")
        } else {
            format!("{id} 已暂停自动路由")
        })
    }

    /// 通过本机控制 API 原子更新账号的非凭证元数据。
    fn update_account(
        &self,
        id: &str,
        label: Option<String>,
        priority: Option<i32>,
        routing_enabled: Option<bool>,
        subscription_expires_on: Option<String>,
    ) -> anyhow::Result<String> {
        let id = AccountId(id.to_string());
        let mut account = self
            .registry
            .find("kimi", &id)?
            .ok_or_else(|| anyhow::anyhow!("账号 {id} 不存在"))?;
        if let Some(label) = label {
            let label = label.trim();
            if label.is_empty() {
                anyhow::bail!("账号别名不能为空");
            }
            account.label = label.to_string();
        }
        if let Some(priority) = priority {
            if !(-10_000..=10_000).contains(&priority) {
                anyhow::bail!("账号优先级必须在 -10000 到 10000 之间");
            }
            account.priority = priority;
        }
        if let Some(enabled) = routing_enabled {
            account
                .extra
                .insert(EXTRA_ROUTING_ENABLED.into(), enabled.into());
        }
        if let Some(expires_on) = subscription_expires_on {
            match normalize_subscription_expiry(&expires_on)? {
                Some(value) => {
                    account.extra.insert(
                        EXTRA_SUBSCRIPTION_EXPIRES_ON.into(),
                        serde_json::Value::String(value),
                    );
                }
                None => {
                    account.extra.remove(EXTRA_SUBSCRIPTION_EXPIRES_ON);
                }
            }
        }
        self.registry.upsert(account)?;
        self.audit
            .append(AuditEvent::ok("update", "kimi", Some(id.0.as_str())));
        Ok(format!("已更新账号 {id}"))
    }
}

use anyhow::Context as _;

/// worker 线程入口：初始化 → 同步本地激活账号 → 全量加载 → 循环处理请求。
fn worker_main(ctx: egui::Context, rx: Receiver<Request>, tx: Sender<Response>) {
    let backend = match Backend::new() {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(Response::Fatal(format!("初始化失败: {e:#}")));
            ctx.request_repaint();
            return;
        }
    };
    let send_list = |backend: &Backend, message: Option<(String, Tone)>| {
        let (rows, router_status) = backend.load_view();
        let _ = tx.send(Response::List {
            rows,
            router_status,
            message,
        });
        ctx.request_repaint();
    };

    send_list(&backend, None);
    while let Ok(request) = rx.recv() {
        let request = match request {
            Request::Control(command) => {
                handle_control_request(&backend, &tx, &ctx, command);
                continue;
            }
            request => request,
        };
        let message = match request {
            Request::Refresh => None,
            Request::Import => match backend.import() {
                Ok(m) => Some((m, Tone::Ok)),
                Err(e) => Some((format!("{e:#}"), Tone::Err)),
            },
            Request::Swap(id) => match backend.swap(&id) {
                Ok(m) => Some((m, Tone::Ok)),
                Err(e) => Some((format!("{e:#}"), Tone::Err)),
            },
            Request::Remove(id) => match backend.remove(&id) {
                Ok(()) => Some((format!("已删除 {id}"), Tone::Ok)),
                Err(e) => Some((format!("删除失败: {e:#}"), Tone::Err)),
            },
            Request::Rename { id, label } => match backend.rename(&id, &label) {
                Ok(m) => Some((m, Tone::Ok)),
                Err(e) => Some((format!("重命名失败: {e:#}"), Tone::Err)),
            },
            Request::SetSubscriptionExpiry { id, expires_on } => {
                match backend.set_subscription_expiry(&id, &expires_on) {
                    Ok(m) => Some((m, Tone::Ok)),
                    Err(e) => Some((format!("保存订阅到期日失败: {e:#}"), Tone::Err)),
                }
            }
            Request::SetRoutingEnabled { id, enabled } => {
                match backend.set_routing_enabled(&id, enabled) {
                    Ok(m) => Some((m, Tone::Ok)),
                    Err(e) => Some((format!("更新路由状态失败: {e:#}"), Tone::Err)),
                }
            }
            Request::StartDeviceAuth(cancel) => {
                let message = device_auth_flow(&backend, &tx, &ctx, cancel);
                Some(message)
            }
            Request::Control(_) => unreachable!("控制请求已在前置分支处理"),
        };
        send_list(&backend, message);
    }
}

/// 串行复用 GUI 后台的 Provider，避免本地 API 绕过刷新锁和原子切换路径。
fn handle_control_request(
    backend: &Backend,
    tx: &Sender<Response>,
    ctx: &egui::Context,
    command: control::Command,
) {
    let (message, refresh_ui) = match command.action {
        control::Action::List => (None, false),
        control::Action::Refresh => (
            Some(("已通过本地 API 刷新额度".to_string(), Tone::Ok)),
            true,
        ),
        control::Action::Activate(id) => match backend.swap(&id) {
            Ok(message) => (Some((message, Tone::Ok)), true),
            Err(error) => {
                let _ = command
                    .reply
                    .send(control::Reply::Error(format!("{error:#}")));
                return;
            }
        },
        control::Action::Update {
            id,
            label,
            priority,
            routing_enabled,
            subscription_expires_on,
        } => match backend.update_account(
            &id,
            label,
            priority,
            routing_enabled,
            subscription_expires_on,
        ) {
            Ok(message) => (Some((message, Tone::Ok)), true),
            Err(error) => {
                let _ = command
                    .reply
                    .send(control::Reply::Error(format!("{error:#}")));
                return;
            }
        },
        control::Action::Remove(id) => match backend.remove(&id) {
            Ok(()) => (Some((format!("已删除 {id}"), Tone::Ok)), true),
            Err(error) => {
                let _ = command
                    .reply
                    .send(control::Reply::Error(format!("{error:#}")));
                return;
            }
        },
    };
    let (rows, router_status) = backend.load_view();
    let snapshots = rows.iter().map(control_snapshot).collect();
    let reply_message = message.as_ref().map(|(message, _)| message.clone());
    let _ = command.reply.send(control::Reply::Accounts {
        accounts: snapshots,
        message: reply_message,
    });
    if refresh_ui {
        let _ = tx.send(Response::List {
            rows,
            router_status,
            message,
        });
        ctx.request_repaint();
    }
}

fn control_snapshot(row: &AccountRow) -> control::AccountSnapshot {
    control::AccountSnapshot {
        id: row.id.clone(),
        label: row.label.clone(),
        email: row.masked_email.clone(),
        active: row.active,
        membership: row.membership.clone(),
        subscription_expires_on: row.subscription_expires_on.clone(),
        routing_enabled: row.routing_enabled,
        priority: row.priority,
        session_count: row.session_count,
        quotas: row
            .quotas
            .iter()
            .map(|quota| control::QuotaSnapshot {
                window: quota.window.clone(),
                used_ratio: quota.ratio,
                text: quota.text.clone(),
                reset_at: quota.reset.clone(),
            })
            .collect(),
        error: row.error.clone(),
    }
}

/// 设备码授权全流程：取链接 → 通知 UI 弹窗 → 轮询等授权 → 入库（不动当前登录文件）。
fn device_auth_flow(
    backend: &Backend,
    tx: &Sender<Response>,
    ctx: &egui::Context,
    cancel: Arc<AtomicBool>,
) -> (String, Tone) {
    let result = (|| -> anyhow::Result<String> {
        let auth = backend
            .runtime
            .block_on(device_flow::request_device_code())
            .map_err(anyhow::Error::from)
            .context("获取授权链接失败")?;
        let url = auth
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| auth.verification_uri.clone());
        let _ = tx.send(Response::AuthLink {
            url,
            user_code: auth.user_code.clone(),
        });
        ctx.request_repaint();
        let blob = backend
            .runtime
            .block_on(device_flow::poll_for_token(&auth, cancel))
            .map_err(anyhow::Error::from)?;
        let account = backend
            .kimi
            .import_raw(blob, None, Some(false))
            .map_err(anyhow::Error::from)
            .context("授权成功但入库失败")?;
        backend
            .audit
            .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
        Ok(format!("授权成功，已添加账号 {}", account.id))
    })();
    match result {
        Ok(m) => (m, Tone::Ok),
        Err(e) => (format!("{e:#}"), Tone::Err),
    }
}

/// 把接口的会员等级（如 `LEVEL_INTERMEDIATE`）美化成展示文本（`Intermediate`）。
fn prettify_membership(level: &str) -> String {
    let stripped = level.strip_prefix("LEVEL_").unwrap_or(level);
    let mut chars = stripped.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => stripped.to_string(),
    }
}

/// 把多窗口额度转成结构化视图。
fn quota_views(quotas: &[Quota]) -> Vec<QuotaView> {
    quotas
        .iter()
        .filter(|q| q.window != QuotaWindow::Month)
        .map(|q| {
            let window = match q.window {
                QuotaWindow::FiveHour => "5小时",
                QuotaWindow::SevenDay => "7天",
                _ => "其他",
            }
            .to_string();
            let ratio = q.usage_ratio().map(|r| r as f32);
            let text = match ratio {
                Some(r) => format!("{:.0}% ({}/{})", r * 100.0, q.used, q.limit),
                None => format!("{}/{}", q.used, q.limit),
            };
            let reset = q.reset_at.map(|t| {
                format!(
                    "重置 {}",
                    t.with_timezone(&chrono::Local).format("%m-%d %H:%M")
                )
            });
            QuotaView {
                window,
                ratio,
                text,
                reset,
            }
        })
        .collect()
}

/// 错误文本压短到一行，避免撑爆界面。
fn compact_error(error: &str) -> String {
    let one_line = error.replace(['\n', '\r'], " ");
    const MAX: usize = 80;
    if one_line.chars().count() > MAX {
        let mut s: String = one_line.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        one_line
    }
}

fn mask_email(value: &str) -> String {
    let Some((local, domain)) = value.split_once('@') else {
        return "***".to_string();
    };
    let prefix = local
        .chars()
        .next()
        .filter(|_| local.chars().count() > 1)
        .map(|value| value.to_string())
        .unwrap_or_default();
    format!("{prefix}***@{domain}")
}

/// 校验并规范化订阅到期日；空字符串表示清除。
fn normalize_subscription_expiry(value: &str) -> anyhow::Result<Option<String>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("日期格式无效，请使用 YYYY-MM-DD，例如 2026-09-30"))?;
    Ok(Some(date.format("%Y-%m-%d").to_string()))
}

fn subscription_summary(
    ui: &mut egui::Ui,
    rows: &[AccountRow],
    router_status: &RouterStatusSnapshot,
) {
    let remaining_values = rows
        .iter()
        .filter_map(account_remaining)
        .collect::<Vec<_>>();
    let remaining = remaining_values.iter().sum::<f32>();
    let remaining_text = if remaining_values.is_empty() {
        "--".to_string()
    } else {
        format!("{remaining:.0}%")
    };
    let (router_label, router_color) = if router_status.running {
        (
            format!("运行中 · {} 个会话", router_status.session_count),
            COLOR_OK,
        )
    } else {
        (
            format!("未连接 · {} 个会话", router_status.session_count),
            ui.visuals().weak_text_color(),
        )
    };

    let fill = if ui.visuals().dark_mode {
        egui::Color32::from_rgb(62, 62, 62)
    } else {
        egui::Color32::from_rgb(238, 238, 238)
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("◔").size(22.0));
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("用量剩余").strong());
                    ui.label(
                        egui::RichText::new(format!("{} 个已连接订阅", rows.len()))
                            .small()
                            .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(&remaining_text).size(18.0));
                    if ui.available_width() > 250.0 {
                        ui.label(
                            egui::RichText::new(format!("ACP {router_label}"))
                                .small()
                                .color(router_color),
                        );
                    }
                });
            });
        });
}

fn account_avatar(ui: &mut egui::Ui, row: &AccountRow) {
    const PALETTE: [egui::Color32; 5] = [
        egui::Color32::from_rgb(45, 122, 84),
        egui::Color32::from_rgb(73, 101, 171),
        egui::Color32::from_rgb(168, 89, 69),
        egui::Color32::from_rgb(128, 91, 157),
        egui::Color32::from_rgb(157, 122, 46),
    ];
    let hash = row.id.bytes().fold(0_usize, |value, byte| {
        value.wrapping_mul(31) + byte as usize
    });
    let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 14.0, PALETTE[hash % PALETTE.len()]);
    let initial = row.label.chars().next().unwrap_or('?').to_string();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
}

// ---------------------------------------------------------------------------
// egui 界面
// ---------------------------------------------------------------------------

/// 设备码授权对话框状态。
struct AuthDialog {
    url: String,
    user_code: String,
    cancel: Arc<AtomicBool>,
}

struct GuiApp {
    to_worker: Sender<Request>,
    from_worker: Receiver<Response>,
    rows: Vec<AccountRow>,
    account_filter: String,
    selected_account_id: Option<String>,
    router_status: RouterStatusSnapshot,
    busy: bool,
    loaded: bool,
    status: String,
    status_tone: Tone,
    pending_delete: Option<String>,
    rename_target: Option<(String, String)>,
    subscription_expiry_target: Option<(String, String)>,
    auth_dialog: Option<AuthDialog>,
    dark_mode: bool,
    control_info: Option<control::Info>,
    control_error: Option<String>,
    last_auto_refresh: Instant,
    auto_refresh_interval_ms: u64,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    tray: Option<desktop_tray::DesktopTray>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    exit_requested: bool,
}

impl GuiApp {
    fn new(ctx: egui::Context, dark_mode: bool, icon: &egui::IconData, start_hidden: bool) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (resp_tx, resp_rx) = channel::<Response>();
        let (control_info, control_error) = match AppPaths::resolve()
            .map_err(anyhow::Error::from)
            .and_then(|paths| control::start(&paths, req_tx.clone()))
        {
            Ok(info) => (Some(info), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let auto_refresh_interval_ms = settings::reload_from_file()
            .unwrap_or_else(|_| settings::current())
            .gui
            .auto_refresh_interval_ms;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let tray = desktop_tray::DesktopTray::new(
            ctx.clone(),
            icon,
            desktop_startup::is_enabled().unwrap_or(false),
            auto_refresh_interval_ms,
        )
        .ok();
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if start_hidden && tray.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        #[cfg(target_os = "macos")]
        macos_app::set_dock_visible(!start_hidden || tray.is_none());
        std::thread::Builder::new()
            .name("kimi-switch-gui-worker".into())
            .spawn(move || worker_main(ctx, req_rx, resp_tx))
            .expect("spawn worker thread");
        Self {
            to_worker: req_tx,
            from_worker: resp_rx,
            rows: Vec::new(),
            account_filter: String::new(),
            selected_account_id: None,
            router_status: RouterStatusSnapshot::default(),
            busy: true,
            loaded: false,
            status: "加载中…".to_string(),
            status_tone: Tone::Info,
            pending_delete: None,
            rename_target: None,
            subscription_expiry_target: None,
            auth_dialog: None,
            dark_mode,
            control_info,
            control_error,
            last_auto_refresh: Instant::now(),
            auto_refresh_interval_ms,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            tray,
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            exit_requested: false,
        }
    }

    fn send(&mut self, request: Request, status: String) {
        self.busy = true;
        self.status = status;
        self.status_tone = Tone::Info;
        if self.to_worker.send(request).is_err() {
            self.busy = false;
            self.status = "后台线程已退出，请重启程序".to_string();
            self.status_tone = Tone::Err;
        }
    }

    fn start_device_auth(&mut self) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.auth_dialog = Some(AuthDialog {
            url: String::new(),
            user_code: String::new(),
            cancel: cancel.clone(),
        });
        self.send(
            Request::StartDeviceAuth(cancel),
            "正在获取授权链接…".to_string(),
        );
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let shortcut_refresh =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::R));
        if shortcut_refresh && !self.busy {
            self.send(Request::Refresh, "正在刷新额度…".to_string());
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if let Some(dialog) = self.auth_dialog.take() {
                dialog.cancel.store(true, Ordering::Relaxed);
                self.status = "已取消添加账号".to_string();
                self.status_tone = Tone::Info;
            } else {
                self.pending_delete = None;
                self.rename_target = None;
                self.subscription_expiry_target = None;
            }
        }
        if !self.busy
            && self.auto_refresh_interval_ms > 0
            && self.last_auto_refresh.elapsed()
                >= Duration::from_millis(self.auto_refresh_interval_ms)
        {
            self.last_auto_refresh = Instant::now();
            self.send(Request::Refresh, "正在自动刷新额度…".to_string());
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            while let Some(action) = self.tray.as_ref().and_then(|tray| tray.try_recv()) {
                match action {
                    desktop_tray::TrayAction::Show => {
                        #[cfg(target_os = "macos")]
                        macos_app::set_dock_visible(true);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    desktop_tray::TrayAction::SetLaunchAtLogin(enabled) => {
                        match desktop_startup::set_enabled(enabled) {
                            Ok(()) => {
                                if let Some(tray) = &self.tray {
                                    tray.set_launch_at_login(enabled);
                                }
                                self.status = if enabled {
                                    "已开启开机启动".to_string()
                                } else {
                                    "已关闭开机启动".to_string()
                                };
                                self.status_tone = Tone::Ok;
                            }
                            Err(error) => {
                                let actual = desktop_startup::is_enabled().unwrap_or(!enabled);
                                if let Some(tray) = &self.tray {
                                    tray.set_launch_at_login(actual);
                                }
                                self.status = format!("设置开机启动失败：{error}");
                                self.status_tone = Tone::Err;
                            }
                        }
                    }
                    desktop_tray::TrayAction::SetRefreshInterval(interval_ms) => {
                        match settings::set_gui_auto_refresh_interval_ms(interval_ms) {
                            Ok(()) => {
                                self.auto_refresh_interval_ms = interval_ms;
                                self.last_auto_refresh = Instant::now();
                                if let Some(tray) = &self.tray {
                                    tray.set_refresh_interval(interval_ms);
                                }
                                self.status = auto_refresh_status(interval_ms);
                                self.status_tone = Tone::Ok;
                            }
                            Err(error) => {
                                if let Some(tray) = &self.tray {
                                    tray.set_refresh_interval(self.auto_refresh_interval_ms);
                                }
                                self.status = format!("保存自动刷新设置失败：{error}");
                                self.status_tone = Tone::Err;
                            }
                        }
                    }
                    desktop_tray::TrayAction::Exit => {
                        self.exit_requested = true;
                    }
                }
            }

            if self.exit_requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            if self.tray.is_some() {
                let (close_requested, minimized) = ctx.input(|input| {
                    let viewport = input.viewport();
                    (viewport.close_requested(), viewport.minimized == Some(true))
                });
                if close_requested {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    #[cfg(target_os = "macos")]
                    macos_app::set_dock_visible(false);
                }
                if minimized {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    #[cfg(target_os = "macos")]
                    macos_app::set_dock_visible(false);
                }
            }
        }

        // 收集后台消息。
        while let Ok(response) = self.from_worker.try_recv() {
            match response {
                Response::List {
                    rows,
                    router_status,
                    message,
                } => {
                    let selected_exists = self
                        .selected_account_id
                        .as_deref()
                        .is_some_and(|selected| rows.iter().any(|row| row.id == selected));
                    if !selected_exists {
                        self.selected_account_id = rows
                            .iter()
                            .find(|row| row.active)
                            .or_else(|| rows.first())
                            .map(|row| row.id.clone());
                    }
                    self.rows = rows;
                    self.router_status = router_status;
                    self.busy = false;
                    self.loaded = true;
                    self.last_auto_refresh = Instant::now();
                    // 授权流程结束（无论成败），关掉授权对话框。
                    if self.auth_dialog.is_some() {
                        self.auth_dialog = None;
                    }
                    if let Some((m, tone)) = message {
                        self.status = m;
                        self.status_tone = tone;
                    } else {
                        // 无消息的列表更新（初次加载/手动刷新）→ 清空状态栏。
                        self.status = String::new();
                    }
                }
                Response::AuthLink { url, user_code } => {
                    // 授权对话框由 StartDeviceAuth 发起时带上 cancel 标志，
                    // 这里只补充链接内容；cancel 从现有对话框保留或新建。
                    let cancel = self
                        .auth_dialog
                        .as_ref()
                        .map(|d| d.cancel.clone())
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                    self.auth_dialog = Some(AuthDialog {
                        url,
                        user_code,
                        cancel,
                    });
                }
                Response::Fatal(m) => {
                    self.busy = false;
                    self.loaded = true;
                    self.status = m;
                    self.status_tone = Tone::Err;
                }
            }
        }

        let mut toolbar_actions = ToolbarActions::default();
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(10.0);
            ui.add_space(6.0);
            if ui.available_width() < 700.0 {
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Kimi Subscription Router").strong());
                    if self.busy {
                        ui.spinner();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    toolbar_action_buttons(ui, self.busy, self.dark_mode, &mut toolbar_actions);
                });
            } else {
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("Kimi Subscription Router").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        toolbar_action_buttons(ui, self.busy, self.dark_mode, &mut toolbar_actions);
                        if self.busy {
                            ui.spinner();
                        }
                    });
                });
            }
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                let color = match self.status_tone {
                    Tone::Info => ui.visuals().weak_text_color(),
                    Tone::Ok => COLOR_OK,
                    Tone::Err => COLOR_FULL,
                };
                let compact = ui.available_width() < 620.0;
                ui.add(
                    egui::Label::new(egui::RichText::new(&self.status).small().color(color))
                        .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(info) = &self.control_info {
                        ui.label(
                            egui::RichText::new(if compact {
                                "本地 API".to_string()
                            } else {
                                format!("本地 API  {}", info.base_url)
                            })
                            .small()
                            .weak(),
                        )
                        .on_hover_text(format!(
                            "{}\n认证令牌保存在 {}",
                            info.base_url,
                            info.token_file.display()
                        ));
                    } else if let Some(error) = &self.control_error {
                        ui.label(
                            egui::RichText::new("本地 API 未启动")
                                .small()
                                .color(COLOR_FULL),
                        )
                        .on_hover_text(error);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.loaded && self.rows.is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.heading(egui::RichText::new("暂无账号").size(18.0));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !self.busy,
                                egui::Button::new(egui::RichText::new("＋ 添加账号").color(
                                    if self.dark_mode {
                                        egui::Color32::from_rgb(24, 24, 24)
                                    } else {
                                        egui::Color32::WHITE
                                    },
                                ))
                                .fill(if self.dark_mode {
                                    egui::Color32::from_rgb(242, 242, 242)
                                } else {
                                    egui::Color32::from_rgb(24, 24, 24)
                                }),
                            )
                            .clicked()
                        {
                            toolbar_actions.add = true;
                        }
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("导入当前账号"))
                            .clicked()
                        {
                            toolbar_actions.import = true;
                        }
                    });
                });
                return;
            }
            if self.loaded {
                subscription_summary(ui, &self.rows, &self.router_status);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.account_filter)
                            .hint_text("搜索账号")
                            .desired_width((ui.available_width() - 130.0).max(180.0)),
                    );
                    if !self.account_filter.is_empty()
                        && ui.button("×").on_hover_text("清除搜索").clicked()
                    {
                        self.account_filter.clear();
                    }
                    let matches = self
                        .rows
                        .iter()
                        .filter(|row| account_matches_filter(row, &self.account_filter))
                        .count();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{matches} / {}", self.rows.len()))
                                .small()
                                .weak(),
                        );
                    });
                });
                ui.add_space(8.0);
            }
            let mut swap_id: Option<String> = None;
            let mut delete_id: Option<String> = None;
            let mut rename_id: Option<String> = None;
            let mut subscription_expiry_id: Option<String> = None;
            let mut routing_change: Option<(String, bool)> = None;
            let mut selection_change: Option<Option<String>> = None;
            egui::Frame::new()
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut matched_any = false;
                        for (idx, row) in self.rows.iter().enumerate() {
                            if !account_matches_filter(row, &self.account_filter) {
                                continue;
                            }
                            matched_any = true;
                            let selected =
                                self.selected_account_id.as_deref() == Some(row.id.as_str());
                            let row_fill = if row.active && self.dark_mode {
                                egui::Color32::from_rgb(34, 49, 67)
                            } else if row.active {
                                egui::Color32::from_rgb(232, 242, 255)
                            } else if selected && self.dark_mode {
                                egui::Color32::from_rgb(62, 62, 62)
                            } else if selected {
                                egui::Color32::from_rgb(232, 232, 232)
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            let row_response = egui::Frame::new()
                                .fill(row_fill)
                                .stroke(if row.active {
                                    egui::Stroke::new(1.5_f32, COLOR_ACCENT)
                                } else {
                                    egui::Stroke::NONE
                                })
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::same(12))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    let header_response = egui::Frame::new()
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                account_avatar(ui, row);
                                                let title_width =
                                                    (ui.available_width() - 120.0).max(150.0);
                                                ui.allocate_ui_with_layout(
                                                    egui::vec2(title_width, 40.0),
                                                    egui::Layout::top_down(egui::Align::Min),
                                                    |ui| {
                                                        ui.add(
                                                            egui::Label::new(
                                                                egui::RichText::new(&row.label)
                                                                    .strong()
                                                                    .size(15.0),
                                                            )
                                                            .truncate(),
                                                        );
                                                        let secondary = row
                                                            .membership
                                                            .as_deref()
                                                            .or(row.masked_email.as_deref())
                                                            .unwrap_or(row.id.as_str());
                                                        ui.label(
                                                            egui::RichText::new(if row.active {
                                                                format!("当前 · {secondary}")
                                                            } else {
                                                                format!(
                                                                    "订阅 {} · {secondary}",
                                                                    idx + 1
                                                                )
                                                            })
                                                            .small()
                                                            .weak(),
                                                        );
                                                    },
                                                );
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        let remaining = account_remaining(row)
                                                            .map(|value| format!("{value:.0}%"))
                                                            .unwrap_or_else(|| "--".to_string());
                                                        ui.label(
                                                            egui::RichText::new(remaining)
                                                                .size(16.0)
                                                                .color(if row.error.is_some() {
                                                                    COLOR_FULL
                                                                } else {
                                                                    ui.visuals().text_color()
                                                                }),
                                                        );
                                                    },
                                                );
                                            });
                                        })
                                        .response
                                        .interact(egui::Sense::click());
                                    if selected {
                                        ui.separator();
                                        ui.horizontal_wrapped(|ui| {
                                            if !row.active
                                                && ui
                                                    .add_enabled(
                                                        !self.busy,
                                                        egui::Button::new("切换到此账号"),
                                                    )
                                                    .clicked()
                                            {
                                                swap_id = Some(row.id.clone());
                                            }
                                            let mut enabled = row.routing_enabled;
                                            if ui
                                                .add_enabled(
                                                    !self.busy,
                                                    egui::Checkbox::new(&mut enabled, "参与路由"),
                                                )
                                                .changed()
                                            {
                                                routing_change = Some((row.id.clone(), enabled));
                                            }
                                            ui.menu_button("⋯", |ui| {
                                                if ui.button("重命名").clicked() {
                                                    rename_id = Some(row.id.clone());
                                                    ui.close_menu();
                                                }
                                                if ui.button("设置到期日").clicked() {
                                                    subscription_expiry_id = Some(row.id.clone());
                                                    ui.close_menu();
                                                }
                                                ui.separator();
                                                if ui
                                                    .button(
                                                        egui::RichText::new("删除账号")
                                                            .color(COLOR_FULL),
                                                    )
                                                    .clicked()
                                                {
                                                    delete_id = Some(row.id.clone());
                                                    ui.close_menu();
                                                }
                                            })
                                            .response
                                            .on_hover_text("更多操作");
                                        });
                                        ui.horizontal_wrapped(|ui| {
                                            if row.label != row.id {
                                                ui.label(
                                                    egui::RichText::new(&row.id).small().weak(),
                                                );
                                            }
                                            if let Some(email) = &row.masked_email {
                                                ui.separator();
                                                ui.label(egui::RichText::new(email).small().weak());
                                            }
                                            if row.session_count > 0 {
                                                ui.separator();
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "{} 个会话",
                                                        row.session_count
                                                    ))
                                                    .small()
                                                    .color(COLOR_ACCENT),
                                                )
                                                .on_hover_text(format!(
                                                    "路由优先级：{}",
                                                    row.priority
                                                ));
                                            }
                                            if let Some(expires_on) = &row.subscription_expires_on {
                                                ui.separator();
                                                ui.label(
                                                    egui::RichText::new("订阅到期").small().weak(),
                                                );
                                                ui.label(
                                                    egui::RichText::new(expires_on)
                                                        .small()
                                                        .strong(),
                                                );
                                            }
                                        });
                                        ui.add_space(4.0);
                                        if let Some(err) = &row.error {
                                            ui.label(
                                                egui::RichText::new(format!("额度查询失败: {err}"))
                                                    .small()
                                                    .color(COLOR_FULL),
                                            );
                                        } else if row.quotas.is_empty() {
                                            ui.label(
                                                egui::RichText::new("额度不可用").small().weak(),
                                            );
                                        } else {
                                            for quota in &row.quotas {
                                                let used =
                                                    quota.ratio.unwrap_or(0.0).clamp(0.0, 1.0);
                                                let remaining = 1.0 - used;
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(&quota.window).strong(),
                                                    );
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            ui.label(
                                                                egui::RichText::new(format!(
                                                                    "已用 {:.0}% · 剩余 {:.0}%",
                                                                    used * 100.0,
                                                                    remaining * 100.0
                                                                ))
                                                                .small(),
                                                            );
                                                        },
                                                    );
                                                });
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(&quota.text)
                                                            .small()
                                                            .weak(),
                                                    );
                                                    if let Some(reset) = &quota.reset {
                                                        ui.separator();
                                                        ui.label(
                                                            egui::RichText::new(
                                                                reset_display_text(reset),
                                                            )
                                                            .small()
                                                            .weak(),
                                                        );
                                                    }
                                                });
                                                usage_progress(ui, used);
                                                ui.add_space(4.0);
                                            }
                                        }
                                    }
                                    header_response
                                })
                                .inner;
                            if row_response.clicked() {
                                selection_change = Some(toggled_account_selection(
                                    self.selected_account_id.as_deref(),
                                    &row.id,
                                ));
                            }
                            ui.separator();
                        }
                        if !matched_any {
                            ui.add_space(24.0);
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("没有匹配的账号").weak());
                            });
                        }
                    });
                });
            if let Some(selected) = selection_change {
                self.selected_account_id = selected;
            }
            if let Some(id) = swap_id {
                self.send(Request::Swap(id.clone()), format!("正在切换到 {id}…"));
            }
            if let Some(id) = delete_id {
                self.pending_delete = Some(id);
            }
            if let Some(id) = rename_id {
                let current = self
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| r.label.clone())
                    .unwrap_or_default();
                self.rename_target = Some((id, current));
            }
            if let Some(id) = subscription_expiry_id {
                let current = self
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .and_then(|row| row.subscription_expires_on.clone())
                    .unwrap_or_default();
                self.subscription_expiry_target = Some((id, current));
            }
            if let Some((id, enabled)) = routing_change {
                self.send(
                    Request::SetRoutingEnabled { id, enabled },
                    "正在更新路由状态…".to_string(),
                );
            }
        });

        if toolbar_actions.toggle_theme {
            let dark_mode = !self.dark_mode;
            match settings::set_gui_dark_mode(dark_mode) {
                Ok(()) => {
                    self.dark_mode = dark_mode;
                    apply_theme(ctx, dark_mode);
                }
                Err(error) => {
                    self.status = format!("保存主题设置失败：{error}");
                    self.status_tone = Tone::Err;
                }
            }
        }
        if toolbar_actions.refresh && !self.busy {
            self.send(Request::Refresh, "正在刷新额度…".to_string());
        } else if toolbar_actions.import && !self.busy {
            self.send(Request::Import, "正在导入…".to_string());
        } else if toolbar_actions.add && !self.busy {
            self.start_device_auth();
        }

        // 授权对话框：展示链接 + 授权码，等待用户在浏览器完成授权。
        if self.auth_dialog.is_some() {
            let (url, user_code, waiting_link, cancel) = {
                let d = self.auth_dialog.as_ref().unwrap();
                (
                    d.url.clone(),
                    d.user_code.clone(),
                    d.url.is_empty(),
                    d.cancel.clone(),
                )
            };
            let mut open = true;
            egui::Window::new("添加账号 · 浏览器授权")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.set_min_width(360.0);
                    if waiting_link {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在获取授权链接…");
                        });
                    } else {
                        ui.label("1. 复制下面的链接，到浏览器打开并登录授权：");
                        ui.add(
                            egui::TextEdit::singleline(&mut url.as_str())
                                .desired_width(ui.available_width())
                                .font(egui::TextStyle::Monospace),
                        );
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("复制链接").clicked() {
                                ui.ctx().copy_text(url.clone());
                            }
                            if ui.button("打开浏览器").clicked() {
                                if let Err(error) = webbrowser::open(&url) {
                                    self.status = format!("打开浏览器失败：{error}");
                                    self.status_tone = Tone::Err;
                                }
                            }
                        });
                        if !user_code.is_empty() {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label("2. 页面如要求输入授权码：");
                                ui.label(
                                    egui::RichText::new(&user_code).strong().color(COLOR_ACCENT),
                                );
                                if ui.button("复制").clicked() {
                                    ui.ctx().copy_text(user_code.clone());
                                }
                            });
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(egui::RichText::new("等待授权中，完成后会自动添加…").weak());
                        });
                    }
                    ui.add_space(6.0);
                    if ui.button("取消").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                        self.auth_dialog = None;
                        self.status = "已取消添加账号".to_string();
                        self.status_tone = Tone::Info;
                    }
                });
            if !open {
                cancel.store(true, Ordering::Relaxed);
                self.auth_dialog = None;
            }
        }

        // 重命名对话框。
        if let Some((id, mut label)) = self.rename_target.clone() {
            let mut open = true;
            let mut confirmed = false;
            egui::Window::new("重命名账号")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("给 {id} 起个别名（只影响本软件里的显示）："));
                    let response = egui::TextEdit::singleline(&mut label)
                        .desired_width(240.0)
                        .show(ui)
                        .response;
                    response.request_focus();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked()
                            || (response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            confirmed = true;
                        }
                        if ui.button("取消").clicked() {
                            self.rename_target = None;
                        }
                    });
                });
            if confirmed {
                self.send(
                    Request::Rename {
                        id: id.clone(),
                        label,
                    },
                    "正在重命名…".to_string(),
                );
                self.rename_target = None;
            } else if !open {
                self.rename_target = None;
            } else {
                self.rename_target = Some((id, label));
            }
        }

        // 月订阅到期日备注对话框。
        if let Some((id, mut expires_on)) = self.subscription_expiry_target.clone() {
            let mut open = true;
            let mut confirmed = false;
            egui::Window::new("月订阅到期日")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("设置 {id} 的月订阅到期日："));
                    let response = egui::TextEdit::singleline(&mut expires_on)
                        .hint_text("YYYY-MM-DD")
                        .desired_width(180.0)
                        .show(ui)
                        .response;
                    response.request_focus();
                    ui.label(egui::RichText::new("留空并保存可清除备注").small().weak());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存").clicked()
                            || (response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            confirmed = true;
                        }
                        if ui.button("取消").clicked() {
                            self.subscription_expiry_target = None;
                        }
                    });
                });
            if confirmed {
                match normalize_subscription_expiry(&expires_on) {
                    Ok(_) => {
                        self.send(
                            Request::SetSubscriptionExpiry {
                                id: id.clone(),
                                expires_on,
                            },
                            "正在保存订阅到期日…".to_string(),
                        );
                        self.subscription_expiry_target = None;
                    }
                    Err(error) => {
                        self.status = error.to_string();
                        self.status_tone = Tone::Err;
                        self.subscription_expiry_target = Some((id, expires_on));
                    }
                }
            } else if !open {
                self.subscription_expiry_target = None;
            } else if self.subscription_expiry_target.is_some() {
                self.subscription_expiry_target = Some((id, expires_on));
            }
        }

        // 删除确认弹窗。
        if let Some(id) = self.pending_delete.clone() {
            let mut open = true;
            egui::Window::new("确认删除")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("确定要从账号库删除 {id} 吗？"));
                    ui.label("Kimi Code 当前的登录文件不受影响，仅移除账号库中的凭证副本。");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("确认删除").clicked() {
                            self.pending_delete = None;
                            self.send(Request::Remove(id.clone()), format!("正在删除 {id}…"));
                        }
                        if ui.button("取消").clicked() {
                            self.pending_delete = None;
                        }
                    });
                });
            if !open {
                self.pending_delete = None;
            }
        }

        if self.busy || self.auth_dialog.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if self.auto_refresh_interval_ms > 0 {
            let interval = Duration::from_millis(self.auto_refresh_interval_ms);
            ctx.request_repaint_after(interval.saturating_sub(self.last_auto_refresh.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        account_matches_filter, account_remaining, auto_refresh_status, mask_email,
        normalize_subscription_expiry, reset_display_text, toggled_account_selection, AccountRow,
        QuotaView,
    };

    #[test]
    fn auto_refresh_status_describes_off_and_selected_interval() {
        assert_eq!(auto_refresh_status(0), "已关闭自动刷新");
        assert_eq!(auto_refresh_status(900_000), "自动刷新已设置为每 15 分钟");
    }

    #[test]
    fn email_is_masked_before_display() {
        assert_eq!(mask_email("alice@example.com"), "a***@example.com");
        assert_eq!(mask_email("x@example.com"), "***@example.com");
        assert_eq!(mask_email("not-an-email"), "***");
    }

    #[test]
    fn account_filter_matches_label_id_and_masked_email() {
        let row = AccountRow {
            id: "user-123".to_string(),
            label: "工作账号".to_string(),
            masked_email: Some("a***@example.com".to_string()),
            subscription_expires_on: None,
            routing_enabled: true,
            priority: 0,
            session_count: 0,
            active: false,
            membership: None,
            quotas: Vec::new(),
            error: None,
        };

        assert!(account_matches_filter(&row, "工作"));
        assert!(account_matches_filter(&row, "USER-123"));
        assert!(account_matches_filter(&row, "EXAMPLE.COM"));
        assert!(!account_matches_filter(&row, "personal"));
    }

    #[test]
    fn account_remaining_uses_supported_weekly_window() {
        let row = AccountRow {
            id: "user-123".to_string(),
            label: "工作账号".to_string(),
            masked_email: None,
            subscription_expires_on: None,
            routing_enabled: true,
            priority: 0,
            session_count: 0,
            active: false,
            membership: None,
            quotas: vec![QuotaView {
                window: "7天".to_string(),
                ratio: Some(0.35),
                text: "已用 35%".to_string(),
                reset: None,
            }],
            error: None,
        };

        assert_eq!(account_remaining(&row), Some(65.0));
    }

    #[test]
    fn clicking_selected_account_collapses_and_other_account_expands() {
        assert_eq!(
            toggled_account_selection(Some("account-a"), "account-a"),
            None
        );
        assert_eq!(
            toggled_account_selection(Some("account-a"), "account-b"),
            Some("account-b".to_string())
        );
    }

    #[test]
    fn reset_display_text_adds_prefix_only_once() {
        assert_eq!(reset_display_text("08-23 17:38"), "重置 08-23 17:38");
        assert_eq!(reset_display_text("重置 08-23 17:38"), "重置 08-23 17:38");
    }

    #[test]
    fn subscription_expiry_accepts_iso_date() {
        assert_eq!(
            normalize_subscription_expiry(" 2026-09-30 ").unwrap(),
            Some("2026-09-30".to_string())
        );
    }

    #[test]
    fn subscription_expiry_empty_value_clears_note() {
        assert_eq!(normalize_subscription_expiry("  ").unwrap(), None);
    }

    #[test]
    fn subscription_expiry_rejects_invalid_date() {
        assert!(normalize_subscription_expiry("2026-02-30").is_err());
        assert!(normalize_subscription_expiry("09/30/2026").is_err());
    }
}
