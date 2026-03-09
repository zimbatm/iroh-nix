//! nix-store: Core types and traits for Nix store operations
//!
//! This crate provides:
//! - Hash index for BLAKE3 <-> SHA256 <-> store path translation
//! - NAR serialization with streaming hash computation
//! - Store abstraction traits (NarInfoProvider, NarInfoIndex, ContentFilter)
//! - Smart protocol types (batch narinfo, closure computation)
//! - Nix path-info queries and protocol helpers
//! - Retry utilities and stale index cleanup
//!
//! Feature flags:
//! - `sqlite` (default): SQLite-backed hash index, garbage collection
//! - `runtime` (default): Tokio runtime, retry utilities, NAR cache

pub mod encoding;
pub mod error;
pub mod smart;

// Always available: hash types, traits, NAR serialization
pub mod hash_index;
pub mod nar;
pub mod store;

// Requires SQLite
#[cfg(feature = "sqlite")]
pub mod gc;
#[cfg(feature = "sqlite")]
pub mod nix_info;
#[cfg(feature = "sqlite")]
pub mod nix_protocol;

// Requires runtime (tokio)
#[cfg(feature = "runtime")]
pub mod nar_cache;
#[cfg(feature = "runtime")]
pub mod retry;

// Core re-exports (always available)
pub use error::{Error, MutexExt, Result};
pub use hash_index::{Blake3Hash, Sha256Hash};
pub use smart::{SmartErrorCode, SmartProvider, SmartRequest, SmartResponse, StorePathInfoCompact};
pub use store::{
    resolve_closure_bfs, AllowAll, ContentFilter, NarInfoIndex, NarInfoProvider, StorePathInfo,
};

// Re-exports that require sqlite
#[cfg(feature = "sqlite")]
pub use hash_index::{HashEntry, HashIndex};
#[cfg(feature = "sqlite")]
pub use nar::{serialize_path, serialize_path_to_writer, NarInfo};
#[cfg(feature = "sqlite")]
pub use nix_info::NixPathInfo;
#[cfg(feature = "sqlite")]
pub use store::{LocalStore, PullThroughStore, SignatureFilter};

// Re-exports that require sqlite (gc depends on hash_index + tokio)
#[cfg(all(feature = "sqlite", feature = "runtime"))]
pub use gc::{
    run_full_cleanup_loop, run_nar_cache_cleanup, run_stale_cleanup, run_stale_cleanup_loop,
};

// Re-exports that require runtime
#[cfg(feature = "runtime")]
pub use nar_cache::NarCache;
#[cfg(feature = "runtime")]
pub use retry::{with_retry, with_retry_stats, RetryConfig, RetryStats};
