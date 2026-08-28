//! Read Grok Build's credentials without modifying its state.
//!
//! `~/.grok/auth.json` maps `"<oidc_issuer>::<client_id>"` to one signed-in
//! principal each. Only the access token is read here: refreshing it would mean
//! writing a rotated refresh token back into the file the CLI owns, and losing
//! that race signs the user out of Grok Build. The CLI refreshes the token
//! itself (~6h lifetime) every time it runs.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    /// Bearer token for cli-chat-proxy. Grok Build names this field `key`.
    pub key: String,
    /// Hard expiry, RFC3339. Absent entries are treated as never-expiring so a
    /// schema change cannot blank the widget on its own.
    pub expires_at: Option<DateTime<Utc>>,
}

pub fn default_path() -> Result<PathBuf> {
    Ok(crate::cache::home_dir()?.join(".grok").join("auth.json"))
}

pub fn read_from(path: &Path) -> Result<Credentials> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        AppError::Credentials(format!(
            "could not read {}: {e}. Run `grok login` first.",
            path.display()
        ))
    })?;
    let entries: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&raw).map_err(|e| {
            AppError::Credentials(format!(
                "could not parse {}: {e}. Run `grok login` again.",
                path.display()
            ))
        })?;

    // More than one principal can be signed in (personal + team). Prefer the
    // token that stays valid longest; anything unparseable is skipped rather
    // than failing the whole file.
    let mut best: Option<Credentials> = None;
    for value in entries.into_values() {
        let Ok(creds) = serde_json::from_value::<Credentials>(value) else {
            continue;
        };
        if creds.key.is_empty() {
            continue;
        }
        match &best {
            Some(current) if current.expires_at >= creds.expires_at => {}
            _ => best = Some(creds),
        }
    }

    best.ok_or_else(|| {
        AppError::Credentials(format!(
            "no usable Grok credentials in {}. Run `grok login`.",
            path.display()
        ))
    })
}

impl Credentials {
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 31, 12, 0, 0).unwrap()
    }

    fn write_auth(body: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{body}").unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn reads_the_single_signed_in_principal() {
        let file = write_auth(
            r#"{"https://auth.x.ai::client-a":
                {"key":"tok","expires_at":"2026-07-31T18:00:00Z","email":"a@b.c"}}"#,
        );
        let creds = read_from(file.path()).unwrap();
        assert_eq!(creds.key, "tok");
        assert!(!creds.is_expired_at(now()));
        assert!(creds.is_expired_at(now() + chrono::Duration::hours(7)));
    }

    #[test]
    fn prefers_the_longest_lived_token_and_skips_broken_entries() {
        let file = write_auth(
            r#"{"a::1": {"key":"old","expires_at":"2026-07-31T13:00:00Z"},
                "b::2": {"key":"new","expires_at":"2026-07-31T20:00:00Z"},
                "c::3": {"no_key":true}}"#,
        );
        assert_eq!(read_from(file.path()).unwrap().key, "new");
    }

    #[test]
    fn entry_without_expiry_is_usable() {
        let file = write_auth(r#"{"a::1": {"key":"tok"}}"#);
        let creds = read_from(file.path()).unwrap();
        assert_eq!(creds.key, "tok");
        assert!(!creds.is_expired_at(now()));
    }

    #[test]
    fn empty_file_names_the_login_command() {
        let file = write_auth("{}");
        let err = read_from(file.path()).unwrap_err();
        assert!(format!("{err:?}").contains("grok login"));
    }
}
