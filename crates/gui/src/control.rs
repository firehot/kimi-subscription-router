//! 仅供本机客户端使用的控制接口。
//!
//! 服务只绑定回环地址，除健康检查外的请求必须携带随机令牌。令牌只写入
//! 用户状态目录中的私有文件，不经接口返回，也不写日志。

use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Sender};
use std::time::Duration;

use anyhow::Context as _;
use kimi_switch_core::paths::AppPaths;
use rand::rngs::OsRng;
use rand::RngCore as _;
use serde::Serialize;
use subtle::ConstantTimeEq as _;
use tiny_http::{Header, Method, Request as HttpRequest, Response as HttpResponse, Server};

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
    pub active: bool,
    pub membership: Option<String>,
    pub subscription_expires_on: Option<String>,
    pub routing_enabled: bool,
    pub quotas: Vec<QuotaSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum Action {
    List,
    Refresh,
    Activate(String),
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

    let thread_token = token.clone();
    std::thread::Builder::new()
        .name("kimi-router-control".into())
        .spawn(move || serve(server, thread_token, requests))
        .context("启动本地控制服务线程失败")?;

    Ok(Info {
        base_url,
        token_file,
    })
}

fn serve(server: Server, token: String, requests: Sender<Request>) {
    for request in server.incoming_requests() {
        handle(request, &token, &requests);
    }
}

fn handle(mut request: HttpRequest, token: &str, requests: &Sender<Request>) {
    if request.method() == &Method::Options {
        respond_json(request, 204, serde_json::json!({}));
        return;
    }

    let path = request.url().split('?').next().unwrap_or(request.url());
    if path == "/v1/health" && request.method() == &Method::Get {
        respond_json(request, 200, serde_json::json!({"ok": true}));
        return;
    }
    if !authorized(&request, token) {
        respond_json(request, 401, serde_json::json!({"error": "unauthorized"}));
        return;
    }

    let action = match (request.method(), path) {
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

fn drain_body(request: &mut HttpRequest) {
    let mut sink = String::new();
    let _ = request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut sink);
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

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置私有权限失败: {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::mpsc::channel;

    use kimi_switch_core::paths::AppPaths;

    use super::{load_or_create_token, start, valid_account_id};

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

    fn raw_get(address: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
