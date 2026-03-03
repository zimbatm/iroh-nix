//! HTTP Binary Cache server for Nix substitution
//!
//! This module implements a minimal HTTP server that speaks the Nix binary cache protocol,
//! allowing Nix to use iroh-nix as a substituter.
//!
//! The server responds to:
//! - GET /nix-cache-info - Cache metadata
//! - GET /<hash>.narinfo - NAR info for a store path
//! - GET /nar/<blake3>.nar - NAR file download

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iroh::Endpoint;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::error::MutexExt;
use crate::gossip::GossipService;
use crate::hash_index::{store_path_basename, HashIndex};
use crate::Result;

/// Configuration for the substituter server
#[derive(Debug, Clone)]
pub struct SubstituterConfig {
    /// Address to bind the HTTP server to
    pub bind_addr: SocketAddr,
    /// Priority of this cache (lower = higher priority)
    pub priority: u32,
}

impl Default for SubstituterConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8080".parse().unwrap(),
            priority: 40,
        }
    }
}

/// Shared P2P context for the substituter (gossip + endpoint for peer fetching)
#[derive(Clone)]
pub struct P2pContext {
    pub gossip: Arc<GossipService>,
    pub endpoint: Endpoint,
}

/// Run the HTTP binary cache server
pub async fn run_substituter(
    config: SubstituterConfig,
    hash_index: Arc<Mutex<HashIndex>>,
    gossip: Option<Arc<GossipService>>,
    endpoint: Endpoint,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!("Substituter listening on http://{}", config.bind_addr);

    let p2p = gossip.map(|g| {
        Arc::new(P2pContext {
            gossip: g,
            endpoint: endpoint.clone(),
        })
    });

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Substituter shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let hash_index = Arc::clone(&hash_index);
                        let config = config.clone();
                        let p2p = p2p.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, addr, &config, hash_index, p2p.as_deref()).await {
                                debug!("Connection error from {}: {}", addr, e);
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Failed to accept connection: {}", e);
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    config: &SubstituterConfig,
    hash_index: Arc<Mutex<HashIndex>>,
    p2p: Option<&P2pContext>,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Read the request line
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        send_response(
            &mut writer,
            400,
            "Bad Request",
            "text/plain",
            b"Bad Request",
            false,
        )
        .await?;
        return Ok(());
    }

    let method = parts[0];
    let path = parts[1];

    debug!("{} {} {}", addr, method, path);

    // Skip headers (read until empty line)
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }

    if method != "GET" && method != "HEAD" {
        send_response(
            &mut writer,
            405,
            "Method Not Allowed",
            "text/plain",
            b"Method Not Allowed",
            false,
        )
        .await?;
        return Ok(());
    }

    let is_head = method == "HEAD";

    // Route the request
    if path == "/nix-cache-info" {
        handle_cache_info(&mut writer, config, is_head).await?;
    } else if path.ends_with(".narinfo") {
        handle_narinfo(&mut writer, path, &hash_index, p2p, is_head).await?;
    } else if path.starts_with("/nar/") && path.ends_with(".nar") {
        handle_nar(&mut writer, path, config, &hash_index, p2p, is_head).await?;
    } else {
        send_response(
            &mut writer,
            404,
            "Not Found",
            "text/plain",
            b"Not Found",
            is_head,
        )
        .await?;
    }

    Ok(())
}

async fn handle_cache_info<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    config: &SubstituterConfig,
    is_head: bool,
) -> Result<()> {
    let body = format!(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\n",
        config.priority
    );
    send_response(
        writer,
        200,
        "OK",
        "text/x-nix-cache-info",
        body.as_bytes(),
        is_head,
    )
    .await
}

async fn handle_narinfo<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    path: &str,
    hash_index: &Arc<Mutex<HashIndex>>,
    p2p: Option<&P2pContext>,
    is_head: bool,
) -> Result<()> {
    // Extract hash from path (e.g., "/abcdef123.narinfo" -> "abcdef123")
    let hash_part = path.trim_start_matches('/').trim_end_matches(".narinfo");

    // Look up by store path hash prefix
    let entry = {
        let index = hash_index.lock_or_err()?;
        index.get_by_store_hash(hash_part).ok().flatten()
    };

    // If not found locally, check gossip provider cache for a peer that has it
    let entry = match entry {
        Some(e) => Some(e),
        None => try_find_via_gossip(hash_part, p2p),
    };

    match entry {
        Some(entry) => {
            // Build narinfo response
            let nar_hash_nix = format!("sha256:{}", entry.sha256.to_nix_base32());
            let blake3_hex = entry.blake3.to_hex();

            // References as space-separated basenames (strip /nix/store/ prefix)
            let references_str: String = entry
                .references
                .iter()
                .filter_map(|r| store_path_basename(r))
                .collect::<Vec<_>>()
                .join(" ");

            let mut body = format!(
                "StorePath: {}\n\
                 URL: nar/{}.nar\n\
                 Compression: none\n\
                 NarHash: {}\n\
                 NarSize: {}\n\
                 References: {}\n",
                entry.store_path, blake3_hex, nar_hash_nix, entry.nar_size, references_str,
            );

            if let Some(deriver) = &entry.deriver {
                if let Some(basename) = store_path_basename(deriver) {
                    body.push_str(&format!("Deriver: {}\n", basename));
                }
            }

            send_response(
                writer,
                200,
                "OK",
                "text/x-nix-narinfo",
                body.as_bytes(),
                is_head,
            )
            .await
        }
        None => {
            send_response(
                writer,
                404,
                "Not Found",
                "text/plain",
                b"Not Found",
                is_head,
            )
            .await
        }
    }
}

/// Try to find a narinfo via the gossip provider cache.
///
/// Scans the gossip provider cache for any entry whose store path matches the
/// requested nix store hash prefix. Returns a synthetic HashEntry if found.
fn try_find_via_gossip(
    store_hash: &str,
    p2p: Option<&P2pContext>,
) -> Option<crate::hash_index::HashEntry> {
    let p2p = p2p?;
    let cache = p2p.gossip.provider_cache().lock().ok()?;

    // Scan all providers for a store path matching this hash prefix
    for providers in cache.providers.values() {
        for provider in providers {
            if let Some(basename) = store_path_basename(&provider.store_path) {
                let provider_hash = basename.split('-').next().unwrap_or("");
                if provider_hash == store_hash {
                    debug!(
                        "Gossip cache hit for {}: {} from peer {}",
                        store_hash, provider.store_path, provider.endpoint_id
                    );
                    // We know the store path and size from gossip, but not sha256/references.
                    // Return a partial entry -- Nix primarily needs StorePath + URL + NarSize.
                    // sha256 hash will be empty (Nix will verify after download).
                    return Some(crate::hash_index::HashEntry {
                        blake3: crate::hash_index::Blake3Hash([0u8; 32]), // unknown
                        sha256: crate::hash_index::Sha256Hash([0u8; 32]), // unknown
                        store_path: provider.store_path.clone(),
                        nar_size: provider.nar_size,
                        references: vec![],
                        deriver: None,
                    });
                }
            }
        }
    }

    None
}

async fn handle_nar<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    path: &str,
    _config: &SubstituterConfig,
    hash_index: &Arc<Mutex<HashIndex>>,
    p2p: Option<&P2pContext>,
    is_head: bool,
) -> Result<()> {
    // Extract blake3 hash from path (e.g., "/nar/abcdef.nar" -> "abcdef")
    let blake3_hex = path.trim_start_matches("/nar/").trim_end_matches(".nar");

    // Parse the hex hash
    let blake3_bytes = match hex::decode(blake3_hex) {
        Ok(b) if b.len() == 32 => b,
        _ => {
            send_response(
                writer,
                400,
                "Bad Request",
                "text/plain",
                b"Invalid hash",
                is_head,
            )
            .await?;
            return Ok(());
        }
    };

    let blake3 = crate::hash_index::Blake3Hash(
        blake3_bytes
            .try_into()
            .map_err(|_| crate::Error::Internal("invalid blake3 hash length".into()))?,
    );

    // Get the hash entry (includes store_path and nar_size)
    let entry = {
        let index = hash_index.lock_or_err()?;
        index.get_by_blake3(&blake3).ok().flatten()
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            send_response(
                writer,
                404,
                "Not Found",
                "text/plain",
                b"Not Found",
                is_head,
            )
            .await?;
            return Ok(());
        }
    };

    // Generate NAR on-demand from store path
    let store_path = std::path::Path::new(&entry.store_path);
    if store_path.exists() {
        // Serve locally
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/x-nix-nar\r\n\
             Content-Length: {}\r\n\
             \r\n",
            entry.nar_size
        );
        writer.write_all(headers.as_bytes()).await?;

        if !is_head {
            let store_path = store_path.to_path_buf();
            let nar_data = tokio::task::spawn_blocking(move || -> crate::Result<Vec<u8>> {
                let (data, _info) = crate::nar::serialize_path(&store_path)?;
                Ok(data)
            })
            .await
            .map_err(|e| crate::Error::Internal(format!("spawn_blocking failed: {}", e)))??;

            for chunk in nar_data.chunks(64 * 1024) {
                writer.write_all(chunk).await?;
            }
        }
    } else if let Some(nar_data) = try_fetch_nar_from_peer(blake3, p2p).await {
        // Store path missing locally but we fetched from a P2P peer
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/x-nix-nar\r\n\
             Content-Length: {}\r\n\
             \r\n",
            nar_data.len()
        );
        writer.write_all(headers.as_bytes()).await?;

        if !is_head {
            for chunk in nar_data.chunks(64 * 1024) {
                writer.write_all(chunk).await?;
            }
        }
    } else {
        send_response(
            writer,
            404,
            "Not Found",
            "text/plain",
            b"Store path not found",
            is_head,
        )
        .await?;
    }

    Ok(())
}

/// Try to fetch a NAR from a P2P peer via gossip.
///
/// Looks up providers in the gossip cache and attempts to fetch the NAR
/// from the first available peer.
async fn try_fetch_nar_from_peer(
    blake3: crate::hash_index::Blake3Hash,
    p2p: Option<&P2pContext>,
) -> Option<bytes::Bytes> {
    let p2p = p2p?;
    let providers = p2p.gossip.get_providers(&blake3);
    if providers.is_empty() {
        return None;
    }

    // Try each provider until one succeeds
    for provider in &providers {
        let remote = iroh_base::EndpointAddr::from(provider.endpoint_id);
        debug!(
            "Attempting P2P fetch of {} from {}",
            blake3, provider.endpoint_id
        );
        match crate::transfer::fetch_nar(&p2p.endpoint, remote, blake3).await {
            Ok((_header, data)) => {
                info!(
                    "P2P fetch succeeded for {} from {} ({} bytes)",
                    blake3,
                    provider.endpoint_id,
                    data.len()
                );
                return Some(data);
            }
            Err(e) => {
                warn!(
                    "P2P fetch failed for {} from {}: {}",
                    blake3, provider.endpoint_id, e
                );
            }
        }
    }

    None
}

async fn send_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: u16,
    status_text: &str,
    content_type: &str,
    body: &[u8],
    is_head: bool,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         \r\n",
        status,
        status_text,
        content_type,
        body.len()
    );
    writer.write_all(response.as_bytes()).await?;
    if !is_head && !body.is_empty() {
        writer.write_all(body).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = SubstituterConfig::default();
        assert_eq!(config.priority, 40);
        assert_eq!(config.bind_addr, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn test_config_custom() {
        let config = SubstituterConfig {
            bind_addr: "0.0.0.0:9090".parse().unwrap(),
            priority: 30,
        };
        assert_eq!(config.priority, 30);
        assert_eq!(config.bind_addr.port(), 9090);
    }

    #[test]
    fn test_extract_hash_from_narinfo_path() {
        // Simulate the extraction logic from handle_narinfo
        let path = "/abc123def456.narinfo";
        let hash_part = path.trim_start_matches('/').trim_end_matches(".narinfo");
        assert_eq!(hash_part, "abc123def456");

        let path2 = "/short.narinfo";
        let hash_part2 = path2.trim_start_matches('/').trim_end_matches(".narinfo");
        assert_eq!(hash_part2, "short");
    }

    #[test]
    fn test_extract_blake3_from_nar_path() {
        // Simulate the extraction logic from handle_nar
        let path = "/nar/abcdef123456.nar";
        let blake3_hex = path.trim_start_matches("/nar/").trim_end_matches(".nar");
        assert_eq!(blake3_hex, "abcdef123456");
    }

    #[test]
    fn test_cache_info_format() {
        let config = SubstituterConfig {
            priority: 25,
            ..Default::default()
        };
        let body = format!(
            "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\n",
            config.priority
        );
        assert!(body.contains("StoreDir: /nix/store"));
        assert!(body.contains("WantMassQuery: 1"));
        assert!(body.contains("Priority: 25"));
    }

    #[test]
    fn test_blake3_hex_validation() {
        // Valid 32-byte (64 hex chars) hash
        let valid_hex = "a".repeat(64);
        let result = hex::decode(&valid_hex);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 32);

        // Invalid: wrong length
        let short_hex = "abc123";
        let result = hex::decode(short_hex);
        assert!(result.is_ok());
        assert_ne!(result.unwrap().len(), 32);

        // Invalid: non-hex characters
        let invalid_hex = "xyz123";
        assert!(hex::decode(invalid_hex).is_err());
    }
}
