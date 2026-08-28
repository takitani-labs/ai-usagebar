//! Grok Build billing over HTTP, using the token the official CLI already holds.
//!
//! Upstream asks the CLI for `x.ai/billing` over ACP. That method is gone: grok
//! 1.0.5 answers `-32601 Method not found` for it, and for every plausible
//! rename (`xai/billing`, `x.ai/usage`, `billing`, …). The figures themselves
//! are still served by the proxy the CLI talks to, so this reads the token the
//! CLI stores and asks the proxy directly. ACP stays as the fallback for the
//! day xAI restores the method.
//!
//! The wire shape needs no translation: `/v1/billing` answers the same
//! `config.{monthlyLimit,used,onDemandCap,billingPeriod*}` object the ACP
//! extension did, so it deserializes into [`BillingResponse`] unchanged. Only
//! the plan name lives elsewhere, on `/v1/settings`.

use std::path::Path;
use std::time::Duration;

use crate::error::{AppError, Result};

use super::creds;
use super::types::BillingResponse;

pub const BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn fetch_billing(auth_path: &Path) -> Result<BillingResponse> {
    let creds = creds::read_from(auth_path)?;
    // The CLI refreshes this token on its own runs and we never write to its
    // file, so a lapsed token is a state to report, not one to repair here.
    if creds.is_expired_at(chrono::Utc::now()) {
        return Err(AppError::Credentials(
            "Grok token expired — run `grok` to refresh it".into(),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AppError::Other(format!("supergrok http client: {e}")))?;

    let mut resp: BillingResponse = get_json(&client, &format!("{BASE_URL}/billing"), &creds.key)
        .await?
        .ok_or_else(|| AppError::Schema("Grok Build billing response was empty".into()))?;

    // The plan name is on a different endpoint, and its absence is not an
    // error: to_snapshot falls back to "SuperGrok".
    if resp.subscription_tier.is_none() {
        resp.subscription_tier = get_json::<Settings>(
            &client,
            &format!("{BASE_URL}/settings"),
            &creds.key,
        )
        .await
        .ok()
        .flatten()
        .and_then(|s| s.subscription_tier_display);
    }
    Ok(resp)
}

#[derive(serde::Deserialize)]
struct Settings {
    subscription_tier_display: Option<String>,
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<Option<T>> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        // Never surface an upstream body: it can carry credentials or markup.
        let body = if matches!(status.as_u16(), 401 | 403) {
            "Grok authentication failed — run `grok login`".into()
        } else {
            format!("Grok API returned HTTP {}", status.as_u16())
        };
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }

    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| AppError::Schema(format!("Grok Build billing response: {e}")))
}
