//! Fetch Grok Build usage from `/v1/billing`.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::cache::{Cache, acquire_lock};
use crate::error::{AppError, Result};
use crate::usage::GrokSnapshot;

use super::creds;
use super::types::BillingResponse;

/// Grok Build's own proxy. The subscription's credits live here; `api.x.ai`
/// bills per token against a separate API balance and knows nothing about them.
pub const BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
/// Stable marker stored alongside code 0 for a successful HTTP response whose
/// payload no longer matches xAI's undocumented billing schema.
pub const SCHEMA_DRIFT_MESSAGE: &str = "Grok API schema drift";
/// Stored when the CLI's cached token has lapsed. Only Grok Build itself can
/// refresh it, so the fix is to run it, not to retry harder.
pub const EXPIRED_TOKEN_MESSAGE: &str = "Grok token expired — run `grok` to refresh it";
/// Stored when the CLI's log shows the last turn was refused for spending.
/// This is NOT the pool running dry: the pool can sit at 36% and still refuse,
/// because a refused request never spends and the counter stops moving.
pub const SPENDING_BLOCK_MESSAGE: &str =
    "Grok is refusing turns: spending limit reached, add credits or upgrade";

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub billing: String,
    /// Entitlement. Separate from billing because xAI keeps them apart: an
    /// account can hold Grok Code access and still report an empty credit pool.
    pub user: String,
    /// The CLI's config feed, which is where the plan is finally named.
    pub settings: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            billing: format!("{BASE_URL}/billing"),
            user: format!("{BASE_URL}/user"),
            settings: format!("{BASE_URL}/settings"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: GrokSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    creds_path: &Path,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    let mut outcome = fetch_pool(client, creds_path, cache, endpoints, cache_ttl).await?;

    // A live refusal outranks whatever diagnostic the cache carried: it is the
    // reason the pool stopped moving, and it applies to the cached path too,
    // which would otherwise stay silent for a whole TTL.
    if let Some(path) = super::block::default_log_path()
        && super::block::active(&path, outcome.snapshot.period_start).is_some()
    {
        outcome.last_error = Some((403, SPENDING_BLOCK_MESSAGE.to_string()));
    }
    Ok(outcome)
}

async fn fetch_pool(
    client: &reqwest::Client,
    creds_path: &Path,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock(&cache.lock_path(), LOCK_TIMEOUT)?;

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(bytes, cache, false)
    {
        return Ok(outcome);
    }
    // Corrupt fresh cache: fall through to a live fetch rather than return a
    // fabricated zero snapshot.

    let creds = creds::read_from(creds_path)?;
    // We never refresh: rotating the CLI's refresh token from here would sign
    // the user out of Grok Build if the write lost a race with the CLI itself.
    if creds.is_expired_at(Utc::now()) {
        cache.mark_stale();
        cache.write_last_error(0, EXPIRED_TOKEN_MESSAGE);
        return fallback_with_error(cache, AppError::Credentials(EXPIRED_TOKEN_MESSAGE.into()));
    }

    match fetch_live(client, &endpoints.billing, &creds.key).await {
        Ok(mut snap) => {
            // Best effort on purpose: entitlement only sharpens the wording of
            // a card the pool already fills in. A card that refuses to render
            // because a secondary endpoint blinked would be a worse trade.
            snap.has_grok_code_access = fetch_access(client, &endpoints.user, &creds.key).await;
            snap.plan_display = fetch_plan_display(client, &endpoints.settings, &creds.key).await;
            let bytes = serde_json::to_vec(&snap_to_json(&snap)).unwrap_or_default();
            cache.write_payload(&bytes)?;
            Ok(FetchOutcome {
                snapshot: snap,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(e) if e.is_transient() => fallback_silent(cache, e),
        Err(e) => {
            cache.mark_stale();
            if let Some((code, msg)) = error_to_pair(&e) {
                cache.write_last_error(code, &msg);
            }
            fallback_with_error(cache, e)
        }
    }
}

fn fallback_silent(cache: &Cache, original: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.maybe_payload()? else {
        return Err(original);
    };
    match reuse_cache(bytes, cache, true) {
        Ok(outcome) => Ok(outcome),
        Err(_) => Err(original),
    }
}

fn fallback_with_error(cache: &Cache, original: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.maybe_payload()? else {
        return Err(original);
    };
    match reuse_cache(bytes, cache, true) {
        Ok(mut outcome) => {
            outcome.last_error = error_to_pair(&original);
            Ok(outcome)
        }
        Err(_) => Err(original),
    }
}

fn error_to_pair(e: &AppError) -> Option<(u16, String)> {
    match e {
        AppError::Http { status, body } => Some((*status, body.clone())),
        // A 2xx response with an unknown shape is not an HTTP 422 response.
        AppError::Schema(_) => Some((0, SCHEMA_DRIFT_MESSAGE.into())),
        e => Some((0, e.to_string())),
    }
}

fn reuse_cache(bytes: Vec<u8>, cache: &Cache, stale: bool) -> Result<FetchOutcome> {
    let snap = parse_cache(&bytes)?;
    Ok(FetchOutcome {
        snapshot: snap,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

fn parse_cache(bytes: &[u8]) -> Result<GrokSnapshot> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    Ok(GrokSnapshot {
        monthly_limit: parse_cache_f64(&v["monthly_limit"], "monthly_limit")?,
        monthly_used: parse_cache_f64(&v["monthly_used"], "monthly_used")?,
        monthly_remaining: parse_cache_f64(&v["monthly_remaining"], "monthly_remaining")?,
        on_demand_cap: parse_cache_f64(&v["on_demand_cap"], "on_demand_cap")?,
        period_start: parse_cache_datetime(&v["period_start"])?,
        period_end: parse_cache_datetime(&v["period_end"])?,
        // Absent in caches written before this field existed, and null whenever
        // the entitlement call did not answer. Both mean "unknown", which is
        // exactly what `as_bool` yields here.
        has_grok_code_access: v["has_grok_code_access"].as_bool(),
        plan_display: v["plan_display"].as_str().map(str::to_owned),
    })
}

fn parse_cache_f64(v: &serde_json::Value, name: &str) -> Result<f64> {
    v.as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| AppError::Schema(format!("grok cache: invalid {name}")))
}

fn parse_cache_datetime(v: &serde_json::Value) -> Result<Option<DateTime<Utc>>> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.into()))
            .map_err(|e| AppError::Schema(format!("grok cache: invalid timestamp: {e}"))),
        _ => Err(AppError::Schema("grok cache: invalid timestamp".into())),
    }
}

fn snap_to_json(snap: &GrokSnapshot) -> serde_json::Value {
    serde_json::json!({
        "monthly_limit": snap.monthly_limit,
        "monthly_used": snap.monthly_used,
        "monthly_remaining": snap.monthly_remaining,
        "on_demand_cap": snap.on_demand_cap,
        "period_start": snap.period_start.map(|dt| dt.to_rfc3339()),
        "period_end": snap.period_end.map(|dt| dt.to_rfc3339()),
        "has_grok_code_access": snap.has_grok_code_access,
        "plan_display": snap.plan_display,
    })
}

/// Entitlement, or `None` when the call fails, times out or answers a shape we
/// do not recognise. A cache written before this field existed also lands here,
/// which is why the renderer must stay honest about not knowing.
async fn fetch_access(client: &reqwest::Client, url: &str, token: &str) -> Option<bool> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .ok()?
    .ok()?;

    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    serde_json::from_slice::<super::types::UserResponse>(&bytes)
        .ok()?
        .has_grok_code_access
}

/// The plan's commercial name, straight from the CLI's settings feed. Best
/// effort for the same reason as the entitlement: it improves the label and
/// nothing depends on it.
async fn fetch_plan_display(client: &reqwest::Client, url: &str, token: &str) -> Option<String> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .ok()?
    .ok()?;

    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    serde_json::from_slice::<super::types::SettingsResponse>(&bytes)
        .ok()?
        .subscription_tier_display
        .filter(|s| !s.trim().is_empty())
}

async fn fetch_live(client: &reqwest::Client, url: &str, token: &str) -> Result<GrokSnapshot> {
    let resp = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("grok timeout: {url}")))??;

    let status = resp.status();

    if !status.is_success() {
        // Never surface upstream/proxy bodies: they can contain credentials or
        // arbitrary markup. Keep the cached diagnostic useful but generic.
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
    let r: BillingResponse = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Schema(format!("grok billing response: {e}")))?;
    r.into_snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn cache_fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("grok"));
        cache.ensure_dir().unwrap();
        (td, cache)
    }

    fn creds_fixture(expires_at: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{"https://auth.x.ai::c": {{"key":"tok","expires_at":"{expires_at}"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        file
    }

    fn sample_json() -> &'static str {
        r#"{"config": {"monthlyLimit": {"val": 4000}, "used": {"val": 200},
                       "onDemandCap": {"val": 0},
                       "billingPeriodStart": "2026-08-01T00:00:00+00:00",
                       "billingPeriodEnd": "2026-09-01T00:00:00+00:00"}}"#
    }

    fn sample_seed() -> serde_json::Value {
        serde_json::json!({
            "monthly_limit": 4000.0,
            "monthly_used": 100.0,
            "monthly_remaining": 3900.0,
            "on_demand_cap": 0.0,
            "period_start": "2026-08-01T00:00:00+00:00",
            "period_end": "2026-09-01T00:00:00+00:00"
        })
    }

    #[tokio::test]
    async fn live_200_returns_snapshot_and_sends_bearer() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/v1/billing")
            .with_status(200)
            .with_body(sample_json())
            .match_header("authorization", "Bearer tok")
            .match_header("accept", "application/json")
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();

        m.assert_async().await;
        assert_eq!(out.snapshot.monthly_limit, 4000.0);
        assert_eq!(out.snapshot.monthly_used, 200.0);
        assert_eq!(out.snapshot.monthly_pct(), Some(5));
        assert!(!out.stale);
    }

    #[tokio::test]
    async fn the_entitlement_is_fetched_and_survives_the_cache_round_trip() {
        let mut server = mockito::Server::new_async().await;
        let billing = server
            .mock("GET", "/v1/billing")
            .with_status(200)
            .with_body(r#"{"config": {"monthlyLimit": {"val": 0}, "used": {"val": 224}}}"#)
            .create_async()
            .await;
        let user = server
            .mock("GET", "/v1/user")
            .with_status(200)
            .with_body(r#"{"hasGrokCodeAccess": true, "email": "x@y.z"}"#)
            .match_header("authorization", "Bearer tok")
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/v1/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let client = reqwest::Client::new();
        let out = fetch_snapshot(&client, creds.path(), &cache, &endpoints, Duration::ZERO)
            .await
            .unwrap();

        billing.assert_async().await;
        user.assert_async().await;
        assert_eq!(out.snapshot.has_grok_code_access, Some(true));
        assert_eq!(out.snapshot.monthly_pct(), None);

        // Served from cache next time, and the entitlement has to come back
        // with it or the card silently downgrades to "unknown" for a whole TTL.
        let cached = fetch_snapshot(
            &client,
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(600),
        )
        .await
        .unwrap();
        assert_eq!(cached.snapshot.has_grok_code_access, Some(true));
    }

    #[tokio::test]
    async fn a_failing_entitlement_call_leaves_the_pool_card_intact() {
        let mut server = mockito::Server::new_async().await;
        let billing = server
            .mock("GET", "/v1/billing")
            .with_status(200)
            .with_body(sample_json())
            .create_async()
            .await;
        let user = server
            .mock("GET", "/v1/user")
            .with_status(500)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/v1/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::ZERO,
        )
        .await
        .unwrap();

        billing.assert_async().await;
        user.assert_async().await;
        // Unknown, not an error, and none of it reaches the user as a failure.
        assert_eq!(out.snapshot.has_grok_code_access, None);
        assert_eq!(out.snapshot.monthly_pct(), Some(5));
        assert_eq!(out.last_error, None);
    }

    #[tokio::test]
    async fn expired_token_serves_stale_cache_without_calling_the_api() {
        let mut server = mockito::Server::new_async().await;
        // Any request at all would be a bug: the token cannot work.
        let m = server
            .mock("GET", "/v1/billing")
            .expect(0)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        cache
            .write_payload(&serde_json::to_vec(&sample_seed()).unwrap())
            .unwrap();
        let creds = creds_fixture("2020-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();

        m.assert_async().await;
        assert!(out.stale);
        assert_eq!(out.snapshot.monthly_used, 100.0);
        let (code, msg) = out.last_error.unwrap();
        assert_eq!(code, 0);
        assert!(msg.contains("expired"));
    }

    #[tokio::test]
    async fn http_401_falls_back_to_cache_with_the_error_attached() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/v1/billing")
            .with_status(401)
            .with_body("{\"error\":\"nope\"}")
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        cache
            .write_payload(&serde_json::to_vec(&sample_seed()).unwrap())
            .unwrap();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();

        assert!(out.stale);
        let (code, msg) = out.last_error.unwrap();
        assert_eq!(code, 401);
        // The upstream body never reaches the tooltip.
        assert!(!msg.contains("nope"));
    }

    #[tokio::test]
    async fn schema_drift_is_recorded_as_code_zero() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/v1/billing")
            .with_status(200)
            .with_body(r#"{"config": {"used": {"val": 1}}}"#)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        cache
            .write_payload(&serde_json::to_vec(&sample_seed()).unwrap())
            .unwrap();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(0),
        )
        .await
        .unwrap();

        assert_eq!(out.last_error, Some((0, SCHEMA_DRIFT_MESSAGE.to_string())));
    }

    #[tokio::test]
    async fn fresh_cache_short_circuits_the_request() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/v1/billing")
            .expect(0)
            .create_async()
            .await;

        let (_td, cache) = cache_fixture();
        cache
            .write_payload(&serde_json::to_vec(&sample_seed()).unwrap())
            .unwrap();
        let creds = creds_fixture("2099-01-01T00:00:00Z");
        let endpoints = Endpoints {
            settings: format!("{}/v1/settings", server.url()),
            user: format!("{}/user", server.url()),
            billing: format!("{}/v1/billing", server.url()),
        };
        let out = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::from_secs(3600),
        )
        .await
        .unwrap();

        m.assert_async().await;
        assert!(!out.stale);
        assert_eq!(out.snapshot.monthly_remaining, 3900.0);
    }
}
