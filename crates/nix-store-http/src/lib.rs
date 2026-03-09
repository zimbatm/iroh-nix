//! HTTP Binary Cache client for fetching from Nix caches like cache.nixos.org
//!
//! Implements `NarInfoProvider` for HTTP binary caches, handling narinfo parsing,
//! compression, and hash translation.

pub mod auth;
pub mod upload;

use std::time::Duration;

use async_compression::tokio::bufread::{BzDecoder, XzDecoder, ZstdDecoder};
use futures_lite::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::io::StreamReader;
use url::Url;

use nix_store::hash_index::{Blake3Hash, Sha256Hash};
use nix_store::smart::{SmartProvider, SmartRequest, SmartResponse};
use nix_store::store::{NarInfoProvider, StorePathInfo};
use nix_store::{Error, Result};

/// Compression type for NAR files
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Xz,
    Zstd,
    Bzip2,
}

impl Compression {
    /// Parse compression from file extension or Content-Type
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "xz" => Compression::Xz,
            "zst" | "zstd" => Compression::Zstd,
            "bz2" => Compression::Bzip2,
            "nar" => Compression::None,
            _ => Compression::None,
        }
    }

    /// Parse compression from narinfo Compression field.
    ///
    /// Nix defaults to bzip2 when the field is empty or absent (nar-info.cc:93-94).
    pub fn from_narinfo(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "xz" => Compression::Xz,
            "zstd" => Compression::Zstd,
            "bzip2" => Compression::Bzip2,
            "none" => Compression::None,
            _ => Compression::Bzip2,
        }
    }
}

/// Parsed narinfo from HTTP cache
#[derive(Debug, Clone)]
pub struct CacheNarInfo {
    /// Store path (e.g., /nix/store/abc123-hello)
    pub store_path: String,
    /// Relative URL to the NAR file
    pub url: String,
    /// Compression type
    pub compression: Compression,
    /// SHA256 hash of the uncompressed NAR
    pub nar_hash: Sha256Hash,
    /// Size of the uncompressed NAR
    pub nar_size: u64,
    /// Size of the compressed file (if different)
    pub file_size: Option<u64>,
    /// References to other store paths (full paths)
    pub references: Vec<String>,
    /// Deriver path (full store path to .drv)
    pub deriver: Option<String>,
    /// Signatures
    pub signatures: Vec<String>,
}

impl CacheNarInfo {
    /// Parse narinfo text format
    pub fn parse(text: &str) -> Result<Self> {
        let mut store_path = None;
        let mut url = None;
        let mut compression = Compression::Bzip2;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut file_size = None;
        let mut references = Vec::new();
        let mut deriver = None;
        let mut signatures = Vec::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (key, value) = match line.split_once(':') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };

            match key {
                "StorePath" => store_path = Some(value.to_string()),
                "URL" => url = Some(value.to_string()),
                "Compression" => compression = Compression::from_narinfo(value),
                "NarHash" => {
                    nar_hash = Some(parse_prefixed_sha256(value)?);
                }
                "NarSize" => {
                    nar_size = value.parse().ok();
                }
                "FileSize" => {
                    file_size = value.parse().ok();
                }
                "References" => {
                    references = value
                        .split_whitespace()
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("/nix/store/{}", s))
                        .collect();
                }
                "Deriver" => {
                    let v = value.trim();
                    if !v.is_empty() && v != "unknown-deriver" {
                        deriver = Some(format!("/nix/store/{}", v));
                    }
                }
                "Sig" => {
                    signatures.push(value.to_string());
                }
                _ => {} // Ignore other fields like CA, etc.
            }
        }

        Ok(CacheNarInfo {
            store_path: store_path
                .ok_or_else(|| Error::Protocol("missing StorePath in narinfo".into()))?,
            url: url.ok_or_else(|| Error::Protocol("missing URL in narinfo".into()))?,
            compression,
            nar_hash: nar_hash
                .ok_or_else(|| Error::Protocol("missing or invalid NarHash in narinfo".into()))?,
            nar_size: nar_size
                .ok_or_else(|| Error::Protocol("missing NarSize in narinfo".into()))?,
            file_size,
            references,
            deriver,
            signatures,
        })
    }
}

/// Parse a prefixed SHA256 hash as used in narinfo fields.
///
/// Supports both Nix-native format (`sha256:<nix32|hex>`) and
/// SRI format (`sha256-<base64>`), matching Nix's `Hash::parseAnyPrefixed`.
fn parse_prefixed_sha256(s: &str) -> Result<Sha256Hash> {
    if let Some(hash_str) = s.strip_prefix("sha256:") {
        return parse_sha256_hash(hash_str);
    }
    if let Some(b64_str) = s.strip_prefix("sha256-") {
        return parse_sha256_base64(b64_str);
    }
    Err(Error::Protocol(format!(
        "unsupported hash format (expected sha256:... or sha256-...): {}",
        s,
    )))
}

/// Parse SHA256 hash from Nix base32 or hex format
fn parse_sha256_hash(s: &str) -> Result<Sha256Hash> {
    // Try hex first (64 chars)
    if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Sha256Hash::from_hex(s);
    }

    // Try Nix base32 (52 chars)
    if s.len() == 52 {
        let bytes = decode_nix_base32(s)?;
        return Sha256Hash::from_slice(&bytes);
    }

    Err(Error::Protocol(format!(
        "invalid SHA256 hash format: {} (len={})",
        s,
        s.len()
    )))
}

/// Parse SHA256 hash from base64 (SRI format)
fn parse_sha256_base64(s: &str) -> Result<Sha256Hash> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let bytes = STANDARD
        .decode(s)
        .map_err(|e| Error::Protocol(format!("invalid base64 in SRI hash: {}", e)))?;
    Sha256Hash::from_slice(&bytes)
}

use nix_store::encoding::decode_nix_base32;

/// Configuration for an HTTP binary cache
#[derive(Debug, Clone)]
pub struct HttpCacheConfig {
    /// Base URL of the cache (e.g., https://cache.nixos.org)
    pub url: Url,
    /// Priority (lower = preferred)
    pub priority: u32,
    /// Request timeout
    pub timeout: Duration,
}

impl HttpCacheConfig {
    /// Create a new HTTP cache config from a URL string
    pub fn new(url: &str) -> Result<Self> {
        let parsed = Url::parse(url)
            .map_err(|e| Error::Protocol(format!("invalid cache URL '{}': {}", url, e)))?;
        Ok(Self {
            url: parsed,
            priority: 40,
            timeout: Duration::from_secs(60),
        })
    }

    /// Set priority
    pub fn with_priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Result of fetching a NAR from HTTP cache
#[derive(Debug)]
pub struct FetchResult {
    /// Decompressed NAR data
    pub nar_data: Vec<u8>,
    /// BLAKE3 hash of the NAR
    pub blake3: Blake3Hash,
    /// SHA256 hash of the NAR
    pub sha256: Sha256Hash,
    /// Size of the NAR
    pub nar_size: u64,
    /// Store path
    pub store_path: String,
    /// Runtime dependencies (full store paths)
    pub references: Vec<String>,
    /// Deriver path (full store path to .drv)
    pub deriver: Option<String>,
    /// Signatures
    pub signatures: Vec<String>,
}

/// HTTP binary cache client
pub struct HttpCacheClient {
    configs: Vec<HttpCacheConfig>,
    client: reqwest::Client,
    /// Whether the first cache supports the smart protocol (lazily detected)
    smart_support: std::sync::Mutex<Option<bool>>,
}

impl HttpCacheClient {
    /// Create a new HTTP cache client
    pub fn new(configs: Vec<HttpCacheConfig>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("iroh-nix")
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| Error::Protocol(format!("failed to create HTTP client: {}", e)))?;

        let mut configs = configs;
        configs.sort_by_key(|c| c.priority);

        Ok(Self {
            configs,
            client,
            smart_support: std::sync::Mutex::new(None),
        })
    }

    /// Check if any caches are configured
    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    /// Fetch narinfo by store path hash (32-char prefix from store path)
    pub async fn fetch_narinfo(&self, store_hash: &str) -> Result<(CacheNarInfo, Url)> {
        let narinfo_path = format!("{}.narinfo", store_hash);

        for config in &self.configs {
            let url = config
                .url
                .join(&narinfo_path)
                .map_err(|e| Error::Protocol(format!("invalid narinfo URL: {}", e)))?;

            tracing::debug!(url = %url, "fetching narinfo");

            let response = match self
                .client
                .get(url.clone())
                .timeout(config.timeout)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(cache = %config.url, error = %e, "cache request failed");
                    continue;
                }
            };

            if !response.status().is_success() {
                tracing::debug!(
                    cache = %config.url,
                    status = %response.status(),
                    "narinfo not found"
                );
                continue;
            }

            let text = response
                .text()
                .await
                .map_err(|e| Error::Protocol(format!("failed to read narinfo: {}", e)))?;

            let narinfo = CacheNarInfo::parse(&text)?;
            return Ok((narinfo, config.url.clone()));
        }

        Err(Error::HashNotFound(format!(
            "narinfo not found in any cache: {}",
            store_hash
        )))
    }

    /// Fetch and decompress NAR, computing BLAKE3 hash
    pub async fn fetch_nar(&self, narinfo: &CacheNarInfo, base_url: &Url) -> Result<FetchResult> {
        let nar_url = base_url
            .join(&narinfo.url)
            .map_err(|e| Error::Protocol(format!("invalid NAR URL: {}", e)))?;

        tracing::debug!(url = %nar_url, compression = ?narinfo.compression, "fetching NAR");

        let response = self
            .client
            .get(nar_url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .map_err(|e| Error::Protocol(format!("failed to fetch NAR: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::Protocol(format!(
                "NAR fetch failed: {}",
                response.status()
            )));
        }

        // Stream response body through decompressor
        let bytes_stream = response
            .bytes_stream()
            .map(|result| result.map_err(std::io::Error::other));
        let stream_reader = StreamReader::new(bytes_stream);
        let buf_reader = BufReader::new(stream_reader);

        let nar_data =
            match narinfo.compression {
                Compression::None => {
                    let mut reader = buf_reader;
                    let mut data = Vec::new();
                    reader
                        .read_to_end(&mut data)
                        .await
                        .map_err(|e| Error::Protocol(format!("failed to read NAR: {}", e)))?;
                    data
                }
                Compression::Xz => {
                    let mut decoder = XzDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder
                        .read_to_end(&mut data)
                        .await
                        .map_err(|e| Error::Protocol(format!("failed to decompress xz: {}", e)))?;
                    data
                }
                Compression::Zstd => {
                    let mut decoder = ZstdDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder.read_to_end(&mut data).await.map_err(|e| {
                        Error::Protocol(format!("failed to decompress zstd: {}", e))
                    })?;
                    data
                }
                Compression::Bzip2 => {
                    let mut decoder = BzDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder.read_to_end(&mut data).await.map_err(|e| {
                        Error::Protocol(format!("failed to decompress bzip2: {}", e))
                    })?;
                    data
                }
            };

        // Compute hashes
        let sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(&nar_data);
            Sha256Hash(hasher.finalize().into())
        };

        // Verify SHA256 matches narinfo
        if sha256 != narinfo.nar_hash {
            return Err(Error::Protocol(format!(
                "SHA256 mismatch: expected {}, got {}",
                narinfo.nar_hash.to_hex(),
                sha256.to_hex()
            )));
        }

        let blake3 = {
            let hash = blake3::hash(&nar_data);
            Blake3Hash(*hash.as_bytes())
        };

        Ok(FetchResult {
            nar_size: nar_data.len() as u64,
            nar_data,
            blake3,
            sha256,
            store_path: narinfo.store_path.clone(),
            references: narinfo.references.clone(),
            deriver: narinfo.deriver.clone(),
            signatures: narinfo.signatures.clone(),
        })
    }

    /// Fetch NAR by store path (combines narinfo lookup and NAR fetch)
    pub async fn fetch_by_store_path(&self, store_path: &str) -> Result<FetchResult> {
        let store_hash = extract_store_hash(store_path)?;
        let (narinfo, base_url) = self.fetch_narinfo(&store_hash).await?;

        if narinfo.store_path != store_path {
            return Err(Error::Protocol(format!(
                "store path mismatch: expected {}, got {}",
                store_path, narinfo.store_path
            )));
        }

        self.fetch_nar(&narinfo, &base_url).await
    }
}

#[async_trait::async_trait]
impl NarInfoProvider for HttpCacheClient {
    async fn get_narinfo(&self, store_hash: &str) -> Result<Option<StorePathInfo>> {
        match self.fetch_narinfo(store_hash).await {
            Ok((narinfo, _base_url)) => Ok(Some(StorePathInfo {
                store_path: narinfo.store_path,
                blake3: Blake3Hash([0u8; 32]), // unknown from HTTP cache
                nar_hash: narinfo.nar_hash,
                nar_size: narinfo.nar_size,
                references: narinfo.references,
                deriver: narinfo.deriver,
                signatures: narinfo.signatures,
            })),
            Err(Error::HashNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_nar(&self, store_path: &str) -> Result<Option<Vec<u8>>> {
        match self.fetch_by_store_path(store_path).await {
            Ok(result) => Ok(Some(result.nar_data)),
            Err(Error::HashNotFound(_)) | Err(Error::StorePathNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[async_trait::async_trait]
impl SmartProvider for HttpCacheClient {
    async fn batch_narinfo(&self, store_hashes: &[String]) -> Result<Vec<Option<StorePathInfo>>> {
        // Try smart protocol if available
        if self.detect_smart_support().await {
            if let Some(config) = self.configs.first() {
                let req = SmartRequest::BatchNarInfo {
                    store_hashes: store_hashes.to_vec(),
                };
                match self
                    .smart_post(config, "/_nix/v1/batch-narinfo", &req)
                    .await
                {
                    Ok(SmartResponse::BatchNarInfo { results }) => {
                        return results
                            .into_iter()
                            .map(|opt| match opt {
                                Some(compact) => compact.to_store_path_info().map(Some),
                                None => Ok(None),
                            })
                            .collect();
                    }
                    Ok(SmartResponse::Error { message, .. }) => {
                        tracing::warn!("smart batch-narinfo error: {}", message);
                    }
                    Ok(_) => {
                        tracing::warn!("unexpected smart response type");
                    }
                    Err(e) => {
                        tracing::debug!("smart batch-narinfo failed, falling back: {}", e);
                    }
                }
            }
        }

        // Fallback: sequential lookups
        let mut results = Vec::with_capacity(store_hashes.len());
        for hash in store_hashes {
            results.push(self.get_narinfo(hash).await?);
        }
        Ok(results)
    }

    async fn resolve_closure(
        &self,
        wants: &[String],
        haves: &[String],
        limit: Option<u32>,
    ) -> Result<(Vec<StorePathInfo>, bool)> {
        // Try smart protocol if available
        if self.detect_smart_support().await {
            if let Some(config) = self.configs.first() {
                let req = SmartRequest::Closure {
                    wants: wants.to_vec(),
                    haves: haves.to_vec(),
                    limit,
                };
                match self.smart_post(config, "/_nix/v1/closure", &req).await {
                    Ok(SmartResponse::PathSet { paths, has_more }) => {
                        let infos: Result<Vec<StorePathInfo>> = paths
                            .into_iter()
                            .map(|compact| compact.to_store_path_info())
                            .collect();
                        return Ok((infos?, has_more));
                    }
                    Ok(SmartResponse::Error { message, .. }) => {
                        tracing::warn!("smart closure error: {}", message);
                    }
                    Ok(_) => {
                        tracing::warn!("unexpected smart response type");
                    }
                    Err(e) => {
                        tracing::debug!("smart closure failed, falling back: {}", e);
                    }
                }
            }
        }

        // Fallback: BFS via sequential lookups
        nix_store::store::resolve_closure_bfs(self, wants, haves, limit).await
    }
}

impl HttpCacheClient {
    /// Detect whether the first configured cache supports the smart protocol.
    /// Caches the result after the first check.
    async fn detect_smart_support(&self) -> bool {
        // Check cache first
        {
            let cached = match self.smart_support.lock() {
                Ok(guard) => guard,
                Err(_) => return false,
            };
            if let Some(supported) = *cached {
                return supported;
            }
        }

        let supported = self.probe_smart_support().await;
        {
            if let Ok(mut cached) = self.smart_support.lock() {
                *cached = Some(supported);
            }
        }
        supported
    }

    /// Probe the first cache for SmartProtocol: 1 in /nix-cache-info
    async fn probe_smart_support(&self) -> bool {
        let config = match self.configs.first() {
            Some(c) => c,
            None => return false,
        };

        let url = match config.url.join("nix-cache-info") {
            Ok(u) => u,
            Err(_) => return false,
        };

        let response = match self.client.get(url).timeout(config.timeout).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return false,
        };

        let text = match response.text().await {
            Ok(t) => t,
            Err(_) => return false,
        };

        for line in text.lines() {
            if let Some(value) = line.strip_prefix("SmartProtocol:") {
                let value = value.trim();
                if value == "1" {
                    tracing::info!("Smart protocol detected on {}", config.url);
                    return true;
                }
            }
        }

        false
    }

    /// Send a smart protocol POST request to a cache
    async fn smart_post(
        &self,
        config: &HttpCacheConfig,
        path: &str,
        req: &SmartRequest,
    ) -> Result<SmartResponse> {
        let url = config
            .url
            .join(path)
            .map_err(|e| Error::Protocol(format!("invalid smart URL: {}", e)))?;

        let body = serde_json::to_vec(req)
            .map_err(|e| Error::Protocol(format!("JSON serialize: {}", e)))?;

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body)
            .timeout(config.timeout)
            .send()
            .await
            .map_err(|e| Error::Protocol(format!("smart request failed: {}", e)))?;

        let resp_bytes = response
            .bytes()
            .await
            .map_err(|e| Error::Protocol(format!("failed to read smart response: {}", e)))?;

        serde_json::from_slice(&resp_bytes)
            .map_err(|e| Error::Protocol(format!("invalid smart JSON response: {}", e)))
    }
}

/// Extract the 32-char hash from a store path
fn extract_store_hash(store_path: &str) -> Result<String> {
    let path = store_path
        .strip_prefix("/nix/store/")
        .ok_or_else(|| Error::InvalidStorePath(store_path.to_string()))?;

    if path.len() < 33 || path.as_bytes()[32] != b'-' {
        return Err(Error::InvalidStorePath(store_path.to_string()));
    }

    Ok(path[..32].to_string())
}

impl FetchResult {
    /// Import the NAR data into the Nix store using `nix-store --import`.
    pub async fn import_to_store(&self) -> Result<String> {
        let export_data = self.build_export_data();

        let mut child = tokio::process::Command::new("nix-store")
            .arg("--import")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Protocol(format!("failed to spawn nix-store: {}", e)))?;

        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&export_data)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write export data: {}", e)))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| Error::Protocol(format!("nix-store failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Protocol(format!(
                "nix-store --import failed: {}",
                stderr
            )));
        }

        Ok(self.store_path.clone())
    }

    /// Build the Nix export format data for `nix-store --import`.
    fn build_export_data(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Path-present marker
        buf.extend_from_slice(&1u64.to_le_bytes());

        // NAR data (written raw, not as a string)
        buf.extend_from_slice(&self.nar_data);

        // Export magic (0x4558494e written as u64 LE)
        buf.extend_from_slice(&0x4558494eu64.to_le_bytes());

        // Store path as a string
        write_nix_string(&mut buf, self.store_path.as_bytes());

        // References as a string set: [u64: count] [string: path]*
        buf.extend_from_slice(&(self.references.len() as u64).to_le_bytes());
        for reference in &self.references {
            write_nix_string(&mut buf, reference.as_bytes());
        }

        // Deriver (empty string = no deriver)
        let deriver = self.deriver.as_deref().unwrap_or("");
        write_nix_string(&mut buf, deriver.as_bytes());

        // No legacy signature
        buf.extend_from_slice(&0u64.to_le_bytes());

        // End-of-paths marker
        buf.extend_from_slice(&0u64.to_le_bytes());

        buf
    }
}

/// Write a Nix-format string: [u64 LE: length][data][zero-padding to 8-byte boundary]
fn write_nix_string(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
    buf.extend_from_slice(s);
    let padding = (8 - (s.len() % 8)) % 8;
    if padding > 0 {
        buf.extend_from_slice(&[0u8; 8][..padding]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_narinfo() {
        let text = r#"
StorePath: /nix/store/00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4-hello-2.12.1
URL: nar/0f9z5mn0clh1rwbq1xc2zzw54zbd5l80p6vvqjvdm1g3d3ngl5a3.nar.xz
Compression: xz
FileHash: sha256:0f9z5mn0clh1rwbq1xc2zzw54zbd5l80p6vvqjvdm1g3d3ngl5a3
FileSize: 50816
NarHash: sha256:0wi0n0aggkc3dxyg5rj0w3zpz13v9a6jlvl5qfm5x8fmkwsmp3r8
NarSize: 226560
References: gm4b0jl9vwc6f5kvlwp880c3kncz6hh5-glibc-2.39-52
Deriver: 0hc5silphps9b7vr5jj17qh9hj03hxfj-hello-2.12.1.drv
Sig: cache.nixos.org-1:example-signature
"#;

        let narinfo = CacheNarInfo::parse(text).unwrap();
        assert_eq!(
            narinfo.store_path,
            "/nix/store/00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4-hello-2.12.1"
        );
        assert_eq!(narinfo.compression, Compression::Xz);
        assert_eq!(narinfo.nar_size, 226560);
        assert_eq!(narinfo.references.len(), 1);
        assert_eq!(
            narinfo.signatures,
            vec!["cache.nixos.org-1:example-signature"]
        );
    }

    #[test]
    fn test_parse_narinfo_sri_hash() {
        let text = "\
StorePath: /nix/store/abc123-test
URL: nar/test.nar
Compression: none
NarHash: sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
NarSize: 100
";
        let narinfo = CacheNarInfo::parse(text).unwrap();
        assert_eq!(narinfo.nar_hash, Sha256Hash([0u8; 32]));
    }

    #[test]
    fn test_extract_store_hash() {
        let hash =
            extract_store_hash("/nix/store/00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4-hello-2.12.1").unwrap();
        assert_eq!(hash, "00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4");
    }

    #[test]
    fn test_compression_from_narinfo() {
        assert_eq!(Compression::from_narinfo("xz"), Compression::Xz);
        assert_eq!(Compression::from_narinfo("zstd"), Compression::Zstd);
        assert_eq!(Compression::from_narinfo("bzip2"), Compression::Bzip2);
        assert_eq!(Compression::from_narinfo("none"), Compression::None);
        assert_eq!(Compression::from_narinfo(""), Compression::Bzip2);
    }

    #[test]
    fn test_build_export_data_format() {
        let result = FetchResult {
            nar_data: vec![0xAA, 0xBB],
            blake3: Blake3Hash([0u8; 32]),
            sha256: Sha256Hash([0u8; 32]),
            nar_size: 2,
            store_path: "/nix/store/abc-test".to_string(),
            references: vec!["/nix/store/dep1-foo".to_string()],
            deriver: Some("/nix/store/xyz-test.drv".to_string()),
            signatures: vec![],
        };

        let data = result.build_export_data();
        let mut pos = 0;

        // Path-present marker: u64 = 1
        assert_eq!(
            u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()),
            1
        );
        pos += 8;

        // NAR data (raw, 2 bytes)
        assert_eq!(&data[pos..pos + 2], &[0xAA, 0xBB]);
        pos += 2;

        // Export magic: u64 = 0x4558494e
        assert_eq!(
            u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()),
            0x4558494e
        );
        pos += 8;

        // Store path string
        let len = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        assert_eq!(len, "/nix/store/abc-test".len());
        assert_eq!(&data[pos..pos + len], b"/nix/store/abc-test");
        pos += len;
        let padding = (8 - (len % 8)) % 8;
        pos += padding;

        // References: count=1
        assert_eq!(
            u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()),
            1
        );
        pos += 8;

        // Reference string
        let len = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        assert_eq!(&data[pos..pos + len], b"/nix/store/dep1-foo");
        pos += len;
        let padding = (8 - (len % 8)) % 8;
        pos += padding;

        // Deriver string
        let len = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        assert_eq!(&data[pos..pos + len], b"/nix/store/xyz-test.drv");
        pos += len;
        let padding = (8 - (len % 8)) % 8;
        pos += padding;

        // No signature: u64 = 0
        assert_eq!(
            u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()),
            0
        );
        pos += 8;

        // End marker: u64 = 0
        assert_eq!(
            u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()),
            0
        );
        pos += 8;

        assert_eq!(pos, data.len());
    }
}
