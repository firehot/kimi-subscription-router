# ACP 多账号路由

`kimi-subscription-router` 是一个 stdio ACP multiplexer。ACP 客户端只启动这一个进程，
路由器为每个可用账号启动一个官方 `kimi acp` 子进程。

```text
ACP client
    |
    v
kimi-subscription-router
    |-- account A -> isolated KIMI_CODE_HOME -> kimi acp
    |-- account B -> isolated KIMI_CODE_HOME -> kimi acp
    `-- sessionId -> persisted account owner
```

## 路由规则

新会话优先使用在 7 天窗口重置前更需要消耗余量、同时仍有 5 小时窗口容量的账号。
分数接近时，依次使用当前粘性会话数、账号 priority 和稳定账号顺序作为 tie-breaker。
额度缓存写入超过 10 分钟后会降级为未知；该账号仍可使用，但排在已确认有容量的
账号之后。

后续请求始终发给会话 owner。路由器在以下情况发起故障转移：

- prompt 前的额度缓存显示 owner 已耗尽；
- owner 被用户取消「参与路由」；
- `session/prompt` 返回明确的 quota/rate-limit JSON-RPC error。

故障转移先对旧 owner 调用 `session/close`，再在新 owner 上调用
`session/resume`，成功后才重试原 prompt。若没有可用账号，返回错误码 `-32042`，
`error.data.nextReset` 包含已知的最近重置时间。

账号注册表每 2 秒同步一次。新增账号的 ACP 子进程完成 `initialize` 后才进入候选池；
删除账号时先停止并等待对应子进程退出，再清除隔离凭证副本。子进程重新启动时使用
代际编号过滤旧进程迟到的响应，避免误伤新进程。

初始化与内部 `session/close` / `session/resume` 动作最长等待 15 秒。等待中的子进程
退出或超时会结束原请求并返回路由器错误。

## 本地数据

- `router-state.json`：会话 owner，不含凭证。
- `router/accounts/<hash>/kimi-home/`：每账号独立配置和 OAuth 文件。
- `router/sessions/`：官方 Kimi Code 会话文件，由所有隔离进程共享。
- `router.lock`：阻止两个 multiplexer 同时运行。

账号目录名使用账号 ID 的 SHA-256 摘要，不在路径中暴露原始 ID。Unix 下账号目录为
`0700`、凭证与状态文件为 `0600`。路由器不实现 token refresh；刷新完全由官方
Kimi Code 子进程及其官方锁协议执行，路由器只吸收原子轮换后的完整凭证文件。

## 当前边界

- 已验证 Kimi Code CLI `0.36.1` 的 `initialize`、`session/new`、`session/list`、
  `session/close`、`session/resume` 和 `session/delete`。
- 路由器只把明确的 prompt-level quota JSON-RPC error 识别为响应式故障转移信号；
  普通工具错误不会触发换号。
- 会话 owner 会持久化，但包含工作目录和 MCP 参数的 resume context 只保存在当前进程
  内存中，避免把潜在敏感配置复制到状态文件。重启后客户端需先执行 `session/load` 或
  `session/resume`，该会话才能自动故障转移。
- 尚未发送真实模型 prompt 验收额度耗尽响应；当前覆盖缓存预判、明确 quota error 分类
  和官方 ACP 会话生命周期。
- Windows 优先创建目录符号链接；普通用户没有符号链接权限时自动回退为目录联接，
  无需启用 Developer Mode。
- 不复制用户自定义 provider、第三方 endpoint、用户级 MCP 配置或内联 API key。
- 不自动写入 Zed、JetBrains 或其他 ACP 客户端配置；接入方式见 README。
