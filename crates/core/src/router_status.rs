//! ACP 路由器的只读运行状态。
//!
//! 状态文件只包含会话归属，不包含凭证。GUI 与本机控制 API 共用这里的解析和
//! 单实例锁探测，避免各自解释 `router-state.json`。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::Path;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionAssignment {
    pub session_id: String,
    pub account_id: String,
    pub assigned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RouterStatusSnapshot {
    pub running: bool,
    pub session_count: usize,
    pub account_session_counts: BTreeMap<String, usize>,
    pub sessions: Vec<SessionAssignment>,
}

#[derive(Debug, Default, Deserialize)]
struct PersistedState {
    #[serde(default)]
    sessions: BTreeMap<String, PersistedOwner>,
}

#[derive(Debug, Deserialize)]
struct PersistedOwner {
    account_id: String,
    assigned_at: DateTime<Utc>,
}

pub fn load_router_status(paths: &AppPaths) -> Result<RouterStatusSnapshot> {
    load_router_status_from(&paths.router_state_file(), &paths.router_lock_file())
}

pub fn load_router_status_from(
    state_path: &Path,
    lock_path: &Path,
) -> Result<RouterStatusSnapshot> {
    let persisted = match std::fs::read_to_string(state_path) {
        Ok(raw) if raw.trim().is_empty() => PersistedState::default(),
        Ok(raw) => serde_json::from_str(&raw)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
        Err(error) => return Err(error.into()),
    };

    let mut account_session_counts = BTreeMap::new();
    let sessions = persisted
        .sessions
        .into_iter()
        .map(|(session_id, owner)| {
            *account_session_counts
                .entry(owner.account_id.clone())
                .or_insert(0) += 1;
            SessionAssignment {
                session_id,
                account_id: owner.account_id,
                assigned_at: owner.assigned_at,
            }
        })
        .collect::<Vec<_>>();

    Ok(RouterStatusSnapshot {
        running: lock_is_held(lock_path)?,
        session_count: sessions.len(),
        account_session_counts,
        sessions,
    })
}

fn lock_is_held(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = open_lock(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if lock_conflict(&error) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn lock_conflict(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    return matches!(error.raw_os_error(), Some(32 | 33));
    #[cfg(not(windows))]
    false
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_session_counts_without_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("router-state.json");
        let lock = temp.path().join("router.lock");
        std::fs::write(
            &state,
            r#"{
                "version": 1,
                "sessions": {
                    "session-b": {"account_id":"b","assigned_at":"2026-08-18T00:00:00Z"},
                    "session-a": {"account_id":"a","assigned_at":"2026-08-17T00:00:00Z"},
                    "session-c": {"account_id":"a","assigned_at":"2026-08-18T01:00:00Z"}
                }
            }"#,
        )
        .unwrap();

        let status = load_router_status_from(&state, &lock).unwrap();
        assert!(!status.running);
        assert_eq!(status.session_count, 3);
        assert_eq!(status.account_session_counts.get("a"), Some(&2));
        assert_eq!(status.sessions[0].session_id, "session-a");
    }

    #[test]
    fn detects_another_lock_holder() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("missing-state.json");
        let lock = temp.path().join("router.lock");
        let held = open_lock(&lock).unwrap();
        held.try_lock_exclusive().unwrap();

        let status = load_router_status_from(&state, &lock).unwrap();
        assert!(status.running);
        FileExt::unlock(&held).unwrap();
    }
}
