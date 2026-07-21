//! Read gemini-cli's OAuth credentials without modifying its state.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since the Unix epoch, as written by gemini-cli.
    pub expiry_date: i64,
}

pub fn default_path() -> Result<PathBuf> {
    Ok(crate::cache::home_dir()?
        .join(".gemini")
        .join("oauth_creds.json"))
}

pub fn read_from(path: &Path) -> Result<OAuthCredentials> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        AppError::Credentials(format!(
            "could not read {}: {e}. Run `gemini` and sign in first.",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        AppError::Credentials(format!(
            "could not parse {}: {e}. Run `gemini` and sign in again.",
            path.display()
        ))
    })
}

/// Refresh this many milliseconds before the hard expiry so a token can't
/// lapse between `loadCodeAssist` and `retrieveUserQuota`. Mirrors
/// google-auth-library's `DEFAULT_EAGER_REFRESH_THRESHOLD_MILLIS` (5 min),
/// which is what gemini-cli itself uses to refresh proactively.
pub const EAGER_REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

impl OAuthCredentials {
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.expiry_date - EAGER_REFRESH_MARGIN_MS <= now_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_credentials_and_detects_expiry_in_milliseconds() {
        let expiry = 10 * 60 * 1000; // 600_000 ms
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{"access_token":"at","refresh_token":"rt","expiry_date":{expiry}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let creds = read_from(file.path()).unwrap();
        // Fresh well before the eager-refresh threshold.
        assert!(!creds.is_expired_at(expiry - EAGER_REFRESH_MARGIN_MS - 1));
        // Within the 5-minute eager-refresh margin: refresh proactively.
        assert!(creds.is_expired_at(expiry - EAGER_REFRESH_MARGIN_MS));
        assert!(creds.is_expired_at(expiry - EAGER_REFRESH_MARGIN_MS + 1));
        // Past the hard expiry: definitely refresh.
        assert!(creds.is_expired_at(expiry));
    }

    #[test]
    fn unreadable_file_is_a_credentials_error() {
        let err = read_from(Path::new("/definitely/missing/gemini/oauth_creds.json")).unwrap_err();
        assert!(matches!(err, AppError::Credentials(_)));
    }

    #[test]
    fn default_path_ends_with_gemini_oauth_creds() {
        assert!(
            default_path()
                .unwrap()
                .ends_with(Path::new(".gemini").join("oauth_creds.json"))
        );
    }
}
