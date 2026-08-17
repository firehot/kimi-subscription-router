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
    AccountId, AccountRegistry, AuditEvent, AuditLog, CredentialStore, FileStore, KeyringStore,
    Provider, Quota, QuotaCache, QuotaWindow, RemovedAccounts,
};
use kimi_switch_kimi::{device_flow, KimiProvider};

mod control;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop_tray {
    use std::sync::mpsc::{channel, Receiver};

    use eframe::egui;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    pub enum TrayAction {
        Show,
        Exit,
    }

    /// 桌面状态栏图标及其事件通道；字段必须存活到程序退出。
    pub struct DesktopTray {
        _icon: TrayIcon,
        actions: Receiver<TrayAction>,
    }

    impl DesktopTray {
        pub fn new(ctx: egui::Context, icon_data: &egui::IconData) -> anyhow::Result<Self> {
            let show_item = MenuItem::with_id("show-window", "显示主窗口", true, None);
            let exit_item = MenuItem::with_id("exit-app", "退出", true, None);
            let menu = Menu::with_items(&[&show_item, &exit_item])?;
            let icon = Icon::from_rgba(icon_data.rgba.clone(), icon_data.width, icon_data.height)?;

            let tray_builder = TrayIconBuilder::new()
                .with_tooltip("Kimi Subscription Router")
                .with_icon(icon)
                .with_menu(Box::new(menu));
            #[cfg(target_os = "windows")]
            let tray_builder = tray_builder.with_menu_on_left_click(false);
            let tray_icon = tray_builder.build()?;

            let (action_tx, actions) = channel();
            let show_id = show_item.id().clone();
            let exit_id = exit_item.id().clone();
            let menu_action_tx = action_tx.clone();
            let menu_ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                let action = if event.id == show_id {
                    Some(TrayAction::Show)
                } else if event.id == exit_id {
                    Some(TrayAction::Exit)
                } else {
                    None
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
            })
        }

        pub fn try_recv(&self) -> Option<TrayAction> {
            self.actions.try_recv().ok()
        }
    }
}

/// 随程序嵌入中文字形，避免依赖目标系统的字体安装情况。
const CJK_FONT: &[u8] = include_bytes!("../assets/NotoSansCJKsc-Regular.otf");

/// 额度条配色（GitHub dark 风格）：充足 → 注意 → 紧张 → 耗尽。
const COLOR_OK: egui::Color32 = egui::Color32::from_rgb(63, 185, 80);
const COLOR_WARN: egui::Color32 = egui::Color32::from_rgb(210, 153, 34);
const COLOR_HIGH: egui::Color32 = egui::Color32::from_rgb(240, 136, 62);
const COLOR_FULL: egui::Color32 = egui::Color32::from_rgb(248, 81, 73);
const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(88, 166, 255);
const EXTRA_NICKNAME: &str = "nickname";
const EXTRA_SUBSCRIPTION_EXPIRES_ON: &str = "subscription_expires_on";
const EXTRA_ROUTING_ENABLED: &str = "routing_enabled";

fn usage_color(ratio: f32) -> egui::Color32 {
    if ratio < 0.5 {
        COLOR_OK
    } else if ratio < 0.8 {
        COLOR_WARN
    } else if ratio < 0.95 {
        COLOR_HIGH
    } else {
        COLOR_FULL
    }
}

/// 程序图标：32×32 圆角蓝底 + 双向箭头（换号），纯代码绘制、无需图片资源。
fn app_icon() -> egui::IconData {
    const N: usize = 32;
    let mut rgba = vec![0_u8; N * N * 4];
    let mut set_px = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * N + x) * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };
    // 圆角方块底（SDF 判定，半径 8）。
    let accent = [COLOR_ACCENT.r(), COLOR_ACCENT.g(), COLOR_ACCENT.b(), 255];
    for y in 0..N {
        for x in 0..N {
            let px = x as f32 + 0.5 - 16.0;
            let py = y as f32 + 0.5 - 16.0;
            let qx = px.abs() - 8.0;
            let qy = py.abs() - 8.0;
            let dist = qx.max(qy).min(0.0) + f32::max(qx, 0.0).hypot(f32::max(qy, 0.0)) - 8.0;
            if dist < 0.0 {
                set_px(x, y, accent);
            }
        }
    }
    // 双向箭头（上→右，下→左），白色。
    let white = [255_u8, 255, 255, 255];
    for y in 0..N {
        for x in 0..N {
            let fy = y as f32;
            let fx = x as f32;
            let top_bar = (10.5..=13.5).contains(&fy) && (9.0..=21.0).contains(&fx);
            let top_head = (fy - 12.0).abs() <= (24.0 - fx) && (21.0..=24.0).contains(&fx);
            let bottom_bar = (18.5..=21.5).contains(&fy) && (11.0..=23.0).contains(&fx);
            let bottom_head = (fy - 20.0).abs() <= (fx - 8.0) && (8.0..=11.0).contains(&fx);
            if top_bar || top_head || bottom_bar || bottom_head {
                set_px(x, y, white);
            }
        }
    }
    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}

fn main() -> eframe::Result<()> {
    let icon = app_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([640.0, 440.0])
            .with_icon(icon.clone()),
        ..Default::default()
    };
    // 启动主题：`KIMI_SWITCH_THEME=light` 浅色，默认深色。
    let dark = !matches!(std::env::var("KIMI_SWITCH_THEME").as_deref(), Ok("light"));
    eframe::run_native(
        "Kimi Subscription Router",
        options,
        Box::new(move |cc| {
            load_cjk_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx, dark);
            Ok(Box::new(GuiApp::new(cc.egui_ctx.clone(), dark, &icon)))
        }),
    )
}

/// egui 默认字体不含 CJK，把内置字体追加为缺失字形的回退字体。
fn load_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
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

/// 深色主题（GitHub Dark 风格调色板）。
fn dark_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut visuals = egui::Visuals::dark();
    // 背景层次：窗口 #161b22，卡片 #21262d，输入框 #0d1117。
    visuals.window_fill = Color32::from_rgb(22, 27, 34);
    visuals.panel_fill = Color32::from_rgb(22, 27, 34);
    visuals.faint_bg_color = Color32::from_rgb(33, 38, 45);
    visuals.extreme_bg_color = Color32::from_rgb(13, 17, 23);
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(48, 54, 61));
    visuals.hyperlink_color = COLOR_ACCENT;
    visuals.selection.bg_fill = COLOR_ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    let text = Color32::from_rgb(201, 209, 217);
    let border = Color32::from_rgb(48, 54, 61);
    // 非交互控件（标签、进度条槽）。
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(33, 38, 45);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(33, 38, 45);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    // 按钮三态。
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(45, 51, 59);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(45, 51, 59);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(60, 68, 77));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(56, 63, 72);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(56, 63, 72);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(110, 118, 129));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.bg_fill = Color32::from_rgb(66, 74, 85);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(66, 74, 85);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(139, 148, 158));
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    // 输入框光标/选中。
    visuals.widgets.open = visuals.widgets.inactive;
    visuals
}

/// 浅色主题（GitHub Light 风格调色板）。
fn light_visuals() -> egui::Visuals {
    use egui::{Color32, Stroke};
    let mut visuals = egui::Visuals::light();
    // 背景层次：窗口 #f6f8fa，卡片纯白，输入框/进度条槽 #e5e9f0。
    visuals.window_fill = Color32::from_rgb(246, 248, 250);
    visuals.panel_fill = Color32::from_rgb(246, 248, 250);
    visuals.faint_bg_color = Color32::WHITE;
    visuals.extreme_bg_color = Color32::from_rgb(229, 233, 240);
    visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(208, 215, 222));
    visuals.hyperlink_color = Color32::from_rgb(9, 105, 218);
    visuals.selection.bg_fill = COLOR_ACCENT;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    let text = Color32::from_rgb(31, 35, 40);
    let border = Color32::from_rgb(208, 215, 222);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(243, 245, 247);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(243, 245, 247);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(243, 245, 247);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(243, 245, 247);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(234, 238, 242);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(234, 238, 242);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(160, 170, 180));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text);
    visuals.widgets.active.bg_fill = Color32::from_rgb(224, 229, 235);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(224, 229, 235);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(140, 150, 160));
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
struct QuotaView {
    window: String,
    ratio: Option<f32>,
    text: String,
    reset: Option<String>,
}

struct AccountRow {
    id: String,
    /// 展示名：用户别名优先，否则使用 Kimi 账号昵称，最后回退到 id。
    label: String,
    /// 用户手动备注的月订阅到期日（YYYY-MM-DD）。
    subscription_expires_on: Option<String>,
    routing_enabled: bool,
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
    fn load_rows(&self) -> Vec<AccountRow> {
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
                if account.label == account.id.0 && nickname.is_none() {
                    if let Ok(Some(value)) = self
                        .runtime
                        .block_on(self.kimi.query_account_label(&account.id))
                    {
                        account.extra.insert(
                            EXTRA_NICKNAME.into(),
                            serde_json::Value::String(value.clone()),
                        );
                        let _ = self.registry.upsert(account.clone());
                        nickname = Some(value);
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
                    id: account.id.0,
                    subscription_expires_on,
                    routing_enabled,
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
        if let Err(e) = self.store.delete("kimi", id.0.as_str(), "blob") {
            tracing::warn!(err=%e, "credential store delete failed (continuing)");
        }
        if let Ok(paths) = AppPaths::resolve() {
            let mut cache = QuotaCache::load(&paths.quota_cache_file());
            cache.remove("kimi", &id.0);
            cache.save(&paths.quota_cache_file());
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
    backend.sync_local_active();

    let send_list = |backend: &Backend, message: Option<(String, Tone)>| {
        let rows = backend.load_rows();
        let _ = tx.send(Response::List { rows, message });
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
    };
    let rows = backend.load_rows();
    let snapshots = rows.iter().map(control_snapshot).collect();
    let reply_message = message.as_ref().map(|(message, _)| message.clone());
    let _ = command.reply.send(control::Reply::Accounts {
        accounts: snapshots,
        message: reply_message,
    });
    if refresh_ui {
        let _ = tx.send(Response::List { rows, message });
        ctx.request_repaint();
    }
}

fn control_snapshot(row: &AccountRow) -> control::AccountSnapshot {
    control::AccountSnapshot {
        id: row.id.clone(),
        label: row.label.clone(),
        active: row.active,
        membership: row.membership.clone(),
        subscription_expires_on: row.subscription_expires_on.clone(),
        routing_enabled: row.routing_enabled,
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

fn subscription_summary(ui: &mut egui::Ui, rows: &[AccountRow]) {
    let remaining_values = rows
        .iter()
        .filter_map(|row| {
            row.quotas
                .iter()
                .find(|quota| quota.window == "7天")
                .and_then(|quota| quota.ratio)
        })
        .map(|ratio| (1.0 - ratio).clamp(0.0, 1.0) * 100.0)
        .collect::<Vec<_>>();
    let remaining = remaining_values.iter().sum::<f32>();
    let active = rows
        .iter()
        .find(|row| row.active)
        .map(|row| row.label.as_str())
        .unwrap_or("未选择");

    let fill = if ui.visuals().dark_mode {
        egui::Color32::from_rgb(28, 33, 40)
    } else {
        egui::Color32::from_rgb(238, 242, 246)
    };
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("订阅池").small().weak());
                    ui.label(
                        egui::RichText::new(format!("{} 个已保存账号", rows.len()))
                            .size(17.0)
                            .strong(),
                    );
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("7 天总剩余").small().weak());
                    let value = if remaining_values.is_empty() {
                        "--".to_string()
                    } else {
                        format!("{remaining:.0}%")
                    };
                    let text = egui::RichText::new(value).size(17.0).strong();
                    ui.label(if remaining_values.is_empty() {
                        text
                    } else {
                        text.color(usage_color(1.0 - (remaining / 100.0).clamp(0.0, 1.0)))
                    });
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("当前账号").small().weak());
                    ui.label(egui::RichText::new(active).size(17.0).strong());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new("每 5 分钟自动刷新").small().weak());
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
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    tray: Option<desktop_tray::DesktopTray>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    exit_requested: bool,
}

impl GuiApp {
    fn new(ctx: egui::Context, dark_mode: bool, icon: &egui::IconData) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (resp_tx, resp_rx) = channel::<Response>();
        let (control_info, control_error) = match AppPaths::resolve()
            .map_err(anyhow::Error::from)
            .and_then(|paths| control::start(&paths, req_tx.clone()))
        {
            Ok(info) => (Some(info), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let tray = desktop_tray::DesktopTray::new(ctx.clone(), icon).ok();
        std::thread::Builder::new()
            .name("kimi-switch-gui-worker".into())
            .spawn(move || worker_main(ctx, req_rx, resp_tx))
            .expect("spawn worker thread");
        Self {
            to_worker: req_tx,
            from_worker: resp_rx,
            rows: Vec::new(),
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
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.busy && self.last_auto_refresh.elapsed() >= Duration::from_secs(5 * 60) {
            self.last_auto_refresh = Instant::now();
            self.send(Request::Refresh, "正在自动刷新额度…".to_string());
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            while let Some(action) = self.tray.as_ref().and_then(|tray| tray.try_recv()) {
                match action {
                    desktop_tray::TrayAction::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
                }
                if minimized {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            }
        }

        // 收集后台消息。
        while let Ok(response) = self.from_worker.try_recv() {
            match response {
                Response::List { rows, message } => {
                    self.rows = rows;
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

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                ui.heading(egui::RichText::new("Kimi Subscription Router").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.busy {
                        ui.spinner();
                    }
                    // 主题切换（深色 ↔ 浅色）。
                    let theme_label = if self.dark_mode { "浅色" } else { "深色" };
                    if ui
                        .button(theme_label)
                        .on_hover_text("切换界面主题")
                        .clicked()
                    {
                        self.dark_mode = !self.dark_mode;
                        apply_theme(ctx, self.dark_mode);
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("刷新"))
                        .clicked()
                    {
                        self.send(Request::Refresh, "正在刷新额度…".to_string());
                    }
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("导入当前账号"))
                        .clicked()
                    {
                        self.send(Request::Import, "正在导入…".to_string());
                    }
                    let add_button = egui::Button::new(
                        egui::RichText::new("＋ 添加账号").color(egui::Color32::WHITE),
                    )
                    .fill(COLOR_ACCENT);
                    if ui.add_enabled(!self.busy, add_button).clicked() {
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
                });
            });
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
                ui.label(egui::RichText::new(&self.status).small().color(color));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(info) = &self.control_info {
                        ui.label(
                            egui::RichText::new(format!("本地 API  {}", info.base_url))
                                .small()
                                .weak(),
                        )
                        .on_hover_text(format!("认证令牌保存在 {}", info.token_file.display()));
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
                    ui.label("暂无账号");
                    ui.label(
                        egui::RichText::new(
                            "点右上角「＋ 添加账号」浏览器授权添加，或先在 Kimi Code 里登录后点「导入当前账号」",
                        )
                        .weak(),
                    );
                });
                return;
            }
            if self.loaded {
                subscription_summary(ui, &self.rows);
                ui.add_space(8.0);
            }
            let mut swap_id: Option<String> = None;
            let mut delete_id: Option<String> = None;
            let mut rename_id: Option<String> = None;
            let mut subscription_expiry_id: Option<String> = None;
            let mut routing_change: Option<(String, bool)> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                // 额度条文字颜色：深色主题用白字，浅色主题用深灰（否则压在浅色槽上看不清）。
                let bar_text_color = if self.dark_mode {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(31, 35, 40)
                };
                for (idx, row) in self.rows.iter().enumerate() {
                    // 卡片：微凸底色 + 细描边 + 大圆角 + 宽内边距。
                    let card_border = ui.visuals().widgets.noninteractive.bg_stroke.color;
                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().faint_bg_color)
                        .stroke(egui::Stroke::new(1.0_f32, card_border))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            // 第一行：序号/激活标记/别名/会员徽章/ID + 右侧操作按钮。
                            ui.horizontal(|ui| {
                                account_avatar(ui, row);
                                if row.active {
                                    ui.label(
                                        egui::RichText::new("● 当前").color(COLOR_OK).strong(),
                                    );
                                } else {
                                    ui.label(egui::RichText::new(format!("{}", idx + 1)).weak());
                                }
                                ui.label(egui::RichText::new(&row.label).strong().size(15.0));
                                if let Some(membership) = &row.membership {
                                    egui::Frame::new()
                                        .stroke(egui::Stroke::new(1.0_f32, COLOR_ACCENT))
                                        .corner_radius(egui::CornerRadius::same(4))
                                        .inner_margin(egui::Margin::symmetric(6, 1))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(membership)
                                                    .small()
                                                    .color(COLOR_ACCENT),
                                            );
                                        });
                                }
                                if row.label != row.id {
                                    ui.label(egui::RichText::new(&row.id).small().weak());
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::Button::new(
                                                    egui::RichText::new("删除")
                                                        .small()
                                                        .color(COLOR_FULL),
                                                ),
                                            )
                                            .clicked()
                                        {
                                            delete_id = Some(row.id.clone());
                                        }
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::Button::new("重命名").small(),
                                            )
                                            .clicked()
                                        {
                                            rename_id = Some(row.id.clone());
                                        }
                                        if ui
                                            .add_enabled(
                                                !self.busy,
                                                egui::Button::new("到期日").small(),
                                            )
                                            .clicked()
                                        {
                                            subscription_expiry_id = Some(row.id.clone());
                                        }
                                        if !row.active
                                            && ui
                                                .add_enabled(
                                                    !self.busy,
                                                    egui::Button::new(
                                                        egui::RichText::new("切换")
                                                            .small()
                                                            .color(egui::Color32::WHITE),
                                                    )
                                                    .fill(COLOR_ACCENT),
                                                )
                                                .clicked()
                                        {
                                            swap_id = Some(row.id.clone());
                                        }
                                    },
                                );
                            });
                            ui.horizontal(|ui| {
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
                                if let Some(expires_on) = &row.subscription_expires_on {
                                    ui.separator();
                                    ui.label(egui::RichText::new("月订阅到期").small().weak());
                                    ui.label(egui::RichText::new(expires_on).small().strong());
                                }
                            });
                            ui.add_space(4.0);
                            // 额度条：固定列宽（窗口名 | 进度条 | 重置时间），保证多行对齐。
                            const LABEL_W: f32 = 52.0;
                            const RESET_W: f32 = 96.0;
                            if let Some(err) = &row.error {
                                ui.label(
                                    egui::RichText::new(format!("额度查询失败: {err}"))
                                        .small()
                                        .color(COLOR_FULL),
                                );
                            } else if row.quotas.is_empty() {
                                ui.label(egui::RichText::new("额度: n/a").small().weak());
                            } else {
                                for q in &row.quotas {
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [LABEL_W, 16.0],
                                            egui::Label::new(
                                                egui::RichText::new(&q.window).small().weak(),
                                            ),
                                        );
                                        let ratio = q.ratio.unwrap_or(0.0).clamp(0.0, 1.0);
                                        let bar_w = (ui.available_width() - RESET_W - 8.0)
                                            .clamp(120.0, f32::INFINITY);
                                        let bar = egui::ProgressBar::new(ratio)
                                            .desired_width(bar_w)
                                            .desired_height(14.0)
                                            .corner_radius(egui::CornerRadius::same(7))
                                            .fill(usage_color(ratio))
                                            .text(
                                                egui::RichText::new(&q.text)
                                                    .small()
                                                    .color(bar_text_color),
                                            );
                                        ui.add(bar);
                                        if let Some(reset) = &q.reset {
                                            ui.add_sized(
                                                [RESET_W, 16.0],
                                                egui::Label::new(
                                                    egui::RichText::new(reset).small().strong(),
                                                )
                                                .truncate(),
                                            );
                                        }
                                    });
                                    ui.add_space(2.0);
                                }
                            }
                        });
                    ui.add_space(4.0);
                }
            });
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
                    ui.set_min_width(420.0);
                    if waiting_link {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在获取授权链接…");
                        });
                    } else {
                        ui.label("1. 复制下面的链接，到浏览器打开并登录授权：");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut url.as_str())
                                    .desired_width(280.0)
                                    .font(egui::TextStyle::Monospace),
                            );
                            if ui.button("复制链接").clicked() {
                                ui.ctx().copy_text(url.clone());
                            }
                            if ui.button("打开浏览器").clicked() {
                                let _ = std::process::Command::new("explorer").arg(&url).spawn();
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_subscription_expiry;

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
