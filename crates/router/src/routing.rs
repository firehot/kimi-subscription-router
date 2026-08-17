//! 额度感知账号选择。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use kimi_switch_core::{Account, Quota, QuotaCache, QuotaStatus, QuotaWindow};

use crate::state::RouterState;

#[derive(Debug, Clone)]
pub struct Selection {
    pub account_id: String,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct PoolExhausted {
    pub next_reset: Option<DateTime<Utc>>,
}

pub struct RouteSelector {
    accounts: Vec<Account>,
    quota_path: std::path::PathBuf,
    runtime_exhausted: HashMap<String, DateTime<Utc>>,
}

impl RouteSelector {
    pub fn new(accounts: Vec<Account>, quota_path: std::path::PathBuf) -> Self {
        Self {
            accounts,
            quota_path,
            runtime_exhausted: HashMap::new(),
        }
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn replace_accounts(&mut self, accounts: Vec<Account>) {
        self.accounts = accounts;
    }

    pub fn mark_exhausted(&mut self, account_id: &str) {
        let cache = QuotaCache::load(&self.quota_path);
        let next_reset = cache
            .get("kimi", account_id)
            .and_then(|entry| earliest_future_reset(&entry.quotas))
            .unwrap_or_else(|| Utc::now() + Duration::minutes(5));
        self.runtime_exhausted
            .insert(account_id.to_string(), next_reset);
    }

    pub fn select(
        &mut self,
        state: &RouterState,
        excluded: &HashSet<String>,
    ) -> Result<Selection, PoolExhausted> {
        self.clear_elapsed_exhaustion();
        let cache = QuotaCache::load(&self.quota_path);
        select_from(
            &self.accounts,
            &cache,
            state,
            excluded,
            &self.runtime_exhausted,
        )
    }

    pub fn account_has_capacity(&mut self, account_id: &str) -> bool {
        self.clear_elapsed_exhaustion();
        if !self
            .accounts
            .iter()
            .any(|account| account.id.0 == account_id && routing_enabled(account))
        {
            return false;
        }
        if self.runtime_exhausted.contains_key(account_id) {
            return false;
        }
        let cache = QuotaCache::load(&self.quota_path);
        match cache.get("kimi", account_id) {
            Some(entry) => !quota_exhausted(&entry.quotas),
            None => true,
        }
    }

    fn clear_elapsed_exhaustion(&mut self) {
        let now = Utc::now();
        self.runtime_exhausted.retain(|_, reset| *reset > now);
    }
}

fn select_from(
    accounts: &[Account],
    cache: &QuotaCache,
    state: &RouterState,
    excluded: &HashSet<String>,
    runtime_exhausted: &HashMap<String, DateTime<Utc>>,
) -> Result<Selection, PoolExhausted> {
    let mut candidates = Vec::new();
    let mut resets = Vec::new();
    for (stable_order, account) in accounts.iter().enumerate() {
        if !routing_enabled(account) || excluded.contains(&account.id.0) {
            continue;
        }
        let quotas = cache.get("kimi", &account.id.0).map(|entry| entry.quotas);
        let runtime_reset = runtime_exhausted.get(&account.id.0).copied();
        let exhausted = runtime_reset.is_some()
            || quotas
                .as_ref()
                .is_some_and(|values| quota_exhausted(values));
        if exhausted {
            if let Some(reset) =
                runtime_reset.or_else(|| quotas.as_ref().and_then(|q| earliest_future_reset(q)))
            {
                resets.push(reset);
            }
            continue;
        }
        let score = quotas.as_deref().map(route_score).unwrap_or(-1.0);
        candidates.push((
            Selection {
                account_id: account.id.0.clone(),
                score,
            },
            state.owned_count(&account.id.0),
            account.priority,
            stable_order,
        ));
    }

    candidates.sort_by(|left, right| {
        right
            .0
            .score
            .partial_cmp(&left.0.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.3.cmp(&right.3))
    });
    candidates
        .into_iter()
        .next()
        .map(|candidate| candidate.0)
        .ok_or_else(|| PoolExhausted {
            next_reset: resets.into_iter().min(),
        })
}

fn routing_enabled(account: &Account) -> bool {
    !account.manual_only()
        && account
            .extra
            .get("routing_enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
}

fn quota_exhausted(quotas: &[Quota]) -> bool {
    quotas.iter().any(|quota| {
        quota.status == QuotaStatus::Exhausted
            || quota.usage_ratio().is_some_and(|ratio| ratio >= 1.0)
    })
}

fn earliest_future_reset(quotas: &[Quota]) -> Option<DateTime<Utc>> {
    let now = Utc::now();
    quotas
        .iter()
        .filter_map(|quota| quota.reset_at)
        .filter(|reset| *reset > now)
        .min()
}

fn route_score(quotas: &[Quota]) -> f64 {
    let now = Utc::now();
    let mut weekly_burn = None;
    let mut short_headroom = None;
    for quota in quotas {
        let Some(ratio) = quota.usage_ratio() else {
            continue;
        };
        let remaining = (1.0 - ratio).clamp(0.0, 1.0);
        match quota.window {
            QuotaWindow::SevenDay => {
                let hours = quota
                    .reset_at
                    .map(|reset| (reset - now).num_seconds().max(900) as f64 / 3600.0)
                    .unwrap_or(168.0);
                weekly_burn = Some(remaining / hours);
            }
            QuotaWindow::FiveHour => short_headroom = Some(remaining),
            _ => {}
        }
    }
    let headroom = short_headroom.unwrap_or(1.0);
    weekly_burn.unwrap_or(0.0) * (0.5 + 0.5 * headroom) + 0.0001 * headroom
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use kimi_switch_core::{AccountId, QuotaStatus};

    fn account(id: &str, priority: i32) -> Account {
        Account {
            provider: "kimi".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: Utc::now(),
            last_used_at: None,
            priority,
            extra: serde_json::Map::new(),
        }
    }

    fn quota(id: &str, window: QuotaWindow, used: u64, reset_hours: i64) -> Quota {
        Quota {
            provider: "kimi".into(),
            account_id: AccountId(id.into()),
            window,
            used,
            limit: 100,
            reset_at: Some(Utc::now() + Duration::hours(reset_hours)),
            status: QuotaStatus::from_percent(used as f64),
            note: None,
        }
    }

    #[test]
    fn prefers_allowance_at_risk_before_earlier_reset() {
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "soon",
            vec![
                quota("soon", QuotaWindow::SevenDay, 10, 24),
                quota("soon", QuotaWindow::FiveHour, 10, 5),
            ],
        );
        cache.set(
            "kimi",
            "later",
            vec![
                quota("later", QuotaWindow::SevenDay, 10, 120),
                quota("later", QuotaWindow::FiveHour, 10, 5),
            ],
        );
        let result = select_from(
            &[account("later", 100), account("soon", 100)],
            &cache,
            &RouterState::default(),
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(result.account_id, "soon");
    }

    #[test]
    fn excludes_depleted_disabled_and_manual_accounts() {
        let mut disabled = account("disabled", 1);
        disabled
            .extra
            .insert("routing_enabled".into(), false.into());
        let mut manual = account("manual", 1);
        manual.extra.insert("manual_only".into(), true.into());
        let ready = account("ready", 100);
        let mut cache = QuotaCache::default();
        cache.set(
            "kimi",
            "depleted",
            vec![quota("depleted", QuotaWindow::FiveHour, 100, 2)],
        );
        let result = select_from(
            &[disabled, manual, account("depleted", 1), ready],
            &cache,
            &RouterState::default(),
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(result.account_id, "ready");
    }

    #[test]
    fn sticky_count_breaks_equal_scores() {
        let accounts = [account("busy", 100), account("idle", 100)];
        let mut state = RouterState::default();
        state.assign("s1", "busy");
        let result = select_from(
            &accounts,
            &QuotaCache::default(),
            &state,
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(result.account_id, "idle");
    }
}
