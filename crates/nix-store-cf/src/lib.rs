//! Cloudflare Worker for Nix binary cache
//!
//! Serves the smart protocol and upload API, backed by D1 for metadata
//! and R2 for NAR content. Authentication via static token or OIDC.
//!
//! Read routes (public):
//!   GET  /nar/<blake3>.nar         -> NAR from R2
//!   POST /_nix/v1/batch-narinfo    -> batch narinfo lookup from D1
//!   POST /_nix/v1/closure          -> closure computation over D1
//!
//! Upload routes (authenticated):
//!   POST /_admin/v1/upload/init    -> stage narinfos, get upload instructions
//!   PUT  /_admin/v1/nar/<blake3>   -> upload NAR directly (raw binary body)
//!   POST /_admin/v1/upload/commit  -> atomically publish staged narinfos

mod d1;
mod oidc;
mod r2;

use nix_store::smart::{SmartRequest, SmartResponse};
use worker::*;

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let router = Router::new();

    router
        // Public read routes
        .get_async("/nar/:blake3.nar", handle_nar)
        .post_async("/_nix/v1/batch-narinfo", handle_batch_narinfo)
        .post_async("/_nix/v1/closure", handle_closure)
        // Authenticated upload routes
        .post_async("/_admin/v1/upload/init", handle_upload_init)
        .put_async("/_admin/v1/nar/:blake3", handle_upload_nar)
        .post_async("/_admin/v1/upload/commit", handle_upload_commit)
        .run(req, env)
        .await
}

// ============================================================================
// Auth helper
// ============================================================================

async fn check_admin_auth(req: &Request, ctx: &RouteContext<()>) -> Result<()> {
    let auth_header = req
        .headers()
        .get("Authorization")?
        .ok_or_else(|| Error::RustError("missing Authorization header".into()))?;

    let static_token = ctx.secret("ADMIN_TOKEN").ok().map(|s| s.to_string());

    let oidc_config = match ctx.secret("OIDC_CONFIG") {
        Ok(secret) => {
            let json = secret.to_string();
            match serde_json::from_str::<oidc::OidcConfig>(&json) {
                Ok(config) => Some(config),
                Err(e) => {
                    console_log!("Failed to parse OIDC_CONFIG: {}", e);
                    None
                }
            }
        }
        Err(_) => None,
    };

    let result =
        oidc::validate_auth(&auth_header, static_token.as_deref(), oidc_config.as_ref()).await;

    if result.is_allowed() {
        Ok(())
    } else {
        Err(Error::RustError("unauthorized".into()))
    }
}

// ============================================================================
// Read handlers
// ============================================================================

async fn handle_nar(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let blake3_hex = ctx
        .param("blake3")
        .ok_or_else(|| Error::RustError("missing blake3".into()))?;
    let bucket = ctx.env.bucket("NAR_BUCKET")?;

    r2::stream_nar(&bucket, blake3_hex).await
}

// ============================================================================
// Smart protocol handlers
// ============================================================================

async fn handle_batch_narinfo(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let body = req.text().await?;
    let smart_req: SmartRequest = serde_json::from_str(&body)
        .map_err(|e| Error::RustError(format!("invalid JSON: {}", e)))?;

    let db = ctx.env.d1("DB")?;

    let resp = match smart_req {
        SmartRequest::BatchNarInfo { store_hashes } => {
            if store_hashes.len() > 1000 {
                SmartResponse::Error {
                    code: nix_store::SmartErrorCode::TooManyHashes,
                    message: "maximum 1000 hashes per request".to_string(),
                }
            } else {
                d1::batch_narinfo(&db, &store_hashes).await?
            }
        }
        _ => SmartResponse::Error {
            code: nix_store::SmartErrorCode::BadRequest,
            message: "expected BatchNarInfo request".to_string(),
        },
    };

    json_response(&resp)
}

async fn handle_closure(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let body = req.text().await?;
    let smart_req: SmartRequest = serde_json::from_str(&body)
        .map_err(|e| Error::RustError(format!("invalid JSON: {}", e)))?;

    let db = ctx.env.d1("DB")?;

    let resp = match smart_req {
        SmartRequest::Closure {
            wants,
            haves,
            limit,
        } => {
            if wants.len() > 100 {
                SmartResponse::Error {
                    code: nix_store::SmartErrorCode::TooManyHashes,
                    message: "maximum 100 wants per request".to_string(),
                }
            } else {
                d1::resolve_closure(&db, &wants, &haves, limit).await?
            }
        }
        _ => SmartResponse::Error {
            code: nix_store::SmartErrorCode::BadRequest,
            message: "expected Closure request".to_string(),
        },
    };

    json_response(&resp)
}

// ============================================================================
// Upload protocol (authenticated, 3-phase)
//
// Phase 1: POST /_admin/v1/upload/init
//   Client sends narinfo metadata for all paths it wants to upload.
//   Server stages them in pending_upload table and returns which NARs
//   need uploading (deduplicates against existing R2 objects).
//
// Phase 2: PUT /_admin/v1/nar/<blake3>
//   Client uploads each NAR as raw binary. Worker streams to R2.
//   No base64, no JSON wrapping -- just raw bytes.
//
// Phase 3: POST /_admin/v1/upload/commit
//   Server verifies all NARs exist in R2, then atomically moves
//   pending narinfos into the live narinfo table.
// ============================================================================

async fn handle_upload_init(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    check_admin_auth(&req, &ctx).await?;

    let body = req.text().await?;

    #[derive(serde::Deserialize)]
    struct InitRequest {
        paths: Vec<nix_store::StorePathInfoCompact>,
    }

    let init: InitRequest = serde_json::from_str(&body)
        .map_err(|e| Error::RustError(format!("invalid JSON: {}", e)))?;

    if init.paths.is_empty() {
        return Response::error("empty paths list", 400);
    }

    if init.paths.len() > 1000 {
        return Response::error("maximum 1000 paths per upload session", 400);
    }

    let db = ctx.env.d1("DB")?;
    let bucket = ctx.env.bucket("NAR_BUCKET")?;

    // Generate a session ID
    let session_id = generate_session_id();

    // Stage narinfos in pending_upload table and check which NARs already exist
    let mut need_upload: Vec<String> = Vec::new();

    for info in &init.paths {
        d1::stage_pending_narinfo(&db, &session_id, info).await?;

        // Check if NAR already exists in R2 (dedup)
        if let Some(ref blake3_hex) = info.blake3 {
            if !r2::nar_exists(&bucket, blake3_hex).await? {
                need_upload.push(blake3_hex.clone());
            }
        }
    }

    #[derive(serde::Serialize)]
    struct InitResponse {
        session_id: String,
        /// blake3 hashes of NARs that need uploading (not already in R2)
        need_upload: Vec<String>,
    }

    let resp = InitResponse {
        session_id,
        need_upload,
    };

    let body = serde_json::to_string(&resp)
        .map_err(|e| Error::RustError(format!("JSON serialize: {}", e)))?;
    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Ok(Response::ok(body)?.with_headers(headers))
}

async fn handle_upload_nar(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    check_admin_auth(&req, &ctx).await?;

    let blake3_hex = ctx
        .param("blake3")
        .ok_or_else(|| Error::RustError("missing blake3".into()))?;

    // Validate blake3 hex format (64 hex chars)
    if blake3_hex.len() != 64 || !blake3_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Response::error("invalid blake3 hash: expected 64 hex characters", 400);
    }

    let bucket = ctx.env.bucket("NAR_BUCKET")?;

    // Read raw binary body and write to R2
    // The worker crate handles streaming internally
    let bytes = match req.bytes().await {
        Ok(b) => b,
        Err(e) => return Response::error(format!("failed to read body: {}", e), 400),
    };

    if bytes.is_empty() {
        return Response::error("empty NAR body", 400);
    }

    r2::put_nar(&bucket, blake3_hex, bytes).await?;

    Response::ok("ok")
}

async fn handle_upload_commit(mut req: Request, ctx: RouteContext<()>) -> Result<Response> {
    check_admin_auth(&req, &ctx).await?;

    let body = req.text().await?;

    #[derive(serde::Deserialize)]
    struct CommitRequest {
        session_id: String,
    }

    let commit: CommitRequest = serde_json::from_str(&body)
        .map_err(|e| Error::RustError(format!("invalid JSON: {}", e)))?;

    let db = ctx.env.d1("DB")?;
    let bucket = ctx.env.bucket("NAR_BUCKET")?;

    // Get all pending narinfos for this session
    let pending = d1::get_pending_narinfos(&db, &commit.session_id).await?;

    if pending.is_empty() {
        return Response::error("no pending uploads for this session", 404);
    }

    // Verify all NARs exist in R2 before committing
    for info in &pending {
        if let Some(ref blake3_hex) = info.blake3 {
            if !r2::nar_exists(&bucket, blake3_hex).await? {
                return Response::error(
                    format!("NAR not found in R2: {}", blake3_hex),
                    409, // Conflict -- upload not complete
                );
            }
        }
    }

    // Atomically move pending narinfos to live table
    d1::commit_pending_narinfos(&db, &commit.session_id).await?;

    #[derive(serde::Serialize)]
    struct CommitResponse {
        committed: usize,
    }

    let resp = CommitResponse {
        committed: pending.len(),
    };

    let resp_body = serde_json::to_string(&resp)
        .map_err(|e| Error::RustError(format!("JSON serialize: {}", e)))?;
    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Ok(Response::ok(resp_body)?.with_headers(headers))
}

// ============================================================================
// Helpers
// ============================================================================

fn json_response(resp: &SmartResponse) -> Result<Response> {
    let body = serde_json::to_string(resp)
        .map_err(|e| Error::RustError(format!("JSON serialize: {}", e)))?;

    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Ok(Response::ok(body)?.with_headers(headers))
}

/// Generate a random session ID (hex-encoded)
fn generate_session_id() -> String {
    // Use js_sys for randomness in Workers
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).unwrap_or_else(|_| {
        // Fallback: use timestamp-based ID
        let ts = js_sys::Date::now() as u64;
        buf[..8].copy_from_slice(&ts.to_le_bytes());
    });
    hex::encode(buf)
}
