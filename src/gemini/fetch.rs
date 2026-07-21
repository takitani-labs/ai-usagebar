//! Gemini Code Assist flow: credentials → OAuth refresh → tier/project → quota.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::{Cache, acquire_lock};
use crate::error::{AppError, Result};
use crate::usage::{GeminiBucket, GeminiSnapshot, UsageWindow};

use super::creds;
use super::oauth;
use super::types::{LoadCodeAssistResponse, QuotaResponse};

pub const BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(25);
const LOCK_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub token: String,
    pub load_code_assist: String,
    pub retrieve_user_quota: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            token: oauth::TOKEN_URL.into(),
            load_code_assist: format!("{BASE_URL}/v1internal:loadCodeAssist"),
            retrieve_user_quota: format!("{BASE_URL}/v1internal:retrieveUserQuota"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: GeminiSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSnapshot {
    tier: String,
    buckets: Vec<CachedBucket>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedBucket {
    model: String,
    utilization_pct: i32,
    resets_at: Option<DateTime<Utc>>,
}

pub async fn fetch_snapshot(
    client: &reqwest::Client,
    creds_path: &Path,
    cache: &Cache,
    endpoints: &Endpoints,
    cache_ttl: Duration,
    client_secret: Option<&str>,
) -> Result<FetchOutcome> {
    cache.ensure_dir()?;
    let _lock = acquire_lock(&cache.lock_path(), LOCK_TIMEOUT)?;
    let auth = creds::read_from(creds_path)?;

    if let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(snapshot) = parse_cache(&bytes)
    {
        return Ok(FetchOutcome {
            snapshot,
            stale: false,
            last_error: cache.read_last_error(),
            cache_age: cache.payload_age(),
        });
    }

    let now_ms = Utc::now().timestamp_millis();
    let access_token = if auth.is_expired_at(now_ms) {
        let Some(secret) = client_secret.filter(|s| !s.is_empty()) else {
            return Err(AppError::Credentials(format!(
                "gemini access token expired and no client secret to refresh it. \
                 Set `oauth_client_secret` under [gemini] or the {} env var \
                 (gemini-cli's public desktop client secret).",
                oauth::CLIENT_SECRET_ENV
            )));
        };
        match tokio::time::timeout(
            REFRESH_TIMEOUT,
            oauth::refresh(client, &endpoints.token, &auth.refresh_token, secret),
        )
        .await
        {
            Ok(Ok(response)) => response.access_token,
            Ok(Err(error)) => return fallback_for_error(cache, error),
            Err(_) => {
                return fallback_silent(
                    cache,
                    AppError::Transport("gemini OAuth refresh timed out".into()),
                );
            }
        }
    } else {
        auth.access_token
    };

    match fetch_live(client, endpoints, &access_token).await {
        Ok(snapshot) => {
            let bytes = serde_json::to_vec(&CachedSnapshot::from(&snapshot))?;
            cache.write_payload(&bytes)?;
            Ok(FetchOutcome {
                snapshot,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(error) if error.is_transient() => fallback_silent(cache, error),
        Err(error) => fallback_for_error(cache, error),
    }
}

async fn fetch_live(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<GeminiSnapshot> {
    let load_bytes = post_json(
        client,
        &endpoints.load_code_assist,
        access_token,
        &serde_json::json!({"metadata": {"pluginType": "GEMINI"}}),
    )
    .await?;
    let load: LoadCodeAssistResponse = serde_json::from_slice(&load_bytes)
        .map_err(|e| AppError::Schema(format!("gemini loadCodeAssist response: {e}")))?;
    let (tier, project) = load.tier_and_project()?;

    let quota_bytes = post_json(
        client,
        &endpoints.retrieve_user_quota,
        access_token,
        &serde_json::json!({"project": project}),
    )
    .await?;
    let quota: QuotaResponse = serde_json::from_slice(&quota_bytes)
        .map_err(|e| AppError::Schema(format!("gemini retrieveUserQuota response: {e}")))?;
    quota.into_snapshot(tier)
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    body: &serde_json::Value,
) -> Result<Vec<u8>> {
    let response = tokio::time::timeout(
        HTTP_TIMEOUT,
        client
            .post(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .json(body)
            .send(),
    )
    .await
    .map_err(|_| AppError::Transport(format!("gemini timeout: {url}")))??;
    let status = response.status();
    if !status.is_success() {
        let body = if matches!(status.as_u16(), 401 | 403) {
            "Gemini authentication failed; run `gemini` to sign in again".into()
        } else {
            format!("Gemini Code Assist API returned HTTP {}", status.as_u16())
        };
        return Err(AppError::Http {
            status: status.as_u16(),
            body,
        });
    }
    Ok(response.bytes().await?.to_vec())
}

fn fallback_for_error(cache: &Cache, error: AppError) -> Result<FetchOutcome> {
    cache.mark_stale();
    let pair = error_pair(&error);
    cache.write_last_error(pair.0, &pair.1);
    let Some(bytes) = cache.maybe_payload()? else {
        return Err(error);
    };
    let snapshot = parse_cache(&bytes)?;
    Ok(FetchOutcome {
        snapshot,
        stale: true,
        last_error: Some(pair),
        cache_age: cache.payload_age(),
    })
}

fn fallback_silent(cache: &Cache, error: AppError) -> Result<FetchOutcome> {
    let Some(bytes) = cache.maybe_payload()? else {
        return Err(error);
    };
    Ok(FetchOutcome {
        snapshot: parse_cache(&bytes)?,
        stale: true,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

fn error_pair(error: &AppError) -> (u16, String) {
    match error {
        AppError::Http { status, body } => (*status, body.clone()),
        _ => (0, error.to_string()),
    }
}

fn parse_cache(bytes: &[u8]) -> Result<GeminiSnapshot> {
    let cached: CachedSnapshot = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Schema(format!("gemini cache: {e}")))?;
    Ok(cached.into())
}

impl From<&GeminiSnapshot> for CachedSnapshot {
    fn from(snapshot: &GeminiSnapshot) -> Self {
        Self {
            tier: snapshot.tier.clone(),
            buckets: snapshot
                .buckets
                .iter()
                .map(|bucket| CachedBucket {
                    model: bucket.model.clone(),
                    utilization_pct: bucket.window.utilization_pct,
                    resets_at: bucket.window.resets_at,
                })
                .collect(),
        }
    }
}

impl From<CachedSnapshot> for GeminiSnapshot {
    fn from(cached: CachedSnapshot) -> Self {
        Self {
            tier: cached.tier,
            buckets: cached
                .buckets
                .into_iter()
                .map(|bucket| GeminiBucket {
                    model: bucket.model,
                    window: UsageWindow {
                        utilization_pct: bucket.utilization_pct.clamp(0, 100),
                        resets_at: bucket.resets_at,
                        window_duration: chrono::Duration::hours(24),
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn credentials(expiry_date: i64) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{"access_token":"test-at","refresh_token":"test-rt","expiry_date":{expiry_date}}}"#
        )
        .unwrap();
        file.flush().unwrap();
        file
    }

    fn cache_fixture() -> (TempDir, Cache) {
        let dir = TempDir::new().unwrap();
        let cache = Cache::at(dir.path().join("gemini"));
        cache.ensure_dir().unwrap();
        (dir, cache)
    }

    #[tokio::test]
    async fn live_flow_loads_project_then_all_request_buckets() {
        let mut server = mockito::Server::new_async().await;
        let load = server
            .mock("POST", "/v1internal:loadCodeAssist")
            .match_header("authorization", "Bearer test-at")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "metadata": {"pluginType": "GEMINI"}
            })))
            .with_status(200)
            .with_body(
                r#"{"currentTier":{"id":"standard-tier"},"cloudaicompanionProject":"project-123"}"#,
            )
            .create_async()
            .await;
        let quota = server
            .mock("POST", "/v1internal:retrieveUserQuota")
            .match_header("authorization", "Bearer test-at")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "project": "project-123"
            })))
            .with_status(200)
            .with_body(r#"{"buckets":[{"tokenType":"REQUESTS","modelId":"gemini-2.5-pro","remainingFraction":0.4,"resetTime":"2026-07-22T13:20:53Z"}]}"#)
            .create_async()
            .await;

        // Well beyond the eager-refresh margin so no token refresh is attempted.
        let creds = credentials(Utc::now().timestamp_millis() + 10 * 60 * 1000);
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints {
            token: format!("{}/token", server.url()),
            load_code_assist: format!("{}/v1internal:loadCodeAssist", server.url()),
            retrieve_user_quota: format!("{}/v1internal:retrieveUserQuota", server.url()),
        };
        let outcome = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::ZERO,
            None,
        )
        .await
        .unwrap();

        assert_eq!(outcome.snapshot.tier, "standard-tier");
        assert_eq!(outcome.snapshot.buckets.len(), 1);
        assert_eq!(outcome.snapshot.buckets[0].window.utilization_pct, 60);
        assert!(!outcome.stale);
        load.assert_async().await;
        quota.assert_async().await;
    }

    #[tokio::test]
    async fn expired_token_without_secret_is_a_credentials_error() {
        // Token past the eager-refresh margin, no cache, no client secret:
        // the refresh path can't run, so surface a clear credentials error.
        let creds = credentials(Utc::now().timestamp_millis() - 60_000);
        let (_dir, cache) = cache_fixture();
        let endpoints = Endpoints::default();
        let err = fetch_snapshot(
            &reqwest::Client::new(),
            creds.path(),
            &cache,
            &endpoints,
            Duration::ZERO,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Credentials(_)));
    }

    #[test]
    fn normalized_cache_round_trips_daily_windows() {
        let snapshot = GeminiSnapshot {
            tier: "standard-tier".into(),
            buckets: vec![GeminiBucket {
                model: "gemini-2.5-pro".into(),
                window: UsageWindow {
                    utilization_pct: 42,
                    resets_at: Some(Utc::now()),
                    window_duration: chrono::Duration::hours(24),
                },
            }],
        };
        let bytes = serde_json::to_vec(&CachedSnapshot::from(&snapshot)).unwrap();
        let parsed = parse_cache(&bytes).unwrap();
        assert_eq!(parsed, snapshot);
    }
}
