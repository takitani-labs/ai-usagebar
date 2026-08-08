//! Read Grok Build's own log for the refusal the billing API never reports.
//!
//! `/v1/billing` describes the credit pool. It does not describe the spending
//! limit that actually refuses turns: this account sat at 36% of its 4000
//! credits while every request came back `403 personal-team-blocked:
//! spending-limit`, and the pool counter then froze, because a refused request
//! spends nothing. A widget reading only the pool reports 64% of headroom while
//! the CLI cannot answer at all.
//!
//! Ten plausible endpoints (`/limits`, `/quotas`, `/usage-limits`,
//! `/entitlement`, …) all answer 404 and `/billing` sends no rate-limit
//! headers, so the CLI's log is the only place on the machine that knows. This
//! reads the tail of it, never writes.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Enough tail to hold the recent turns of a busy session without reading a
/// log that grows past a megabyte a week.
const TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub at: DateTime<Utc>,
    /// The upstream text, kept verbatim: it names the fix (buy credits or
    /// subscribe) and that is the whole value of surfacing it.
    pub message: String,
}

pub fn default_log_path() -> Option<PathBuf> {
    crate::cache::home_dir()
        .ok()
        .map(|h| h.join(".grok").join("logs").join("unified.jsonl"))
}

/// The active spending refusal, if the last thing the CLI tried was refused.
///
/// A block is reported only when no successful inference followed it, so a
/// refusal the user has already resolved stops showing as soon as one turn
/// goes through. `period_start` scopes it to the current billing period: the
/// spending limit resets with the period, so a refusal from a closed one says
/// nothing about today.
pub fn active(log_path: &Path, period_start: Option<DateTime<Utc>>) -> Option<Block> {
    let tail = read_tail(log_path, TAIL_BYTES)?;
    let mut newest_block: Option<Block> = None;
    let mut newest_success: Option<DateTime<Utc>> = None;

    for line in tail.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(at) = v["ts"].as_str().and_then(parse_ts) else {
            continue;
        };
        match v["msg"].as_str() {
            Some("shell.turn.inference_done") => {
                if newest_success.is_none_or(|s| at > s) {
                    newest_success = Some(at);
                }
            }
            Some("shell.turn.inference_failed") | Some("turn.terminal_failure") => {
                let message = v["ctx"]["message"].as_str().unwrap_or_default();
                let status = v["ctx"]["status_code"].as_u64().unwrap_or_default();
                if !is_spending_refusal(status, message) {
                    continue;
                }
                if newest_block.as_ref().is_none_or(|b| at > b.at) {
                    newest_block = Some(Block {
                        at,
                        message: message.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    let block = newest_block?;
    if newest_success.is_some_and(|s| s >= block.at) {
        return None;
    }
    if period_start.is_some_and(|start| block.at < start) {
        return None;
    }
    Some(block)
}

/// A spending refusal, not any 4xx. The marker xAI sends is
/// `personal-team-blocked:spending-limit`; the credit wording is accepted too
/// so a renamed marker does not silence the widget entirely.
fn is_spending_refusal(status: u64, message: &str) -> bool {
    if !matches!(status, 402 | 403) {
        return false;
    }
    let m = message.to_ascii_lowercase();
    m.contains("spending-limit") || m.contains("run out of credits")
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.into())
}

/// Last `want` bytes of a file, starting at the first whole line. Returns None
/// when the log is absent, which is the normal state on a machine that never
/// ran Grok Build.
fn read_tail(path: &Path, want: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(want);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(want.min(len) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if from == 0 {
        return Some(text);
    }
    // The window almost certainly split a line; drop the partial head.
    match text.find('\n') {
        Some(i) => Some(text[i + 1..].to_string()),
        None => Some(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;

    const REFUSAL: &str = "API error (status 403 Forbidden): personal-team-blocked:spending-limit: You have run out of credits or need a Grok subscription.";

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
    }

    fn line(ts: &str, msg: &str, status: u64, message: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","msg":"{msg}","ctx":{{"status_code":{status},"message":"{message}"}}}}"#
        )
    }

    fn log(lines: &[String]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn reports_a_refusal_that_nothing_succeeded_after() {
        let f = log(&[
            line("2026-08-08T13:00:00Z", "shell.turn.inference_done", 200, ""),
            line(
                "2026-08-08T16:23:02Z",
                "turn.terminal_failure",
                403,
                REFUSAL,
            ),
        ]);
        let b = active(f.path(), Some(at(1, 0))).unwrap();
        assert_eq!(b.at, Utc.with_ymd_and_hms(2026, 8, 8, 16, 23, 2).unwrap());
        assert!(b.message.contains("spending-limit"));
    }

    #[test]
    fn a_later_success_clears_it() {
        let f = log(&[
            line(
                "2026-08-08T16:23:02Z",
                "turn.terminal_failure",
                403,
                REFUSAL,
            ),
            line("2026-08-08T17:00:00Z", "shell.turn.inference_done", 200, ""),
        ]);
        assert_eq!(active(f.path(), Some(at(1, 0))), None);
    }

    #[test]
    fn a_refusal_from_a_closed_billing_period_is_ignored() {
        // The spending limit resets with the period, so July says nothing.
        let f = log(&[line(
            "2026-07-30T10:00:00Z",
            "turn.terminal_failure",
            403,
            REFUSAL,
        )]);
        assert_eq!(active(f.path(), Some(at(1, 0))), None);
        // Without a known period, the refusal still stands on its own.
        assert!(active(f.path(), None).is_some());
    }

    #[test]
    fn unrelated_failures_are_not_spending_refusals() {
        let f = log(&[
            line("2026-08-08T10:00:00Z", "turn.terminal_failure", 500, "boom"),
            line(
                "2026-08-08T11:00:00Z",
                "turn.terminal_failure",
                403,
                "forbidden: bad tool call",
            ),
            line(
                "2026-08-08T12:00:00Z",
                "shell.turn.inference_failed",
                429,
                "slow down",
            ),
        ]);
        assert_eq!(active(f.path(), None), None);
    }

    #[test]
    fn a_missing_log_is_not_an_error() {
        assert_eq!(active(Path::new("/nonexistent/unified.jsonl"), None), None);
    }

    #[test]
    fn the_tail_window_starts_at_a_whole_line() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Pad past the window so the head gets cut mid-line.
        for i in 0..40_000 {
            writeln!(f, r#"{{"ts":"2026-08-01T00:00:00Z","msg":"pad","n":{i}}}"#).unwrap();
        }
        writeln!(
            f,
            "{}",
            line(
                "2026-08-08T16:23:02Z",
                "turn.terminal_failure",
                403,
                REFUSAL
            )
        )
        .unwrap();
        f.flush().unwrap();
        assert!(active(f.path(), None).is_some(), "tail must stay parseable");
    }
}
