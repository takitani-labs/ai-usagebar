//! Lenient wire types for the Cloud Code Code Assist APIs.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{AppError, Result};
use crate::usage::{GeminiBucket, GeminiSnapshot, UsageWindow};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LoadCodeAssistResponse {
    #[serde(rename = "currentTier")]
    current_tier: Option<Tier>,
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct Tier {
    id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct QuotaResponse {
    buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct QuotaBucket {
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
    #[serde(rename = "tokenType")]
    token_type: String,
    #[serde(rename = "modelId")]
    model_id: String,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
}

impl LoadCodeAssistResponse {
    pub fn tier_and_project(self) -> Result<(String, String)> {
        let tier = self
            .current_tier
            .and_then(|tier| tier.id)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AppError::Schema("gemini: missing currentTier.id".into()))?;
        let project = self
            .project
            .filter(|project| !project.is_empty())
            .ok_or_else(|| AppError::Schema("gemini: missing cloudaicompanionProject".into()))?;
        Ok((tier, project))
    }
}

impl QuotaResponse {
    pub fn into_snapshot(self, tier: String) -> Result<GeminiSnapshot> {
        let mut buckets = Vec::new();
        for bucket in self.buckets {
            if bucket.token_type != "REQUESTS" || bucket.model_id.is_empty() {
                continue;
            }
            let Some(remaining) = bucket.remaining_fraction else {
                continue;
            };
            let resets_at = parse_reset(bucket.reset_time.as_deref())?;
            buckets.push(GeminiBucket {
                model: bucket.model_id,
                window: UsageWindow {
                    utilization_pct: utilization_pct(remaining),
                    resets_at,
                    window_duration: chrono::Duration::hours(24),
                },
            });
        }
        buckets.sort_by(|a, b| a.model.cmp(&b.model));
        Ok(GeminiSnapshot { tier, buckets })
    }
}

pub fn utilization_pct(remaining_fraction: f64) -> i32 {
    ((1.0 - remaining_fraction) * 100.0)
        .round()
        .clamp(0.0, 100.0) as i32
}

fn parse_reset(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    match value {
        None | Some("") => Ok(None),
        Some(value) => DateTime::parse_from_rfc3339(value)
            .map(|dt| Some(dt.into()))
            .map_err(|e| AppError::Schema(format!("gemini: unparseable resetTime: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quota_skips_unusable_buckets_and_sorts_models() {
        let raw = r#"{
            "buckets": [
                {"tokenType":"REQUESTS","modelId":"gemini-2.5-pro","remainingFraction":0.25,"resetTime":"2026-07-22T13:20:53Z","futureField":true},
                {"tokenType":"REQUESTS","modelId":"gemini-2.0-flash","remainingFraction":1},
                {"tokenType":"REQUESTS","modelId":"gemini-missing-fraction"},
                {"tokenType":"TOKENS","modelId":"gemini-token-bucket","remainingFraction":0}
            ],
            "futureTopLevel": {}
        }"#;
        let snapshot = serde_json::from_str::<QuotaResponse>(raw)
            .unwrap()
            .into_snapshot("standard-tier".into())
            .unwrap();

        assert_eq!(snapshot.tier, "standard-tier");
        assert_eq!(snapshot.buckets.len(), 2);
        assert_eq!(snapshot.buckets[0].model, "gemini-2.0-flash");
        assert_eq!(snapshot.buckets[0].window.utilization_pct, 0);
        assert_eq!(snapshot.buckets[1].model, "gemini-2.5-pro");
        assert_eq!(snapshot.buckets[1].window.utilization_pct, 75);
        assert!(snapshot.buckets[1].window.resets_at.is_some());
    }

    #[test]
    fn remaining_fraction_rounds_and_clamps_to_percent() {
        assert_eq!(utilization_pct(1.0), 0);
        assert_eq!(utilization_pct(0.755), 25);
        assert_eq!(utilization_pct(0.5), 50);
        assert_eq!(utilization_pct(0.0), 100);
        assert_eq!(utilization_pct(1.5), 0);
        assert_eq!(utilization_pct(-0.5), 100);
    }

    #[test]
    fn load_response_extracts_tier_and_project() {
        let response: LoadCodeAssistResponse = serde_json::from_str(
            r#"{"currentTier":{"id":"standard-tier"},"cloudaicompanionProject":"project-123","newField":1}"#,
        )
        .unwrap();
        assert_eq!(
            response.tier_and_project().unwrap(),
            ("standard-tier".into(), "project-123".into())
        );
    }
}
