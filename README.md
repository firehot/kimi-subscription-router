# Kimi Subscription Router

<p align="center">
  <img src="crates/gui/assets/kimi-brand-icon.png" width="88" alt="Kimi Subscription Router 图标">
</p>

Kimi Subscription Router 是一款面向 [Kimi Code](https://www.kimi.com/) 用户的非官方多账号管理工具，支持 Windows 和 macOS。

当前版本：**0.1**（Cargo 版本 `0.1.0`）。

它可以保存多个 Kimi Code 账号、查看 5 小时与 7 天额度、快速切换当前账号，并可选地把多个订阅接入同一个 ACP 入口。所有账号凭证只保存在当前电脑上。

> 本项目与 Moonshot AI / Kimi 官方无关。请遵守 Kimi 服务条款并妥善管理自己的账号。

![Kimi Subscription Router 深色界面](docs/screenshot-dark.png)

<details>
<summary>查看浅色界面</summary>

![Kimi Subscription Router 浅色界面](docs/screenshot-light.png)

</details>

## 主要功能

- 通过浏览器授权添加 Kimi Code 账号。
- 导入本机 Kimi Code 当前登录的账号。
- 查看每个账号的 5 小时和 7 天额度、已用比例、剩余比例及重置时间。
- 一键切换当前使用账号，切换过程使用原子写入和失败回滚。
- 给账号设置别名和订阅到期日备注。
- 搜索账号，并显示账号名称、掩码邮箱、会员等级和路由会话数。
- 为每个账号单独设置是否参与 ACP 自动路由。
- 支持深色、浅色主题，并在重启后保留选择。
- 关闭或最小化窗口后驻留 Windows 系统托盘或 macOS 菜单栏。
- 可在状态栏菜单中设置开机启动和自动刷新间隔。
- 提供可选的命令行工具、本机控制 API 和多账号 ACP 路由器。

## 安装

### GitHub Releases

正式版本在 GitHub Releases 提供：

- Windows x86-64 便携 ZIP。
- Windows x86-64 用户级安装程序。
- macOS Apple Silicon 与 Intel 的应用 ZIP 和 DMG。
- 各平台 SHA-256 校验清单。

### macOS 从源码安装

需要 Rust 1.80 或更高版本：

```bash
git clone https://github.com/firehot/kimi-subscription-router.git
cd kimi-subscription-router
./scripts/install-macos.sh
```

应用会安装到 `~/Applications/Kimi Subscription Router.app`。升级时，安装脚本会备份旧应用；账号和凭证保存在应用包外，不会因为替换应用而丢失。

### 从源码构建

```bash
cargo build --release
```

生成面向用户的 Windows GUI：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\package-windows.ps1 release
```

主要产物：

| 文件 | 用途 |
|---|---|
| `target/release/Kimi Subscription Router.exe` | Windows 图形界面 |
| `target/release/Kimi Subscription Router.app` | macOS 图形界面 |
| `target/release/Kimi Subscription Router CLI.exe` | Windows 命令行账号管理工具 |
| `target/release/Kimi Subscription Router CLI` | macOS 发布包中的命令行账号管理工具 |
| `target/release/kimi-subscription-router` | ACP 多账号路由入口；Windows 下为 `.exe` |

macOS 不应直接双击裸可执行文件，请打包为 `.app`：

```bash
./scripts/package-macos-app.sh release
open "target/release/Kimi Subscription Router.app"
```

## 快速开始

### 1. 添加账号

打开应用后，可以使用以下任一方式：

- 点击 **＋ 添加账号**，在浏览器中完成 Kimi 授权。
- 点击 **导入当前账号**，保存本机 Kimi Code 当前登录的账号。

浏览器授权只会把新账号加入账号库，不会自动替换当前使用账号。

### 2. 查看额度

账号列表右侧显示该账号的 7 天剩余比例。点击账号标题行可以展开详情，再次点击同一标题行可以收起。

展开后会显示：

- 5 小时与 7 天窗口。
- 已使用和剩余百分比。
- 蓝色已用进度与灰色未用进度。
- 当前窗口重置时间。
- 订阅到期日、路由状态和会话数量。

### 3. 切换账号

当前正在使用的账号始终带有蓝色描边和 **当前** 标记。

展开另一个账号，点击 **切换到此账号**。切换不依赖额度查询，即使网络不可用仍可执行。切换完成后，新账号会成为高亮账号。

### 4. 管理账号

展开账号后点击 **⋯**：

| 操作 | 说明 |
|---|---|
| 重命名 | 设置只在本工具中显示的账号别名 |
| 设置到期日 | 记录订阅到期日期；不会修改 Kimi 订阅 |
| 删除账号 | 从本工具账号库移除，操作前会再次确认 |

删除账号不会修改 Kimi Code 当前登录文件。如果删除的是当前使用账号，Kimi Code 仍会保持该账号，直到手动切换或重新登录。

## 状态栏与设置

关闭或最小化主窗口后，应用会继续驻留系统状态栏：

- Windows：系统托盘。
- macOS：菜单栏；主窗口隐藏时程序坞图标也会隐藏。

从状态栏菜单可以：

- 显示主窗口。
- 开启或关闭开机启动。
- 关闭自动刷新，或设置为每 2、5、15、30、60 分钟刷新。
- 完全退出应用。

窗口右上角的太阳/月亮按钮可以切换主题；主题设置会在重启后保留。按 `Cmd/Ctrl+R` 可以刷新额度，按 `Esc` 可以关闭当前对话框。

## ACP 多账号路由（可选）

高级用户可以让 ACP 客户端只连接一个 `kimi-subscription-router` 进程，再由路由器把新会话分配给可用账号。

路由特点：

- 新会话根据额度、重置时间、优先级和已有会话数选择账号。
- 后续请求保持原账号归属，避免会话上下文丢失。
- 账号耗尽或停止参与路由时，可以恢复到其他可用账号。
- 每个账号使用独立的 Kimi Code 运行目录和 OAuth 文件。

在 GUI 中添加账号并勾选 **参与路由**，然后将 ACP 客户端的自定义 agent command 设置为 `kimi-subscription-router` 的绝对路径。详细配置、状态文件和限制见 [ACP 路由文档](docs/ACP-ROUTER.md)。

## 命令行（可选）

macOS：

```bash
"./Kimi Subscription Router CLI"                   # 查看账号与额度
"./Kimi Subscription Router CLI" login kimi        # 导入当前 Kimi Code 账号
"./Kimi Subscription Router CLI" swap <编号或 ID>  # 切换账号
"./Kimi Subscription Router CLI" rm <编号或 ID>    # 删除账号
```

Windows PowerShell：

```powershell
& ".\Kimi Subscription Router CLI.exe"                   # 查看账号与额度
& ".\Kimi Subscription Router CLI.exe" login kimi        # 导入当前 Kimi Code 账号
& ".\Kimi Subscription Router CLI.exe" swap <编号或 ID>  # 切换账号
& ".\Kimi Subscription Router CLI.exe" rm <编号或 ID>    # 删除账号
```

GUI 和 CLI 使用同一份本机账号库，可以混合使用。

## 数据与安全

- OAuth token 只写入当前用户的本机应用数据目录。
- Unix 系统下凭证文件使用仅当前用户可读写的权限。
- Kimi OAuth 和额度请求只发送到 `auth.kimi.com` 与 `api.kimi.com`。
- 项目不包含遥测，不会上传账号列表、额度或使用记录。
- refresh token 刷新与官方 Kimi Code 客户端使用相同的锁协议协调。
- 手动切换账号不依赖网络，也不依赖额度接口是否可用。
- 本机控制 API 只监听随机的 `127.0.0.1` 端口，敏感接口需要本机随机令牌。

可以设置绝对路径环境变量 `KIMI_SWITCH_HOME`，把配置、数据、状态和缓存统一放到指定目录。控制 API 的接口说明见 [CONTROL-API.md](docs/CONTROL-API.md)。

## 常见问题

### 首次启动显示额度查询 401

应用会尝试与官方 Kimi Code 客户端协调刷新 token，然后重新查询额度。如果持续失败，请在浏览器重新授权该账号，或在 Kimi Code 中重新登录后使用 **导入当前账号**。

### 切换后需要重启 Kimi Code 吗？

通常不需要。工具会原子替换 Kimi Code 使用的本机凭证文件。已经运行的会话是否立即读取新凭证取决于 Kimi Code 当前会话状态；新请求或新会话会使用切换后的账号。

### 为什么账号列表右侧显示 `--`？

该账号暂时没有可验证的 7 天额度数据。展开账号可以查看具体错误；手动切换仍然可用。

### 关闭窗口后为什么应用仍在运行？

关闭窗口默认隐藏到系统状态栏，以便继续自动刷新和提供本机控制服务。请从状态栏菜单选择 **退出** 才会完全结束进程。

### 删除或升级应用会删除账号吗？

不会。账号库和凭证保存在应用包或可执行文件之外。删除这些本机数据前，请先确认不再需要保存的账号。

## 致谢

本项目基于原始项目继续开发，并在设计和实现过程中参考、受益于以下开源项目：

- [yuan2627/kimi-switch](https://github.com/yuan2627/kimi-switch)：本项目的原始项目，提供了 Kimi Code 多账号保存、额度查询、账号切换以及 Rust GUI/CLI 的基础实现。
- [MoonshotAI/kimi-cli](https://github.com/MoonshotAI/kimi-cli)：Kimi Code 官方 CLI。本项目以其本机凭证格式、OAuth 行为和 ACP 能力作为兼容目标。
- [b-nnett/codex-subscription-router](https://github.com/b-nnett/codex-subscription-router)：多订阅路由、会话粘性、账号选择器和用量展示的产品设计为本项目提供了重要参考。

感谢上述项目的作者与贡献者。本项目是独立实现，相关项目的名称和商标归各自权利人所有。

## 许可证

本项目派生自采用 MIT License 的 [yuan2627/kimi-switch](https://github.com/yuan2627/kimi-switch)，保留了原项目的版权声明和许可文本，并继续以 [MIT License](LICENSE) 开源。第三方项目与素材的归属说明见 [NOTICE.md](NOTICE.md)。

GUI 内嵌的 Noto Sans SC 字体采用 [SIL Open Font License 1.1](crates/gui/assets/OFL.txt)。Kimi 品牌图像来源和使用说明见 [KIMI-BRAND-SOURCE.md](crates/gui/assets/KIMI-BRAND-SOURCE.md)。
