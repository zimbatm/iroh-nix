//! Custom iroh protocols for iroh-nix
//!
//! Defines the NAR blob protocol and RPC protocol for communicating
//! between iroh-nix nodes.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::hash_index::{Blake3Hash, Sha256Hash};

/// ALPN identifier for the NAR blob protocol
pub const NAR_PROTOCOL_ALPN: &[u8] = b"/iroh-nix/nar/1";

/// ALPN identifier for the RPC protocol
pub const RPC_PROTOCOL_ALPN: &[u8] = b"/iroh-nix/rpc/1";

/// ALPN identifier for the build-queue protocol
pub const BUILD_QUEUE_PROTOCOL_ALPN: &[u8] = b"/iroh-nix/build-queue/1";

/// Request to fetch a NAR - can be by BLAKE3 hash or store path
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NarRequest {
    /// Request by BLAKE3 hash (requires pre-indexed path)
    ByBlake3 { hash: [u8; 32] },
    /// Request by store path (enables on-demand serving of any /nix/store path)
    ByStorePath { store_path: String },
}

impl NarRequest {
    /// Create a new NAR request by BLAKE3 hash
    pub fn new(hash: Blake3Hash) -> Self {
        Self::ByBlake3 { hash: hash.0 }
    }

    /// Create a new NAR request by store path
    pub fn by_store_path(store_path: impl Into<String>) -> Self {
        Self::ByStorePath {
            store_path: store_path.into(),
        }
    }

    /// Get the BLAKE3 hash if this is a ByBlake3 request
    pub fn blake3(&self) -> Option<Blake3Hash> {
        match self {
            Self::ByBlake3 { hash } => Some(Blake3Hash(*hash)),
            Self::ByStorePath { .. } => None,
        }
    }

    /// Get the store path if this is a ByStorePath request
    pub fn store_path(&self) -> Option<&str> {
        match self {
            Self::ByBlake3 { .. } => None,
            Self::ByStorePath { store_path } => Some(store_path),
        }
    }
}

/// Response header for NAR transfer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarResponseHeader {
    /// Size of the NAR in bytes
    pub size: u64,
    /// SHA256 hash for Nix verification
    pub sha256: [u8; 32],
    /// Store path this NAR corresponds to
    pub store_path: String,
}

impl NarResponseHeader {
    /// Create a new response header
    pub fn new(size: u64, sha256: Sha256Hash, store_path: String) -> Self {
        Self {
            size,
            sha256: sha256.0,
            store_path,
        }
    }

    /// Get SHA256 as Sha256Hash
    pub fn sha256(&self) -> Sha256Hash {
        Sha256Hash(self.sha256)
    }
}

/// RPC request types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcRequest {
    /// Query narinfo for a store path
    QueryNarInfo { store_path: String },

    /// Query which store paths are available
    QueryAvailable { store_paths: Vec<String> },

    /// Query the closure (transitive dependencies) of a store path
    QueryClosure { store_path: String },

    /// Announce that we have a store path available
    AnnounceAvailable {
        store_path: String,
        blake3: [u8; 32],
        nar_size: u64,
    },

    /// Ping for health check
    Ping,
}

/// RPC response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcResponse {
    /// Narinfo for a store path
    NarInfo(Option<NarInfoResponse>),

    /// List of available store paths (subset of queried)
    Available { store_paths: Vec<String> },

    /// Closure of a store path
    Closure { store_paths: Vec<String> },

    /// Acknowledgment of announcement
    AnnounceAck,

    /// Pong response
    Pong,

    /// Error response
    Error { message: String },
}

/// Narinfo response data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NarInfoResponse {
    /// Store path
    pub store_path: String,
    /// URL/reference to fetch the NAR (could be BLAKE3 hash)
    pub nar_hash: [u8; 32],
    /// SHA256 hash of NAR content
    pub nar_sha256: [u8; 32],
    /// Size of the NAR
    pub nar_size: u64,
    /// References (other store paths this depends on)
    pub references: Vec<String>,
    /// Deriver (if known)
    pub deriver: Option<String>,
    /// Signatures
    pub signatures: Vec<String>,
}

/// Gossip message types for the iroh-nix network
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipMessage {
    /// Query: Who has this store path?
    StoreQuery {
        /// Store path being queried
        store_path: String,
        /// Request ID for correlation
        request_id: u64,
    },

    /// Response: I have this store path
    StoreResponse {
        /// Store path
        store_path: String,
        /// Request ID this responds to
        request_id: u64,
        /// BLAKE3 hash of the NAR
        blake3: [u8; 32],
        /// Size of the NAR
        nar_size: u64,
    },

    /// Announcement: I just built/received this
    BuildComplete {
        /// Store path that was built
        store_path: String,
        /// BLAKE3 hash of the NAR
        blake3: [u8; 32],
        /// Size of the NAR
        nar_size: u64,
    },

    /// Intent to build a derivation
    BuildIntent {
        /// Derivation hash
        drv_hash: String,
        /// Output paths that will be produced
        output_paths: Vec<String>,
        /// Priority (higher = more important)
        priority: u8,
        /// Node ID claiming this build
        node_id: [u8; 32],
    },

    /// Claim: I will build this derivation
    BuildClaim {
        /// Derivation hash being claimed
        drv_hash: String,
        /// Node ID making the claim
        node_id: [u8; 32],
        /// Estimated build time in seconds (for conflict resolution)
        estimated_seconds: u32,
    },

    /// Warning: I'm about to GC this store path
    GcWarning {
        /// Store path that will be deleted
        store_path: String,
        /// BLAKE3 hash
        blake3: [u8; 32],
        /// Seconds until deletion
        seconds_until_deletion: u32,
    },
}

impl GossipMessage {
    /// Serialize the message to bytes
    pub fn to_bytes(&self) -> Result<Bytes, postcard::Error> {
        let bytes = postcard::to_allocvec(self)?;
        Ok(Bytes::from(bytes))
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ============================================================================
// Build Queue Protocol Messages
// ============================================================================

/// Request message from builder to requester
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildQueueRequest {
    /// Pull the next available job
    Pull {
        /// System features this builder supports (e.g., ["x86_64-linux", "kvm"])
        system_features: Vec<String>,
        /// Whether to stream build logs back to the requester
        #[serde(default)]
        stream_logs: bool,
    },

    /// Send a heartbeat for an active job
    Heartbeat {
        /// Job ID being worked on
        job_id: u64,
        /// Current status message
        status: String,
    },

    /// Stream build log output
    BuildLog {
        /// Job ID
        job_id: u64,
        /// Log data (UTF-8 encoded)
        data: Vec<u8>,
        /// Stream: 1 = stdout, 2 = stderr
        stream: u8,
    },

    /// Report job completion
    Complete {
        /// Job ID
        job_id: u64,
        /// Build outputs
        outputs: Vec<BuildOutputProto>,
        /// Signature R component (32 bytes)
        signature_r: [u8; 32],
        /// Signature S component (32 bytes)
        signature_s: [u8; 32],
    },

    /// Report job failure
    Fail {
        /// Job ID
        job_id: u64,
        /// Error message
        error: String,
    },
}

/// Response message from requester to builder
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildQueueResponse {
    /// A job to build
    Job {
        /// Unique job ID
        job_id: u64,
        /// Derivation hash (32 bytes)
        drv_hash: [u8; 32],
        /// Path to derivation file
        drv_path: String,
        /// Expected output paths
        outputs: Vec<String>,
        /// Input store paths needed for the build (with BLAKE3 hashes for fetching)
        input_paths: Vec<InputPathProto>,
    },

    /// No job available right now (wait or disconnect)
    NoJob,

    /// Heartbeat acknowledged
    HeartbeatAck,

    /// Job completion acknowledged
    CompleteAck,

    /// Job failure acknowledged
    FailAck,

    /// Log data acknowledged
    LogAck,

    /// Error response
    Error { message: String },
}

/// Build output in protocol messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutputProto {
    /// Store path (e.g., /nix/store/xxx-name)
    pub store_path: String,
    /// BLAKE3 hash of the NAR
    pub blake3: [u8; 32],
    /// SHA256 hash of the NAR
    pub sha256: [u8; 32],
    /// NAR size in bytes
    pub nar_size: u64,
}

/// Input path in protocol messages (dependencies needed for a build)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputPathProto {
    /// Store path (e.g., /nix/store/xxx-name)
    pub store_path: String,
    /// BLAKE3 hash of the NAR (for fetching)
    pub blake3: [u8; 32],
}

impl BuildQueueRequest {
    /// Serialize the request to bytes
    pub fn to_bytes(&self) -> Result<Bytes, postcard::Error> {
        let bytes = postcard::to_allocvec(self)?;
        Ok(Bytes::from(bytes))
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

impl BuildQueueResponse {
    /// Serialize the response to bytes
    pub fn to_bytes(&self) -> Result<Bytes, postcard::Error> {
        let bytes = postcard::to_allocvec(self)?;
        Ok(Bytes::from(bytes))
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

// ============================================================================
// Gossip Messages and Topics
// ============================================================================

/// Gossip topic identifiers
pub mod topics {
    use blake3::Hash;

    /// Topic for store path queries
    pub fn store_query() -> Hash {
        blake3::hash(b"iroh-nix/store-query")
    }

    /// Topic for build intents and claims
    pub fn build_coordination() -> Hash {
        blake3::hash(b"iroh-nix/build-coordination")
    }

    /// Topic for GC warnings
    pub fn gc_warnings() -> Hash {
        blake3::hash(b"iroh-nix/gc-warnings")
    }

    /// Topic for general announcements
    pub fn announcements() -> Hash {
        blake3::hash(b"iroh-nix/announcements")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gossip_message_roundtrip() {
        let msg = GossipMessage::BuildComplete {
            store_path: "/nix/store/abc123-test".to_string(),
            blake3: [1u8; 32],
            nar_size: 1024,
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::BuildComplete {
                store_path,
                blake3,
                nar_size,
            } => {
                assert_eq!(store_path, "/nix/store/abc123-test");
                assert_eq!(blake3, [1u8; 32]);
                assert_eq!(nar_size, 1024);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_nar_request_by_blake3_roundtrip() {
        let req = NarRequest::new(Blake3Hash([42u8; 32]));
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: NarRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.blake3(), Some(Blake3Hash([42u8; 32])));
        assert_eq!(decoded.store_path(), None);
    }

    #[test]
    fn test_nar_request_by_store_path_roundtrip() {
        let req = NarRequest::by_store_path("/nix/store/abc123-hello");
        let bytes = postcard::to_allocvec(&req).unwrap();
        let decoded: NarRequest = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.blake3(), None);
        assert_eq!(decoded.store_path(), Some("/nix/store/abc123-hello"));
    }
}
