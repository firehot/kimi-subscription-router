# 本机控制 API

GUI 启动时会创建只绑定回环地址的控制服务。该接口用于本机脚本或桌面集成，
不会开放到局域网，也不会返回账号凭证。

## 发现服务

运行时状态目录中包含：

- `control-endpoint.json`：包含 `baseUrl`，例如 `http://127.0.0.1:43821/v1`。
- `control-token`：随机 256 位令牌；Unix 下仅当前用户可读。

状态目录遵循系统用户目录约定。设置绝对路径环境变量 `KIMI_SWITCH_HOME` 时，
两个文件位于 `$KIMI_SWITCH_HOME/data/state/`。

除 `GET /v1/health` 外，请求必须携带：

```text
X-Kimi-Router-Token: <control-token 文件内容>
```

## 接口

```http
GET /v1/health

GET /v1/accounts
X-Kimi-Router-Token: ...

GET /v1/router/status
X-Kimi-Router-Token: ...

GET /v1/router/sessions
X-Kimi-Router-Token: ...

GET /v1/router/sessions/{session-id}
X-Kimi-Router-Token: ...

GET /v1/events
X-Kimi-Router-Token: ...
Accept: text/event-stream

POST /v1/refresh
X-Kimi-Router-Token: ...

POST /v1/accounts/{account-id}/activate
X-Kimi-Router-Token: ...

PATCH /v1/accounts/{account-id}
X-Kimi-Router-Token: ...
Content-Type: application/json

{"label":"工作号","priority":50,"routingEnabled":true,"subscriptionExpiresOn":"2026-09-30"}

DELETE /v1/accounts/{account-id}
X-Kimi-Router-Token: ...
```

激活操作复用 GUI/CLI 的原子写入和快照回滚路径，不依赖额度查询。账号 ID 只允许
ASCII 字母、数字、点、下划线和连字符。

`PATCH` 的字段均可选；`subscriptionExpiresOn` 传空字符串会清除到期日备注。
`priority` 越小越优先，仅在路由评分和会话数均相同时作为决胜条件。删除操作复用
GUI 的账号墓碑、凭证清理和额度缓存清理流程。路由状态接口只返回非敏感的
`sessionId -> accountId` 归属和时间，不返回会话内容、工作目录、MCP 参数或凭证。
账号列表中的 `email` 已在服务端掩码，不返回完整邮箱。
`/v1/events` 使用 Server-Sent Events，在路由运行状态或会话归属变化时发送
`router-status` 事件；空闲连接发送 keep-alive 注释，不会阻塞其他控制请求。

## 示例

```bash
router_state_dir="$KIMI_SWITCH_HOME/data/state"
router_base=$(jq -r .baseUrl "$router_state_dir/control-endpoint.json")
router_token=$(tr -d '\n' < "$router_state_dir/control-token")

curl -fsS \
  -H "X-Kimi-Router-Token: $router_token" \
  "$router_base/accounts"
```

不要把令牌放入命令历史、仓库、日志或远程请求。
