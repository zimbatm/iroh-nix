//! HTTP Binary Cache client for fetching from Nix caches like cache.nixos.org
//!
//! This module provides a client for fetching NAR files from HTTP binary caches,
//! handling narinfo parsing, compression, and hash translation.

use std::time::Duration;

use async_compression::tokio::bufread::{BzDecoder, XzDecoder, ZstdDecoder};
use futures_lite::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, BufReader};
use tokio_util::io::StreamReader;
use url::Url;

use crate::hash_index::{Blake3Hash, Sha256Hash};
use crate::{Error, Result};

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

    /// Parse compression from narinfo Compression field
    pub fn from_narinfo(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "xz" => Compression::Xz,
            "zstd" => Compression::Zstd,
            "bzip2" => Compression::Bzip2,
            "none" | "" => Compression::None,
            _ => Compression::None,
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
    /// References to other store paths
    pub references: Vec<String>,
}

impl CacheNarInfo {
    /// Parse narinfo text format
    pub fn parse(text: &str) -> Result<Self> {
        let mut store_path = None;
        let mut url = None;
        let mut compression = Compression::None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut file_size = None;
        let mut references = Vec::new();

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
                    // Format: sha256:base32 or sha256:hex
                    if let Some(hash_str) = value.strip_prefix("sha256:") {
                        nar_hash = Some(parse_sha256_hash(hash_str)?);
                    }
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
                _ => {} // Ignore other fields like Deriver, Sig, CA, etc.
            }
        }

        Ok(CacheNarInfo {
            store_path: store_path.ok_or_else(|| Error::HttpCache("missing StorePath".into()))?,
            url: url.ok_or_else(|| Error::HttpCache("missing URL".into()))?,
            compression,
            nar_hash: nar_hash
                .ok_or_else(|| Error::HttpCache("missing or invalid NarHash".into()))?,
            nar_size: nar_size.ok_or_else(|| Error::HttpCache("missing NarSize".into()))?,
            file_size,
            references,
        })
    }
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

    Err(Error::HttpCache(format!(
        "invalid SHA256 hash format: {} (len={})",
        s,
        s.len()
    )))
}

/// Decode Nix base32 encoding
fn decode_nix_base32(s: &str) -> Result<Vec<u8>> {
    const NIX_BASE32_CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

    let mut lookup = [255u8; 128];
    for (i, &c) in NIX_BASE32_CHARS.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    // Each 5 bits of output comes from one base32 char
    // 52 chars * 5 bits = 260 bits, but we only need 256 (32 bytes)
    let out_len = (len * 5) / 8;
    let mut out = vec![0u8; out_len];

    for n in (0..len).rev() {
        let c = chars[len - 1 - n];
        if c as u32 >= 128 {
            return Err(Error::HttpCache(format!("invalid base32 char: {}", c)));
        }
        let digit = lookup[c as usize];
        if digit == 255 {
            return Err(Error::HttpCache(format!("invalid base32 char: {}", c)));
        }

        let b = n * 5;
        let byte_idx = b / 8;
        let bit_idx = b % 8;

        if byte_idx < out_len {
            out[byte_idx] |= digit << bit_idx;
        }
        if bit_idx > 3 && byte_idx + 1 < out_len {
            out[byte_idx + 1] |= digit >> (8 - bit_idx);
        }
    }

    Ok(out)
}

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
            .map_err(|e| Error::HttpCache(format!("invalid cache URL '{}': {}", url, e)))?;
        Ok(Self {
            url: parsed,
            priority: 40, // Default priority (same as cache.nixos.org)
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
}

/// HTTP binary cache client
pub struct HttpCacheClient {
    configs: Vec<HttpCacheConfig>,
    client: reqwest::Client,
}

impl HttpCacheClient {
    /// Create a new HTTP cache client
    pub fn new(configs: Vec<HttpCacheConfig>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("iroh-nix")
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| Error::HttpCache(format!("failed to create HTTP client: {}", e)))?;

        // Sort configs by priority
        let mut configs = configs;
        configs.sort_by_key(|c| c.priority);

        Ok(Self { configs, client })
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
                .map_err(|e| Error::HttpCache(format!("invalid narinfo URL: {}", e)))?;

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
                .map_err(|e| Error::HttpCache(format!("failed to read narinfo: {}", e)))?;

            let narinfo = CacheNarInfo::parse(&text)?;
            return Ok((narinfo, config.url.clone()));
        }

        Err(Error::HttpCache(format!(
            "narinfo not found in any cache: {}",
            store_hash
        )))
    }

    /// Fetch and decompress NAR, computing BLAKE3 hash
    pub async fn fetch_nar(&self, narinfo: &CacheNarInfo, base_url: &Url) -> Result<FetchResult> {
        let nar_url = base_url
            .join(&narinfo.url)
            .map_err(|e| Error::HttpCache(format!("invalid NAR URL: {}", e)))?;

        tracing::debug!(url = %nar_url, compression = ?narinfo.compression, "fetching NAR");

        let response = self
            .client
            .get(nar_url)
            .timeout(Duration::from_secs(300)) // 5 min for large NARs
            .send()
            .await
            .map_err(|e| Error::HttpCache(format!("failed to fetch NAR: {}", e)))?;

        if !response.status().is_success() {
            return Err(Error::HttpCache(format!(
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
                        .map_err(|e| Error::HttpCache(format!("failed to read NAR: {}", e)))?;
                    data
                }
                Compression::Xz => {
                    let mut decoder = XzDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder
                        .read_to_end(&mut data)
                        .await
                        .map_err(|e| Error::HttpCache(format!("failed to decompress xz: {}", e)))?;
                    data
                }
                Compression::Zstd => {
                    let mut decoder = ZstdDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder.read_to_end(&mut data).await.map_err(|e| {
                        Error::HttpCache(format!("failed to decompress zstd: {}", e))
                    })?;
                    data
                }
                Compression::Bzip2 => {
                    let mut decoder = BzDecoder::new(buf_reader);
                    let mut data = Vec::new();
                    decoder.read_to_end(&mut data).await.map_err(|e| {
                        Error::HttpCache(format!("failed to decompress bzip2: {}", e))
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
            return Err(Error::HttpCache(format!(
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
        })
    }

    /// Fetch NAR by store path (combines narinfo lookup and NAR fetch)
    pub async fn fetch_by_store_path(&self, store_path: &str) -> Result<FetchResult> {
        // Extract hash from store path
        let store_hash = extract_store_hash(store_path)?;

        let (narinfo, base_url) = self.fetch_narinfo(&store_hash).await?;

        // Verify store path matches
        if narinfo.store_path != store_path {
            return Err(Error::HttpCache(format!(
                "store path mismatch: expected {}, got {}",
                store_path, narinfo.store_path
            )));
        }

        self.fetch_nar(&narinfo, &base_url).await
    }
}

/// Extract the 32-char hash from a store path
fn extract_store_hash(store_path: &str) -> Result<String> {
    // /nix/store/abc123...-name -> abc123...
    let path = store_path
        .strip_prefix("/nix/store/")
        .ok_or_else(|| Error::InvalidStorePath(store_path.to_string()))?;

    // Hash is 32 chars followed by '-'
    if path.len() < 33 || path.as_bytes()[32] != b'-' {
        return Err(Error::InvalidStorePath(store_path.to_string()));
    }

    Ok(path[..32].to_string())
}

use tokio::io::AsyncWriteExt;

impl FetchResult {
    /// Import the NAR data into the Nix store
    pub async fn import_to_store(&self) -> Result<String> {
        // Use nix-store --restore to import the NAR
        let mut child = tokio::process::Command::new("nix-store")
            .args(["--restore", &self.store_path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::HttpCache(format!("failed to spawn nix-store: {}", e)))?;

        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&self.nar_data)
            .await
            .map_err(|e| Error::HttpCache(format!("failed to write NAR data: {}", e)))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| Error::HttpCache(format!("nix-store failed: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::HttpCache(format!(
                "nix-store --restore failed: {}",
                stderr
            )));
        }

        Ok(self.store_path.clone())
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
        assert_eq!(
            narinfo.url,
            "nar/0f9z5mn0clh1rwbq1xc2zzw54zbd5l80p6vvqjvdm1g3d3ngl5a3.nar.xz"
        );
        assert_eq!(narinfo.compression, Compression::Xz);
        assert_eq!(narinfo.nar_size, 226560);
        assert_eq!(narinfo.file_size, Some(50816));
        assert_eq!(narinfo.references.len(), 1);
    }

    #[test]
    fn test_extract_store_hash() {
        let hash =
            extract_store_hash("/nix/store/00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4-hello-2.12.1").unwrap();
        assert_eq!(hash, "00bgk9l11v5g2ab1sdn3xv5c8zgk6rp4");
    }

    #[test]
    fn test_decode_nix_base32() {
        // All zeros should decode to all zero bytes
        let zeros = "0000000000000000000000000000000000000000000000000000";
        let decoded = decode_nix_base32(zeros).unwrap();
        assert_eq!(decoded.len(), 32);
        assert!(decoded.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_compression_from_narinfo() {
        assert_eq!(Compression::from_narinfo("xz"), Compression::Xz);
        assert_eq!(Compression::from_narinfo("zstd"), Compression::Zstd);
        assert_eq!(Compression::from_narinfo("bzip2"), Compression::Bzip2);
        assert_eq!(Compression::from_narinfo("none"), Compression::None);
        assert_eq!(Compression::from_narinfo(""), Compression::None);
    }
}
