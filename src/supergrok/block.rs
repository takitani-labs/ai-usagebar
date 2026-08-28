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
//! reads it backwards, never writes.
//!
//! Backwards, and not a fixed tail: this used to read the last 256KB, which
//! stopped being enough once the log passed 4MB and kept growing ~100KB a day.
//! On 2026-08-10 the refusal sat 114KB before the start of that window, so the
//! one signal the machine had for an active block was already invisible. The
//! answer only ever depends on the NEWEST relevant line, so the scan now stops
//! at the first one it meets instead of sizing a window and hoping.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Step size for walking the log backwards. Large enough that the tool-call
/// noise between two turns rarely costs a second read, small enough that the
/// common case (the newest turn decides) touches one chunk of a log that is
/// already megabytes long.
const CHUNK_BYTES: u64 = 64 * 1024;
/// Give up rather than walk a pathological log to its head. A refusal buried
/// under this much newer traffic is not the state the user is in now.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;

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
    // Exactly one line decides this: the newest one that settles anything.
    // Everything older is already answered by it, which is why the scan can
    // stop there instead of reading a window and comparing timestamps.
    match events_newest_first(log_path).next()? {
        // A turn that went through means whatever was refused before it has
        // been resolved.
        Event::Success => None,
        Event::Refusal(block) => period_start
            .is_none_or(|start| block.at >= start)
            .then_some(block),
    }
}

enum Event {
    Success,
    Refusal(Block),
}

/// The log's relevant lines, newest first, reading only as far back as the
/// caller consumes. Lines that settle nothing (tool calls, prompts, failures
/// that are not about spending) never surface.
fn events_newest_first(log_path: &Path) -> impl Iterator<Item = Event> {
    let mut file = std::fs::File::open(log_path).ok();
    let len = file
        .as_ref()
        .and_then(|f| f.metadata().ok())
        .map(|m| m.len())
        .unwrap_or(0);

    let mut end = len;
    // The head of a chunk is almost certainly half a line whose beginning lives
    // in the next (older) chunk, so it rides along to be joined there.
    let mut carry = String::new();
    let mut pending: Vec<Event> = Vec::new();

    std::iter::from_fn(move || {
        loop {
            if let Some(event) = pending.pop() {
                return Some(event);
            }
            let f = file.as_mut()?;
            if end == 0 || len - end >= MAX_SCAN_BYTES {
                return None;
            }

            let start = end.saturating_sub(CHUNK_BYTES);
            let mut buf = vec![0u8; (end - start) as usize];
            if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
                return None;
            }
            let mut text = String::from_utf8_lossy(&buf).into_owned();
            text.push_str(&carry);

            let body = match text.find('\n') {
                // At the head of the file nothing is truncated, so keep it all.
                _ if start == 0 => {
                    carry = String::new();
                    text
                }
                Some(i) => {
                    carry = text[..i].to_string();
                    text[i + 1..].to_string()
                }
                // One line longer than a whole chunk: carry it and read on.
                None => {
                    carry = text;
                    String::new()
                }
            };
            end = start;

            // Oldest-first into the stack, so popping yields newest-first.
            pending.extend(body.lines().filter_map(classify));
        }
    })
}

fn classify(line: &str) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let at = v["ts"].as_str().and_then(parse_ts)?;
    match v["msg"].as_str()? {
        "shell.turn.inference_done" => Some(Event::Success),
        "shell.turn.inference_failed" | "turn.terminal_failure" => {
            let message = v["ctx"]["message"].as_str().unwrap_or_default();
            let status = v["ctx"]["status_code"].as_u64().unwrap_or_default();
            is_spending_refusal(status, message).then(|| {
                Event::Refusal(Block {
                    at,
                    message: message.to_string(),
                })
            })
        }
        _ => None,
    }
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

    /// Pad lines that settle nothing: tool noise never clears a block.
    fn pad_line(i: usize) -> String {
        format!(r#"{{"ts":"2026-08-08T17:00:00Z","msg":"shell.tool.exec_done","n":{i}}}"#)
    }

    #[test]
    fn a_refusal_buried_under_newer_noise_is_still_found() {
        // The bug this scan replaced. On 2026-08-10 the only refusal sat 114KB
        // before the start of the old 256KB window, on a 4.7MB log, so the one
        // signal the machine had was invisible.
        let mut f = tempfile::NamedTempFile::new().unwrap();
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
        for i in 0..8_000 {
            writeln!(f, "{}", pad_line(i)).unwrap();
        }
        f.flush().unwrap();

        assert!(
            std::fs::metadata(f.path()).unwrap().len() > 256 * 1024,
            "the padding has to outgrow the window this replaced"
        );
        assert!(
            active(f.path(), None).is_some(),
            "a refusal must not scroll out of view"
        );
    }

    #[test]
    fn a_later_success_still_clears_it_across_many_chunks() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
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
        for i in 0..8_000 {
            writeln!(f, "{}", pad_line(i)).unwrap();
        }
        writeln!(
            f,
            "{}",
            line("2026-08-08T18:00:00Z", "shell.turn.inference_done", 200, "")
        )
        .unwrap();
        f.flush().unwrap();

        assert_eq!(active(f.path(), None), None);
    }

    #[test]
    fn a_refusal_split_across_a_chunk_boundary_survives() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        let refusal = line(
            "2026-08-08T16:23:02Z",
            "turn.terminal_failure",
            403,
            REFUSAL,
        );
        writeln!(f, "{refusal}").unwrap();
        // Pad until the boundary, counted back from EOF, lands inside that
        // line. This is the only case the carry logic exists for.
        let mut written = refusal.len() + 1;
        let target = (refusal.len() / 2) as u64 + CHUNK_BYTES;
        let mut i = 0;
        while (written as u64) < target {
            let p = pad_line(i);
            writeln!(f, "{p}").unwrap();
            written += p.len() + 1;
            i += 1;
        }
        f.flush().unwrap();

        assert!(
            active(f.path(), None).is_some(),
            "the carry must rejoin a line the chunking split"
        );
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
