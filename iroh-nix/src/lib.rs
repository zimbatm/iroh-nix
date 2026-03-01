//! iroh-nix: Distributed Nix build system using iroh for P2P artifact distribution
//!
//! This crate provides:
//! - P2P distribution of Nix build artifacts using iroh-blobs
//! - Gossip-based discovery of cached artifacts
//! - Build coordination across multiple machines
//! - Stale index cleanup

pub mod build;
pub mod builder;
pub mod cli;
pub mod error;
pub mod gc;
pub mod gossip;
pub mod hash_index;
pub mod http_cache;
pub mod nar;
pub mod nix_info;
pub mod node;
pub mod protocol;
pub mod retry;
pub mod substituter;
pub mod transfer;

pub use build::{
    BuildJob, BuildLogLine, BuildOutput, BuildQueue, BuildResult, DrvHash, JobId, JobOutcome,
    LeasedJob, LogEntry, QueueStats,
};
pub use builder::{BuilderConfig, BuilderWorker};
pub use error::{Error, MutexExt, Result};
pub use gc::{run_stale_cleanup, run_stale_cleanup_loop};
pub use gossip::{GossipMessage, GossipService, ProviderInfo, RequesterInfo};
pub use hash_index::{Blake3Hash, HashIndex};
pub use http_cache::{CacheNarInfo, FetchResult, HttpCacheClient, HttpCacheConfig};
pub use nix_info::NixPathInfo;
pub use node::{Node, NodeConfig, NodeStats};
pub use retry::{with_retry, with_retry_providers, with_retry_stats, RetryConfig, RetryStats};
pub use transfer::ImportResult;
