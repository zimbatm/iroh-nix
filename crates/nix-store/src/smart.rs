//! Smart protocol types for batch narinfo lookups and closure computation
//!
//! The smart protocol reduces HTTP round-trips by allowing batch lookups
//! and server-side closure computation (wants - haves). It is transport-agnostic:
//! JSON over HTTP, postcard over QUIC/iroh.

use serde::{Deserialize, Serialize};

use crate::hash_index::{store_path_basename, Blake3Hash};
use crate::store::{NarInfoProvider, StorePathInfo};
use crate::Result;

// ============================================================================
// Wire types
// ============================================================================

/// Smart protocol request variants
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SmartRequest {
    /// Batch narinfo lookup
    BatchNarInfo {
        /// 32-char nix base32 store hash prefixes
        store_hashes: Vec<String>,
    },
    /// Closure computation: closure(wants) - haves
    Closure {
        /// Store hashes we need closures for
        wants: Vec<String>,
        /// Store hashes we already have
        haves: Vec<String>,
        /// Maximum number of paths to return (pagination)
        limit: Option<u32>,
    },
}

/// Smart protocol response variants
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SmartResponse {
    /// Batch narinfo results (same order as request)
    BatchNarInfo {
        results: Vec<Option<StorePathInfoCompact>>,
    },
    /// Set of paths the client needs
    PathSet {
        paths: Vec<StorePathInfoCompact>,
        /// True if more paths are available (pagination)
        has_more: bool,
    },
    /// Error response
    Error {
        code: SmartErrorCode,
        message: String,
    },
}

/// Compact wire representation of narinfo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePathInfoCompact {
    pub store_path: String,
    /// "sha256:<nix32>"
    pub nar_hash: String,
    pub nar_size: u64,
    /// Basenames only (e.g., "abc123-name")
    pub references: Vec<String>,
    /// Basename only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deriver: Option<String>,
    pub signatures: Vec<String>,
    /// BLAKE3 hex, only if known
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
}

/// Error codes for the smart protocol
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SmartErrorCode {
    BadRequest,
    NotFound,
    Internal,
    TooManyHashes,
}

// ============================================================================
// Conversions
// ============================================================================

impl StorePathInfoCompact {
    /// Convert from the internal StorePathInfo representation
    pub fn from_store_path_info(info: &StorePathInfo) -> Self {
        let nar_hash = format!("sha256:{}", info.nar_hash.to_nix_base32());

        let references = info
            .references
            .iter()
            .filter_map(|r| store_path_basename(r).map(|s| s.to_string()))
            .collect();

        let deriver = info
            .deriver
            .as_deref()
            .and_then(|d| store_path_basename(d).map(|s| s.to_string()));

        let blake3 = if info.blake3.0 != [0u8; 32] {
            Some(info.blake3.to_hex())
        } else {
            None
        };

        Self {
            store_path: info.store_path.clone(),
            nar_hash,
            nar_size: info.nar_size,
            references,
            deriver,
            signatures: info.signatures.clone(),
            blake3,
        }
    }

    /// Convert to the internal StorePathInfo representation
    pub fn to_store_path_info(&self) -> Result<StorePathInfo> {
        let nar_hash = parse_compact_nar_hash(&self.nar_hash)?;

        let references = self
            .references
            .iter()
            .map(|r| format!("/nix/store/{}", r))
            .collect();

        let deriver = self.deriver.as_ref().map(|d| format!("/nix/store/{}", d));

        let blake3 = match &self.blake3 {
            Some(hex) => Blake3Hash::from_hex(hex)?,
            None => Blake3Hash([0u8; 32]),
        };

        Ok(StorePathInfo {
            store_path: self.store_path.clone(),
            blake3,
            nar_hash,
            nar_size: self.nar_size,
            references,
            deriver,
            signatures: self.signatures.clone(),
        })
    }
}

/// Parse "sha256:<nix32>" into a Sha256Hash
fn parse_compact_nar_hash(s: &str) -> Result<crate::hash_index::Sha256Hash> {
    let hash_str = s
        .strip_prefix("sha256:")
        .ok_or_else(|| crate::Error::Protocol(format!("expected sha256: prefix, got: {}", s)))?;

    // Nix base32 is 52 chars for SHA256
    if hash_str.len() == 52 {
        // Decode nix base32
        let bytes = decode_nix_base32(hash_str)?;
        crate::hash_index::Sha256Hash::from_slice(&bytes)
    } else if hash_str.len() == 64 {
        // Also accept hex
        crate::hash_index::Sha256Hash::from_hex(hash_str)
    } else {
        Err(crate::Error::Protocol(format!(
            "invalid sha256 hash length: {}",
            hash_str.len()
        )))
    }
}

use crate::encoding::decode_nix_base32;

// ============================================================================
// SmartProvider trait
// ============================================================================

/// Extends NarInfoProvider with batch and closure operations
#[async_trait::async_trait]
pub trait SmartProvider: NarInfoProvider {
    /// Batch lookup of narinfo by store hashes.
    /// Returns results in the same order as the input hashes.
    async fn batch_narinfo(&self, store_hashes: &[String]) -> Result<Vec<Option<StorePathInfo>>>;

    /// Compute the closure delta: closure(wants) - haves.
    /// Returns (paths_needed, has_more).
    async fn resolve_closure(
        &self,
        wants: &[String],
        haves: &[String],
        limit: Option<u32>,
    ) -> Result<(Vec<StorePathInfo>, bool)>;
}

// ============================================================================
// Extract store hash from a basename
// ============================================================================

/// Extract the store hash from a basename like "abc123-name" -> "abc123"
pub fn store_hash_from_basename(basename: &str) -> Option<&str> {
    let dash_pos = basename.find('-')?;
    if dash_pos >= 1 {
        Some(&basename[..dash_pos])
    } else {
        None
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_index::Sha256Hash;

    #[test]
    fn test_compact_roundtrip() {
        let info = StorePathInfo {
            store_path: "/nix/store/abc123-hello".to_string(),
            blake3: Blake3Hash([1u8; 32]),
            nar_hash: Sha256Hash([2u8; 32]),
            nar_size: 1024,
            references: vec!["/nix/store/dep1-foo".to_string()],
            deriver: Some("/nix/store/xyz-test.drv".to_string()),
            signatures: vec!["cache.nixos.org-1:sig".to_string()],
        };

        let compact = StorePathInfoCompact::from_store_path_info(&info);
        assert_eq!(compact.store_path, info.store_path);
        assert!(compact.nar_hash.starts_with("sha256:"));
        assert_eq!(compact.nar_size, 1024);
        assert_eq!(compact.references, vec!["dep1-foo"]);
        assert_eq!(compact.deriver, Some("xyz-test.drv".to_string()));
        assert!(compact.blake3.is_some());

        let back = compact.to_store_path_info().unwrap();
        assert_eq!(back.store_path, info.store_path);
        assert_eq!(back.nar_hash, info.nar_hash);
        assert_eq!(back.nar_size, info.nar_size);
        assert_eq!(back.references, info.references);
        assert_eq!(back.deriver, info.deriver);
        assert_eq!(back.blake3, info.blake3);
    }

    #[test]
    fn test_compact_zeroed_blake3() {
        let info = StorePathInfo {
            store_path: "/nix/store/abc123-hello".to_string(),
            blake3: Blake3Hash([0u8; 32]),
            nar_hash: Sha256Hash([0u8; 32]),
            nar_size: 100,
            references: vec![],
            deriver: None,
            signatures: vec![],
        };

        let compact = StorePathInfoCompact::from_store_path_info(&info);
        assert!(compact.blake3.is_none());
    }

    #[test]
    fn test_store_hash_from_basename() {
        assert_eq!(store_hash_from_basename("abc123-hello"), Some("abc123"));
        assert_eq!(
            store_hash_from_basename("gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0"),
            Some("gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03")
        );
        assert_eq!(store_hash_from_basename("nohash"), None);
    }

    #[test]
    fn test_smart_request_json_roundtrip() {
        let req = SmartRequest::BatchNarInfo {
            store_hashes: vec!["abc123".to_string(), "def456".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: SmartRequest = serde_json::from_str(&json).unwrap();
        match back {
            SmartRequest::BatchNarInfo { store_hashes } => {
                assert_eq!(store_hashes, vec!["abc123", "def456"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_smart_request_closure_json_roundtrip() {
        let req = SmartRequest::Closure {
            wants: vec!["w1".to_string()],
            haves: vec!["h1".to_string()],
            limit: Some(100),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: SmartRequest = serde_json::from_str(&json).unwrap();
        match back {
            SmartRequest::Closure {
                wants,
                haves,
                limit,
            } => {
                assert_eq!(wants, vec!["w1"]);
                assert_eq!(haves, vec!["h1"]);
                assert_eq!(limit, Some(100));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_smart_response_json_roundtrip() {
        let resp = SmartResponse::Error {
            code: SmartErrorCode::BadRequest,
            message: "bad".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: SmartResponse = serde_json::from_str(&json).unwrap();
        match back {
            SmartResponse::Error { code, message } => {
                assert_eq!(code, SmartErrorCode::BadRequest);
                assert_eq!(message, "bad");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_smart_request_postcard_roundtrip() {
        let req = SmartRequest::BatchNarInfo {
            store_hashes: vec!["abc123".to_string()],
        };
        let bytes = postcard::to_allocvec(&req).unwrap();
        let back: SmartRequest = postcard::from_bytes(&bytes).unwrap();
        match back {
            SmartRequest::BatchNarInfo { store_hashes } => {
                assert_eq!(store_hashes, vec!["abc123"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    // Test SmartProvider (via LocalStore) with a mock-free in-memory store
    mod smart_provider {
        use super::*;
        use crate::hash_index::HashIndex;
        use crate::store::LocalStore;
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        fn setup_test_store() -> (Arc<Mutex<HashIndex>>, LocalStore) {
            let index = HashIndex::in_memory().unwrap();

            // Insert entries with references forming a graph:
            //   A -> B, C
            //   B -> D
            //   C -> D
            //   D -> (no deps)
            let entries = vec![
                crate::hash_index::HashEntry {
                    blake3: Blake3Hash([1u8; 32]),
                    sha256: Sha256Hash([11u8; 32]),
                    store_path: "/nix/store/aaaaaa-a".to_string(),
                    nar_size: 100,
                    references: vec![
                        "/nix/store/bbbbbb-b".to_string(),
                        "/nix/store/cccccc-c".to_string(),
                    ],
                    deriver: None,
                },
                crate::hash_index::HashEntry {
                    blake3: Blake3Hash([2u8; 32]),
                    sha256: Sha256Hash([22u8; 32]),
                    store_path: "/nix/store/bbbbbb-b".to_string(),
                    nar_size: 200,
                    references: vec!["/nix/store/dddddd-d".to_string()],
                    deriver: None,
                },
                crate::hash_index::HashEntry {
                    blake3: Blake3Hash([3u8; 32]),
                    sha256: Sha256Hash([33u8; 32]),
                    store_path: "/nix/store/cccccc-c".to_string(),
                    nar_size: 300,
                    references: vec!["/nix/store/dddddd-d".to_string()],
                    deriver: None,
                },
                crate::hash_index::HashEntry {
                    blake3: Blake3Hash([4u8; 32]),
                    sha256: Sha256Hash([44u8; 32]),
                    store_path: "/nix/store/dddddd-d".to_string(),
                    nar_size: 400,
                    references: vec![],
                    deriver: None,
                },
            ];

            for entry in &entries {
                index.insert(entry).unwrap();
            }

            let index = Arc::new(Mutex::new(index));
            let store = LocalStore::new(Arc::clone(&index));
            (index, store)
        }

        #[tokio::test]
        async fn test_batch_narinfo() {
            let (_index, store) = setup_test_store();
            let smart = store;

            let results = smart
                .batch_narinfo(&[
                    "aaaaaa".to_string(),
                    "nonexistent".to_string(),
                    "bbbbbb".to_string(),
                ])
                .await
                .unwrap();

            assert_eq!(results.len(), 3);
            assert!(results[0].is_some());
            assert_eq!(
                results[0].as_ref().unwrap().store_path,
                "/nix/store/aaaaaa-a"
            );
            assert!(results[1].is_none());
            assert!(results[2].is_some());
            assert_eq!(
                results[2].as_ref().unwrap().store_path,
                "/nix/store/bbbbbb-b"
            );
        }

        #[tokio::test]
        async fn test_closure_full() {
            let (_index, store) = setup_test_store();
            let smart = store;

            // Want A, have nothing -> should get A, B, C, D
            let (paths, has_more) = smart
                .resolve_closure(&["aaaaaa".to_string()], &[], None)
                .await
                .unwrap();

            assert!(!has_more);
            assert_eq!(paths.len(), 4);
            let store_paths: HashSet<&str> = paths.iter().map(|p| p.store_path.as_str()).collect();
            assert!(store_paths.contains("/nix/store/aaaaaa-a"));
            assert!(store_paths.contains("/nix/store/bbbbbb-b"));
            assert!(store_paths.contains("/nix/store/cccccc-c"));
            assert!(store_paths.contains("/nix/store/dddddd-d"));
        }

        #[tokio::test]
        async fn test_closure_with_haves() {
            let (_index, store) = setup_test_store();
            let smart = store;

            // Want A, have D -> should get A, B, C (not D)
            let (paths, has_more) = smart
                .resolve_closure(&["aaaaaa".to_string()], &["dddddd".to_string()], None)
                .await
                .unwrap();

            assert!(!has_more);
            assert_eq!(paths.len(), 3);
            let store_paths: HashSet<&str> = paths.iter().map(|p| p.store_path.as_str()).collect();
            assert!(store_paths.contains("/nix/store/aaaaaa-a"));
            assert!(store_paths.contains("/nix/store/bbbbbb-b"));
            assert!(store_paths.contains("/nix/store/cccccc-c"));
            assert!(!store_paths.contains("/nix/store/dddddd-d"));
        }

        #[tokio::test]
        async fn test_closure_with_limit() {
            let (_index, store) = setup_test_store();
            let smart = store;

            // Want A with limit 2
            let (paths, has_more) = smart
                .resolve_closure(&["aaaaaa".to_string()], &[], Some(2))
                .await
                .unwrap();

            assert_eq!(paths.len(), 2);
            assert!(has_more);
        }

        #[tokio::test]
        async fn test_closure_diamond_deps() {
            let (_index, store) = setup_test_store();
            let smart = store;

            // B and C both depend on D. Requesting closure of B and C should
            // not duplicate D.
            let (paths, _) = smart
                .resolve_closure(&["bbbbbb".to_string(), "cccccc".to_string()], &[], None)
                .await
                .unwrap();

            // Should get B, C, D (no duplicates)
            assert_eq!(paths.len(), 3);
            let store_paths: Vec<&str> = paths.iter().map(|p| p.store_path.as_str()).collect();
            // D should appear exactly once
            assert_eq!(
                store_paths
                    .iter()
                    .filter(|&&p| p == "/nix/store/dddddd-d")
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn test_closure_missing_want() {
            let (_index, store) = setup_test_store();
            let smart = store;

            // Want a non-existent hash
            let (paths, has_more) = smart
                .resolve_closure(&["nonexistent".to_string()], &[], None)
                .await
                .unwrap();

            assert!(paths.is_empty());
            assert!(!has_more);
        }
    }
}
