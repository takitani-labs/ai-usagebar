//! OAuth refresh for the public gemini-cli desktop application client.

use serde::Deserialize;

use crate::error::{AppError, Result};

pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";

/// Env var an operator can set with gemini-cli's public desktop client secret,
/// so it never has to live in this repo. Config `[gemini] oauth_client_secret`
/// is the other source.
pub const CLIENT_SECRET_ENV: &str = "GEMINI_OAUTH_CLIENT_SECRET";

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoint: &str,
    refresh_token: &str,
    client_secret: &str,
) -> Result<RefreshResponse> {
    let form = [
        ("client_id", CLIENT_ID),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    let response = client.post(endpoint).form(&form).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::Http {
            status: status.as_u16(),
            body: "Gemini OAuth refresh failed; run `gemini` to sign in again".into(),
        });
    }
    response
        .json()
        .await
        .map_err(|e| AppError::Schema(format!("gemini token response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_uses_gemini_cli_form_fields() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/token")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("client_id".into(), CLIENT_ID.into()),
                mockito::Matcher::UrlEncoded("client_secret".into(), "test-secret".into()),
                mockito::Matcher::UrlEncoded("refresh_token".into(), "old-rt".into()),
                mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            ]))
            .with_status(200)
            .with_body(r#"{"access_token":"new-at","expires_in":3600}"#)
            .create_async()
            .await;

        let response = refresh(
            &reqwest::Client::new(),
            &format!("{}/token", server.url()),
            "old-rt",
            "test-secret",
        )
        .await
        .unwrap();
        assert_eq!(response.access_token, "new-at");
        assert_eq!(response.expires_in, Some(3600));
        mock.assert_async().await;
    }
}
