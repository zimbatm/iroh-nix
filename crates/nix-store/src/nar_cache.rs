//! On-disk NAR cache
//!
//! Stores fetched NAR files as `<cache_dir>/nar/<blake3_hex>.nar` so that
//! subsequent requests can be served from disk without re-fetching from
//! upstream HTTP caches.
//!
//! Supports an optional maximum size with LRU eviction based on file mtime.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{debug, info, warn};

use crate::hash_index::Blake3Hash;
use crate::Result;

/// On-disk cache of NAR files, keyed by BLAKE3 hash.
///
/// When `max_size > 0`, the cache evicts least-recently-used files (by mtime)
/// after each `put()` to stay within the size budget.
#[derive(Clone)]
pub struct NarCache {
    nar_dir: PathBuf,
    /// Maximum total size in bytes. 0 means unlimited.
    max_size: u64,
}

impl NarCache {
    /// Create a new NarCache under `cache_dir/nar/`.
    ///
    /// `max_size` is the maximum total size in bytes (0 = unlimited).
    /// Creates the directory if it does not exist.
    pub fn new(cache_dir: &Path, max_size: u64) -> Result<Self> {
        let nar_dir = cache_dir.join("nar");
        std::fs::create_dir_all(&nar_dir)?;
        Ok(Self { nar_dir, max_size })
    }

    /// Read a cached NAR by its BLAKE3 hash. Returns `None` if not cached.
    ///
    /// Touches the file mtime to mark it as recently used for LRU eviction.
    pub fn get(&self, blake3: &Blake3Hash) -> Result<Option<Vec<u8>>> {
        let path = self.nar_path(blake3);
        match std::fs::read(&path) {
            Ok(data) => {
                debug!("NAR cache hit: {}", blake3.to_hex());
                // Touch mtime so this file is considered recently used
                if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                    let _ = file.set_modified(SystemTime::now());
                }
                Ok(Some(data))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write NAR data to the cache (atomic: write to .tmp then rename).
    ///
    /// After writing, runs LRU eviction if the cache exceeds `max_size`.
    pub fn put(&self, blake3: &Blake3Hash, data: &[u8]) -> Result<()> {
        let final_path = self.nar_path(blake3);
        let tmp_path = self.nar_dir.join(format!("{}.nar.tmp", blake3.to_hex()));

        std::fs::write(&tmp_path, data)?;
        std::fs::rename(&tmp_path, &final_path)?;
        debug!("NAR cache put: {} ({} bytes)", blake3.to_hex(), data.len());

        self.evict_if_needed();
        Ok(())
    }

    /// Remove a cached NAR. Returns true if the file existed.
    pub fn remove(&self, blake3: &Blake3Hash) -> bool {
        let path = self.nar_path(blake3);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                debug!("NAR cache remove: {}", blake3.to_hex());
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                warn!("NAR cache remove failed for {}: {}", blake3.to_hex(), e);
                false
            }
        }
    }

    /// Check if a NAR is cached.
    pub fn contains(&self, blake3: &Blake3Hash) -> bool {
        self.nar_path(blake3).exists()
    }

    /// List all BLAKE3 hashes currently in the cache.
    pub fn list_cached_hashes(&self) -> Vec<Blake3Hash> {
        let mut hashes = Vec::new();
        let entries = match std::fs::read_dir(&self.nar_dir) {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to list NAR cache directory: {}", e);
                return hashes;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(hex) = name.strip_suffix(".nar") {
                if let Ok(hash) = Blake3Hash::from_hex(hex) {
                    hashes.push(hash);
                }
            }
        }
        hashes
    }

    /// Return the total size in bytes of all `.nar` files in the cache.
    pub fn total_size(&self) -> u64 {
        let entries = match std::fs::read_dir(&self.nar_dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };

        let mut total = 0u64;
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".nar") {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// Evict oldest files (by mtime) until total size is within `max_size`.
    ///
    /// Does nothing if `max_size` is 0 (unlimited).
    fn evict_if_needed(&self) {
        if self.max_size == 0 {
            return;
        }

        let entries = match std::fs::read_dir(&self.nar_dir) {
            Ok(entries) => entries,
            Err(e) => {
                warn!("NAR cache eviction: failed to read dir: {}", e);
                return;
            }
        };

        // Collect (path, size, mtime) for each .nar file
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let mut total_size = 0u64;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".nar") {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total_size += size;
            files.push((entry.path(), size, mtime));
        }

        if total_size <= self.max_size {
            return;
        }

        // Sort oldest-first (smallest mtime first)
        files.sort_by_key(|&(_, _, mtime)| mtime);

        let mut evicted_count = 0usize;
        let mut evicted_bytes = 0u64;

        for (path, size, _) in &files {
            if total_size <= self.max_size {
                break;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {
                    total_size -= size;
                    evicted_count += 1;
                    evicted_bytes += size;
                }
                Err(e) => {
                    warn!("NAR cache eviction: failed to remove {:?}: {}", path, e);
                }
            }
        }

        if evicted_count > 0 {
            info!(
                "NAR cache eviction: removed {} files ({} bytes), cache now {} bytes",
                evicted_count, evicted_bytes, total_size
            );
        }
    }

    fn nar_path(&self, blake3: &Blake3Hash) -> PathBuf {
        self.nar_dir.join(format!("{}.nar", blake3.to_hex()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_blake3(byte: u8) -> Blake3Hash {
        Blake3Hash([byte; 32])
    }

    #[test]
    fn test_put_get_remove() {
        let tmp = TempDir::new().unwrap();
        let cache = NarCache::new(tmp.path(), 0).unwrap();

        let hash = test_blake3(0xAA);
        let data = b"fake nar data";

        // Initially absent
        assert!(!cache.contains(&hash));
        assert!(cache.get(&hash).unwrap().is_none());

        // Put
        cache.put(&hash, data).unwrap();
        assert!(cache.contains(&hash));

        // Get
        let got = cache.get(&hash).unwrap().unwrap();
        assert_eq!(got, data);

        // Remove
        assert!(cache.remove(&hash));
        assert!(!cache.contains(&hash));
        assert!(!cache.remove(&hash)); // already gone
    }

    #[test]
    fn test_list_cached_hashes() {
        let tmp = TempDir::new().unwrap();
        let cache = NarCache::new(tmp.path(), 0).unwrap();

        let h1 = test_blake3(0x01);
        let h2 = test_blake3(0x02);

        cache.put(&h1, b"nar1").unwrap();
        cache.put(&h2, b"nar2").unwrap();

        let mut listed = cache.list_cached_hashes();
        listed.sort_by_key(|h| h.0);

        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], h1);
        assert_eq!(listed[1], h2);
    }

    #[test]
    fn test_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let cache = NarCache::new(tmp.path(), 0).unwrap();

        let hash = test_blake3(0xBB);
        cache.put(&hash, b"first").unwrap();
        cache.put(&hash, b"second").unwrap();

        let got = cache.get(&hash).unwrap().unwrap();
        assert_eq!(got, b"second");
    }

    #[test]
    fn test_total_size() {
        let tmp = TempDir::new().unwrap();
        let cache = NarCache::new(tmp.path(), 0).unwrap();

        assert_eq!(cache.total_size(), 0);

        cache.put(&test_blake3(0x01), b"aaaa").unwrap(); // 4 bytes
        cache.put(&test_blake3(0x02), b"bbbbbb").unwrap(); // 6 bytes

        assert_eq!(cache.total_size(), 10);
    }

    #[test]
    fn test_eviction_removes_oldest() {
        let tmp = TempDir::new().unwrap();
        // max_size = 15 bytes
        let cache = NarCache::new(tmp.path(), 15).unwrap();

        let h1 = test_blake3(0x01);
        let h2 = test_blake3(0x02);
        let h3 = test_blake3(0x03);

        // Put 3 files: 6 + 6 + 6 = 18 bytes, exceeds 15
        // We need distinct mtimes so the eviction order is deterministic.
        // Put h1 as the oldest entry (6 bytes).
        // Set explicit mtimes so eviction order is deterministic.
        cache.put(&h1, b"aaaaaa").unwrap();
        let path1 = cache.nar_path(&h1);
        let old_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let file1 = std::fs::File::options().write(true).open(&path1).unwrap();
        file1.set_modified(old_time).unwrap();

        cache.put(&h2, b"bbbbbb").unwrap(); // 6 bytes
        let path2 = cache.nar_path(&h2);
        let mid_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        let file2 = std::fs::File::options().write(true).open(&path2).unwrap();
        file2.set_modified(mid_time).unwrap();

        // This put brings total to 18, triggers eviction which should remove h1 (oldest)
        cache.put(&h3, b"cccccc").unwrap();

        // h1 should be evicted (oldest mtime)
        assert!(!cache.contains(&h1), "oldest entry should be evicted");
        // h2 and h3 should remain (12 bytes <= 15)
        assert!(cache.contains(&h2));
        assert!(cache.contains(&h3));
        assert!(cache.total_size() <= 15);
    }

    #[test]
    fn test_get_updates_mtime_for_lru() {
        let tmp = TempDir::new().unwrap();
        // max_size = 15 bytes
        let cache = NarCache::new(tmp.path(), 15).unwrap();

        let h1 = test_blake3(0x01);
        let h2 = test_blake3(0x02);

        cache.put(&h1, b"aaaaaa").unwrap(); // 6 bytes
        cache.put(&h2, b"bbbbbb").unwrap(); // 6 bytes

        // Set h1 to an old mtime
        let path1 = cache.nar_path(&h1);
        let old_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let file1 = std::fs::File::options().write(true).open(&path1).unwrap();
        file1.set_modified(old_time).unwrap();

        // Set h2 to a mid mtime
        let path2 = cache.nar_path(&h2);
        let mid_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        let file2 = std::fs::File::options().write(true).open(&path2).unwrap();
        file2.set_modified(mid_time).unwrap();

        // Access h1 via get() -- this should update its mtime to now
        let _ = cache.get(&h1).unwrap();

        // Now add h3 which pushes total to 18, triggering eviction
        let h3 = test_blake3(0x03);
        cache.put(&h3, b"cccccc").unwrap();

        // h2 should be evicted (it now has the oldest mtime)
        // h1 should survive (its mtime was refreshed by get())
        assert!(
            cache.contains(&h1),
            "recently accessed entry should survive"
        );
        assert!(!cache.contains(&h2), "stale entry should be evicted");
        assert!(cache.contains(&h3));
    }

    #[test]
    fn test_unlimited_max_size_no_eviction() {
        let tmp = TempDir::new().unwrap();
        // max_size = 0 means unlimited
        let cache = NarCache::new(tmp.path(), 0).unwrap();

        // Put a lot of data -- nothing should be evicted
        for i in 0..20u8 {
            cache.put(&test_blake3(i), &vec![i; 100]).unwrap();
        }

        assert_eq!(cache.list_cached_hashes().len(), 20);
        assert_eq!(cache.total_size(), 2000);
    }
}
