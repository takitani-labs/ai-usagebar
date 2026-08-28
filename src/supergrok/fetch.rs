//! SuperGrok fetch/cache orchestration around Grok Build's billing ACP method.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::{Cache, MAX_STALE, acquire_lock_async};
use crate::error::{AppError, Result};
use crate::usage::{SuperGrokPeriod, SuperGrokSnapshot};

use super::scope::ScopePaths;
use super::{acp, block, scope, types};

const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_SCHEMA: u8 = 3;

#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub snapshot: SuperGrokSnapshot,
    pub stale: bool,
    pub last_error: Option<(u16, String)>,
    pub cache_age: Option<Duration>,
}

/// Stored when the CLI's log shows the last turn was refused for spending.
/// NOT the pool running dry: this account has no pool at all (monthlyLimit 0)
/// and can still be refused, which is exactly why the log is the only source.
pub const SPENDING_BLOCK_MESSAGE: &str =
    "Grok is refusing turns: spending limit reached, add credits or upgrade";

pub async fn fetch_snapshot(
    grok_binary: &Path,
    scope_paths: &ScopePaths,
    cache: &Cache,
    cache_ttl: Duration,
) -> Result<FetchOutcome> {
    let mut outcome = fetch_snapshot_with(
        cache,
        cache_ttl,
        Utc::now(),
        || scope::fingerprint(scope_paths),
        || billing_via_token_then_acp(grok_binary, &scope_paths.auth),
    )
    .await?;

    // A live refusal outranks whatever the billing figures say, and it applies
    // to the cached path too, which would otherwise stay silent for a whole
    // TTL. The log sits next to the auth file, so a GROK_HOME override is
    // honoured for free. `period_start` is None because the snapshot does not
    // carry one: the "a later success clears it" rule is what keeps this
    // honest, and it does not need the period.
    let log = scope_paths
        .auth
        .parent()
        .map(|dir| dir.join("logs").join("unified.jsonl"));
    if let Some(log) = log
        && block::active(&log, None).is_some()
    {
        outcome.last_error = Some((403, SPENDING_BLOCK_MESSAGE.to_string()));
    }
    Ok(outcome)
}

async fn fetch_snapshot_with<S, F, Fut>(
    cache: &Cache,
    cache_ttl: Duration,
    now: DateTime<Utc>,
    read_scope: S,
    fetch_billing: F,
) -> Result<FetchOutcome>
where
    S: Fn() -> Option<String>,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<types::BillingResponse>>,
{
    cache.ensure_dir()?;
    let _lock = acquire_lock_async(&cache.lock_path(), LOCK_TIMEOUT).await?;
    let scope_before = read_scope();

    if let Some(account_scope) = scope_before.as_deref()
        && let Some(bytes) = cache.fresh_payload(cache_ttl)?
        && let Ok(outcome) = reuse_cache(&bytes, cache, false, account_scope)
        && !period_has_ended(&outcome.snapshot, now)
    {
        return Ok(outcome);
    }

    match fetch_billing().await {
        Ok(response) => {
            let account_scope = scope_before.as_deref().unwrap_or("uncached");
            let mut snapshot = match types::to_snapshot(response, account_scope) {
                Ok(snapshot) => snapshot,
                Err(error) => return fallback(cache, scope_before.as_deref(), now, error),
            };
            let scope_after = read_scope();

            // A concurrent login, config change, or token rotation means the
            // account identity was not stable across the request. Return the
            // live result, but do not bind it to either cache scope.
            if scope_before.is_some() && scope_before == scope_after {
                let account_scope = scope_before.as_deref().expect("checked Some");
                snapshot.account = account_scope.to_string();
                let bytes =
                    serde_json::to_vec(&CachedEnvelope::from_snapshot(account_scope, &snapshot))?;
                cache.write_payload(&bytes)?;
            } else {
                snapshot.account = "uncached".into();
            }

            Ok(FetchOutcome {
                snapshot,
                stale: false,
                last_error: None,
                cache_age: Some(Duration::ZERO),
            })
        }
        Err(error) => fallback(cache, scope_before.as_deref(), now, error),
    }
}

fn period_has_ended(snapshot: &SuperGrokSnapshot, now: DateTime<Utc>) -> bool {
    snapshot.reset_at.is_some_and(|reset| reset <= now)
}

fn metered_default() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedEnvelope {
    schema: u8,
    scope: String,
    snapshot: CachedSnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSnapshot {
    plan: String,
    #[serde(default = "metered_default")]
    metered: bool,
    percent: i32,
    period: String,
    reset_at: Option<DateTime<Utc>>,
    prepaid_balance: Option<f64>,
}

impl CachedEnvelope {
    fn from_snapshot(scope: &str, snapshot: &SuperGrokSnapshot) -> Self {
        let period = match snapshot.period {
            SuperGrokPeriod::Weekly => "weekly",
            SuperGrokPeriod::Monthly => "monthly",
            SuperGrokPeriod::Unknown => "unknown",
        };
        Self {
            schema: CACHE_SCHEMA,
            scope: scope.to_string(),
            snapshot: CachedSnapshot {
                plan: snapshot.plan.clone(),
                metered: snapshot.metered,
                percent: snapshot.weekly_pct,
                period: period.to_string(),
                reset_at: snapshot.reset_at,
                prepaid_balance: snapshot.prepaid_balance,
            },
        }
    }
}

fn parse_cache(bytes: &[u8], account_scope: &str) -> Result<SuperGrokSnapshot> {
    let cached: CachedEnvelope = serde_json::from_slice(bytes)?;
    if cached.schema != CACHE_SCHEMA {
        return Err(AppError::Schema(
            "SuperGrok cache schema is obsolete; refetching".into(),
        ));
    }
    if cached.scope != account_scope {
        return Err(AppError::Schema(
            "SuperGrok cache belongs to a different login; refetching".into(),
        ));
    }
    if !(0..=100).contains(&cached.snapshot.percent) {
        return Err(AppError::Schema(
            "SuperGrok cached percentage is out of range".into(),
        ));
    }
    if cached.snapshot.plan.chars().count() > 128
        || cached.snapshot.plan.chars().any(char::is_control)
    {
        return Err(AppError::Schema(
            "SuperGrok cached plan label is invalid".into(),
        ));
    }
    let period = match cached.snapshot.period.as_str() {
        "weekly" => SuperGrokPeriod::Weekly,
        "monthly" => SuperGrokPeriod::Monthly,
        "unknown" => SuperGrokPeriod::Unknown,
        _ => {
            return Err(AppError::Schema(
                "SuperGrok cached period kind is invalid".into(),
            ));
        }
    };
    if cached
        .snapshot
        .prepaid_balance
        .is_some_and(|balance| !balance.is_finite() || balance < 0.0)
    {
        return Err(AppError::Schema(
            "SuperGrok cached prepaid balance is invalid".into(),
        ));
    }

    Ok(SuperGrokSnapshot {
        plan: cached.snapshot.plan,
        account: account_scope.to_string(),
        metered: cached.snapshot.metered,
        weekly_pct: cached.snapshot.percent,
        period,
        reset_at: cached.snapshot.reset_at,
        prepaid_balance: cached.snapshot.prepaid_balance,
    })
}

fn reuse_cache(
    bytes: &[u8],
    cache: &Cache,
    stale: bool,
    account_scope: &str,
) -> Result<FetchOutcome> {
    Ok(FetchOutcome {
        snapshot: parse_cache(bytes, account_scope)?,
        stale,
        last_error: cache.read_last_error(),
        cache_age: cache.payload_age(),
    })
}

fn fallback(
    cache: &Cache,
    account_scope: Option<&str>,
    now: DateTime<Utc>,
    original: AppError,
) -> Result<FetchOutcome> {
    let Some(account_scope) = account_scope else {
        return Err(original);
    };
    let Some(bytes) = cache.fallback_payload(MAX_STALE)? else {
        return Err(original);
    };
    match reuse_cache(&bytes, cache, true, account_scope) {
        Ok(mut outcome) if !period_has_ended(&outcome.snapshot, now) => {
            let error = error_to_pair(&original);
            cache.mark_stale();
            cache.write_last_error(error.0, &error.1);
            outcome.last_error = Some(error);
            Ok(outcome)
        }
        _ => Err(original),
    }
}

fn error_to_pair(error: &AppError) -> (u16, String) {
    match error {
        AppError::Http { status, body } => (*status, body.clone()),
        other => (0, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 0, 0, 0).unwrap()
    }

    fn fixture() -> (TempDir, Cache) {
        let td = TempDir::new().unwrap();
        let cache = Cache::at(td.path().join("supergrok"));
        (td, cache)
    }

    fn weekly_response(percent: f64) -> types::BillingResponse {
        serde_json::from_value(serde_json::json!({
            "config": {
                "creditUsagePercent": percent,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "end": "2026-08-14T00:00:00Z"
                }
            },
            "subscription_tier": "SuperGrok"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn live_fetch_writes_only_an_opaque_scope_to_cache() {
        let (_td, cache) = fixture();
        let outcome = fetch_snapshot_with(
            &cache,
            Duration::ZERO,
            now(),
            || Some("opaque-digest".into()),
            || async { Ok(weekly_response(12.4)) },
        )
        .await
        .unwrap();
        assert_eq!(outcome.snapshot.weekly_pct, 12);
        assert_eq!(outcome.snapshot.period, SuperGrokPeriod::Weekly);

        let cache_text = std::fs::read_to_string(cache.payload_path()).unwrap();
        assert!(cache_text.contains("opaque-digest"));
        assert!(!cache_text.contains("access_token"));
        assert!(!cache_text.contains("user_id"));
    }

    #[tokio::test]
    async fn fresh_cache_skips_the_acp_process() {
        let (_td, cache) = fixture();
        cache.ensure_dir().unwrap();
        let snapshot = types::to_snapshot(weekly_response(7.0), "scope-a").unwrap();
        cache
            .write_payload(
                &serde_json::to_vec(&CachedEnvelope::from_snapshot("scope-a", &snapshot)).unwrap(),
            )
            .unwrap();
        let called = AtomicBool::new(false);

        let outcome = fetch_snapshot_with(
            &cache,
            Duration::from_secs(3600),
            now(),
            || Some("scope-a".into()),
            || async {
                called.store(true, Ordering::SeqCst);
                Ok(weekly_response(99.0))
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.snapshot.weekly_pct, 7);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_scope_change_during_fetch_returns_live_but_does_not_cache() {
        let (_td, cache) = fixture();
        let calls = AtomicUsize::new(0);
        let outcome = fetch_snapshot_with(
            &cache,
            Duration::ZERO,
            now(),
            || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                Some(if call == 0 { "before" } else { "after" }.into())
            },
            || async { Ok(weekly_response(20.0)) },
        )
        .await
        .unwrap();
        assert_eq!(outcome.snapshot.account, "uncached");
        assert!(!cache.payload_path().exists());
    }

    #[tokio::test]
    async fn failure_falls_back_only_for_the_same_scope_and_live_period() {
        let (_td, cache) = fixture();
        cache.ensure_dir().unwrap();
        let snapshot = types::to_snapshot(weekly_response(33.0), "scope-a").unwrap();
        cache
            .write_payload(
                &serde_json::to_vec(&CachedEnvelope::from_snapshot("scope-a", &snapshot)).unwrap(),
            )
            .unwrap();

        let fallback = fetch_snapshot_with(
            &cache,
            Duration::ZERO,
            now(),
            || Some("scope-a".into()),
            || async { Err(AppError::Transport("offline".into())) },
        )
        .await
        .unwrap();
        assert!(fallback.stale);
        assert_eq!(fallback.snapshot.weekly_pct, 33);

        let other_scope = fetch_snapshot_with(
            &cache,
            Duration::ZERO,
            now(),
            || Some("scope-b".into()),
            || async { Err(AppError::Transport("offline".into())) },
        )
        .await;
        assert!(other_scope.is_err());
    }

    #[tokio::test]
    async fn malformed_live_billing_preserves_the_last_good_same_scope_cache() {
        let (_td, cache) = fixture();
        cache.ensure_dir().unwrap();
        let snapshot = types::to_snapshot(weekly_response(33.0), "scope-a").unwrap();
        cache
            .write_payload(
                &serde_json::to_vec(&CachedEnvelope::from_snapshot("scope-a", &snapshot)).unwrap(),
            )
            .unwrap();

        let fallback = fetch_snapshot_with(
            &cache,
            Duration::ZERO,
            now(),
            || Some("scope-a".into()),
            || async { Ok(weekly_response(999.0)) },
        )
        .await
        .unwrap();
        assert!(fallback.stale);
        assert_eq!(fallback.snapshot.weekly_pct, 33);
        assert!(
            fallback
                .last_error
                .as_ref()
                .is_some_and(|(_, message)| message.contains("outside the supported range"))
        );
    }

    #[tokio::test]
    async fn missing_scope_disables_cache_reuse() {
        let (_td, cache) = fixture();
        cache.ensure_dir().unwrap();
        let snapshot = types::to_snapshot(weekly_response(33.0), "scope-a").unwrap();
        cache
            .write_payload(
                &serde_json::to_vec(&CachedEnvelope::from_snapshot("scope-a", &snapshot)).unwrap(),
            )
            .unwrap();
        let outcome = fetch_snapshot_with(
            &cache,
            Duration::from_secs(3600),
            now(),
            || None,
            || async { Err(AppError::Transport("offline".into())) },
        )
        .await;
        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn an_ended_period_is_never_resurrected_on_failure() {
        let (_td, cache) = fixture();
        cache.ensure_dir().unwrap();
        let snapshot = types::to_snapshot(weekly_response(88.0), "scope-a").unwrap();
        cache
            .write_payload(
                &serde_json::to_vec(&CachedEnvelope::from_snapshot("scope-a", &snapshot)).unwrap(),
            )
            .unwrap();
        let after_reset = Utc.with_ymd_and_hms(2026, 8, 15, 0, 0, 0).unwrap();
        let outcome = fetch_snapshot_with(
            &cache,
            Duration::from_secs(3600),
            after_reset,
            || Some("scope-a".into()),
            || async { Err(AppError::Transport("offline".into())) },
        )
        .await;
        assert!(outcome.is_err());
    }

    #[test]
    fn cached_percentages_and_periods_are_strictly_validated() {
        let base = serde_json::json!({
            "schema": CACHE_SCHEMA,
            "scope": "scope-a",
            "snapshot": {
                "plan": "SuperGrok",
                "percent": 5,
                "period": "weekly",
                "reset_at": null,
                "prepaid_balance": null
            }
        });
        for (field, value) in [
            ("percent", serde_json::json!(101)),
            ("period", serde_json::json!("yearly")),
            ("prepaid_balance", serde_json::json!(-1.0)),
        ] {
            let mut malformed = base.clone();
            malformed["snapshot"][field] = value;
            assert!(
                parse_cache(malformed.to_string().as_bytes(), "scope-a").is_err(),
                "field: {field}"
            );
        }

        let mut obsolete = base;
        obsolete["schema"] = serde_json::json!(1);
        obsolete["account"] = serde_json::json!("person@example.test");
        assert!(parse_cache(obsolete.to_string().as_bytes(), "scope-a").is_err());
    }
}

/// The token transport first, ACP second.
///
/// Upstream's ACP path is the documented one, but grok 1.0.5 answers
/// `-32601 Method not found` for `x.ai/billing`, so on this machine it never
/// succeeds. Trying the token first means the row works today; keeping ACP
/// behind it means the row keeps working, unchanged, if xAI restores the
/// method or ships a CLI that has it.
async fn billing_via_token_then_acp(
    grok_binary: &std::path::Path,
    auth_path: &std::path::Path,
) -> Result<super::types::BillingResponse> {
    match super::token::fetch_billing(auth_path).await {
        Ok(resp) => Ok(resp),
        Err(token_err) => match acp::fetch_billing(grok_binary).await {
            Ok(resp) => Ok(resp),
            // The token error is the one worth reporting: it is the path that
            // is expected to work, so its reason is the actionable one.
            Err(_) => Err(token_err),
        },
    }
}
