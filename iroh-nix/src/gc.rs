//! Stale index cleanup for iroh-nix
//!
//! Periodically removes hash index entries whose store paths no longer exist on disk.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::MutexExt;
use crate::hash_index::HashIndex;

/// Run a periodic loop that prunes stale index entries.
///
/// An entry is stale when its store path no longer exists on the filesystem.
/// This replaces the old replica-aware GC with a simpler in-process cleanup.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_index::{Blake3Hash, HashEntry, Sha256Hash};

    #[test]
    fn test_stale_cleanup_removes_missing_paths() {
        let index = HashIndex::in_memory().unwrap();

        // Add an entry with a non-existent store path
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

        // Entry should be gone
        let idx = hash_index.lock().unwrap();
        assert!(idx.get_by_blake3(&entry.blake3).unwrap().is_none());
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

        // Cancel immediately
        cancel.cancel();

        // Should complete without hanging
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cleanup loop should stop promptly")
            .expect("task should not panic");
    }
}
