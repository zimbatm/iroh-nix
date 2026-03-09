//! Iroh transport layer for Nix store operations
//!
//! This crate provides P2P NAR transfer and gossip-based discovery using iroh.
//! It wraps iroh networking for:
//! - Transferring NAR blobs between nodes (request/response over QUIC)
//! - Gossip-based peer discovery (who has which store paths)
//! - Smart protocol (batch narinfo, closure) over QUIC
//!
//! Build-queue and remote-build concerns live in a separate crate.

pub mod gossip;
pub mod nar_transfer;
pub mod smart_quic;

// ALPN identifiers
/// ALPN identifier for the NAR blob protocol
pub const NAR_PROTOCOL_ALPN: &[u8] = b"/iroh-nix/nar/1";

/// ALPN identifier for the smart protocol (batch narinfo, closure)
pub const SMART_PROTOCOL_ALPN: &[u8] = b"/iroh-nix/smart/1";

// Re-exports for backwards compatibility
pub use gossip::{gossip_topic, GossipMessage, GossipService, ProviderCache, ProviderInfo};
pub use nar_transfer::{
    fetch_nar, fetch_nar_with_config, handle_nar_accepted, ImportResult, NarRequest,
    NarResponseHeader,
};
pub use smart_quic::{handle_smart_accepted, smart_request};
