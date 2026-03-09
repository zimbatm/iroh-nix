//! iroh-nix: Distributed Nix binary cache using iroh for P2P artifact distribution

pub mod cli;
pub mod node;

// Re-export core types from workspace crates
pub use nix_store::store::{
    AllowAll, ContentFilter, LocalStore, NarInfoIndex, NarInfoProvider, PullThroughStore,
    StorePathInfo,
};
pub use nix_store::{
    run_full_cleanup_loop, run_stale_cleanup_loop, Blake3Hash, Error, HashEntry, HashIndex,
    NarCache, Result, RetryConfig, Sha256Hash,
};
pub use nix_store_iroh::{GossipService, ProviderInfo, NAR_PROTOCOL_ALPN};
pub use nix_substituter::SubstituterConfig;
pub use node::{ConfiguredRelayMode, Node, NodeConfig};
