//! HTTP Binary Cache server for Nix substitution
//!
//! Serves both the standard Nix binary cache protocol ("dumb") and the smart
//! protocol over HTTP.
//!
//! Dumb routes (unchanged):
//! - GET /nix-cache-info
//! - GET /<hash>.narinfo
//! - GET /nar/<blake3>.nar
//!
//! Smart routes (new):
//! - POST /_nix/v1/batch-narinfo
//! - POST /_nix/v1/closure

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use nix_store::hash_index::store_path_basename;
use nix_store::smart::{SmartProvider, SmartRequest, SmartResponse, StorePathInfoCompact};
use nix_store::store::ContentFilter;
use nix_store::Result;

/// Maximum POST body size (1 MB)
const MAX_BODY_SIZE: usize = 1024 * 1024;

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

/// Run the HTTP binary cache server
pub async fn run_substituter(
    config: Arc<SubstituterConfig>,
    provider: Arc<dyn SmartProvider>,
    filter: Arc<dyn ContentFilter>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!("Substituter listening on http://{}", config.bind_addr);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Substituter shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        let provider = Arc::clone(&provider);
                        let filter = Arc::clone(&filter);
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, addr, &config, &provider, &filter).await {
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

/// Parsed HTTP request headers
struct ParsedRequest {
    method: String,
    path: String,
    content_length: usize,
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    config: &SubstituterConfig,
    provider: &Arc<dyn SmartProvider>,
    filter: &Arc<dyn ContentFilter>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
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

    let method = parts[0].to_string();
    let path = parts[1].to_string();

    debug!("{} {} {}", addr, method, path);

    // Parse headers (extract Content-Length for POST)
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            match value.trim().parse() {
                Ok(v) => content_length = v,
                Err(e) => {
                    warn!("invalid Content-Length header {:?}: {}", value.trim(), e);
                }
            }
        }
    }

    let req = ParsedRequest {
        method,
        path,
        content_length,
    };

    // Route: smart protocol POST endpoints
    if req.method == "POST" {
        if req.path == "/_nix/v1/batch-narinfo" || req.path == "/_nix/v1/closure" {
            return handle_smart_post(&mut reader, &mut writer, &req, provider).await;
        }
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

    if req.method != "GET" && req.method != "HEAD" {
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

    let is_head = req.method == "HEAD";

    // Route: dumb protocol GET endpoints
    if req.path == "/nix-cache-info" {
        handle_cache_info(&mut writer, config, is_head).await?;
    } else if req.path.ends_with(".narinfo") {
        handle_narinfo(&mut writer, &req.path, provider, filter, is_head).await?;
    } else if req.path.starts_with("/nar/") && req.path.ends_with(".nar") {
        handle_nar(&mut writer, &req.path, provider, is_head).await?;
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

// ============================================================================
// Smart protocol handlers
// ============================================================================

async fn handle_smart_post<R, W>(
    reader: &mut R,
    writer: &mut W,
    req: &ParsedRequest,
    provider: &Arc<dyn SmartProvider>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Read the request body
    if req.content_length > MAX_BODY_SIZE {
        let resp = SmartResponse::Error {
            code: nix_store::SmartErrorCode::TooManyHashes,
            message: "request body too large".to_string(),
        };
        return send_json_response(writer, 413, &resp).await;
    }

    let mut body = vec![0u8; req.content_length];
    reader.read_exact(&mut body).await?;

    let smart_req: SmartRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp = SmartResponse::Error {
                code: nix_store::SmartErrorCode::BadRequest,
                message: format!("invalid JSON: {}", e),
            };
            return send_json_response(writer, 400, &resp).await;
        }
    };

    let resp = match smart_req {
        SmartRequest::BatchNarInfo { store_hashes } => {
            match provider.batch_narinfo(&store_hashes).await {
                Ok(results) => {
                    let compact_results: Vec<Option<StorePathInfoCompact>> = results
                        .iter()
                        .map(|opt| opt.as_ref().map(StorePathInfoCompact::from_store_path_info))
                        .collect();
                    SmartResponse::BatchNarInfo {
                        results: compact_results,
                    }
                }
                Err(e) => SmartResponse::Error {
                    code: nix_store::SmartErrorCode::Internal,
                    message: format!("{}", e),
                },
            }
        }
        SmartRequest::Closure {
            wants,
            haves,
            limit,
        } => match provider.resolve_closure(&wants, &haves, limit).await {
            Ok((paths, has_more)) => {
                let compact_paths: Vec<StorePathInfoCompact> = paths
                    .iter()
                    .map(StorePathInfoCompact::from_store_path_info)
                    .collect();
                SmartResponse::PathSet {
                    paths: compact_paths,
                    has_more,
                }
            }
            Err(e) => SmartResponse::Error {
                code: nix_store::SmartErrorCode::Internal,
                message: format!("{}", e),
            },
        },
    };

    send_json_response(writer, 200, &resp).await
}

async fn send_json_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: u16,
    resp: &SmartResponse,
) -> Result<()> {
    let body = serde_json::to_vec(resp)
        .map_err(|e| nix_store::Error::Internal(format!("JSON serialize: {}", e)))?;
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        413 => "Payload Too Large",
        _ => "Error",
    };
    send_response(
        writer,
        status,
        status_text,
        "application/json",
        &body,
        false,
    )
    .await
}

// ============================================================================
// Dumb protocol handlers (unchanged logic)
// ============================================================================

async fn handle_cache_info<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    config: &SubstituterConfig,
    is_head: bool,
) -> Result<()> {
    let body = format!(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\nSmartProtocol: 1\n",
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
    provider: &Arc<dyn SmartProvider>,
    filter: &Arc<dyn ContentFilter>,
    is_head: bool,
) -> Result<()> {
    let hash_part = path.trim_start_matches('/').trim_end_matches(".narinfo");
    let entry = provider.get_narinfo(hash_part).await?;

    match entry {
        Some(info) if filter.allow_narinfo(&info) => {
            let nar_hash_nix = format!("sha256:{}", info.nar_hash.to_nix_base32());
            let blake3_hex = info.blake3.to_hex();

            let references_str: String = info
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
                info.store_path, blake3_hex, nar_hash_nix, info.nar_size, references_str,
            );

            if let Some(deriver) = &info.deriver {
                if let Some(basename) = store_path_basename(deriver) {
                    body.push_str(&format!("Deriver: {}\n", basename));
                }
            }

            for sig in &info.signatures {
                body.push_str(&format!("Sig: {}\n", sig));
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
        _ => {
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

async fn handle_nar<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    path: &str,
    provider: &Arc<dyn SmartProvider>,
    is_head: bool,
) -> Result<()> {
    let blake3_hex = path.trim_start_matches("/nar/").trim_end_matches(".nar");

    let blake3 = match nix_store::hash_index::Blake3Hash::from_hex(blake3_hex) {
        Ok(h) => h,
        Err(_) => {
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

    match provider.get_nar_by_blake3(&blake3).await? {
        Some(data) => {
            send_response(writer, 200, "OK", "application/x-nix-nar", &data, is_head).await
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
    fn test_extract_hash_from_narinfo_path() {
        let path = "/abc123def456.narinfo";
        let hash_part = path.trim_start_matches('/').trim_end_matches(".narinfo");
        assert_eq!(hash_part, "abc123def456");
    }

    #[test]
    fn test_extract_blake3_from_nar_path() {
        let path = "/nar/abcdef123456.nar";
        let blake3_hex = path.trim_start_matches("/nar/").trim_end_matches(".nar");
        assert_eq!(blake3_hex, "abcdef123456");
    }

    #[test]
    fn test_cache_info_format_includes_smart_protocol() {
        let config = SubstituterConfig {
            priority: 25,
            ..Default::default()
        };
        let body = format!(
            "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\nSmartProtocol: 1\n",
            config.priority
        );
        assert!(body.contains("StoreDir: /nix/store"));
        assert!(body.contains("WantMassQuery: 1"));
        assert!(body.contains("Priority: 25"));
        assert!(body.contains("SmartProtocol: 1"));
    }
}
