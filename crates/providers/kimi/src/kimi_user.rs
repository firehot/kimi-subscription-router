//! Kimi 账号资料查询：GET {base}/me，只提取本地展示所需的账号名称。

use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::Account;
use kimi_switch_engine::AccountProfile;

/// 从 `/me` 响应提取可展示资料；只保留名称和邮箱，不缓存原始响应。
pub fn parse_account_profile(body: &str) -> AccountProfile {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return AccountProfile::default();
    };
    let display_label = ["nickname", "username", "email"]
        .into_iter()
        .filter_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(String::from);
    let email = value
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    AccountProfile {
        display_label,
        email,
    }
}

/// active 账号查询 401 时，复用官方锁协议恢复一次令牌后只重试一次。
pub async fn fetch_account_profile_with_active_recovery(
    access_token: &str,
    account: &Account,
) -> Result<AccountProfile> {
    let api_base = crate::kimi_usage::base_url();
    match fetch_account_profile_at(access_token, &api_base).await {
        Err(error) if account.active && is_unauthorized(&error) => {
            let Some(fresh_access) =
                crate::oauth::recover_active_401(access_token, account).await?
            else {
                return Err(error);
            };
            fetch_account_profile_at(&fresh_access, &api_base).await
        }
        result => result,
    }
}

async fn fetch_account_profile_at(access_token: &str, api_base: &str) -> Result<AccountProfile> {
    let url = format!("{}/me", api_base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "kimi-switch")
        .send()
        .await
        .map_err(|error| Error::Provider(format!("kimi user info request failed: {error}")))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "kimi user info HTTP {status}: {body}"
        )));
    }
    Ok(parse_account_profile(&body))
}

fn is_unauthorized(error: &Error) -> bool {
    matches!(error, Error::Provider(message) if message.starts_with("kimi user info HTTP 401 "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockServer;

    #[test]
    fn nickname_is_preferred() {
        let body =
            r#"{"user_id":"u1","nickname":"Alice","username":"alice01","email":"a@example.com"}"#;
        assert_eq!(
            parse_account_profile(body).display_label.as_deref(),
            Some("Alice")
        );
    }

    #[test]
    fn username_and_email_are_fallbacks() {
        let username = r#"{"user_id":"u1","nickname":"","username":"alice01"}"#;
        assert_eq!(
            parse_account_profile(username).display_label.as_deref(),
            Some("alice01")
        );

        let email = r#"{"user_id":"u1","email":"a@example.com"}"#;
        assert_eq!(
            parse_account_profile(email).display_label.as_deref(),
            Some("a@example.com")
        );
    }

    #[test]
    fn malformed_or_unnamed_response_has_no_label() {
        assert_eq!(parse_account_profile("not json"), AccountProfile::default());
        assert_eq!(
            parse_account_profile(r#"{"user_id":"u1"}"#),
            AccountProfile::default()
        );
    }

    #[tokio::test]
    async fn fetches_profile_from_me_endpoint() {
        let server = MockServer::start(vec![("200 OK", r#"{"user_id":"u1","nickname":"Alice"}"#)]);
        let profile = fetch_account_profile_at("token", server.base_url())
            .await
            .unwrap();
        assert_eq!(profile.display_label.as_deref(), Some("Alice"));
        assert_eq!(server.finish(), vec!["GET /me HTTP/1.1"]);
    }

    #[test]
    fn profile_keeps_email_separate_from_display_name() {
        let profile = parse_account_profile(
            r#"{"nickname":"Alice","email":"alice@example.com","access_token":"ignored"}"#,
        );
        assert_eq!(profile.display_label.as_deref(), Some("Alice"));
        assert_eq!(profile.email.as_deref(), Some("alice@example.com"));
    }
}
