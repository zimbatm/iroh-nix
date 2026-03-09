//! Core store abstractions for Nix binary caches
//!
//! Defines the trait hierarchy that all store backends implement:
//! - `NarInfoProvider` -- read-only store interface
//! - `NarInfoIndex` -- extends provider with write operations
//! - `ContentFilter` -- decides which paths to expose
//!
//! When the `sqlite` feature is enabled, provides `LocalStore` and `PullThroughStore`
//! implementations backed by `HashIndex`.

use crate::error::MutexExt;
use crate::hash_index::{Blake3Hash, Sha256Hash};
use crate::Result;

/// Unified store path info (replaces HashEntry / CacheNarInfo / NarInfoResponse)
#[derive(Debug, Clone)]
pub struct StorePathInfo {
    /// Full store path (e.g., /nix/store/abc123...-name)
    pub store_path: String,
    /// BLAKE3 hash of the NAR content (may be zeroed if unknown)
    pub blake3: Blake3Hash,
    /// SHA256 hash of the NAR content (used by Nix)
    pub nar_hash: Sha256Hash,
    /// Size of the NAR in bytes
    pub nar_size: u64,
    /// Runtime dependencies (full store paths)
    pub references: Vec<String>,
    /// Deriver path (optional, full store path to .drv)
    pub deriver: Option<String>,
    /// Signatures
    pub signatures: Vec<String>,
}

/// Read-only store interface -- the core abstraction for any Nix binary cache
#[async_trait::async_trait]
pub trait NarInfoProvider: Send + Sync {
    /// Look up narinfo by store hash (the 32-char prefix from /nix/store/<hash>-name)
    async fn get_narinfo(&self, store_hash: &str) -> Result<Option<StorePathInfo>>;

    /// Get NAR data for a store path
    async fn get_nar(&self, store_path: &str) -> Result<Option<Vec<u8>>>;

    /// Get NAR data by BLAKE3 hash (for serving /nar/<blake3>.nar requests)
    async fn get_nar_by_blake3(&self, _blake3: &Blake3Hash) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// Extends `NarInfoProvider` with write operations for indexing
#[async_trait::async_trait]
pub trait NarInfoIndex: NarInfoProvider {
    /// Index a store path info
    async fn index_path(&self, info: &StorePathInfo) -> Result<()>;

    /// Remove a store path from the index by store hash
    async fn remove_path(&self, store_hash: &str) -> Result<bool>;
}

/// Compute the closure delta: BFS over store hashes, collecting paths
/// that are needed (in `wants` closure) but not already in `haves`.
///
/// This is the shared implementation used by all `SmartProvider` backends.
pub async fn resolve_closure_bfs(
    provider: &dyn NarInfoProvider,
    wants: &[String],
    haves: &[String],
    limit: Option<u32>,
) -> Result<(Vec<StorePathInfo>, bool)> {
    use std::collections::{HashSet, VecDeque};

    let have_set: HashSet<&str> = haves.iter().map(|s| s.as_str()).collect();
    let mut needed: Vec<StorePathInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let limit = limit.unwrap_or(u32::MAX) as usize;

    for want in wants {
        queue.push_back(want.clone());
    }

    while let Some(hash) = queue.pop_front() {
        if have_set.contains(hash.as_str()) || seen.contains(&hash) {
            continue;
        }
        seen.insert(hash.clone());

        let info = match provider.get_narinfo(&hash).await? {
            Some(info) => info,
            None => continue,
        };

        for ref_path in &info.references {
            if let Some(ref_hash) = crate::hash_index::extract_store_hash(ref_path) {
                if !have_set.contains(ref_hash.as_str()) && !seen.contains(&ref_hash) {
                    queue.push_back(ref_hash);
                }
            }
        }

        needed.push(info);

        if needed.len() >= limit {
            let has_more = !queue.is_empty();
            return Ok((needed, has_more));
        }
    }

    Ok((needed, false))
}

/// Decides which store paths to expose through a binary cache
pub trait ContentFilter: Send + Sync {
    /// Return true if this narinfo should be served
    fn allow_narinfo(&self, info: &StorePathInfo) -> bool;
}

/// Allow all store paths (no filtering)
pub struct AllowAll;

impl ContentFilter for AllowAll {
    fn allow_narinfo(&self, _info: &StorePathInfo) -> bool {
        true
    }
}

// ============================================================================
// SQLite-backed implementations (require "sqlite" + "runtime" features)
// ============================================================================

#[cfg(feature = "sqlite")]
mod sqlite_impls {
    use super::*;

    use crate::hash_index::extract_store_hash;
    use crate::smart::SmartProvider;

    /// Filter by trusted signing keys
    pub struct SignatureFilter {
        pub trusted_keys: Vec<String>,
    }

    impl ContentFilter for SignatureFilter {
        fn allow_narinfo(&self, info: &StorePathInfo) -> bool {
            if self.trusted_keys.is_empty() {
                return true;
            }
            info.signatures
                .iter()
                .any(|sig| self.trusted_keys.iter().any(|key| sig.starts_with(key)))
        }
    }

    /// A local store backed by HashIndex + on-demand NAR generation
    pub struct LocalStore {
        hash_index: std::sync::Arc<std::sync::Mutex<crate::hash_index::HashIndex>>,
    }

    impl LocalStore {
        /// Create a new LocalStore wrapping a HashIndex
        pub fn new(
            hash_index: std::sync::Arc<std::sync::Mutex<crate::hash_index::HashIndex>>,
        ) -> Self {
            Self { hash_index }
        }
    }

    #[async_trait::async_trait]
    impl NarInfoProvider for LocalStore {
        async fn get_narinfo(&self, store_hash: &str) -> Result<Option<StorePathInfo>> {
            let index = self
                .hash_index
                .lock_or_err()?;
            let entry = index.get_by_store_hash(store_hash)?;
            Ok(entry.map(|e| StorePathInfo {
                store_path: e.store_path,
                blake3: e.blake3,
                nar_hash: e.sha256,
                nar_size: e.nar_size,
                references: e.references,
                deriver: e.deriver,
                signatures: vec![],
            }))
        }

        async fn get_nar(&self, store_path: &str) -> Result<Option<Vec<u8>>> {
            let path = std::path::Path::new(store_path);
            if !path.exists() {
                return Ok(None);
            }
            let store_path = store_path.to_string();
            let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let (data, _info) = crate::nar::serialize_path(std::path::Path::new(&store_path))?;
                Ok(data)
            })
            .await
            .map_err(|e| crate::Error::Internal(format!("spawn_blocking failed: {}", e)))??;
            Ok(Some(data))
        }

        async fn get_nar_by_blake3(&self, blake3: &Blake3Hash) -> Result<Option<Vec<u8>>> {
            let store_path = {
                let index = self
                    .hash_index
                    .lock()
                    .map_err(|_| crate::Error::Internal("mutex poisoned".into()))?;
                match index.get_by_blake3(blake3)? {
                    Some(entry) => entry.store_path,
                    None => return Ok(None),
                }
            };
            self.get_nar(&store_path).await
        }
    }

    #[async_trait::async_trait]
    impl NarInfoIndex for LocalStore {
        async fn index_path(&self, info: &StorePathInfo) -> Result<()> {
            let entry = crate::hash_index::HashEntry {
                blake3: info.blake3,
                sha256: info.nar_hash,
                store_path: info.store_path.clone(),
                nar_size: info.nar_size,
                references: info.references.clone(),
                deriver: info.deriver.clone(),
            };
            let index = self
                .hash_index
                .lock_or_err()?;
            index.insert(&entry)
        }

        async fn remove_path(&self, store_hash: &str) -> Result<bool> {
            let index = self
                .hash_index
                .lock_or_err()?;
            let entry = index.get_by_store_hash(store_hash)?;
            match entry {
                Some(e) => index.delete_by_blake3(&e.blake3),
                None => Ok(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl SmartProvider for LocalStore {
        async fn batch_narinfo(
            &self,
            store_hashes: &[String],
        ) -> Result<Vec<Option<StorePathInfo>>> {
            let index = self
                .hash_index
                .lock_or_err()?;
            let hash_refs: Vec<&str> = store_hashes.iter().map(|s| s.as_str()).collect();
            let entries = index.get_batch_by_store_hash(&hash_refs)?;
            Ok(entries
                .into_iter()
                .map(|opt| {
                    opt.map(|e| StorePathInfo {
                        store_path: e.store_path,
                        blake3: e.blake3,
                        nar_hash: e.sha256,
                        nar_size: e.nar_size,
                        references: e.references,
                        deriver: e.deriver,
                        signatures: vec![],
                    })
                })
                .collect())
        }

        async fn resolve_closure(
            &self,
            wants: &[String],
            haves: &[String],
            limit: Option<u32>,
        ) -> Result<(Vec<StorePathInfo>, bool)> {
            resolve_closure_bfs(self, wants, haves, limit).await
        }
    }

    /// A pull-through cache that checks local first, falls through to upstreams
    pub struct PullThroughStore {
        local: std::sync::Arc<dyn NarInfoIndex>,
        upstreams: Vec<std::sync::Arc<dyn NarInfoProvider>>,
        nar_cache: Option<crate::nar_cache::NarCache>,
    }

    impl PullThroughStore {
        /// Create a new pull-through store (without on-disk NAR cache)
        pub fn new(
            local: std::sync::Arc<dyn NarInfoIndex>,
            upstreams: Vec<std::sync::Arc<dyn NarInfoProvider>>,
        ) -> Self {
            Self {
                local,
                upstreams,
                nar_cache: None,
            }
        }

        /// Create a pull-through store with an on-disk NAR cache
        pub fn with_nar_cache(
            local: std::sync::Arc<dyn NarInfoIndex>,
            upstreams: Vec<std::sync::Arc<dyn NarInfoProvider>>,
            nar_cache: crate::nar_cache::NarCache,
        ) -> Self {
            Self {
                local,
                upstreams,
                nar_cache: Some(nar_cache),
            }
        }
    }

    #[async_trait::async_trait]
    impl NarInfoProvider for PullThroughStore {
        async fn get_narinfo(&self, store_hash: &str) -> Result<Option<StorePathInfo>> {
            if let Some(info) = self.local.get_narinfo(store_hash).await? {
                return Ok(Some(info));
            }

            for upstream in &self.upstreams {
                if let Some(info) = upstream.get_narinfo(store_hash).await? {
                    let _ = self.local.index_path(&info).await;
                    return Ok(Some(info));
                }
            }

            Ok(None)
        }

        async fn get_nar(&self, store_path: &str) -> Result<Option<Vec<u8>>> {
            if let Some(data) = self.local.get_nar(store_path).await? {
                return Ok(Some(data));
            }

            if let Some(ref nar_cache) = self.nar_cache {
                if let Some(store_hash) = extract_store_hash(store_path) {
                    if let Some(info) = self.local.get_narinfo(&store_hash).await? {
                        if info.blake3.0 != [0u8; 32] {
                            if let Some(data) = nar_cache.get(&info.blake3)? {
                                return Ok(Some(data));
                            }
                        }
                    }
                }
            }

            for upstream in &self.upstreams {
                if let Some(data) = upstream.get_nar(store_path).await? {
                    if let Some(ref nar_cache) = self.nar_cache {
                        let blake3_hash = blake3::hash(&data);
                        let blake3 = Blake3Hash(*blake3_hash.as_bytes());

                        if let Err(e) = nar_cache.put(&blake3, &data) {
                            tracing::warn!("Failed to cache NAR: {}", e);
                        }

                        if let Some(store_hash) = extract_store_hash(store_path) {
                            if let Ok(Some(mut info)) = self.local.get_narinfo(&store_hash).await {
                                if info.blake3.0 == [0u8; 32] {
                                    info.blake3 = blake3;
                                    let _ = self.local.index_path(&info).await;
                                }
                            }
                        }
                    }
                    return Ok(Some(data));
                }
            }

            Ok(None)
        }

        async fn get_nar_by_blake3(&self, blake3: &Blake3Hash) -> Result<Option<Vec<u8>>> {
            if let Some(data) = self.local.get_nar_by_blake3(blake3).await? {
                return Ok(Some(data));
            }

            if let Some(ref nar_cache) = self.nar_cache {
                if let Some(data) = nar_cache.get(blake3)? {
                    return Ok(Some(data));
                }
            }

            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl SmartProvider for PullThroughStore {
        async fn batch_narinfo(
            &self,
            store_hashes: &[String],
        ) -> Result<Vec<Option<StorePathInfo>>> {
            let mut results = Vec::with_capacity(store_hashes.len());
            for hash in store_hashes {
                results.push(self.get_narinfo(hash).await?);
            }
            Ok(results)
        }

        async fn resolve_closure(
            &self,
            wants: &[String],
            haves: &[String],
            limit: Option<u32>,
        ) -> Result<(Vec<StorePathInfo>, bool)> {
            resolve_closure_bfs(self, wants, haves, limit).await
        }
    }
}

#[cfg(feature = "sqlite")]
pub use sqlite_impls::{LocalStore, PullThroughStore, SignatureFilter};

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::hash_index::{Blake3Hash, HashIndex, Sha256Hash};
    use std::sync::{Arc, Mutex};

    /// Generate a deterministic 32-char nix-base32 store hash from a name byte.
    fn store_hash_for(byte: u8) -> String {
        let chars = b"0123456789abcdfghijklmnpqrsvwxyz";
        let c = chars[(byte % 32) as usize] as char;
        std::iter::repeat(c).take(32).collect()
    }

    fn make_info(name: &str, refs: &[&str]) -> StorePathInfo {
        let hash_byte = name.as_bytes()[0];
        let hash_str = store_hash_for(hash_byte);
        StorePathInfo {
            store_path: format!("/nix/store/{}-{}", hash_str, name),
            blake3: Blake3Hash([hash_byte; 32]),
            nar_hash: Sha256Hash([hash_byte; 32]),
            nar_size: 1000,
            references: refs
                .iter()
                .map(|r| {
                    let rh = store_hash_for(r.as_bytes()[0]);
                    format!("/nix/store/{}-{}", rh, r)
                })
                .collect(),
            deriver: None,
            signatures: vec![],
        }
    }

    fn setup_local_store(entries: &[StorePathInfo]) -> Arc<LocalStore> {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = HashIndex::open(tmp.path().join("test.db")).unwrap();
        for info in entries {
            let entry = crate::hash_index::HashEntry {
                blake3: info.blake3,
                sha256: info.nar_hash,
                store_path: info.store_path.clone(),
                nar_size: info.nar_size,
                references: info.references.clone(),
                deriver: info.deriver.clone(),
            };
            index.insert(&entry).unwrap();
        }
        // Keep TempDir alive by leaking it (test only)
        std::mem::forget(tmp);
        Arc::new(LocalStore::new(Arc::new(Mutex::new(index))))
    }

    #[tokio::test]
    async fn test_pull_through_narinfo_local_hit() {
        let info = make_info("hello", &[]);
        let local = setup_local_store(&[info.clone()]);
        let store = PullThroughStore::new(local, vec![]);

        let store_hash = &info.store_path["/nix/store/".len().."/nix/store/".len() + 32];
        let result = store.get_narinfo(store_hash).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().store_path, info.store_path);
    }

    #[tokio::test]
    async fn test_pull_through_narinfo_upstream_fallback() {
        // Local store is empty, upstream has the path
        let info = make_info("world", &[]);
        let local = setup_local_store(&[]);
        let upstream = setup_local_store(&[info.clone()]);

        let store = PullThroughStore::new(local.clone(), vec![upstream]);

        let store_hash = &info.store_path["/nix/store/".len().."/nix/store/".len() + 32];
        let result = store.get_narinfo(store_hash).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().store_path, info.store_path);

        // Verify it was cached in local
        let local_result = local.get_narinfo(store_hash).await.unwrap();
        assert!(local_result.is_some());
    }

    #[tokio::test]
    async fn test_pull_through_narinfo_miss() {
        let local = setup_local_store(&[]);
        let store = PullThroughStore::new(local, vec![]);

        let result = store
            .get_narinfo("nonexistent00000000000000000000")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_resolve_closure_bfs_simple() {
        let a = make_info("a", &["b"]);
        let b = make_info("b", &[]);
        let store = setup_local_store(&[a.clone(), b.clone()]);

        let store_hash_a = &a.store_path["/nix/store/".len().."/nix/store/".len() + 32];
        let (paths, has_more) =
            resolve_closure_bfs(store.as_ref(), &[store_hash_a.to_string()], &[], None)
                .await
                .unwrap();

        assert!(!has_more);
        assert_eq!(paths.len(), 2);
        let names: Vec<&str> = paths.iter().map(|p| p.store_path.as_str()).collect();
        assert!(names.contains(&a.store_path.as_str()));
        assert!(names.contains(&b.store_path.as_str()));
    }

    #[tokio::test]
    async fn test_resolve_closure_bfs_with_haves() {
        let a = make_info("a", &["b"]);
        let b = make_info("b", &[]);
        let store = setup_local_store(&[a.clone(), b.clone()]);

        let store_hash_a = &a.store_path["/nix/store/".len().."/nix/store/".len() + 32];
        let store_hash_b = &b.store_path["/nix/store/".len().."/nix/store/".len() + 32];

        let (paths, _) = resolve_closure_bfs(
            store.as_ref(),
            &[store_hash_a.to_string()],
            &[store_hash_b.to_string()],
            None,
        )
        .await
        .unwrap();

        // b should be excluded since it's in haves
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].store_path, a.store_path);
    }

    #[tokio::test]
    async fn test_resolve_closure_bfs_with_limit() {
        let a = make_info("a", &["b", "c"]);
        let b = make_info("b", &[]);
        let c = make_info("c", &[]);
        let store = setup_local_store(&[a.clone(), b.clone(), c.clone()]);

        let store_hash_a = &a.store_path["/nix/store/".len().."/nix/store/".len() + 32];
        let (paths, has_more) =
            resolve_closure_bfs(store.as_ref(), &[store_hash_a.to_string()], &[], Some(1))
                .await
                .unwrap();

        assert_eq!(paths.len(), 1);
        assert!(has_more);
    }
}
