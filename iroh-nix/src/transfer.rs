//! NAR transfer protocol implementation
//!
//! This module implements the actual P2P transfer of NAR blobs between nodes.
//! The protocol supports two request types:
//! 1. ByBlake3: Request by BLAKE3 hash (requires pre-indexed path)
//! 2. ByStorePath: Request by store path (enables on-demand serving)
//!
//! NAR data is generated on-demand from the filesystem, not from stored blobs.

use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use iroh::endpoint::{RecvStream, SendStream};
use iroh::Endpoint;
use iroh_base::EndpointAddr;
use tracing::{debug, info, warn};

use crate::error::MutexExt;
use crate::hash_index::{Blake3Hash, HashEntry, HashIndex, Sha256Hash};
use crate::http_cache::HttpCacheClient;
use crate::nix_info::{is_valid_store_path, NixPathInfo};
use crate::protocol::{NarRequest, NarResponseHeader, NAR_PROTOCOL_ALPN};
use crate::{Error, Result};

/// Maximum NAR size we'll accept (10 GB)
const MAX_NAR_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Buffer size for streaming transfers
const TRANSFER_BUFFER_SIZE: usize = 64 * 1024;

/// Handle an accepted NAR request connection
///
/// Supports both ByBlake3 (indexed) and ByStorePath (on-demand) requests.
/// NAR data is generated on-demand from the filesystem.
/// If http_cache is provided, it will be used as a fallback for missing content.
pub async fn handle_nar_accepted(
    connection: iroh::endpoint::Connection,
    hash_index: Arc<Mutex<HashIndex>>,
    http_cache: Option<Arc<HttpCacheClient>>,
) -> Result<()> {
    let remote_id = connection.remote_id();

    debug!("NAR connection from {}", remote_id);

    // Accept bidirectional stream
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| Error::Protocol(format!("failed to accept stream: {}", e)))?;

    // Read the request
    let request = read_nar_request(&mut recv).await?;

    let result = match &request {
        NarRequest::ByBlake3 { hash } => {
            let blake3 = Blake3Hash(*hash);
            debug!("NAR request by blake3: {}", blake3);
            handle_nar_by_blake3(&mut send, &hash_index, blake3, http_cache.as_deref()).await
        }
        NarRequest::ByStorePath { store_path } => {
            debug!("NAR request by store path: {}", store_path);
            handle_nar_by_store_path(&mut send, &hash_index, store_path, http_cache.as_deref())
                .await
        }
    };

    if let Err(ref e) = result {
        warn!("NAR request failed for {}: {}", remote_id, e);
    }

    // Finish the stream
    send.finish().ok();

    result
}

/// Handle NAR request by BLAKE3 hash (requires indexed path)
async fn handle_nar_by_blake3(
    send: &mut SendStream,
    hash_index: &Arc<Mutex<HashIndex>>,
    blake3: Blake3Hash,
    http_cache: Option<&HttpCacheClient>,
) -> Result<()> {
    // Look up in index
    let entry = {
        let index = hash_index.lock_or_err()?;
        index.get_by_blake3(&blake3)?
    };

    match entry {
        Some(entry) => {
            let store_path = Path::new(&entry.store_path);
            if store_path.exists() {
                stream_nar_from_path(send, &entry.store_path, entry.nar_size, entry.sha256).await?;
                info!("Sent NAR {} ({} bytes)", entry.store_path, entry.nar_size);
                Ok(())
            } else if let Some(cache) = http_cache {
                // Path is indexed but missing locally - try HTTP cache
                info!(
                    "Store path missing locally, fetching from HTTP cache: {}",
                    entry.store_path
                );
                fetch_and_stream_from_http(send, hash_index, cache, &entry.store_path, Some(blake3))
                    .await
            } else {
                warn!("Store path missing for {}: {}", blake3, entry.store_path);
                send_error_response(send, "store path not found on disk").await
            }
        }
        None => {
            debug!("Unknown blake3 hash requested: {}", blake3);
            send_error_response(send, "hash not found in index").await
        }
    }
}

/// Handle NAR request by store path (on-demand serving)
async fn handle_nar_by_store_path(
    send: &mut SendStream,
    hash_index: &Arc<Mutex<HashIndex>>,
    store_path: &str,
    http_cache: Option<&HttpCacheClient>,
) -> Result<()> {
    // Validate store path format
    if !is_valid_store_path(store_path) {
        return send_error_response(send, "invalid store path format").await;
    }

    let fs_path = Path::new(store_path);
    if fs_path.exists() {
        // Check if we have it indexed (for cached metadata)
        let cached_entry = {
            let index = hash_index.lock_or_err()?;
            index.get_by_store_path(store_path)?
        };

        if let Some(entry) = cached_entry {
            // Use cached metadata
            stream_nar_from_path(send, store_path, entry.nar_size, entry.sha256).await?;
            info!(
                "Sent NAR {} ({} bytes) [cached]",
                store_path, entry.nar_size
            );
        } else {
            // Query Nix for metadata (on-demand)
            let nix_info = NixPathInfo::query(store_path).await?;
            let sha256 = Sha256Hash(nix_info.sha256_bytes()?);
            stream_nar_from_path(send, store_path, nix_info.nar_size, sha256).await?;
            info!(
                "Sent NAR {} ({} bytes) [on-demand]",
                store_path, nix_info.nar_size
            );
        }
        Ok(())
    } else if let Some(cache) = http_cache {
        // Store path not on disk - try HTTP cache
        info!(
            "Store path not found locally, fetching from HTTP cache: {}",
            store_path
        );
        fetch_and_stream_from_http(send, hash_index, cache, store_path, None).await
    } else {
        send_error_response(send, "store path not found").await
    }
}

/// Stream NAR directly from a store path to the network
async fn stream_nar_from_path(
    send: &mut SendStream,
    store_path: &str,
    nar_size: u64,
    sha256: Sha256Hash,
) -> Result<()> {
    // Create header
    let header = NarResponseHeader::new(nar_size, sha256, store_path.to_string());

    // Serialize header
    let header_bytes = postcard::to_allocvec(&header)
        .map_err(|e| Error::Protocol(format!("failed to serialize header: {}", e)))?;

    // Send success marker + header length + header
    send.write_all(&[1u8]) // 1 = success
        .await
        .map_err(|e| Error::Protocol(format!("failed to write success marker: {}", e)))?;

    send.write_all(&(header_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("failed to write header length: {}", e)))?;

    send.write_all(&header_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write header: {}", e)))?;

    // Serialize NAR on-demand from filesystem using spawn_blocking
    // because NarWriter uses sync I/O
    let path = Path::new(store_path).to_path_buf();
    let nar_data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let (data, _info) = crate::nar::serialize_path(&path)?;
        Ok(data)
    })
    .await
    .map_err(|e| Error::Protocol(format!("NAR serialization task failed: {}", e)))??;

    // Stream the NAR data
    for chunk in nar_data.chunks(TRANSFER_BUFFER_SIZE) {
        send.write_all(chunk)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write NAR data: {}", e)))?;
    }

    Ok(())
}

/// Fetch NAR from HTTP cache and stream to peer
///
/// Fetches from configured HTTP binary caches, computes BLAKE3,
/// updates the hash index, and streams the NAR to the peer.
async fn fetch_and_stream_from_http(
    send: &mut SendStream,
    hash_index: &Arc<Mutex<HashIndex>>,
    http_cache: &HttpCacheClient,
    store_path: &str,
    expected_blake3: Option<Blake3Hash>,
) -> Result<()> {
    // Fetch from HTTP cache
    let result = http_cache.fetch_by_store_path(store_path).await?;

    // Verify BLAKE3 if expected
    if let Some(expected) = expected_blake3 {
        if result.blake3 != expected {
            return Err(Error::Protocol(format!(
                "BLAKE3 mismatch from HTTP cache: expected {}, got {}",
                expected, result.blake3
            )));
        }
    }

    // Update hash index for future requests
    {
        let entry = HashEntry {
            blake3: result.blake3,
            sha256: result.sha256,
            store_path: result.store_path.clone(),
            nar_size: result.nar_size,
            references: result.references.clone(),
            deriver: result.deriver.clone(),
        };
        let index = hash_index.lock_or_err()?;
        index.insert(&entry)?;
    }

    // Stream NAR to peer
    let header = NarResponseHeader::new(result.nar_size, result.sha256, result.store_path.clone());

    let header_bytes = postcard::to_allocvec(&header)
        .map_err(|e| Error::Protocol(format!("failed to serialize header: {}", e)))?;

    // Send success marker + header length + header
    send.write_all(&[1u8]) // 1 = success
        .await
        .map_err(|e| Error::Protocol(format!("failed to write success marker: {}", e)))?;

    send.write_all(&(header_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("failed to write header length: {}", e)))?;

    send.write_all(&header_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write header: {}", e)))?;

    // Stream the NAR data
    for chunk in result.nar_data.chunks(TRANSFER_BUFFER_SIZE) {
        send.write_all(chunk)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write NAR data: {}", e)))?;
    }

    info!(
        "Proxied NAR {} from HTTP cache ({} bytes, blake3: {})",
        result.store_path, result.nar_size, result.blake3
    );

    Ok(())
}

/// Read a NAR request from a stream
async fn read_nar_request(recv: &mut RecvStream) -> Result<NarRequest> {
    // Read length prefix (u32)
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request length: {}", e)))?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 1024 {
        return Err(Error::Protocol("request too large".into()));
    }

    // Read request data
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request: {}", e)))?;

    // Deserialize
    let request: NarRequest = postcard::from_bytes(&buf)
        .map_err(|e| Error::Protocol(format!("invalid request: {}", e)))?;

    Ok(request)
}

/// Send an error response
async fn send_error_response(send: &mut SendStream, message: &str) -> Result<()> {
    // Send error marker + message
    send.write_all(&[0u8]) // 0 = error
        .await
        .map_err(|e| Error::Protocol(format!("failed to write error marker: {}", e)))?;

    let msg_bytes = message.as_bytes();
    send.write_all(&(msg_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("failed to write error length: {}", e)))?;

    send.write_all(msg_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write error message: {}", e)))?;

    Ok(())
}

/// Fetch a NAR from a remote node (internal, no retry)
async fn fetch_nar_once(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
) -> Result<(NarResponseHeader, Bytes)> {
    debug!("Connecting to {} for NAR {}", remote.id, blake3);

    // Connect to the remote node
    let connection = endpoint
        .connect(remote.clone(), NAR_PROTOCOL_ALPN)
        .await
        .map_err(|e| Error::Connection(e.to_string()))?;

    // Open bidirectional stream
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| Error::Protocol(format!("failed to open stream: {}", e)))?;

    // Send request
    let request = NarRequest::new(blake3);
    let request_bytes = postcard::to_allocvec(&request)
        .map_err(|e| Error::Protocol(format!("failed to serialize request: {}", e)))?;

    send.write_all(&(request_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("failed to write request length: {}", e)))?;

    send.write_all(&request_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write request: {}", e)))?;

    send.finish().ok();

    // Read response
    let mut status = [0u8; 1];
    recv.read_exact(&mut status)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read status: {}", e)))?;

    if status[0] == 0 {
        // Error response
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| Error::Protocol(format!("failed to read error length: {}", e)))?;
        let len = u32::from_le_bytes(len_buf) as usize;

        let mut msg_buf = vec![0u8; len.min(1024)];
        recv.read_exact(&mut msg_buf)
            .await
            .map_err(|e| Error::Protocol(format!("failed to read error: {}", e)))?;

        let msg = String::from_utf8_lossy(&msg_buf);
        return Err(Error::Remote(msg.into_owned()));
    }

    // Success - read header
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read header length: {}", e)))?;
    let header_len = u32::from_le_bytes(len_buf) as usize;

    if header_len > 64 * 1024 {
        return Err(Error::Protocol("header too large".into()));
    }

    let mut header_buf = vec![0u8; header_len];
    recv.read_exact(&mut header_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read header: {}", e)))?;

    let header: NarResponseHeader = postcard::from_bytes(&header_buf)
        .map_err(|e| Error::Protocol(format!("invalid header: {}", e)))?;

    if header.size > MAX_NAR_SIZE {
        return Err(Error::Protocol(format!(
            "NAR too large: {} bytes",
            header.size
        )));
    }

    debug!(
        "Receiving NAR: {} ({} bytes)",
        header.store_path, header.size
    );

    // Read NAR data
    let mut nar_data = Vec::with_capacity(header.size as usize);
    let mut remaining = header.size;
    let mut buf = vec![0u8; TRANSFER_BUFFER_SIZE];

    while remaining > 0 {
        let to_read = (remaining as usize).min(TRANSFER_BUFFER_SIZE);
        let n = recv
            .read(&mut buf[..to_read])
            .await
            .map_err(|e| Error::Protocol(format!("failed to read NAR data: {}", e)))?
            .ok_or_else(|| Error::Protocol("unexpected end of stream".into()))?;

        if n == 0 {
            return Err(Error::Protocol("unexpected end of stream".into()));
        }

        nar_data.extend_from_slice(&buf[..n]);
        remaining -= n as u64;
    }

    // Verify BLAKE3 hash
    let computed_hash = blake3::hash(&nar_data);
    if computed_hash.as_bytes() != blake3.as_bytes() {
        return Err(Error::Protocol(format!(
            "BLAKE3 hash mismatch: expected {}, got {}",
            blake3,
            hex::encode(computed_hash.as_bytes())
        )));
    }

    info!(
        "Received and verified NAR {} ({} bytes)",
        header.store_path, header.size
    );

    Ok((header, Bytes::from(nar_data)))
}

/// Fetch a NAR from a remote node with retry logic
pub async fn fetch_nar(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
) -> Result<(NarResponseHeader, Bytes)> {
    fetch_nar_with_config(
        endpoint,
        remote,
        blake3,
        &crate::retry::RetryConfig::default(),
    )
    .await
}

/// Fetch a NAR from a remote node with custom retry configuration
pub async fn fetch_nar_with_config(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
    retry_config: &crate::retry::RetryConfig,
) -> Result<(NarResponseHeader, Bytes)> {
    info!("Fetching NAR {} from {}", blake3, remote.id);

    crate::retry::with_retry(
        retry_config,
        &format!("fetch_nar({})", &blake3.to_hex()[..8]),
        || fetch_nar_once(endpoint, remote.clone(), blake3),
    )
    .await
}

/// Result of importing a NAR
#[derive(Debug, Clone)]
pub struct ImportResult {
    pub store_path: String,
    pub blake3: Blake3Hash,
    pub sha256: Sha256Hash,
    pub nar_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NarRequest, NarResponseHeader};

    #[test]
    fn test_max_nar_size_constant() {
        // MAX_NAR_SIZE should be 10 GB
        assert_eq!(MAX_NAR_SIZE, 10 * 1024 * 1024 * 1024);
        assert_eq!(MAX_NAR_SIZE, 10_737_418_240);
    }

    #[test]
    fn test_transfer_buffer_size_constant() {
        // TRANSFER_BUFFER_SIZE should be 64 KB
        assert_eq!(TRANSFER_BUFFER_SIZE, 64 * 1024);
        assert_eq!(TRANSFER_BUFFER_SIZE, 65_536);
    }

    #[test]
    fn test_import_result_construction() {
        let blake3 = Blake3Hash([1u8; 32]);
        let sha256 = Sha256Hash([2u8; 32]);

        let result = ImportResult {
            store_path: "/nix/store/abc123-test".to_string(),
            blake3,
            sha256,
            nar_size: 12345,
        };

        assert_eq!(result.store_path, "/nix/store/abc123-test");
        assert_eq!(result.blake3.0, [1u8; 32]);
        assert_eq!(result.sha256.0, [2u8; 32]);
        assert_eq!(result.nar_size, 12345);
    }

    #[test]
    fn test_nar_request_blake3_accessor() {
        let hash = Blake3Hash([42u8; 32]);
        let request = NarRequest::new(hash);

        // Verify the blake3() accessor returns the correct hash
        let retrieved = request.blake3().expect("should be ByBlake3 variant");
        assert_eq!(retrieved.0, [42u8; 32]);
    }

    #[test]
    fn test_nar_response_header_fields() {
        let sha256 = Sha256Hash([3u8; 32]);
        let header =
            NarResponseHeader::new(1024 * 1024, sha256, "/nix/store/xyz789-package".to_string());

        assert_eq!(header.size, 1024 * 1024);
        assert_eq!(header.sha256, [3u8; 32]);
        assert_eq!(header.store_path, "/nix/store/xyz789-package");

        // Test sha256() accessor
        let retrieved_sha256 = header.sha256();
        assert_eq!(retrieved_sha256.0, [3u8; 32]);
    }

    #[test]
    fn test_nar_response_header_serialization() {
        let sha256 = Sha256Hash([4u8; 32]);
        let header = NarResponseHeader::new(999_999, sha256, "/nix/store/test-path".to_string());

        // Serialize and deserialize
        let bytes = postcard::to_allocvec(&header).unwrap();
        let decoded: NarResponseHeader = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.size, 999_999);
        assert_eq!(decoded.sha256, [4u8; 32]);
        assert_eq!(decoded.store_path, "/nix/store/test-path");
    }
}
