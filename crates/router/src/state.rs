//! 会话到账号的粘性归属状态。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionOwner {
    pub account_id: String,
    pub assigned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterState {
    #[serde(default = "state_version")]
    pub version: u32,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionOwner>,
}

impl Default for RouterState {
    fn default() -> Self {
        Self {
            version: state_version(),
            sessions: BTreeMap::new(),
        }
    }
}

fn state_version() -> u32 {
    1
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<RouterState> {
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(RouterState::default()),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("parse router state {}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RouterState::default())
            }
            Err(error) => {
                Err(error).with_context(|| format!("read router state {}", self.path.display()))
            }
        }
    }

    pub fn save(&self, state: &RouterState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(state)?;
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&tmp, raw)?;
        restrict_file(&tmp)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

impl RouterState {
    pub fn owner(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .get(session_id)
            .map(|owner| owner.account_id.as_str())
    }

    pub fn assign(&mut self, session_id: impl Into<String>, account_id: impl Into<String>) {
        self.sessions.insert(
            session_id.into(),
            SessionOwner {
                account_id: account_id.into(),
                assigned_at: Utc::now(),
            },
        );
    }

    pub fn remove(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    pub fn owned_count(&self, account_id: &str) -> usize {
        self.sessions
            .values()
            .filter(|owner| owner.account_id == account_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_sticky_ownership_without_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("router-state.json"));
        let mut state = RouterState::default();
        state.assign("session-a", "account-a");
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.owner("session-a"), Some("account-a"));
        let raw = fs::read_to_string(temp.path().join("router-state.json")).unwrap();
        assert!(!raw.contains("access_token"));
        assert!(!raw.contains("refresh_token"));
    }
}
