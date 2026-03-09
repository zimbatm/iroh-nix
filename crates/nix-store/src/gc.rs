//! Stale index cleanup for nix-store
//!
//! Periodically removes hash index entries whose store paths no longer exist on disk.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::MutexExt;
use crate::hash_index::HashIndex;
use crate::nar_cache::NarCache;

/// Run a periodic loop that prunes stale index entries.
///
/// An entry is stale when its store path no longer exists on the filesystem.
pub async fn run_stale_cleanup_loop(
    hash_index: Arc<Mutex<HashIndex>>,
    interval: Duration,
    cancel: CancellationToken,
) {
    info!(
        "Stale index cleanup loop started (interval: {:?})",
        interval
    );

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Stale index cleanup loop stopped");
                return;
            }
            _ = tick.tick() => {
                run_stale_cleanup(&hash_index);
            }
        }
    }
}

/// Remove index entries whose store paths no longer exist on disk.
///
/// Returns the number of entries removed.
pub fn run_stale_cleanup(hash_index: &Arc<Mutex<HashIndex>>) -> usize {
    let entries = match hash_index.lock_or_err().and_then(|idx| idx.list_all()) {
        Ok(e) => e,
        Err(e) => {
            warn!("Stale cleanup: failed to list entries: {}", e);
            return 0;
        }
    };

    let mut removed = 0usize;
    for entry in &entries {
        if !std::path::Path::new(&entry.store_path).exists() {
            debug!("Pruning stale entry: {}", entry.store_path);
            match hash_index
                .lock_or_err()
                .and_then(|idx| idx.delete_by_blake3(&entry.blake3))
            {
                Ok(_) => removed += 1,
                Err(e) => {
                    warn!(
                        "Stale cleanup: failed to delete {}: {}",
                        entry.store_path, e
                    );
                }
            }
        }
    }

    if removed > 0 {
        info!("Stale cleanup: removed {} entries", removed);
    }

    removed
}

/// Remove cached NAR files whose blake3 hashes have no corresponding index entry.
///
/// Returns the number of orphaned files removed.
pub fn run_nar_cache_cleanup(hash_index: &Arc<Mutex<HashIndex>>, nar_cache: &NarCache) -> usize {
    let cached_hashes = nar_cache.list_cached_hashes();
    let mut removed = 0usize;

    for blake3 in &cached_hashes {
        let has_entry = match hash_index
            .lock_or_err()
            .and_then(|idx| idx.get_by_blake3(blake3))
        {
            Ok(entry) => entry.is_some(),
            Err(e) => {
                warn!(
                    "NAR cache cleanup: failed to look up {}: {}",
                    blake3.to_hex(),
                    e
                );
                continue;
            }
        };

        if !has_entry {
            debug!("Removing orphaned NAR cache file: {}", blake3.to_hex());
            if nar_cache.remove(blake3) {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        info!("NAR cache cleanup: removed {} orphaned files", removed);
    }

    removed
}

/// Run a periodic cleanup loop that prunes both stale index entries and orphaned NAR cache files.
pub async fn run_full_cleanup_loop(
    hash_index: Arc<Mutex<HashIndex>>,
    nar_cache: NarCache,
    interval: Duration,
    cancel: CancellationToken,
) {
    info!("Full cleanup loop started (interval: {:?})", interval);

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Full cleanup loop stopped");
                return;
            }
            _ = tick.tick() => {
                run_stale_cleanup(&hash_index);
                run_nar_cache_cleanup(&hash_index, &nar_cache);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_index::{Blake3Hash, HashEntry, Sha256Hash};

    #[test]
    fn test_stale_cleanup_removes_missing_paths() {
        let index = HashIndex::in_memory().unwrap();

        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/does-not-exist-test".to_string(),
            nar_size: 1024,
            references: vec![],
            deriver: None,
        };
        index.insert(&entry).unwrap();

        let hash_index = Arc::new(Mutex::new(index));
        let removed = run_stale_cleanup(&hash_index);

        assert_eq!(removed, 1);

        let idx = hash_index.lock().unwrap();
        assert!(idx.get_by_blake3(&entry.blake3).unwrap().is_none());
    }

    #[test]
    fn test_nar_cache_cleanup_removes_orphaned_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = HashIndex::in_memory().unwrap();

        // Insert an entry in the index
        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/has-index-entry-test".to_string(),
            nar_size: 100,
            references: vec![],
            deriver: None,
        };
        index.insert(&entry).unwrap();

        let hash_index = Arc::new(Mutex::new(index));
        let nar_cache = crate::NarCache::new(tmp.path(), 0).unwrap();

        // Put two NARs in the cache: one with an index entry, one orphaned
        nar_cache.put(&Blake3Hash([1u8; 32]), b"has entry").unwrap();
        nar_cache.put(&Blake3Hash([9u8; 32]), b"orphaned").unwrap();

        let removed = run_nar_cache_cleanup(&hash_index, &nar_cache);
        assert_eq!(removed, 1);

        // The one with an index entry should still be there
        assert!(nar_cache.contains(&Blake3Hash([1u8; 32])));
        // The orphaned one should be gone
        assert!(!nar_cache.contains(&Blake3Hash([9u8; 32])));
    }

    #[tokio::test]
    async fn test_cleanup_loop_cancellation() {
        let index = HashIndex::in_memory().unwrap();
        let hash_index = Arc::new(Mutex::new(index));
        let cancel = CancellationToken::new();

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(run_stale_cleanup_loop(
            hash_index,
            Duration::from_secs(3600),
            cancel_clone,
        ));

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cleanup loop should stop promptly")
            .expect("task should not panic");
    }
}
