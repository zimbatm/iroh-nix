//! D1 (Cloudflare serverless SQLite) queries for narinfo metadata

use nix_store::smart::{SmartErrorCode, SmartResponse, StorePathInfoCompact};
use worker::*;

/// Look up a single narinfo by store hash from D1
pub async fn get_narinfo(
    db: &D1Database,
    store_hash: &str,
) -> Result<Option<StorePathInfoCompact>> {
    let stmt = db
        .prepare("SELECT store_path, nar_hash_hex, nar_size, references_basenames, deriver, signatures, blake3_hex FROM narinfo WHERE store_hash = ?1")
        .bind(&[store_hash.into()])?;

    let result = stmt.first::<D1NarinfoRow>(None).await?;
    Ok(result.map(|row| row.into_compact(store_hash)))
}

/// Batch lookup of narinfo entries by store hashes
pub async fn batch_narinfo(db: &D1Database, store_hashes: &[String]) -> Result<SmartResponse> {
    let mut results = Vec::with_capacity(store_hashes.len());

    for hash in store_hashes {
        let info = get_narinfo(db, hash).await?;
        results.push(info);
    }

    Ok(SmartResponse::BatchNarInfo { results })
}

/// Compute closure delta using BFS over D1
pub async fn resolve_closure(
    db: &D1Database,
    wants: &[String],
    haves: &[String],
    limit: Option<u32>,
) -> Result<SmartResponse> {
    use std::collections::{HashSet, VecDeque};

    let have_set: HashSet<&str> = haves.iter().map(|s| s.as_str()).collect();
    let mut needed: Vec<StorePathInfoCompact> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let limit = limit.unwrap_or(10_000) as usize;

    // Cap at 10k to prevent DoS
    if limit > 10_000 {
        return Ok(SmartResponse::Error {
            code: SmartErrorCode::TooManyHashes,
            message: "limit exceeds maximum of 10000".to_string(),
        });
    }

    for want in wants {
        queue.push_back(want.clone());
    }

    while let Some(hash) = queue.pop_front() {
        if have_set.contains(hash.as_str()) || seen.contains(&hash) {
            continue;
        }
        seen.insert(hash.clone());

        let info = match get_narinfo(db, &hash).await? {
            Some(info) => info,
            None => continue,
        };

        // Enqueue references
        for ref_basename in &info.references {
            if let Some(ref_hash) = ref_basename.split('-').next() {
                let ref_hash = ref_hash.to_string();
                if !have_set.contains(ref_hash.as_str()) && !seen.contains(&ref_hash) {
                    queue.push_back(ref_hash);
                }
            }
        }

        needed.push(info);

        if needed.len() >= limit {
            let has_more = !queue.is_empty();
            return Ok(SmartResponse::PathSet {
                paths: needed,
                has_more,
            });
        }
    }

    Ok(SmartResponse::PathSet {
        paths: needed,
        has_more: false,
    })
}

// ============================================================================
// Upload staging operations
// ============================================================================

/// Stage a narinfo in the pending_upload table
pub async fn stage_pending_narinfo(
    db: &D1Database,
    session_id: &str,
    info: &StorePathInfoCompact,
) -> Result<()> {
    let store_hash = extract_store_hash(&info.store_path).unwrap_or_default();
    let references_str = info.references.join(" ");
    let signatures_str = info.signatures.join(",");

    let stmt = db
        .prepare(
            "INSERT OR REPLACE INTO pending_upload \
             (session_id, store_hash, store_path, nar_hash_hex, nar_size, \
              references_basenames, deriver, signatures, blake3_hex) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&[
            session_id.into(),
            store_hash.into(),
            info.store_path.as_str().into(),
            info.nar_hash.as_str().into(),
            (info.nar_size as f64).into(),
            references_str.as_str().into(),
            info.deriver.as_deref().unwrap_or("").into(),
            signatures_str.as_str().into(),
            info.blake3.as_deref().unwrap_or("").into(),
        ])?;

    stmt.run().await?;
    Ok(())
}

/// Get all pending narinfos for a session
pub async fn get_pending_narinfos(
    db: &D1Database,
    session_id: &str,
) -> Result<Vec<StorePathInfoCompact>> {
    let stmt = db
        .prepare(
            "SELECT store_path, nar_hash_hex, nar_size, references_basenames, \
             deriver, signatures, blake3_hex \
             FROM pending_upload WHERE session_id = ?1",
        )
        .bind(&[session_id.into()])?;

    let results = stmt.all().await?;
    let rows: Vec<D1NarinfoRow> = results.results()?;

    Ok(rows.into_iter().map(|row| row.into_compact("")).collect())
}

/// Atomically move pending narinfos to the live narinfo table.
///
/// This is the commit step: narinfos become visible to readers only after
/// this succeeds. Uses D1 batch to run INSERT + DELETE atomically.
pub async fn commit_pending_narinfos(db: &D1Database, session_id: &str) -> Result<()> {
    // D1 batch executes all statements in a single transaction
    let insert_stmt = db
        .prepare(
            "INSERT OR REPLACE INTO narinfo \
             (store_hash, store_path, nar_hash_hex, nar_size, \
              references_basenames, deriver, signatures, blake3_hex) \
             SELECT store_hash, store_path, nar_hash_hex, nar_size, \
                    references_basenames, deriver, signatures, blake3_hex \
             FROM pending_upload WHERE session_id = ?1",
        )
        .bind(&[session_id.into()])?;

    let delete_stmt = db
        .prepare("DELETE FROM pending_upload WHERE session_id = ?1")
        .bind(&[session_id.into()])?;

    // D1 batch runs as a single transaction
    db.batch(vec![insert_stmt, delete_stmt]).await?;

    Ok(())
}

/// Clean up stale pending uploads older than max_age_seconds
pub async fn cleanup_stale_pending(db: &D1Database, max_age_seconds: u64) -> Result<u64> {
    let stmt = db
        .prepare("DELETE FROM pending_upload WHERE created_at < unixepoch() - ?1")
        .bind(&[(max_age_seconds as f64).into()])?;

    let result = stmt.run().await?;
    Ok(result.meta().and_then(|m| m.changed_db).unwrap_or(false) as u64)
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Extract store hash from a full store path like /nix/store/<hash>-name
fn extract_store_hash(store_path: &str) -> Option<&str> {
    store_path
        .strip_prefix("/nix/store/")
        .and_then(|s| s.split('-').next())
}

/// D1 row representation for narinfo
#[derive(serde::Deserialize)]
struct D1NarinfoRow {
    store_path: String,
    nar_hash_hex: String,
    nar_size: f64,
    references_basenames: String,
    deriver: Option<String>,
    signatures: String,
    blake3_hex: Option<String>,
}

impl D1NarinfoRow {
    fn into_compact(self, _store_hash: &str) -> StorePathInfoCompact {
        let references: Vec<String> = self
            .references_basenames
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        let signatures: Vec<String> = self
            .signatures
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        let deriver = self.deriver.filter(|d| !d.is_empty());
        let blake3 = self.blake3_hex.filter(|b| !b.is_empty());

        StorePathInfoCompact {
            store_path: self.store_path,
            nar_hash: format!("sha256:{}", self.nar_hash_hex),
            nar_size: self.nar_size as u64,
            references,
            deriver,
            signatures,
            blake3,
        }
    }
}
