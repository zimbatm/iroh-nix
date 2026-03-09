//! NAR transfer over QUIC
//!
//! Handles bidirectional streaming of NAR blobs between nodes using
//! iroh's QUIC transport. Supports both BLAKE3-based and store-path-based
//! requests with on-demand NAR serialization.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use iroh::endpoint::{RecvStream, SendStream};
use iroh::Endpoint;
use iroh_base::EndpointAddr;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use nix_store::hash_index::{Blake3Hash, Sha256Hash};
use nix_store::store::NarInfoProvider;
use nix_store::{Error, Result};

use crate::NAR_PROTOCOL_ALPN;

/// Maximum NAR size we'll accept (10 GB)
pub(crate) const MAX_NAR_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Buffer size for streaming transfers
pub(crate) const TRANSFER_BUFFER_SIZE: usize = 64 * 1024;

/// Request to fetch a NAR - can be by BLAKE3 hash or store path
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NarRequest {
    /// Request by BLAKE3 hash (requires pre-indexed path)
    ByBlake3 { hash: [u8; 32] },
    /// Request by store path (enables on-demand serving of any /nix/store path)
    ByStorePath { store_path: String },
}

impl NarRequest {
    /// Create a new NAR request by BLAKE3 hash
    pub fn new(hash: Blake3Hash) -> Self {
        Self::ByBlake3 { hash: hash.0 }
    }

    /// Create a new NAR request by store path
    pub fn by_store_path(store_path: impl Into<String>) -> Self {
        Self::ByStorePath {
            store_path: store_path.into(),
        }
    }

    /// Get the BLAKE3 hash if this is a ByBlake3 request
    pub fn blake3(&self) -> Option<Blake3Hash> {
        match self {
            Self::ByBlake3 { hash } => Some(Blake3Hash(*hash)),
            Self::ByStorePath { .. } => None,
        }
    }

    /// Get the store path if this is a ByStorePath request
    pub fn store_path(&self) -> Option<&str> {
        match self {
            Self::ByBlake3 { .. } => None,
            Self::ByStorePath { store_path } => Some(store_path),
        }
    }
}

/// Response header for NAR transfer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarResponseHeader {
    /// Store path this NAR corresponds to
    pub store_path: String,
    /// Size of the NAR in bytes
    pub size: u64,
    /// SHA256 hash for Nix verification
    pub sha256: [u8; 32],
}

impl NarResponseHeader {
    /// Create a new response header
    pub fn new(size: u64, sha256: Sha256Hash, store_path: String) -> Self {
        Self {
            store_path,
            size,
            sha256: sha256.0,
        }
    }

    /// Get SHA256 as Sha256Hash
    pub fn sha256(&self) -> Sha256Hash {
        Sha256Hash(self.sha256)
    }
}

/// Result of importing a NAR into the local store
#[derive(Debug, Clone)]
pub struct ImportResult {
    /// The store path that was imported
    pub store_path: String,
    /// BLAKE3 hash of the NAR content
    pub blake3: Blake3Hash,
    /// SHA256 hash of the NAR content
    pub sha256: Sha256Hash,
    /// Size of the NAR in bytes
    pub nar_size: u64,
}

/// Handle an accepted NAR request connection (server side)
///
/// Receives a `NarRequest` from the remote peer, looks up the store path
/// via the provided `NarInfoProvider`, generates NAR data on-demand, and
/// streams the response back.
pub async fn handle_nar_accepted(
    connection: iroh::endpoint::Connection,
    store: Arc<dyn NarInfoProvider>,
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
            handle_nar_by_blake3(&mut send, &store, blake3).await
        }
        NarRequest::ByStorePath { store_path } => {
            debug!("NAR request by store path: {}", store_path);
            handle_nar_by_store_path(&mut send, &store, store_path).await
        }
    };

    if let Err(ref e) = result {
        warn!("NAR request failed for {}: {}", remote_id, e);
    }

    // Finish the stream
    send.finish().ok();

    result
}

/// Handle NAR request by BLAKE3 hash
async fn handle_nar_by_blake3(
    send: &mut SendStream,
    store: &Arc<dyn NarInfoProvider>,
    blake3: Blake3Hash,
) -> Result<()> {
    let blake3_hex = blake3.to_hex();
    let info = store.get_narinfo(&blake3_hex).await?;

    match info {
        Some(info) => {
            let store_path = &info.store_path;
            let path = Path::new(store_path);
            if path.exists() {
                stream_nar_from_path(send, store_path, info.nar_size, info.nar_hash).await?;
                info!("Sent NAR {} ({} bytes)", store_path, info.nar_size);
                Ok(())
            } else {
                warn!("Store path missing for {}: {}", blake3, store_path);
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
    store: &Arc<dyn NarInfoProvider>,
    store_path: &str,
) -> Result<()> {
    if !nix_store::nix_info::is_valid_store_path(store_path) {
        return send_error_response(send, "invalid store path format").await;
    }

    let fs_path = Path::new(store_path);
    if !fs_path.exists() {
        return send_error_response(send, "store path not found").await;
    }

    // Try to find cached metadata via the store hash
    let store_hash = nix_store::hash_index::extract_store_hash(store_path);
    let cached_info = if let Some(ref hash) = store_hash {
        store.get_narinfo(hash).await?
    } else {
        None
    };

    if let Some(info) = cached_info {
        stream_nar_from_path(send, store_path, info.nar_size, info.nar_hash).await?;
        info!("Sent NAR {} ({} bytes) [cached]", store_path, info.nar_size);
    } else {
        // Query Nix for metadata on-demand
        let nix_info = nix_store::NixPathInfo::query(store_path).await?;
        let sha256 = Sha256Hash(nix_info.sha256_bytes()?);
        stream_nar_from_path(send, store_path, nix_info.nar_size, sha256).await?;
        info!(
            "Sent NAR {} ({} bytes) [on-demand]",
            store_path, nix_info.nar_size
        );
    }
    Ok(())
}

/// Stream NAR directly from a store path to the network
async fn stream_nar_from_path(
    send: &mut SendStream,
    store_path: &str,
    nar_size: u64,
    sha256: Sha256Hash,
) -> Result<()> {
    let header = NarResponseHeader::new(nar_size, sha256, store_path.to_string());

    let header_bytes = postcard::to_allocvec(&header)
        .map_err(|e| Error::Protocol(format!("failed to serialize header: {}", e)))?;

    // Send success marker + header length + header
    send.write_all(&[1u8])
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
        let (data, _info) = nix_store::nar::serialize_path(&path)?;
        Ok(data)
    })
    .await
    .map_err(|e| Error::Protocol(format!("NAR serialization task failed: {}", e)))??;

    // Stream the NAR data in chunks
    for chunk in nar_data.chunks(TRANSFER_BUFFER_SIZE) {
        send.write_all(chunk)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write NAR data: {}", e)))?;
    }

    Ok(())
}

/// Read a NAR request from a stream
async fn read_nar_request(recv: &mut RecvStream) -> Result<NarRequest> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request length: {}", e)))?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 1024 {
        return Err(Error::Protocol("request too large".into()));
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request: {}", e)))?;

    let request: NarRequest = postcard::from_bytes(&buf)
        .map_err(|e| Error::Protocol(format!("invalid request: {}", e)))?;

    Ok(request)
}

/// Send an error response to the remote peer
async fn send_error_response(send: &mut SendStream, message: &str) -> Result<()> {
    send.write_all(&[0u8])
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

/// Fetch a NAR from a remote node (single attempt, no retry)
async fn fetch_nar_once(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
) -> Result<(NarResponseHeader, Bytes)> {
    debug!("Connecting to {} for NAR {}", remote.id, blake3);

    let connection = endpoint
        .connect(remote.clone(), NAR_PROTOCOL_ALPN)
        .await
        .map_err(|e| Error::Protocol(format!("connection failed: {}", e)))?;

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

    // Read response status
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
        return Err(Error::Protocol(format!("remote error: {}", msg)));
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

/// Fetch a NAR from a remote node with default retry configuration
pub async fn fetch_nar(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
) -> Result<(NarResponseHeader, Bytes)> {
    fetch_nar_with_config(endpoint, remote, blake3, &nix_store::RetryConfig::default()).await
}

/// Fetch a NAR from a remote node with a custom retry configuration
pub async fn fetch_nar_with_config(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    blake3: Blake3Hash,
    retry_config: &nix_store::RetryConfig,
) -> Result<(NarResponseHeader, Bytes)> {
    info!("Fetching NAR {} from {}", blake3, remote.id);

    nix_store::with_retry(
        retry_config,
        &format!("fetch_nar({})", &blake3.to_hex()[..8]),
        |e: &Error| e.is_transient(),
        || fetch_nar_once(endpoint, remote.clone(), blake3),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nar_request_by_blake3_roundtrip() {
        let req = NarRequest::new(Blake3Hash([42u8; 32]));
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: NarRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.blake3(), Some(Blake3Hash([42u8; 32])));
        assert_eq!(decoded.store_path(), None);
    }

    #[test]
    fn test_nar_request_by_store_path_roundtrip() {
        let req = NarRequest::by_store_path("/nix/store/abc123-hello");
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: NarRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.blake3(), None);
        assert_eq!(decoded.store_path(), Some("/nix/store/abc123-hello"));
    }

    #[test]
    fn test_nar_response_header_serialization() {
        let sha256 = Sha256Hash([4u8; 32]);
        let header = NarResponseHeader::new(999_999, sha256, "/nix/store/test-path".to_string());

        let bytes = postcard::to_allocvec(&header).unwrap();
        let decoded: NarResponseHeader = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.size, 999_999);
        assert_eq!(decoded.sha256, [4u8; 32]);
        assert_eq!(decoded.store_path, "/nix/store/test-path");
        assert_eq!(decoded.sha256(), Sha256Hash([4u8; 32]));
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
    fn test_max_nar_size_constant() {
        assert_eq!(MAX_NAR_SIZE, 10 * 1024 * 1024 * 1024);
        assert_eq!(MAX_NAR_SIZE, 10_737_418_240);
    }

    #[test]
    fn test_transfer_buffer_size_constant() {
        assert_eq!(TRANSFER_BUFFER_SIZE, 64 * 1024);
        assert_eq!(TRANSFER_BUFFER_SIZE, 65_536);
    }
}
