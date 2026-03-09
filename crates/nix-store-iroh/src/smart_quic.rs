//! Smart protocol over QUIC
//!
//! Handles batch narinfo and closure requests between nodes using iroh's
//! QUIC transport with postcard-encoded framing.

use std::sync::Arc;

use iroh::endpoint::SendStream;
use iroh::Endpoint;
use iroh_base::EndpointAddr;
use tracing::debug;

use nix_store::smart::{SmartProvider, SmartRequest, SmartResponse, StorePathInfoCompact};
use nix_store::{Error, Result};

use crate::SMART_PROTOCOL_ALPN;

/// Handle an accepted smart protocol connection (server side)
///
/// Reads a postcard-encoded `SmartRequest`, processes it via the provided
/// `SmartProvider`, and writes back a postcard-encoded `SmartResponse`.
/// Framing: `[4 bytes LE length][postcard payload]`
pub async fn handle_smart_accepted(
    connection: iroh::endpoint::Connection,
    store: Arc<dyn SmartProvider>,
) -> Result<()> {
    let remote_id = connection.remote_id();
    debug!("Smart protocol connection from {}", remote_id);

    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| Error::Protocol(format!("failed to accept stream: {}", e)))?;

    // Read request: [4 bytes LE length][postcard(SmartRequest)]
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request length: {}", e)))?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 1024 * 1024 {
        let resp = SmartResponse::Error {
            code: nix_store::SmartErrorCode::TooManyHashes,
            message: "request too large".to_string(),
        };
        write_smart_response(&mut send, &resp).await?;
        send.finish().ok();
        return Ok(());
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read request: {}", e)))?;

    let request: SmartRequest = match postcard::from_bytes(&buf) {
        Ok(r) => r,
        Err(e) => {
            let resp = SmartResponse::Error {
                code: nix_store::SmartErrorCode::BadRequest,
                message: format!("invalid postcard: {}", e),
            };
            write_smart_response(&mut send, &resp).await?;
            send.finish().ok();
            return Ok(());
        }
    };

    let resp = match request {
        SmartRequest::BatchNarInfo { store_hashes } => {
            match store.batch_narinfo(&store_hashes).await {
                Ok(results) => SmartResponse::BatchNarInfo {
                    results: results
                        .iter()
                        .map(|opt| opt.as_ref().map(StorePathInfoCompact::from_store_path_info))
                        .collect(),
                },
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
        } => match store.resolve_closure(&wants, &haves, limit).await {
            Ok((paths, has_more)) => SmartResponse::PathSet {
                paths: paths
                    .iter()
                    .map(StorePathInfoCompact::from_store_path_info)
                    .collect(),
                has_more,
            },
            Err(e) => SmartResponse::Error {
                code: nix_store::SmartErrorCode::Internal,
                message: format!("{}", e),
            },
        },
    };

    write_smart_response(&mut send, &resp).await?;
    send.finish().ok();
    Ok(())
}

/// Write a SmartResponse to a QUIC send stream with length-prefix framing
async fn write_smart_response(send: &mut SendStream, resp: &SmartResponse) -> Result<()> {
    let resp_bytes = postcard::to_allocvec(resp)
        .map_err(|e| Error::Protocol(format!("serialize smart response: {}", e)))?;

    send.write_all(&(resp_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("write smart response length: {}", e)))?;

    send.write_all(&resp_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("write smart response: {}", e)))?;

    Ok(())
}

/// Send a smart protocol request to a remote node over QUIC
///
/// Connects using the `/iroh-nix/smart/1` ALPN, sends a postcard-encoded request,
/// and reads back a postcard-encoded response.
pub async fn smart_request(
    endpoint: &Endpoint,
    remote: EndpointAddr,
    req: SmartRequest,
) -> Result<SmartResponse> {
    debug!("Smart request to {}", remote.id);

    let connection = endpoint
        .connect(remote, SMART_PROTOCOL_ALPN)
        .await
        .map_err(|e| Error::Protocol(format!("smart connection failed: {}", e)))?;

    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| Error::Protocol(format!("failed to open stream: {}", e)))?;

    // Send request: [4 bytes LE length][postcard(SmartRequest)]
    let req_bytes = postcard::to_allocvec(&req)
        .map_err(|e| Error::Protocol(format!("serialize smart request: {}", e)))?;

    send.write_all(&(req_bytes.len() as u32).to_le_bytes())
        .await
        .map_err(|e| Error::Protocol(format!("write smart request length: {}", e)))?;

    send.write_all(&req_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("write smart request: {}", e)))?;

    send.finish().ok();

    // Read response: [4 bytes LE length][postcard(SmartResponse)]
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("read smart response length: {}", e)))?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 10 * 1024 * 1024 {
        return Err(Error::Protocol("smart response too large".into()));
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Protocol(format!("read smart response: {}", e)))?;

    let resp: SmartResponse = postcard::from_bytes(&buf)
        .map_err(|e| Error::Protocol(format!("invalid smart response: {}", e)))?;

    Ok(resp)
}
