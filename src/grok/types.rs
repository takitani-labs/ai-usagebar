//! Wire types for Grok Build's `/v1/billing` endpoint.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{AppError, Result};
use crate::usage::GrokSnapshot;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct BillingResponse {
    config: Option<BillingConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct BillingConfig {
    #[serde(rename = "monthlyLimit", alias = "monthly_limit")]
    monthly_limit: Option<Amount>,
    used: Option<Amount>,
    #[serde(rename = "onDemandCap", alias = "on_demand_cap")]
    on_demand_cap: Option<Amount>,
    #[serde(rename = "billingPeriodStart", alias = "billing_period_start")]
    billing_period_start: Option<String>,
    #[serde(rename = "billingPeriodEnd", alias = "billing_period_end")]
    billing_period_end: Option<String>,
}

/// Credit amounts arrive wrapped as `{"val": N}`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Amount {
    val: Option<NumericOrString>,
}

/// xAI sends whole credits today but charges fractional ones, and other
/// vendors on this proxy quote the same fields as strings. Accept both rather
/// than blanking the widget the first time a decimal shows up.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum NumericOrString {
    Number(f64),
    String(String),
}

impl NumericOrString {
    fn as_f64(&self) -> Option<f64> {
        match self {
            NumericOrString::Number(n) => n.is_finite().then_some(*n),
            NumericOrString::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        }
    }
}

impl BillingResponse {
    pub fn into_snapshot(self) -> Result<GrokSnapshot> {
        let config = self
            .config
            .ok_or_else(|| AppError::Schema("grok: missing billing config".into()))?;

        // The monthly allowance is the whole point of the row: without it there
        // is no denominator, and silently showing 0% would hide the drift.
        let monthly_limit = parse_amount(&config.monthly_limit, "monthlyLimit")?
            .ok_or_else(|| AppError::Schema("grok: missing monthlyLimit".into()))?;
        let monthly_used = parse_amount(&config.used, "used")?.unwrap_or(0.0);
        let on_demand_cap = parse_amount(&config.on_demand_cap, "onDemandCap")?.unwrap_or(0.0);

        Ok(GrokSnapshot {
            monthly_limit,
            monthly_used,
            monthly_remaining: (monthly_limit - monthly_used).max(0.0),
            on_demand_cap,
            period_start: parse_timestamp(config.billing_period_start.as_deref())?,
            period_end: parse_timestamp(config.billing_period_end.as_deref())?,
        })
    }
}

fn parse_amount(amount: &Option<Amount>, name: &str) -> Result<Option<f64>> {
    match amount.as_ref().and_then(|a| a.val.as_ref()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| AppError::Schema(format!("grok: invalid numeric value for {name}"))),
    }
}

fn parse_timestamp(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None | Some("") => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.into()))
            .map_err(|e| AppError::Schema(format!("grok: unparseable billing period: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "config": {
            "monthlyLimit": {"val": 4000},
            "used": {"val": 120},
            "onDemandCap": {"val": 0},
            "billingPeriodStart": "2026-08-01T00:00:00+00:00",
            "billingPeriodEnd": "2026-09-01T00:00:00+00:00",
            "history": [{"billingCycle": {"year": 2026, "month": 7},
                         "totalUsed": {"val": 0}}]
        }
    }"#;

    fn snapshot_of(raw: &str) -> Result<GrokSnapshot> {
        serde_json::from_str::<BillingResponse>(raw)
            .unwrap()
            .into_snapshot()
    }

    #[test]
    fn parses_the_representative_payload() {
        let snap = snapshot_of(SAMPLE).unwrap();
        assert_eq!(snap.monthly_limit, 4000.0);
        assert_eq!(snap.monthly_used, 120.0);
        assert_eq!(snap.monthly_remaining, 3880.0);
        assert_eq!(snap.monthly_pct(), 3);
        assert!(snap.period_end.is_some());
    }

    #[test]
    fn accepts_fractional_and_string_credits() {
        let snap =
            snapshot_of(r#"{"config": {"monthlyLimit": {"val": "4000"}, "used": {"val": 12.5}}}"#)
                .unwrap();
        assert_eq!(snap.monthly_used, 12.5);
        assert_eq!(snap.monthly_remaining, 3987.5);
    }

    #[test]
    fn overspending_never_reports_negative_credits() {
        let snap =
            snapshot_of(r#"{"config": {"monthlyLimit": {"val": 100}, "used": {"val": 130}}}"#)
                .unwrap();
        assert_eq!(snap.monthly_remaining, 0.0);
        assert_eq!(snap.monthly_pct(), 100);
    }

    #[test]
    fn missing_monthly_limit_is_schema_drift() {
        let err = snapshot_of(r#"{"config": {"used": {"val": 1}}}"#).unwrap_err();
        assert!(matches!(err, AppError::Schema(_)));
    }

    #[test]
    fn missing_config_is_schema_drift() {
        let err = snapshot_of("{}").unwrap_err();
        assert!(matches!(err, AppError::Schema(_)));
    }

    #[test]
    fn absent_usage_counts_as_nothing_spent() {
        let snap = snapshot_of(r#"{"config": {"monthlyLimit": {"val": 4000}}}"#).unwrap();
        assert_eq!(snap.monthly_used, 0.0);
        assert_eq!(snap.monthly_pct(), 0);
    }
}
