//! Query Nix for store path metadata
//!
//! This module provides utilities to query Nix's database for store path
//! information like NAR hash and size, avoiding expensive recomputation.

use base64::Engine;
use serde::Deserialize;
use tokio::process::Command;

use crate::{Error, Result};

/// Information about a store path from Nix
#[derive(Debug, Clone, Deserialize)]
pub struct NixPathInfo {
    /// The store path
    pub path: String,
    /// NAR hash in Nix format (e.g., "sha256-base64..." or "sha256:nix32...")
    #[serde(rename = "narHash")]
    pub nar_hash: String,
    /// NAR size in bytes
    #[serde(rename = "narSize")]
    pub nar_size: u64,
    /// References (dependencies)
    #[serde(default)]
    pub references: Vec<String>,
    /// Deriver path (if known)
    pub deriver: Option<String>,
    /// Signatures
    #[serde(default)]
    pub signatures: Vec<String>,
}

impl NixPathInfo {
    /// Query Nix for path info
    ///
    /// Uses `nix path-info --json` to get metadata about a store path.
    pub async fn query(store_path: &str) -> Result<Self> {
        let output = Command::new("nix")
            .args(["path-info", "--json", store_path])
            .output()
            .await
            .map_err(|e| Error::Protocol(format!("failed to run nix path-info: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::StorePathNotFound(format!(
                "{}: {}",
                store_path,
                stderr.trim()
            )));
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Protocol(format!("invalid json from nix path-info: {}", e)))?;

        // nix path-info --json returns an object keyed by store path
        let info = json
            .get(store_path)
            .ok_or_else(|| Error::StorePathNotFound(store_path.to_string()))?;

        serde_json::from_value(info.clone())
            .map_err(|e| Error::Protocol(format!("failed to parse nix path-info: {}", e)))
    }

    /// Parse the SHA256 hash from Nix's format to raw bytes
    ///
    /// Handles:
    /// - "sha256-<base64>" format (modern Nix)
    /// - "sha256:<nix32>" format (older Nix)
    pub fn sha256_bytes(&self) -> Result<[u8; 32]> {
        // Handle "sha256-<base64>" format (SRI format)
        if let Some(b64) = self.nar_hash.strip_prefix("sha256-") {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| Error::Protocol(format!("invalid base64 in narHash: {}", e)))?;
            return bytes
                .try_into()
                .map_err(|_| Error::Protocol("narHash base64 is not 32 bytes".into()));
        }

        // Handle "sha256:<nix32>" format
        if let Some(nix32) = self.nar_hash.strip_prefix("sha256:") {
            return crate::encoding::decode_nix_base32_fixed(nix32);
        }

        Err(Error::Protocol(format!(
            "unsupported narHash format: {}",
            self.nar_hash
        )))
    }
}

/// Check if a store path exists locally
pub fn path_exists(store_path: &str) -> bool {
    std::path::Path::new(store_path).exists()
}

/// Validate that a path looks like a valid Nix store path
pub fn is_valid_store_path(path: &str) -> bool {
    path.starts_with("/nix/store/") && path.len() > "/nix/store/".len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_store_path() {
        assert!(is_valid_store_path("/nix/store/abc123-hello"));
        assert!(is_valid_store_path(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg"
        ));
        assert!(!is_valid_store_path("/nix/store/"));
        assert!(!is_valid_store_path("/nix/store"));
        assert!(!is_valid_store_path("/tmp/foo"));
        assert!(!is_valid_store_path(""));
    }

    #[test]
    fn test_sha256_bytes_sri_format() {
        let info = NixPathInfo {
            path: "/nix/store/test".to_string(),
            nar_hash: "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
            nar_size: 100,
            references: vec![],
            deriver: None,
            signatures: vec![],
        };

        let bytes = info.sha256_bytes().unwrap();
        assert_eq!(bytes, [0u8; 32]);
    }
}
