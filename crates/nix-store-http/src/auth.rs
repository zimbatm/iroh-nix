//! Client-side authentication for upload operations
//!
//! Supports static token auth and OIDC token detection from CI environments
//! (GitHub Actions, GitLab CI).

use nix_store::{Error, Result};

/// Source of an OIDC token
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OidcSource {
    GitHubActions,
    GitLabCI,
    GenericEnv,
}

/// Authentication provider for cache upload operations
#[derive(Debug, Clone)]
pub enum AuthProvider {
    /// No authentication
    None,
    /// Static bearer token (e.g. from NIX_CACHE_TOKEN env var)
    StaticToken(String),
    /// OIDC token from a CI provider
    Oidc { token: String, source: OidcSource },
}

impl AuthProvider {
    /// Auto-detect authentication from environment variables.
    ///
    /// Detection order:
    /// 1. `NIX_CACHE_TOKEN` -> StaticToken
    /// 2. GitHub Actions OIDC (`ACTIONS_ID_TOKEN_REQUEST_URL` + `ACTIONS_ID_TOKEN_REQUEST_TOKEN`)
    /// 3. GitLab CI (`CI_JOB_JWT_V2`)
    /// 4. Falls back to `None`
    pub async fn detect(client: &reqwest::Client, audience: Option<&str>) -> Self {
        // 1. Static token from env
        if let Ok(token) = std::env::var("NIX_CACHE_TOKEN") {
            if !token.is_empty() {
                tracing::info!("Using static token from NIX_CACHE_TOKEN");
                return AuthProvider::StaticToken(token);
            }
        }

        // 2. GitHub Actions OIDC
        if let (Ok(request_url), Ok(request_token)) = (
            std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
            std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
        ) {
            if !request_url.is_empty() && !request_token.is_empty() {
                match fetch_github_oidc_token(client, &request_url, &request_token, audience).await
                {
                    Ok(token) => {
                        tracing::info!("Using OIDC token from GitHub Actions");
                        return AuthProvider::Oidc {
                            token,
                            source: OidcSource::GitHubActions,
                        };
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch GitHub Actions OIDC token: {}", e);
                    }
                }
            }
        }

        // 3. GitLab CI JWT
        if let Ok(token) = std::env::var("CI_JOB_JWT_V2") {
            if !token.is_empty() {
                tracing::info!("Using OIDC token from GitLab CI");
                return AuthProvider::Oidc {
                    token,
                    source: OidcSource::GitLabCI,
                };
            }
        }

        tracing::debug!("No authentication detected");
        AuthProvider::None
    }

    /// Create a static token provider
    pub fn from_static(token: String) -> Self {
        AuthProvider::StaticToken(token)
    }

    /// Apply authentication to an HTTP request builder.
    /// Returns the builder with the Authorization header added (if authenticated).
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AuthProvider::None => builder,
            AuthProvider::StaticToken(token) => builder.bearer_auth(token),
            AuthProvider::Oidc { token, .. } => builder.bearer_auth(token),
        }
    }

    /// Returns true if no authentication is configured
    pub fn is_none(&self) -> bool {
        matches!(self, AuthProvider::None)
    }
}

/// Fetch an OIDC token from GitHub Actions token endpoint
async fn fetch_github_oidc_token(
    client: &reqwest::Client,
    request_url: &str,
    request_token: &str,
    audience: Option<&str>,
) -> Result<String> {
    let mut url = request_url.to_string();
    if let Some(aud) = audience {
        url.push_str(&format!("&audience={}", aud));
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        value: String,
    }

    let response = client
        .get(&url)
        .header("Authorization", format!("bearer {}", request_token))
        .header("Accept", "application/json; api-version=2.0")
        .send()
        .await
        .map_err(|e| Error::Auth(format!("GitHub OIDC request failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(Error::Auth(format!(
            "GitHub OIDC token request returned {}",
            response.status()
        )));
    }

    let token_resp: TokenResponse = response
        .json()
        .await
        .map_err(|e| Error::Auth(format!("GitHub OIDC response parse error: {}", e)))?;

    Ok(token_resp.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_provider_from_static() {
        let auth = AuthProvider::from_static("my-token".to_string());
        assert!(!auth.is_none());
        match auth {
            AuthProvider::StaticToken(t) => assert_eq!(t, "my-token"),
            _ => panic!("expected StaticToken"),
        }
    }

    #[test]
    fn test_auth_provider_none() {
        let auth = AuthProvider::None;
        assert!(auth.is_none());
    }

    // NOTE: env-var-based detection tests are inherently racy in parallel test
    // execution. We combine them into a single sequential test.
    #[tokio::test]
    async fn test_detect_from_env() {
        // Clear all auth env vars first
        std::env::remove_var("NIX_CACHE_TOKEN");
        std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
        std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        std::env::remove_var("CI_JOB_JWT_V2");

        let client = reqwest::Client::new();

        // With no env vars, should fall back to None
        let auth = AuthProvider::detect(&client, None).await;
        assert!(auth.is_none(), "expected None with no env vars set");

        // With NIX_CACHE_TOKEN set, should detect StaticToken
        std::env::set_var("NIX_CACHE_TOKEN", "test-token-123");
        let auth = AuthProvider::detect(&client, None).await;
        std::env::remove_var("NIX_CACHE_TOKEN");

        match auth {
            AuthProvider::StaticToken(t) => assert_eq!(t, "test-token-123"),
            _ => panic!("expected StaticToken from NIX_CACHE_TOKEN"),
        }
    }
}
