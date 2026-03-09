//! Upload client for pushing store paths to a remote cache
//!
//! Implements the 3-phase upload protocol:
//! 1. Init: send narinfo metadata, get back which NARs need uploading
//! 2. Upload: PUT each needed NAR as raw bytes
//! 3. Commit: atomically publish the narinfos

use std::collections::HashMap;

use nix_store::hash_index::Blake3Hash;
use nix_store::smart::StorePathInfoCompact;
use nix_store::store::StorePathInfo;
use nix_store::{Error, Result};
use url::Url;

use crate::auth::AuthProvider;

/// Response from the upload init endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct InitResponse {
    /// Session ID for subsequent upload/commit calls
    pub session_id: String,
    /// BLAKE3 hashes of NARs that need uploading (not already on server)
    pub need_upload: Vec<String>,
}

/// Response from the upload commit endpoint
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CommitResponse {
    /// Number of narinfos committed
    pub committed: usize,
}

/// Client for uploading store paths to a remote cache
pub struct UploadClient {
    base_url: Url,
    client: reqwest::Client,
    auth: AuthProvider,
}

impl UploadClient {
    /// Create a new upload client
    pub fn new(base_url: Url, auth: AuthProvider) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("iroh-nix")
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| Error::Protocol(format!("failed to create HTTP client: {}", e)))?;

        Ok(Self {
            base_url,
            client,
            auth,
        })
    }

    /// Phase 1: Initialize an upload session.
    ///
    /// Sends narinfo metadata for all paths. The server returns a session ID
    /// and a list of BLAKE3 hashes for NARs that need uploading.
    pub async fn upload_init(&self, paths: &[StorePathInfoCompact]) -> Result<InitResponse> {
        #[derive(serde::Serialize)]
        struct InitRequest<'a> {
            paths: &'a [StorePathInfoCompact],
        }

        let url = self
            .base_url
            .join("_admin/v1/upload/init")
            .map_err(|e| Error::Protocol(format!("invalid upload URL: {}", e)))?;

        let response = self
            .auth
            .apply(self.client.post(url))
            .json(&InitRequest { paths })
            .send()
            .await
            .map_err(|e| Error::Protocol(format!("upload init request failed: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(Error::Auth(format!("upload init: {}", response.status())));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Protocol(format!(
                "upload init failed ({}): {}",
                status, body
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::Protocol(format!("upload init response parse error: {}", e)))
    }

    /// Phase 2: Upload a single NAR blob.
    ///
    /// Sends raw NAR bytes to the server via PUT.
    pub async fn upload_nar(&self, blake3_hex: &str, data: &[u8]) -> Result<()> {
        let url = self
            .base_url
            .join(&format!("_admin/v1/nar/{}", blake3_hex))
            .map_err(|e| Error::Protocol(format!("invalid NAR upload URL: {}", e)))?;

        let response = self
            .auth
            .apply(self.client.put(url))
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| Error::Protocol(format!("NAR upload failed: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(Error::Auth(format!("NAR upload: {}", response.status())));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Protocol(format!(
                "NAR upload failed ({}): {}",
                status, body
            )));
        }

        Ok(())
    }

    /// Phase 3: Commit the upload session.
    ///
    /// After all NARs are uploaded, this atomically publishes the narinfos.
    pub async fn upload_commit(&self, session_id: &str) -> Result<CommitResponse> {
        #[derive(serde::Serialize)]
        struct CommitRequest<'a> {
            session_id: &'a str,
        }

        let url = self
            .base_url
            .join("_admin/v1/upload/commit")
            .map_err(|e| Error::Protocol(format!("invalid commit URL: {}", e)))?;

        let response = self
            .auth
            .apply(self.client.post(url))
            .json(&CommitRequest { session_id })
            .send()
            .await
            .map_err(|e| Error::Protocol(format!("upload commit request failed: {}", e)))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(Error::Auth(format!("upload commit: {}", response.status())));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Protocol(format!(
                "upload commit failed ({}): {}",
                status, body
            )));
        }

        response
            .json()
            .await
            .map_err(|e| Error::Protocol(format!("commit response parse error: {}", e)))
    }

    /// Convenience: upload a set of store paths with their NAR data.
    ///
    /// Converts StorePathInfo to compact form, runs the 3-phase protocol,
    /// and uploads needed NARs concurrently with the given concurrency limit.
    pub async fn upload_paths(
        &self,
        paths: &[StorePathInfo],
        nar_data: HashMap<Blake3Hash, Vec<u8>>,
        concurrency: usize,
    ) -> Result<CommitResponse> {
        let compact: Vec<StorePathInfoCompact> = paths
            .iter()
            .map(StorePathInfoCompact::from_store_path_info)
            .collect();

        // Phase 1: Init
        let init = self.upload_init(&compact).await?;
        tracing::info!(
            session_id = %init.session_id,
            need_upload = init.need_upload.len(),
            "upload session initialized"
        );

        if init.need_upload.is_empty() {
            // All NARs already exist, just commit
            return self.upload_commit(&init.session_id).await;
        }

        // Phase 2: Upload needed NARs concurrently
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
        let mut handles = Vec::new();

        for blake3_hex in &init.need_upload {
            let blake3 = Blake3Hash::from_hex(blake3_hex)?;
            let data = nar_data.get(&blake3).ok_or_else(|| {
                Error::Protocol(format!(
                    "server requested NAR {} but we don't have it",
                    blake3_hex
                ))
            })?;

            let sem = semaphore.clone();
            let data = data.clone();
            let blake3_hex = blake3_hex.clone();

            // Build a new URL and auth for the spawned task
            let url = self
                .base_url
                .join(&format!("_admin/v1/nar/{}", blake3_hex))
                .map_err(|e| Error::Protocol(format!("invalid NAR upload URL: {}", e)))?;

            let request = self
                .auth
                .apply(self.client.put(url))
                .header("Content-Type", "application/octet-stream")
                .body(data);

            handles.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|_| Error::Internal("upload semaphore closed".into()))?;
                let response = request
                    .send()
                    .await
                    .map_err(|e| Error::Protocol(format!("NAR upload failed: {}", e)))?;
                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    return Err(Error::Protocol(format!(
                        "NAR upload {} failed ({}): {}",
                        blake3_hex, status, body
                    )));
                }
                tracing::debug!(blake3 = %blake3_hex, "NAR uploaded");
                Ok(())
            }));
        }

        // Wait for all uploads
        for handle in handles {
            handle
                .await
                .map_err(|e| Error::Internal(format!("upload task panicked: {}", e)))??;
        }

        // Phase 3: Commit
        self.upload_commit(&init.session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_response_deserialize() {
        let json = r#"{"session_id":"abc123","need_upload":["deadbeef","cafebabe"]}"#;
        let resp: InitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.session_id, "abc123");
        assert_eq!(resp.need_upload, vec!["deadbeef", "cafebabe"]);
    }

    #[test]
    fn test_init_response_empty_uploads() {
        let json = r#"{"session_id":"xyz","need_upload":[]}"#;
        let resp: InitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.session_id, "xyz");
        assert!(resp.need_upload.is_empty());
    }

    #[test]
    fn test_commit_response_deserialize() {
        let json = r#"{"committed":5}"#;
        let resp: CommitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.committed, 5);
    }
}
