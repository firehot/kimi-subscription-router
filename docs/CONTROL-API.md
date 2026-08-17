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

POST /v1/refresh
X-Kimi-Router-Token: ...

POST /v1/accounts/{account-id}/activate
X-Kimi-Router-Token: ...
```

激活操作复用 GUI/CLI 的原子写入和快照回滚路径，不依赖额度查询。账号 ID 只允许
ASCII 字母、数字、点、下划线和连字符。

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
