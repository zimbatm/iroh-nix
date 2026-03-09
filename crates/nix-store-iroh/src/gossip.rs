//! Gossip-based peer discovery for Nix store paths
//!
//! Wraps iroh-gossip to provide a higher-level API for announcing and querying
//! Nix store paths across a P2P network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::Endpoint;
use iroh_base::EndpointId;
use iroh_gossip::api::{Event, GossipReceiver, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use nix_store::hash_index::Blake3Hash;
use nix_store::{Error, MutexExt, Result};

/// Gossip message types for the iroh-nix network
///
/// These messages are broadcast over iroh-gossip to all peers sharing the
/// same network topic. Build-related messages live in a separate crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipMessage {
    /// Announce that we have a store path available
    Have {
        /// BLAKE3 hash of the NAR
        blake3: [u8; 32],
        /// The store path (e.g., /nix/store/abc123-name)
        store_path: String,
        /// NAR size in bytes
        nar_size: u64,
    },

    /// Query for a store path ("who has this?")
    Want {
        /// BLAKE3 hash we're looking for
        blake3: [u8; 32],
    },

    /// Response to a Want query - "I have it, connect to me"
    IHave {
        /// BLAKE3 hash
        blake3: [u8; 32],
        /// Endpoint ID of the node that has this artifact
        endpoint_id: [u8; 32],
        /// The store path
        store_path: String,
        /// NAR size in bytes
        nar_size: u64,
    },

    /// Warning: this node is about to garbage-collect a store path
    GcWarning {
        /// BLAKE3 hash of the NAR that will be deleted
        blake3: [u8; 32],
        /// Human-readable warning message
        message: String,
    },
}

impl GossipMessage {
    /// Serialize the message to bytes for sending over gossip
    pub fn to_bytes(&self) -> Result<Bytes> {
        let bytes = postcard::to_allocvec(self)
            .map_err(|e| Error::Protocol(format!("serialize: {}", e)))?;
        Ok(Bytes::from(bytes))
    }

    /// Deserialize from bytes received over gossip
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(|e| Error::Protocol(format!("deserialize: {}", e)))
    }
}

/// Create a gossip topic ID from a network identifier
///
/// Nodes that share the same `network_id` will join the same gossip topic
/// and discover each other.
pub fn gossip_topic(network_id: &str) -> TopicId {
    TopicId::from_bytes(blake3::hash(network_id.as_bytes()).into())
}

/// Maximum gossip message size (8 KB should be plenty for metadata)
const MAX_MESSAGE_SIZE: usize = 8192;

/// Information about a provider (a peer that has a particular artifact)
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// The peer's endpoint ID
    pub endpoint_id: EndpointId,
    /// The store path
    pub store_path: String,
    /// NAR size in bytes
    pub nar_size: u64,
}

/// Tracks known providers for artifacts, keyed by BLAKE3 hash
#[derive(Debug, Default)]
pub struct ProviderCache {
    /// Map from BLAKE3 hash to known providers
    providers: HashMap<[u8; 32], Vec<ProviderInfo>>,
}

impl ProviderCache {
    /// Add a provider for a given BLAKE3 hash
    pub fn add_provider(&mut self, blake3: [u8; 32], info: ProviderInfo) {
        let providers = self.providers.entry(blake3).or_default();
        // Don't add duplicates
        if !providers.iter().any(|p| p.endpoint_id == info.endpoint_id) {
            providers.push(info);
        }
    }

    /// Get all known providers for a BLAKE3 hash
    pub fn get_providers(&self, blake3: &[u8; 32]) -> Option<&Vec<ProviderInfo>> {
        self.providers.get(blake3)
    }

    /// Clear all cached providers
    pub fn clear(&mut self) {
        self.providers.clear();
    }

    /// Remove all entries for a specific endpoint (e.g., when a peer disconnects)
    pub(crate) fn remove_provider(&mut self, endpoint_id: &EndpointId) {
        for providers in self.providers.values_mut() {
            providers.retain(|p| p.endpoint_id != *endpoint_id);
        }
    }
}

/// Gossip service for peer discovery and store-path announcements
///
/// Wraps iroh-gossip to provide a higher-level API for announcing and
/// querying Nix store paths across the P2P network.
pub struct GossipService {
    /// The underlying gossip instance
    gossip: Gossip,
    /// Sender for the main topic
    sender: GossipSender,
    /// Topic ID for the network
    topic: TopicId,
    /// Cache of known providers
    provider_cache: Arc<Mutex<ProviderCache>>,
    /// Our endpoint ID
    our_id: EndpointId,
}

impl GossipService {
    /// Create a new gossip service
    ///
    /// The `network_id` is used to derive the topic -- nodes with the same
    /// network_id will discover each other. `bootstrap` is the set of peers
    /// to initially connect to.
    pub async fn new(
        endpoint: &Endpoint,
        network_id: &str,
        bootstrap: Vec<EndpointId>,
    ) -> Result<Self> {
        let topic = gossip_topic(network_id);

        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        let bootstrap_count = bootstrap.len();
        let gossip_topic = gossip
            .subscribe(topic, bootstrap)
            .await
            .map_err(|e| Error::Protocol(format!("failed to subscribe to gossip: {}", e)))?;

        let (sender, receiver) = gossip_topic.split();
        let our_id = endpoint.id();
        let provider_cache = Arc::new(Mutex::new(ProviderCache::default()));

        // Spawn the receiver task
        let cache_clone = Arc::clone(&provider_cache);
        tokio::spawn(async move {
            Self::run_message_loop(receiver, cache_clone).await;
        });

        info!(
            "Gossip service started on topic {} (bootstrap peers: {})",
            topic.fmt_short(),
            bootstrap_count
        );

        Ok(Self {
            gossip,
            sender,
            topic,
            provider_cache,
            our_id,
        })
    }

    /// Announce that we have a store path available
    pub async fn announce_have(
        &self,
        blake3: Blake3Hash,
        store_path: &str,
        nar_size: u64,
    ) -> Result<()> {
        let msg = GossipMessage::Have {
            blake3: *blake3.as_bytes(),
            store_path: store_path.to_string(),
            nar_size,
        };
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Protocol(format!("broadcast failed: {}", e)))?;
        debug!("Announced: {} ({})", store_path, blake3);
        Ok(())
    }

    /// Query the network for providers of a BLAKE3 hash
    pub async fn query_want(&self, blake3: Blake3Hash) -> Result<()> {
        let msg = GossipMessage::Want {
            blake3: *blake3.as_bytes(),
        };
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Protocol(format!("broadcast failed: {}", e)))?;
        debug!("Queried for: {}", blake3);
        Ok(())
    }

    /// Get known providers for a BLAKE3 hash from the local cache
    pub fn get_providers(&self, blake3: &Blake3Hash) -> Vec<ProviderInfo> {
        let Ok(cache) = self.provider_cache.lock() else {
            return Vec::new();
        };
        cache
            .get_providers(blake3.as_bytes())
            .cloned()
            .unwrap_or_default()
    }

    /// Get a reference to the provider cache
    pub fn provider_cache(&self) -> &Arc<Mutex<ProviderCache>> {
        &self.provider_cache
    }

    /// Get the underlying gossip instance
    pub fn gossip(&self) -> &Gossip {
        &self.gossip
    }

    /// Get our endpoint ID
    pub fn our_id(&self) -> EndpointId {
        self.our_id
    }

    /// Get the topic ID
    pub fn topic(&self) -> TopicId {
        self.topic
    }

    /// Add bootstrap peers to the gossip topic
    pub async fn add_peers(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.sender
            .join_peers(peers)
            .await
            .map_err(|e| Error::Protocol(format!("join_peers failed: {}", e)))?;
        Ok(())
    }

    /// Gracefully shut down the gossip service
    pub async fn shutdown(self) -> Result<()> {
        self.gossip
            .shutdown()
            .await
            .map_err(|e| Error::Protocol(format!("gossip shutdown failed: {}", e)))?;
        Ok(())
    }

    /// Main message loop -- processes incoming gossip messages
    ///
    /// Runs in a spawned task and updates the provider cache based on
    /// Have / IHave messages. Want messages are logged for upstream hooks.
    /// GcWarning messages are logged as warnings.
    async fn run_message_loop(
        mut receiver: GossipReceiver,
        provider_cache: Arc<Mutex<ProviderCache>>,
    ) {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    if let Err(e) =
                        Self::handle_message(&msg.content, msg.delivered_from, &provider_cache)
                    {
                        warn!("Error handling gossip message: {}", e);
                    }
                }
                Ok(Event::NeighborUp(peer)) => {
                    info!("Gossip peer joined: {}", peer);
                }
                Ok(Event::NeighborDown(peer)) => {
                    debug!("Gossip peer left: {}", peer);
                    if let Ok(mut cache) = provider_cache.lock() {
                        cache.remove_provider(&peer);
                    }
                }
                Ok(Event::Lagged) => {
                    warn!("Gossip receiver lagged, some messages were dropped");
                }
                Err(e) => {
                    warn!("Gossip receiver error: {}", e);
                }
            }
        }
        debug!("Gossip receiver loop ended");
    }

    /// Handle a single incoming gossip message
    fn handle_message(
        content: &[u8],
        from: EndpointId,
        provider_cache: &Arc<Mutex<ProviderCache>>,
    ) -> Result<()> {
        let msg = GossipMessage::from_bytes(content)?;

        match msg {
            GossipMessage::Have {
                blake3,
                store_path,
                nar_size,
            } => {
                debug!(
                    "Peer {} has {} ({})",
                    from,
                    store_path,
                    hex::encode(&blake3[..8])
                );
                let mut cache = provider_cache.lock_or_err()?;
                cache.add_provider(
                    blake3,
                    ProviderInfo {
                        endpoint_id: from,
                        store_path,
                        nar_size,
                    },
                );
            }
            GossipMessage::Want { blake3 } => {
                debug!("Peer {} wants {}", from, hex::encode(&blake3[..8]));
                // Upstream code can hook into the provider cache to respond
            }
            GossipMessage::IHave {
                blake3,
                endpoint_id,
                store_path,
                nar_size,
            } => {
                let Ok(provider_id) = EndpointId::from_bytes(&endpoint_id) else {
                    return Err(Error::Protocol("invalid endpoint ID in IHave".into()));
                };
                debug!(
                    "Peer {} reports {} has {} (response to Want)",
                    from,
                    provider_id,
                    hex::encode(&blake3[..8])
                );
                let mut cache = provider_cache.lock_or_err()?;
                cache.add_provider(
                    blake3,
                    ProviderInfo {
                        endpoint_id: provider_id,
                        store_path,
                        nar_size,
                    },
                );
            }
            GossipMessage::GcWarning { blake3, message } => {
                warn!(
                    "GC warning from {}: {} ({})",
                    from,
                    message,
                    hex::encode(&blake3[..8])
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gossip_message_have_roundtrip() {
        let msg = GossipMessage::Have {
            blake3: [42u8; 32],
            store_path: "/nix/store/test123-hello".to_string(),
            nar_size: 12345,
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::Have {
                blake3,
                store_path,
                nar_size,
            } => {
                assert_eq!(blake3, [42u8; 32]);
                assert_eq!(store_path, "/nix/store/test123-hello");
                assert_eq!(nar_size, 12345);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_message_want_roundtrip() {
        let msg = GossipMessage::Want { blake3: [99u8; 32] };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::Want { blake3 } => {
                assert_eq!(blake3, [99u8; 32]);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_message_ihave_roundtrip() {
        let msg = GossipMessage::IHave {
            blake3: [55u8; 32],
            endpoint_id: [77u8; 32],
            store_path: "/nix/store/abc123-package".to_string(),
            nar_size: 98765,
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::IHave {
                blake3,
                endpoint_id,
                store_path,
                nar_size,
            } => {
                assert_eq!(blake3, [55u8; 32]);
                assert_eq!(endpoint_id, [77u8; 32]);
                assert_eq!(store_path, "/nix/store/abc123-package");
                assert_eq!(nar_size, 98765);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_message_gc_warning_roundtrip() {
        let msg = GossipMessage::GcWarning {
            blake3: [11u8; 32],
            message: "will be deleted in 60s".to_string(),
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::GcWarning { blake3, message } => {
                assert_eq!(blake3, [11u8; 32]);
                assert_eq!(message, "will be deleted in 60s");
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_topic_deterministic() {
        let topic1 = gossip_topic("my-network");
        let topic2 = gossip_topic("my-network");
        assert_eq!(topic1, topic2);

        let topic3 = gossip_topic("other-network");
        assert_ne!(topic1, topic3);
    }

    #[test]
    fn test_provider_cache() {
        use iroh_base::SecretKey;

        let mut cache = ProviderCache::default();
        let hash = [1u8; 32];
        let secret_key = SecretKey::generate(&mut rand::rng());
        let endpoint_id = secret_key.public();

        cache.add_provider(
            hash,
            ProviderInfo {
                endpoint_id,
                store_path: "/nix/store/test".to_string(),
                nar_size: 100,
            },
        );

        assert!(cache.get_providers(&hash).is_some());
        assert_eq!(cache.get_providers(&hash).unwrap().len(), 1);

        // Don't add duplicates
        cache.add_provider(
            hash,
            ProviderInfo {
                endpoint_id,
                store_path: "/nix/store/test".to_string(),
                nar_size: 100,
            },
        );
        assert_eq!(cache.get_providers(&hash).unwrap().len(), 1);

        // Clear empties the cache
        cache.clear();
        assert!(cache.get_providers(&hash).is_none());
    }

    #[test]
    fn test_provider_cache_remove_provider() {
        use iroh_base::SecretKey;

        let mut cache = ProviderCache::default();
        let hash = [1u8; 32];
        let secret_key = SecretKey::generate(&mut rand::rng());
        let endpoint_id = secret_key.public();

        cache.add_provider(
            hash,
            ProviderInfo {
                endpoint_id,
                store_path: "/nix/store/test".to_string(),
                nar_size: 100,
            },
        );

        assert_eq!(cache.get_providers(&hash).unwrap().len(), 1);
        cache.remove_provider(&endpoint_id);
        assert_eq!(cache.get_providers(&hash).unwrap().len(), 0);
    }
}
