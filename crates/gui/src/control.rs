//! 仅供本机客户端使用的控制接口。
//!
//! 服务只绑定回环地址，除健康检查外的请求必须携带随机令牌。令牌只写入
//! 用户状态目录中的私有文件，不经接口返回，也不写日志。

use std::fs;
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Sender};
use std::time::Duration;

use anyhow::Context as _;
use kimi_switch_core::paths::AppPaths;
use kimi_switch_core::router_status::load_router_status_from;
use rand::rngs::OsRng;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use tiny_http::{
    Header, Method, Request as HttpRequest, Response as HttpResponse, Server, StatusCode,
};

use crate::Request;

const TOKEN_HEADER: &str = "X-Kimi-Router-Token";
const MAX_BODY_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    pub window: String,
    pub used_ratio: Option<f32>,
    pub text: String,
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub active: bool,
    pub membership: Option<String>,
    pub subscription_expires_on: Option<String>,
    pub routing_enabled: bool,
    pub priority: i32,
    pub session_count: usize,
    pub quotas: Vec<QuotaSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum Action {
    List,
    Refresh,
    Activate(String),
    Update {
        id: String,
        label: Option<String>,
        priority: Option<i32>,
        routing_enabled: Option<bool>,
        subscription_expires_on: Option<String>,
    },
    Remove(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountUpdate {
    label: Option<String>,
    priority: Option<i32>,
    routing_enabled: Option<bool>,
    subscription_expires_on: Option<String>,
}

#[derive(Debug)]
pub struct Command {
    pub action: Action,
    pub reply: std::sync::mpsc::SyncSender<Reply>,
}

#[derive(Debug)]
pub enum Reply {
    Accounts {
        accounts: Vec<AccountSnapshot>,
        message: Option<String>,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Info {
    pub base_url: String,
    pub token_file: std::path::PathBuf,
}

pub fn start(paths: &AppPaths, requests: Sender<Request>) -> anyhow::Result<Info> {
    let token_file = paths.control_token_file();
    let token = load_or_create_token(&token_file)?;
    let server = Server::http("127.0.0.1:0")
        .map_err(|error| anyhow::anyhow!("启动本地控制服务失败: {error}"))?;
    let base_url = format!("http://{}/v1", server.server_addr());
    write_endpoint(&paths.control_endpoint_file(), &base_url)?;
    let router_state = paths.router_state_file();
    let router_lock = paths.router_lock_file();

    let thread_token = token.clone();
    std::thread::Builder::new()
        .name("kimi-router-control".into())
        .spawn(move || serve(server, thread_token, requests, router_state, router_lock))
        .context("启动本地控制服务线程失败")?;

    Ok(Info {
        base_url,
        token_file,
    })
}

fn serve(
    server: Server,
    token: String,
    requests: Sender<Request>,
    router_state: PathBuf,
    router_lock: PathBuf,
) {
    for request in server.incoming_requests() {
        let token = token.clone();
        let requests = requests.clone();
        let router_state = router_state.clone();
        let router_lock = router_lock.clone();
        let _ = std::thread::Builder::new()
            .name("kimi-router-control-request".into())
            .spawn(move || handle(request, &token, &requests, &router_state, &router_lock));
    }
}

fn handle(
    mut request: HttpRequest,
    token: &str,
    requests: &Sender<Request>,
    router_state: &Path,
    router_lock: &Path,
) {
    if request.method() == &Method::Options {
        respond_json(request, 204, serde_json::json!({}));
        return;
    }

    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or(request.url())
        .to_string();
    if path == "/v1/health" && request.method() == &Method::Get {
        respond_json(request, 200, serde_json::json!({"ok": true}));
        return;
    }
    if !authorized(&request, token) {
        respond_json(request, 401, serde_json::json!({"error": "unauthorized"}));
        return;
    }

    if request.method() == &Method::Get && path == "/v1/events" {
        respond_events(
            request,
            router_state.to_path_buf(),
            router_lock.to_path_buf(),
        );
        return;
    }

    let requested_session = path
        .strip_prefix("/v1/router/sessions/")
        .filter(|id| valid_session_id(id));
    if request.method() == &Method::Get
        && (matches!(path.as_str(), "/v1/router/status" | "/v1/router/sessions")
            || requested_session.is_some())
    {
        match load_router_status_from(router_state, router_lock) {
            Ok(status) if path == "/v1/router/status" => {
                respond_json(request, 200, serde_json::json!({"router": status}));
            }
            Ok(status) if path == "/v1/router/sessions" => {
                respond_json(
                    request,
                    200,
                    serde_json::json!({"sessions": status.sessions}),
                );
            }
            Ok(status) => match status
                .sessions
                .into_iter()
                .find(|session| Some(session.session_id.as_str()) == requested_session)
            {
                Some(session) => {
                    respond_json(request, 200, serde_json::json!({"session": session}));
                }
                None => {
                    respond_json(
                        request,
                        404,
                        serde_json::json!({"error": "session not found"}),
                    );
                }
            },
            Err(error) => {
                respond_json(
                    request,
                    500,
                    serde_json::json!({"error": format!("读取路由状态失败: {error}")}),
                );
            }
        }
        return;
    }

    let action = match (request.method(), path.as_str()) {
        (&Method::Get, "/v1/accounts") => Action::List,
        (&Method::Post, "/v1/refresh") => Action::Refresh,
        (&Method::Post, _) => match path
            .strip_prefix("/v1/accounts/")
            .and_then(|value| value.strip_suffix("/activate"))
        {
            Some(id) if valid_account_id(id) => Action::Activate(id.to_string()),
            _ => {
                drain_body(&mut request);
                respond_json(request, 404, serde_json::json!({"error": "not found"}));
                return;
            }
        },
        (&Method::Patch, _) => match path.strip_prefix("/v1/accounts/") {
            Some(id) if valid_account_id(id) => {
                let update = match read_json::<AccountUpdate>(&mut request) {
                    Ok(value) => value,
                    Err(error) => {
                        respond_json(request, 400, serde_json::json!({"error": error}));
                        return;
                    }
                };
                Action::Update {
                    id: id.to_string(),
                    label: update.label,
                    priority: update.priority,
                    routing_enabled: update.routing_enabled,
                    subscription_expires_on: update.subscription_expires_on,
                }
            }
            _ => {
                drain_body(&mut request);
                respond_json(request, 404, serde_json::json!({"error": "not found"}));
                return;
            }
        },
        (&Method::Delete, _) => match path.strip_prefix("/v1/accounts/") {
            Some(id) if valid_account_id(id) => Action::Remove(id.to_string()),
            _ => {
                drain_body(&mut request);
                respond_json(request, 404, serde_json::json!({"error": "not found"}));
                return;
            }
        },
        _ => {
            drain_body(&mut request);
            respond_json(
                request,
                405,
                serde_json::json!({"error": "method not allowed"}),
            );
            return;
        }
    };
    drain_body(&mut request);

    let (reply_tx, reply_rx) = sync_channel(1);
    if requests
        .send(Request::Control(Command {
            action,
            reply: reply_tx,
        }))
        .is_err()
    {
        respond_json(
            request,
            503,
            serde_json::json!({"error": "application worker unavailable"}),
        );
        return;
    }

    match reply_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Reply::Accounts { accounts, message }) => respond_json(
            request,
            200,
            serde_json::json!({"accounts": accounts, "message": message}),
        ),
        Ok(Reply::Error(error)) => respond_json(request, 400, serde_json::json!({"error": error})),
        Err(_) => respond_json(
            request,
            504,
            serde_json::json!({"error": "application worker timeout"}),
        ),
    }
}

fn authorized(request: &HttpRequest, expected: &str) -> bool {
    let Some(provided) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv(TOKEN_HEADER))
        .map(|header| header.value.as_str())
    else {
        return false;
    };
    provided.len() == expected.len() && provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn drain_body(request: &mut HttpRequest) {
    let mut sink = String::new();
    let _ = request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut sink);
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut HttpRequest) -> Result<T, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut body)
        .map_err(|error| format!("读取请求失败: {error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("JSON 格式无效: {error}"))
}

fn respond_json(request: HttpRequest, status: u16, value: serde_json::Value) {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    let mut response = HttpResponse::from_string(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", "application/json; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        ("X-Content-Type-Options", "nosniff"),
    ] {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

fn respond_events(request: HttpRequest, state_path: PathBuf, lock_path: PathBuf) {
    let stream = RouterEventStream::new(state_path, lock_path);
    let headers = [
        ("Content-Type", "text/event-stream; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Connection", "keep-alive"),
        ("X-Content-Type-Options", "nosniff"),
    ]
    .into_iter()
    .filter_map(|(name, value)| Header::from_bytes(name, value).ok())
    .collect();
    let response = HttpResponse::new(StatusCode(200), headers, stream, None, None);
    let _ = request.respond(response);
}

struct RouterEventStream {
    state_path: PathBuf,
    lock_path: PathBuf,
    pending: Cursor<Vec<u8>>,
    previous: Option<String>,
    first: bool,
}

impl RouterEventStream {
    fn new(state_path: PathBuf, lock_path: PathBuf) -> Self {
        Self {
            state_path,
            lock_path,
            pending: Cursor::new(Vec::new()),
            previous: None,
            first: true,
        }
    }

    fn refill(&mut self) {
        if !self.first {
            std::thread::sleep(Duration::from_secs(2));
        }
        self.first = false;
        let next = match load_router_status_from(&self.state_path, &self.lock_path) {
            Ok(status) => serde_json::to_string(&serde_json::json!({
                "type": "router-status",
                "router": status,
            }))
            .unwrap_or_else(|_| "{}".into()),
            Err(error) => serde_json::to_string(&serde_json::json!({
                "type": "router-error",
                "error": error.to_string(),
            }))
            .unwrap_or_else(|_| "{}".into()),
        };
        let body = if self.previous.as_deref() == Some(next.as_str()) {
            ": keep-alive\n\n".to_string()
        } else {
            self.previous = Some(next.clone());
            format!("event: router-status\ndata: {next}\n\n")
        };
        self.pending = Cursor::new(body.into_bytes());
    }
}

impl std::io::Read for RouterEventStream {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.position() as usize >= self.pending.get_ref().len() {
            self.refill();
        }
        self.pending.read(output)
    }
}

fn load_or_create_token(path: &Path) -> anyhow::Result<String> {
    if let Ok(value) = fs::read_to_string(path) {
        let value = value.trim();
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            restrict_permissions(path)?;
            return Ok(value.to_string());
        }
    }

    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, format!("{token}\n"))
        .with_context(|| format!("写入控制令牌失败: {}", temporary.display()))?;
    restrict_permissions(&temporary)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("安装控制令牌失败: {}", path.display()))?;
    restrict_permissions(path)?;
    Ok(token)
}

fn write_endpoint(path: &Path, base_url: &str) -> anyhow::Result<()> {
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "baseUrl": base_url,
        "tokenFile": "control-token"
    }))?;
    fs::write(path, body).with_context(|| format!("写入控制服务地址失败: {}", path.display()))
}

fn restrict_permissions(path: &Path) -> anyhow::Result<()> {
    kimi_switch_core::private_fs::restrict_file(path)
        .with_context(|| format!("设置私有权限失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::mpsc::channel;
    use std::thread;
    use std::time::Duration;

    use kimi_switch_core::paths::AppPaths;

    #[cfg(unix)]
    use super::load_or_create_token;
    use super::{start, valid_account_id, Action, Reply, RouterEventStream};

    #[test]
    fn account_id_accepts_safe_path_segment() {
        assert!(valid_account_id("user-01_example.test"));
    }

    #[test]
    fn account_id_rejects_path_traversal_and_encoding() {
        assert!(!valid_account_id("../credentials"));
        assert!(!valid_account_id("user%2Fother"));
        assert!(!valid_account_id("user/other"));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("control-token");
        let token = load_or_create_token(&path).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn health_is_public_but_accounts_require_token() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_dir: temp.path().join("config"),
            data_dir: temp.path().join("data"),
            state_dir: temp.path().join("state"),
            cache_dir: temp.path().join("cache"),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let (request_tx, _request_rx) = channel();
        let info = start(&paths, request_tx).unwrap();
        let address = info
            .base_url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/v1")
            .unwrap();

        let health = raw_get(address, "/v1/health");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");

        let accounts = raw_get(address, "/v1/accounts");
        assert!(accounts.starts_with("HTTP/1.1 401"), "{accounts}");
        assert!(accounts.contains("unauthorized"), "{accounts}");
    }

    #[test]
    fn router_status_and_session_owner_require_token_and_return_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(
            paths.router_state_file(),
            r#"{"version":1,"sessions":{"session-a":{"account_id":"account-a","assigned_at":"2026-08-18T00:00:00Z"}}}"#,
        )
        .unwrap();
        let (request_tx, _request_rx) = channel();
        let info = start(&paths, request_tx).unwrap();
        let token = std::fs::read_to_string(&info.token_file).unwrap();
        let address = server_address(&info.base_url);

        let status = raw_request(address, "GET", "/v1/router/status", token.trim(), "");
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert!(status.contains("\"sessionCount\":1"), "{status}");

        let owner = raw_request(
            address,
            "GET",
            "/v1/router/sessions/session-a",
            token.trim(),
            "",
        );
        assert!(owner.starts_with("HTTP/1.1 200"), "{owner}");
        assert!(owner.contains("\"accountId\":\"account-a\""), "{owner}");
    }

    #[test]
    fn event_stream_starts_with_router_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("router-state.json");
        let lock = temp.path().join("router.lock");
        std::fs::write(&state, r#"{"version":1,"sessions":{}}"#).unwrap();
        let mut stream = RouterEventStream::new(state, lock);
        let mut output = [0_u8; 2048];
        let count = stream.read(&mut output).unwrap();
        let event = String::from_utf8_lossy(&output[..count]);
        assert!(event.starts_with("event: router-status\n"), "{event}");
        assert!(event.contains("\"sessionCount\":0"), "{event}");
    }

    #[test]
    fn patch_account_dispatches_validated_update() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let (request_tx, request_rx) = channel();
        let info = start(&paths, request_tx).unwrap();
        let token = std::fs::read_to_string(&info.token_file).unwrap();
        let address = server_address(&info.base_url).to_string();
        let client = thread::spawn(move || {
            raw_request(
                &address,
                "PATCH",
                "/v1/accounts/account-a",
                token.trim(),
                r#"{"label":"工作号","priority":7,"routingEnabled":false}"#,
            )
        });

        let command = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let crate::Request::Control(command) = command else {
            panic!("unexpected GUI request");
        };
        match command.action {
            Action::Update {
                id,
                label,
                priority,
                routing_enabled,
                ..
            } => {
                assert_eq!(id, "account-a");
                assert_eq!(label.as_deref(), Some("工作号"));
                assert_eq!(priority, Some(7));
                assert_eq!(routing_enabled, Some(false));
            }
            other => panic!("unexpected action: {other:?}"),
        }
        command
            .reply
            .send(Reply::Accounts {
                accounts: Vec::new(),
                message: Some("ok".into()),
            })
            .unwrap();
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
        }
    }

    fn server_address(base_url: &str) -> &str {
        base_url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/v1")
            .unwrap()
    }

    fn raw_get(address: &str, path: &str) -> String {
        raw_request(address, "GET", path, "", "")
    }

    fn raw_request(address: &str, method: &str, path: &str, token: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        let token_header = if token.is_empty() {
            String::new()
        } else {
            format!("X-Kimi-Router-Token: {token}\r\n")
        };
        stream
            .write_all(
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{token_header}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
