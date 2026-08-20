//! 每账号 Kimi Code 数据目录与共享会话目录。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use kimi_switch_core::paths::router_account_dir_name;
use kimi_switch_core::{Account, AccountId};
use kimi_switch_kimi::KimiProvider;

pub struct AccountHome {
    pub account_id: String,
    pub path: PathBuf,
    credential_file: PathBuf,
}

impl AccountHome {
    pub fn prepare(
        router_root: &Path,
        shared_sessions: &Path,
        account: &Account,
        provider: &Arc<KimiProvider>,
    ) -> Result<Self> {
        fs::create_dir_all(shared_sessions)?;
        restrict_dir(shared_sessions)?;

        let path = router_root
            .join("accounts")
            .join(router_account_dir_name(&account.id.0))
            .join("kimi-home");
        fs::create_dir_all(path.join("credentials"))?;
        restrict_dir(&path)?;
        restrict_dir(&path.join("credentials"))?;

        let credential_file = path.join("credentials").join("kimi-code.json");
        if credential_file.exists() {
            let raw = fs::read_to_string(&credential_file)
                .with_context(|| format!("read {}", credential_file.display()))?;
            provider
                .absorb_blob(&account.id, &raw)
                .context("absorb credentials rotated by Kimi Code")?;
        } else {
            let raw = provider
                .export_blob(&account.id)
                .with_context(|| format!("export credentials for {}", account.id))?;
            write_private(&credential_file, raw.as_bytes())?;
        }

        materialize_runtime_config(&provider.home(), &path)?;
        link_shared_sessions(&path.join("sessions"), shared_sessions)?;
        Ok(Self {
            account_id: account.id.0.clone(),
            path,
            credential_file,
        })
    }

    /// 吸收 Kimi 官方进程按其锁协议轮换后的最新凭证。
    pub fn absorb_credentials(&self, provider: &Arc<KimiProvider>) -> Result<()> {
        let raw = fs::read_to_string(&self.credential_file)
            .with_context(|| format!("read {}", self.credential_file.display()))?;
        provider
            .absorb_blob(&AccountId(self.account_id.clone()), &raw)
            .context("persist credentials rotated by Kimi Code")
    }

    /// 账号被删除后清除路由器隔离目录中的凭证副本。
    pub fn purge_credentials(&self) -> Result<()> {
        match fs::remove_file(&self.credential_file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("remove {}", self.credential_file.display()))
            }
        }
    }
}

/// 只复制官方 Kimi OAuth provider 所需配置，过滤自定义端点与内联密钥。
fn materialize_runtime_config(source_home: &Path, account_home: &Path) -> Result<()> {
    let source_path = source_home.join("config.toml");
    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("read Kimi config {}", source_path.display()))?;
    let source: toml::Value = toml::from_str(&raw).context("parse Kimi config")?;
    let source_root = source
        .as_table()
        .context("Kimi config root must be a TOML table")?;
    let mut output = toml::map::Map::new();

    for key in ["default_model", "thinking"] {
        if let Some(value) = source_root.get(key) {
            output.insert(key.into(), value.clone());
        }
    }

    let managed_provider = source_root
        .get("providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get("managed:kimi-code"))
        .cloned()
        .context("Kimi managed OAuth provider is missing from config.toml")?;
    validate_official_oauth_section("provider", &managed_provider)?;
    let mut providers = toml::map::Map::new();
    providers.insert("managed:kimi-code".into(), managed_provider);
    output.insert("providers".into(), toml::Value::Table(providers));

    if let Some(models) = source_root.get("models").and_then(toml::Value::as_table) {
        let filtered = models
            .iter()
            .filter(|(_, value)| {
                value.get("provider").and_then(toml::Value::as_str) == Some("managed:kimi-code")
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<toml::map::Map<_, _>>();
        if !filtered.is_empty() {
            output.insert("models".into(), toml::Value::Table(filtered));
        }
    }

    if let Some(services) = source_root.get("services").and_then(toml::Value::as_table) {
        let mut filtered = toml::map::Map::new();
        for key in ["moonshot_search", "moonshot_fetch"] {
            if let Some(value) = services.get(key) {
                validate_official_oauth_section(key, value)?;
                filtered.insert(key.into(), value.clone());
            }
        }
        if !filtered.is_empty() {
            output.insert("services".into(), toml::Value::Table(filtered));
        }
    }

    let serialized = toml::to_string_pretty(&toml::Value::Table(output))?;
    write_private(&account_home.join("config.toml"), serialized.as_bytes())
}

fn validate_official_oauth_section(name: &str, value: &toml::Value) -> Result<()> {
    let table = value
        .as_table()
        .with_context(|| format!("Kimi {name} config must be a table"))?;
    let api_key = table
        .get("api_key")
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    if !api_key.is_empty() {
        bail!("Kimi {name} config uses a non-OAuth inline credential");
    }
    let base_url = table
        .get("base_url")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("Kimi {name} base_url is missing"))?;
    if !official_kimi_url(base_url) {
        bail!("Kimi {name} config uses a non-official endpoint");
    }
    let oauth = table
        .get("oauth")
        .and_then(toml::Value::as_table)
        .with_context(|| format!("Kimi {name} OAuth reference is missing"))?;
    if oauth.get("storage").and_then(toml::Value::as_str) != Some("file")
        || oauth.get("key").and_then(toml::Value::as_str) != Some("oauth/kimi-code")
    {
        bail!("Kimi {name} config uses an unexpected OAuth credential slot");
    }
    Ok(())
}

fn official_kimi_url(url: &str) -> bool {
    ["https://api.kimi.com", "https://auth.kimi.com"]
        .iter()
        .any(|origin| url == *origin || url.starts_with(&format!("{origin}/")))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    restrict_file(&tmp)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn link_shared_sessions(link: &Path, target: &Path) -> Result<()> {
    if link.exists() {
        let metadata = fs::symlink_metadata(link)?;
        if metadata.file_type().is_symlink() {
            let current = fs::read_link(link)?;
            if current == target {
                return Ok(());
            }
            bail!(
                "session link {} points to unexpected target {}",
                link.display(),
                current.display()
            );
        }
        bail!(
            "router-owned path {} is not a session link; move it aside before retrying",
            link.display()
        );
    }
    create_dir_link(target, link)
        .with_context(|| format!("link {} -> {}", link.display(), target.display()))
}

#[cfg(unix)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(symlink_error) => {
            // 目录联接不要求 Developer Mode 或 SeCreateSymbolicLinkPrivilege。
            let output = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(link)
                .arg(target)
                .output()?;
            if output.status.success() && link.exists() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    symlink_error.kind(),
                    format!(
                        "symbolic link failed ({symlink_error}); junction failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ))
            }
        }
    }
}

fn restrict_dir(path: &Path) -> Result<()> {
    kimi_switch_core::private_fs::restrict_dir(path)?;
    Ok(())
}

fn restrict_file(path: &Path) -> Result<()> {
    kimi_switch_core::private_fs::restrict_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_directory_does_not_expose_account_id() {
        let name = router_account_dir_name("user@example.com");
        assert_eq!(name.len(), 32);
        assert!(!name.contains("user"));
        assert_eq!(name, router_account_dir_name("user@example.com"));
    }

    #[cfg(unix)]
    #[test]
    fn links_router_owned_session_directory() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("shared");
        let link = temp.path().join("home").join("sessions");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::create_dir_all(&target).unwrap();
        link_shared_sessions(&link, &target).unwrap();
        assert_eq!(fs::read_link(link).unwrap(), target);
    }

    #[test]
    fn runtime_config_keeps_only_official_oauth_provider() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(
            source.join("config.toml"),
            r#"
default_model = "kimi-code/model"

[providers."managed:kimi-code"]
type = "openai_legacy"
api_key = ""
base_url = "https://api.kimi.com/coding/v1"

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code"

[providers.evil]
type = "openai"
api_key = "secret"
base_url = "https://example.com"

[models."kimi-code/model"]
provider = "managed:kimi-code"
model = "model"

[models.evil]
provider = "evil"
model = "evil"
"#,
        )
        .unwrap();

        materialize_runtime_config(&source, &target).unwrap();
        let output = fs::read_to_string(target.join("config.toml")).unwrap();
        assert!(output.contains("managed:kimi-code"));
        assert!(output.contains("api.kimi.com"));
        assert!(!output.contains("example.com"));
        assert!(!output.contains("secret"));
    }

    #[test]
    fn purge_credentials_removes_isolated_copy() {
        let temp = tempfile::tempdir().unwrap();
        let credential_file = temp.path().join("credentials").join("kimi-code.json");
        fs::create_dir_all(credential_file.parent().unwrap()).unwrap();
        fs::write(&credential_file, br#"{"access_token":"private"}"#).unwrap();
        let home = AccountHome {
            account_id: "account-a".into(),
            path: temp.path().to_path_buf(),
            credential_file: credential_file.clone(),
        };

        home.purge_credentials().unwrap();
        home.purge_credentials().unwrap();
        assert!(!credential_file.exists());
    }
}
