use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProjectsReport {
    pub generated_at: DateTime<Utc>,
    pub windows: Windows,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Windows {
    pub session: UsageWindow,
    pub weekly: UsageWindow,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageWindow {
    pub hours: u64,
    pub projects: Vec<ProjectUsage>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProjectUsage {
    pub id: String,
    pub label: String,
    pub share_pct: f64,
    pub weighted_tokens: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub profiles: Vec<String>,
}
