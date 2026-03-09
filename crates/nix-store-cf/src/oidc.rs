//! Server-side OIDC JWT validation for the Cloudflare Worker
//!
//! Validates bearer tokens against configured OIDC providers (e.g. GitHub Actions,
//! GitLab CI). Falls back to static token comparison.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use worker::*;

/// OIDC configuration, loaded from the OIDC_CONFIG Worker secret
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OidcConfig {
    pub providers: Vec<OidcProviderConfig>,
}

/// Configuration for a single OIDC provider
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OidcProviderConfig {
    /// Issuer URL (e.g. "https://token.actions.githubusercontent.com")
    pub issuer: String,
    /// Expected audience claim
    pub audience: String,
    /// Bound claims: claim name -> list of allowed glob patterns
    #[serde(default)]
    pub bound_claims: HashMap<String, Vec<String>>,
}

/// Result of authentication validation
#[derive(Debug)]
pub enum AuthResult {
    /// Authenticated via static token
    StaticToken,
    /// Authenticated via OIDC
    Oidc { issuer: String, subject: String },
    /// Authentication denied
    Denied(String),
}

impl AuthResult {
    pub fn is_allowed(&self) -> bool {
        !matches!(self, AuthResult::Denied(_))
    }
}

/// Validate an Authorization header against static token and/or OIDC config.
///
/// Tries static token first (constant-time comparison), then each OIDC provider.
pub async fn validate_auth(
    auth_header: &str,
    static_token: Option<&str>,
    oidc_config: Option<&OidcConfig>,
) -> AuthResult {
    let token = match auth_header.strip_prefix("Bearer ") {
        Some(t) => t.trim(),
        None => return AuthResult::Denied("expected Bearer token".into()),
    };

    // Try static token first (constant-time comparison)
    if let Some(expected) = static_token {
        if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return AuthResult::StaticToken;
        }
    }

    // Try OIDC providers
    if let Some(config) = oidc_config {
        for provider in &config.providers {
            match validate_oidc_token(token, provider).await {
                Ok(result) => return result,
                Err(e) => {
                    console_log!("OIDC validation error for {}: {}", provider.issuer, e);
                }
            }
        }
    }

    AuthResult::Denied("no valid authentication found".into())
}

/// Validate a JWT token against a single OIDC provider
async fn validate_oidc_token(
    token: &str,
    provider: &OidcProviderConfig,
) -> std::result::Result<AuthResult, String> {
    // Parse JWT header to get kid
    let header =
        jsonwebtoken::decode_header(token).map_err(|e| format!("invalid JWT header: {}", e))?;
    let kid = header
        .kid
        .ok_or_else(|| "JWT header missing kid".to_string())?;

    // Fetch JWKS for this provider
    let jwks = fetch_jwks(&provider.issuer).await?;

    // Find the matching key
    let jwk = jwks
        .keys
        .iter()
        .find(|k| k.kid.as_deref() == Some(&kid))
        .ok_or_else(|| format!("no JWK found with kid={}", kid))?;

    // Build decoding key from JWK
    let decoding_key =
        jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| format!("invalid JWK: {}", e))?;

    // Set up validation
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[&provider.audience]);

    // Decode and validate
    let token_data = jsonwebtoken::decode::<HashMap<String, serde_json::Value>>(
        token,
        &decoding_key,
        &validation,
    )
    .map_err(|e| format!("JWT validation failed: {}", e))?;

    let claims = token_data.claims;

    // Check bound claims
    for (claim_name, allowed_patterns) in &provider.bound_claims {
        let claim_value = claims
            .get(claim_name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("missing required claim: {}", claim_name))?;

        let matches = allowed_patterns
            .iter()
            .any(|pattern| glob_match::glob_match(pattern, claim_value));

        if !matches {
            return Ok(AuthResult::Denied(format!(
                "claim '{}' value '{}' does not match any allowed pattern",
                claim_name, claim_value
            )));
        }
    }

    let subject = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(AuthResult::Oidc {
        issuer: provider.issuer.clone(),
        subject,
    })
}

/// OIDC discovery document (subset of fields we need)
#[derive(Deserialize)]
struct OidcDiscovery {
    jwks_uri: String,
}

/// Fetch JWKS from an OIDC provider's discovery endpoint
async fn fetch_jwks(issuer: &str) -> std::result::Result<jsonwebtoken::jwk::JwkSet, String> {
    // Fetch discovery document
    let discovery_url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );

    let discovery_resp = fetch_url(&discovery_url).await?;
    let discovery: OidcDiscovery = serde_json::from_str(&discovery_resp)
        .map_err(|e| format!("invalid OIDC discovery document: {}", e))?;

    // Fetch JWKS
    let jwks_resp = fetch_url(&discovery.jwks_uri).await?;
    let jwks: jsonwebtoken::jwk::JwkSet =
        serde_json::from_str(&jwks_resp).map_err(|e| format!("invalid JWKS: {}", e))?;

    Ok(jwks)
}

/// Fetch a URL using the Worker Fetch API
async fn fetch_url(url: &str) -> std::result::Result<String, String> {
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let request = Request::new_with_init(url, &init)
        .map_err(|e| format!("failed to create request for {}: {:?}", url, e))?;
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|e| format!("fetch {} failed: {:?}", url, e))?;
    response
        .text()
        .await
        .map_err(|e| format!("failed to read response from {}: {:?}", url, e))
}

/// Constant-time byte comparison to prevent timing attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oidc_config_deserialize() {
        let json = r#"{
            "providers": [
                {
                    "issuer": "https://token.actions.githubusercontent.com",
                    "audience": "https://cache.example.com",
                    "bound_claims": {
                        "repository": ["myorg/*"],
                        "ref": ["refs/heads/main", "refs/tags/*"]
                    }
                }
            ]
        }"#;

        let config: OidcConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(
            config.providers[0].issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert_eq!(config.providers[0].audience, "https://cache.example.com");
        assert_eq!(
            config.providers[0].bound_claims["repository"],
            vec!["myorg/*"]
        );
    }

    #[test]
    fn test_oidc_config_no_bound_claims() {
        let json = r#"{
            "providers": [{
                "issuer": "https://gitlab.com",
                "audience": "https://cache.example.com"
            }]
        }"#;
        let config: OidcConfig = serde_json::from_str(json).unwrap();
        assert!(config.providers[0].bound_claims.is_empty());
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_glob_matching() {
        assert!(glob_match::glob_match("myorg/*", "myorg/my-repo"));
        assert!(!glob_match::glob_match("myorg/*", "other/repo"));
        assert!(glob_match::glob_match("refs/tags/*", "refs/tags/v1.0"));
        assert!(!glob_match::glob_match("refs/tags/*", "refs/heads/main"));
        assert!(glob_match::glob_match("refs/heads/main", "refs/heads/main"));
    }

    #[test]
    fn test_auth_result_is_allowed() {
        assert!(AuthResult::StaticToken.is_allowed());
        assert!(AuthResult::Oidc {
            issuer: "test".into(),
            subject: "sub".into()
        }
        .is_allowed());
        assert!(!AuthResult::Denied("reason".into()).is_allowed());
    }
}
