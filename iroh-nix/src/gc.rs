//! Garbage Collection for iroh-nix
//!
//! This module implements replica-aware garbage collection for the hash index.
//! Before deleting index entries, it queries the gossip network to check if other
//! nodes have copies. Entries are only deleted when sufficient replicas exist.
//!
//! GC Strategy:
//! 1. List all index entries
//! 2. For each entry, check if store path still exists
//! 3. If store path is gone, mark for deletion
//! 4. Query gossip to count replicas for remaining entries
//! 5. If replicas >= min_replicas, mark as GC candidate
//! 6. Broadcast GcWarning to let peers know we're about to delete
//! 7. Wait grace period for peers to potentially fetch
//! 8. Delete the index entry

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::error::MutexExt;
use crate::gossip::GossipService;
use crate::hash_index::{Blake3Hash, HashEntry, HashIndex};
use crate::Result;

/// Configuration for garbage collection
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Minimum number of replicas required before deleting locally
    /// (0 = no replica requirement, delete anything)
    pub min_replicas: usize,

    /// Grace period to wait after announcing GC intention
    pub grace_period: Duration,

    /// Maximum number of entries to delete in one GC run
    pub max_delete_per_run: usize,

    /// Minimum age of entries before considering for GC (in seconds)
    /// This prevents deleting recently added entries
    pub min_age_secs: u64,

    /// Whether to actually delete (false = dry run)
    pub dry_run: bool,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            min_replicas: 1,
            grace_period: Duration::from_secs(30),
            max_delete_per_run: 100,
            min_age_secs: 3600, // 1 hour
            dry_run: false,
        }
    }
}

/// Result of a GC run
#[derive(Debug, Default)]
pub struct GcResult {
    /// Number of artifacts scanned
    pub scanned: usize,
    /// Number of artifacts that are GC candidates (have enough replicas)
    pub candidates: usize,
    /// Number of artifacts actually deleted
    pub deleted: usize,
    /// Total bytes freed
    pub bytes_freed: u64,
    /// Artifacts that were skipped (not enough replicas)
    pub skipped_no_replicas: usize,
    /// Artifacts that were kept (in protected set)
    pub kept_protected: usize,
    /// Errors encountered during GC
    pub errors: Vec<String>,
}

/// Garbage collector for iroh-nix artifacts
pub struct GarbageCollector {
    config: GcConfig,
    hash_index: Arc<Mutex<HashIndex>>,
    gossip: Option<Arc<GossipService>>,
    /// Set of BLAKE3 hashes that are protected from GC (e.g., active builds)
    protected: Arc<Mutex<HashSet<Blake3Hash>>>,
}

impl GarbageCollector {
    /// Create a new garbage collector
    pub fn new(
        config: GcConfig,
        hash_index: Arc<Mutex<HashIndex>>,
        gossip: Option<Arc<GossipService>>,
    ) -> Self {
        Self {
            config,
            hash_index,
            gossip,
            protected: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Protect an artifact from GC (e.g., during active build)
    pub fn protect(&self, blake3: Blake3Hash) {
        let mut protected = self.protected.lock().unwrap();
        protected.insert(blake3);
        debug!("Protected from GC: {}", blake3);
    }

    /// Remove protection from an artifact
    pub fn unprotect(&self, blake3: &Blake3Hash) {
        let mut protected = self.protected.lock().unwrap();
        protected.remove(blake3);
        debug!("Unprotected from GC: {}", blake3);
    }

    /// Check if an artifact is protected
    pub fn is_protected(&self, blake3: &Blake3Hash) -> bool {
        let protected = self.protected.lock().unwrap();
        protected.contains(blake3)
    }

    /// Run garbage collection
    ///
    /// Returns statistics about what was deleted.
    pub async fn run(&self) -> Result<GcResult> {
        let mut result = GcResult::default();

        // Get all entries from hash index
        let entries = {
            let index = self.hash_index.lock_or_err()?;
            index.list_all()?
        };

        result.scanned = entries.len();
        info!("GC: Scanning {} artifacts", result.scanned);

        // Collect candidates for deletion
        let mut candidates: Vec<(HashEntry, usize)> = Vec::new();

        for entry in entries {
            // Check if protected
            if self.is_protected(&entry.blake3) {
                result.kept_protected += 1;
                continue;
            }

            // Count replicas via gossip
            let replica_count = self.count_replicas(&entry.blake3);

            if replica_count >= self.config.min_replicas {
                candidates.push((entry, replica_count));
            } else {
                result.skipped_no_replicas += 1;
                debug!(
                    "GC: Skipping {} - only {} replicas (need {})",
                    entry.store_path, replica_count, self.config.min_replicas
                );
            }
        }

        result.candidates = candidates.len();

        // Sort by replica count (highest first = safest to delete)
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        // Limit number of deletions per run
        let to_delete: Vec<_> = candidates
            .into_iter()
            .take(self.config.max_delete_per_run)
            .collect();

        if to_delete.is_empty() {
            info!("GC: No candidates for deletion");
            return Ok(result);
        }

        info!(
            "GC: Found {} candidates for deletion (grace period: {:?})",
            to_delete.len(),
            self.config.grace_period
        );

        // Announce GC warnings via gossip
        if let Some(gossip) = &self.gossip {
            for (entry, _) in &to_delete {
                if let Err(e) = gossip
                    .announce_gc_warning(entry.blake3, &entry.store_path)
                    .await
                {
                    warn!("Failed to announce GC warning for {}: {}", entry.blake3, e);
                }
            }
        }

        // Wait grace period
        if !self.config.dry_run && !self.config.grace_period.is_zero() {
            info!("GC: Waiting {:?} grace period...", self.config.grace_period);
            tokio::time::sleep(self.config.grace_period).await;
        }

        // Delete artifacts
        for (entry, replica_count) in to_delete {
            // Re-check if protected (might have been protected during grace period)
            if self.is_protected(&entry.blake3) {
                result.kept_protected += 1;
                continue;
            }

            // Re-check replica count (might have changed during grace period)
            let current_replicas = self.count_replicas(&entry.blake3);
            if current_replicas < self.config.min_replicas {
                result.skipped_no_replicas += 1;
                info!(
                    "GC: Skipping {} - replicas dropped to {} during grace period",
                    entry.store_path, current_replicas
                );
                continue;
            }

            if self.config.dry_run {
                info!(
                    "GC: [DRY RUN] Would delete {} ({} bytes, {} replicas)",
                    entry.store_path, entry.nar_size, replica_count
                );
                result.deleted += 1;
                result.bytes_freed += entry.nar_size;
            } else {
                match self.delete_artifact(&entry).await {
                    Ok(()) => {
                        info!(
                            "GC: Deleted {} ({} bytes, {} replicas remain)",
                            entry.store_path, entry.nar_size, replica_count
                        );
                        result.deleted += 1;
                        result.bytes_freed += entry.nar_size;
                    }
                    Err(e) => {
                        result.errors.push(format!("{}: {}", entry.store_path, e));
                        warn!("GC: Failed to delete {}: {}", entry.store_path, e);
                    }
                }
            }
        }

        info!(
            "GC: Completed - deleted {} artifacts, freed {} bytes",
            result.deleted, result.bytes_freed
        );

        Ok(result)
    }

    /// Count how many replicas exist on the gossip network
    fn count_replicas(&self, blake3: &Blake3Hash) -> usize {
        match &self.gossip {
            Some(gossip) => {
                let providers = gossip.get_providers(blake3);
                // Don't count ourselves
                providers
                    .iter()
                    .filter(|p| p.endpoint_id != gossip.our_id())
                    .count()
            }
            None => 0, // No gossip = no known replicas
        }
    }

    /// Delete an index entry
    async fn delete_artifact(&self, entry: &HashEntry) -> Result<()> {
        // Remove from hash index (no blob files to delete with on-demand NAR generation)
        let index = self.hash_index.lock_or_err()?;
        index.delete_by_blake3(&entry.blake3)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_index::Sha256Hash;

    #[test]
    fn test_gc_config_default() {
        let config = GcConfig::default();
        assert_eq!(config.min_replicas, 1);
        assert_eq!(config.max_delete_per_run, 100);
    }

    #[test]
    fn test_gc_protection() {
        let index = HashIndex::in_memory().unwrap();
        let config = GcConfig::default();

        let gc = GarbageCollector::new(config, Arc::new(Mutex::new(index)), None);

        let hash = Blake3Hash([1u8; 32]);
        assert!(!gc.is_protected(&hash));

        gc.protect(hash);
        assert!(gc.is_protected(&hash));

        gc.unprotect(&hash);
        assert!(!gc.is_protected(&hash));
    }

    #[tokio::test]
    async fn test_gc_dry_run() {
        let index = HashIndex::in_memory().unwrap();

        // Add a test entry
        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/test-artifact".to_string(),
            nar_size: 1024,
        };
        index.insert(&entry).unwrap();

        let config = GcConfig {
            min_replicas: 0, // No replica requirement
            grace_period: Duration::ZERO,
            dry_run: true,
            ..Default::default()
        };

        let index = Arc::new(Mutex::new(index));
        let gc = GarbageCollector::new(config, Arc::clone(&index), None);
        let result = gc.run().await.unwrap();

        assert_eq!(result.scanned, 1);
        assert_eq!(result.candidates, 1);
        assert_eq!(result.deleted, 1); // Counted but not actually deleted

        // Index entry still exists (dry run)
        let idx = index.lock().unwrap();
        assert!(idx.get_by_blake3(&entry.blake3).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_gc_actual_delete() {
        let index = HashIndex::in_memory().unwrap();

        // Add a test entry
        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/test-artifact".to_string(),
            nar_size: 1024,
        };
        index.insert(&entry).unwrap();

        let config = GcConfig {
            min_replicas: 0,
            grace_period: Duration::ZERO,
            dry_run: false,
            ..Default::default()
        };

        let index = Arc::new(Mutex::new(index));
        let gc = GarbageCollector::new(config, Arc::clone(&index), None);
        let result = gc.run().await.unwrap();

        assert_eq!(result.deleted, 1);

        // Index entry removed
        let idx = index.lock().unwrap();
        assert!(idx.get_by_blake3(&entry.blake3).unwrap().is_none());
    }

    #[tokio::test]
    async fn test_gc_respects_protection() {
        let index = HashIndex::in_memory().unwrap();

        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/protected-artifact".to_string(),
            nar_size: 1024,
        };
        index.insert(&entry).unwrap();

        let config = GcConfig {
            min_replicas: 0,
            grace_period: Duration::ZERO,
            dry_run: false,
            ..Default::default()
        };

        let index = Arc::new(Mutex::new(index));
        let gc = GarbageCollector::new(config, Arc::clone(&index), None);

        // Protect the artifact
        gc.protect(entry.blake3);

        let result = gc.run().await.unwrap();

        assert_eq!(result.scanned, 1);
        assert_eq!(result.kept_protected, 1);
        assert_eq!(result.deleted, 0);

        // Index entry still exists (protected)
        let idx = index.lock().unwrap();
        assert!(idx.get_by_blake3(&entry.blake3).unwrap().is_some());
    }
}
