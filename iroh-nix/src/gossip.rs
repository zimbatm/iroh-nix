//! Gossip-based discovery for iroh-nix
//!
//! This module implements peer discovery and announcements using iroh-gossip.
//! Nodes can:
//! - Announce when they have a new store path available
//! - Query for store paths ("who has X?")
//! - Announce when they need builders (pull-based build coordination)
//! - Announce build completions

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

use crate::build::BuildResult;
use crate::hash_index::Blake3Hash;
use crate::{Error, Result};

/// ALPN for gossip protocol
pub const GOSSIP_ALPN: &[u8] = b"/iroh-nix/gossip/1";

/// Maximum gossip message size (8 KB should be plenty for metadata)
const MAX_MESSAGE_SIZE: usize = 8192;

/// Gossip message types
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

    /// Query for a store path
    Want {
        /// BLAKE3 hash we're looking for
        blake3: [u8; 32],
    },

    /// Response to a Want query - "I have it"
    IHave {
        /// BLAKE3 hash
        blake3: [u8; 32],
        /// The store path
        store_path: String,
        /// NAR size in bytes
        nar_size: u64,
    },

    /// Announce that we need builders with certain features
    /// Builders with matching features should connect to our build queue RPC
    NeedBuilder {
        /// The requester's endpoint ID (where to connect)
        requester: [u8; 32],
        /// Required system features (e.g., ["x86_64-linux", "kvm"])
        system_features: Vec<String>,
    },

    /// Announce a build completion (so others can fetch the outputs)
    BuildComplete(BuildResult),

    /// Announce intention to garbage collect an artifact
    /// This gives peers a chance to fetch it before deletion
    GcWarning {
        /// BLAKE3 hash of the NAR being deleted
        blake3: [u8; 32],
        /// The store path
        store_path: String,
    },
}

impl GossipMessage {
    /// Serialize to bytes for sending
    pub fn to_bytes(&self) -> Result<Bytes> {
        let bytes = postcard::to_allocvec(self)
            .map_err(|e| Error::Protocol(format!("serialize: {}", e)))?;
        Ok(Bytes::from(bytes))
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes).map_err(|e| Error::Protocol(format!("deserialize: {}", e)))
    }
}

/// Information about a provider (peer that has an artifact)
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// The peer's endpoint ID
    pub endpoint_id: EndpointId,
    /// The store path
    pub store_path: String,
    /// NAR size in bytes
    pub nar_size: u64,
}

/// Information about a requester that needs builders
#[derive(Debug, Clone)]
pub struct RequesterInfo {
    /// The requester's endpoint ID
    pub endpoint_id: EndpointId,
    /// Required system features
    pub system_features: Vec<String>,
}

/// Tracks known providers for artifacts
#[derive(Debug, Default)]
pub struct ProviderCache {
    /// Map from BLAKE3 hash to known providers
    pub(crate) providers: HashMap<[u8; 32], Vec<ProviderInfo>>,
}

impl ProviderCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a provider for a hash
    pub fn add_provider(&mut self, blake3: [u8; 32], info: ProviderInfo) {
        let providers = self.providers.entry(blake3).or_default();
        // Don't add duplicates
        if !providers.iter().any(|p| p.endpoint_id == info.endpoint_id) {
            providers.push(info);
        }
    }

    /// Get providers for a hash
    pub fn get_providers(&self, blake3: &[u8; 32]) -> Option<&Vec<ProviderInfo>> {
        self.providers.get(blake3)
    }

    /// Remove a provider (e.g., when peer disconnects)
    pub fn remove_provider(&mut self, endpoint_id: &EndpointId) {
        for providers in self.providers.values_mut() {
            providers.retain(|p| p.endpoint_id != *endpoint_id);
        }
    }
}

/// Tracks requesters that need builders
#[derive(Debug, Default)]
pub struct RequesterCache {
    /// Map from endpoint ID to requester info
    requesters: HashMap<[u8; 32], RequesterInfo>,
}

impl RequesterCache {
    /// Create a new empty cache
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or update a requester
    pub fn add_requester(&mut self, info: RequesterInfo) {
        self.requesters.insert(*info.endpoint_id.as_bytes(), info);
    }

    /// Remove a requester
    pub fn remove_requester(&mut self, endpoint_id: &EndpointId) {
        self.requesters.remove(endpoint_id.as_bytes());
    }

    /// Get requesters that match our system features
    pub fn get_matching_requesters(&self, our_features: &[String]) -> Vec<RequesterInfo> {
        self.requesters
            .values()
            .filter(|r| {
                // Check if we have all required features
                r.system_features.iter().all(|f| our_features.contains(f))
            })
            .cloned()
            .collect()
    }

    /// Get all requesters
    pub fn all_requesters(&self) -> Vec<RequesterInfo> {
        self.requesters.values().cloned().collect()
    }
}

/// Callback for NeedBuilder messages (for builders to react)
pub type NeedBuilderCallback = Arc<dyn Fn(RequesterInfo) + Send + Sync + 'static>;

/// Callback for BuildComplete messages
pub type BuildCompleteCallback = Arc<dyn Fn(BuildResult) + Send + Sync + 'static>;

/// The gossip service for a node
pub struct GossipService {
    /// The underlying gossip instance
    gossip: Gossip,
    /// Sender for the main topic
    sender: GossipSender,
    /// Cache of known providers
    provider_cache: Arc<Mutex<ProviderCache>>,
    /// Cache of requesters needing builders
    requester_cache: Arc<Mutex<RequesterCache>>,
    /// Our endpoint ID
    our_id: EndpointId,
    /// Topic ID for the network
    topic_id: TopicId,
}

impl GossipService {
    /// Create a new gossip service
    ///
    /// The `network_id` is used to derive the topic - nodes with the same
    /// network_id will discover each other.
    pub async fn new(
        endpoint: &Endpoint,
        network_id: &str,
        bootstrap: Vec<EndpointId>,
    ) -> Result<Self> {
        Self::with_callbacks(endpoint, network_id, bootstrap, None, None).await
    }

    /// Create a new gossip service with callbacks
    pub async fn with_callbacks(
        endpoint: &Endpoint,
        network_id: &str,
        bootstrap: Vec<EndpointId>,
        need_builder_callback: Option<NeedBuilderCallback>,
        build_complete_callback: Option<BuildCompleteCallback>,
    ) -> Result<Self> {
        // Create topic ID from network name
        let topic_id = TopicId::from_bytes(blake3::hash(network_id.as_bytes()).into());

        // Create gossip instance
        let gossip = Gossip::builder()
            .max_message_size(MAX_MESSAGE_SIZE)
            .spawn(endpoint.clone());

        // Subscribe to the topic
        let bootstrap_count = bootstrap.len();
        let topic = gossip
            .subscribe(topic_id, bootstrap)
            .await
            .map_err(|e| Error::Gossip(format!("failed to subscribe: {}", e)))?;

        let (sender, receiver) = topic.split();
        let our_id = endpoint.id();
        let provider_cache = Arc::new(Mutex::new(ProviderCache::new()));
        let requester_cache = Arc::new(Mutex::new(RequesterCache::new()));

        // Spawn the receiver task
        let cache_clone = Arc::clone(&provider_cache);
        let requester_cache_clone = Arc::clone(&requester_cache);
        tokio::spawn(async move {
            Self::run_receiver(
                receiver,
                cache_clone,
                requester_cache_clone,
                our_id,
                need_builder_callback,
                build_complete_callback,
            )
            .await;
        });

        info!(
            "Gossip service started on topic {} (bootstrap peers: {})",
            topic_id.fmt_short(),
            bootstrap_count
        );

        Ok(Self {
            gossip,
            sender,
            provider_cache,
            requester_cache,
            our_id,
            topic_id,
        })
    }

    /// Run the receiver loop
    async fn run_receiver(
        mut receiver: GossipReceiver,
        provider_cache: Arc<Mutex<ProviderCache>>,
        requester_cache: Arc<Mutex<RequesterCache>>,
        our_id: EndpointId,
        need_builder_callback: Option<NeedBuilderCallback>,
        build_complete_callback: Option<BuildCompleteCallback>,
    ) {
        while let Some(event) = receiver.next().await {
            match event {
                Ok(Event::Received(msg)) => {
                    if let Err(e) = Self::handle_message(
                        &msg.content,
                        msg.delivered_from,
                        &provider_cache,
                        &requester_cache,
                        our_id,
                        &need_builder_callback,
                        &build_complete_callback,
                    ) {
                        warn!("Error handling gossip message: {}", e);
                    }
                }
                Ok(Event::NeighborUp(peer)) => {
                    info!("Gossip peer joined: {}", peer);
                }
                Ok(Event::NeighborDown(peer)) => {
                    debug!("Gossip peer left: {}", peer);
                    // Remove from caches
                    if let Ok(mut cache) = provider_cache.lock() {
                        cache.remove_provider(&peer);
                    }
                    if let Ok(mut cache) = requester_cache.lock() {
                        cache.remove_requester(&peer);
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

    /// Handle an incoming gossip message
    fn handle_message(
        content: &[u8],
        from: EndpointId,
        provider_cache: &Arc<Mutex<ProviderCache>>,
        requester_cache: &Arc<Mutex<RequesterCache>>,
        _our_id: EndpointId,
        need_builder_callback: &Option<NeedBuilderCallback>,
        build_complete_callback: &Option<BuildCompleteCallback>,
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
                let mut cache = provider_cache.lock().unwrap();
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
                // The Node can hook into this to respond if we have it
            }
            GossipMessage::IHave {
                blake3,
                store_path,
                nar_size,
            } => {
                debug!(
                    "Peer {} has {} (response to Want)",
                    from,
                    hex::encode(&blake3[..8])
                );
                let mut cache = provider_cache.lock().unwrap();
                cache.add_provider(
                    blake3,
                    ProviderInfo {
                        endpoint_id: from,
                        store_path,
                        nar_size,
                    },
                );
            }
            GossipMessage::NeedBuilder {
                requester,
                system_features,
            } => {
                let Ok(requester_id) = EndpointId::from_bytes(&requester) else {
                    return Err(Error::Protocol("invalid requester ID".into()));
                };
                info!(
                    "Received NeedBuilder from {} with features: {:?}",
                    requester_id, system_features
                );

                let info = RequesterInfo {
                    endpoint_id: requester_id,
                    system_features,
                };

                // Add to cache
                {
                    let mut cache = requester_cache.lock().unwrap();
                    cache.add_requester(info.clone());
                }

                // Notify callback
                if let Some(callback) = need_builder_callback {
                    callback(info);
                }
            }
            GossipMessage::BuildComplete(result) => {
                debug!("Build completed: {}", result.drv_hash);
                // Add outputs to provider cache
                {
                    let mut cache = provider_cache.lock().unwrap();
                    let Ok(builder_id) = EndpointId::from_bytes(&result.builder) else {
                        return Err(Error::Protocol("invalid builder ID".into()));
                    };
                    for output in &result.outputs {
                        cache.add_provider(
                            output.blake3,
                            ProviderInfo {
                                endpoint_id: builder_id,
                                store_path: output.store_path.clone(),
                                nar_size: output.nar_size,
                            },
                        );
                    }
                }

                // Notify callback
                if let Some(callback) = build_complete_callback {
                    callback(result);
                }
            }
            GossipMessage::GcWarning { blake3, store_path } => {
                info!(
                    "Peer {} will GC {} ({})",
                    from,
                    store_path,
                    hex::encode(&blake3[..8])
                );
                // Remove from provider cache since it's going away
                let mut cache = provider_cache.lock().unwrap();
                if let Some(providers) = cache.providers.get_mut(&blake3) {
                    providers.retain(|p| p.endpoint_id != from);
                }
            }
        }

        Ok(())
    }

    /// Announce that we have a store path
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
            .map_err(|e| Error::Gossip(format!("broadcast failed: {}", e)))?;
        debug!("Announced: {} ({})", store_path, blake3);
        Ok(())
    }

    /// Query for a store path
    pub async fn query_want(&self, blake3: Blake3Hash) -> Result<()> {
        let msg = GossipMessage::Want {
            blake3: *blake3.as_bytes(),
        };
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Gossip(format!("broadcast failed: {}", e)))?;
        debug!("Queried for: {}", blake3);
        Ok(())
    }

    /// Announce that we need builders with certain features
    pub async fn announce_need_builder(&self, system_features: Vec<String>) -> Result<()> {
        let msg = GossipMessage::NeedBuilder {
            requester: *self.our_id.as_bytes(),
            system_features: system_features.clone(),
        };
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Gossip(format!("broadcast failed: {}", e)))?;
        info!(
            "Announced need builder with features: {:?}",
            system_features
        );
        Ok(())
    }

    /// Announce a build completion
    pub async fn announce_build_complete(&self, result: BuildResult) -> Result<()> {
        let msg = GossipMessage::BuildComplete(result.clone());
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Gossip(format!("broadcast failed: {}", e)))?;
        info!("Announced build complete: {}", result.drv_hash);
        Ok(())
    }

    /// Announce intention to garbage collect an artifact
    pub async fn announce_gc_warning(&self, blake3: Blake3Hash, store_path: &str) -> Result<()> {
        let msg = GossipMessage::GcWarning {
            blake3: *blake3.as_bytes(),
            store_path: store_path.to_string(),
        };
        self.sender
            .broadcast(msg.to_bytes()?)
            .await
            .map_err(|e| Error::Gossip(format!("broadcast failed: {}", e)))?;
        debug!("Announced GC warning: {} ({})", store_path, blake3);
        Ok(())
    }

    /// Get known providers for a hash
    pub fn get_providers(&self, blake3: &Blake3Hash) -> Vec<ProviderInfo> {
        let cache = self.provider_cache.lock().unwrap();
        cache
            .get_providers(blake3.as_bytes())
            .cloned()
            .unwrap_or_default()
    }

    /// Get requesters that match our system features
    pub fn get_matching_requesters(&self, our_features: &[String]) -> Vec<RequesterInfo> {
        let cache = self.requester_cache.lock().unwrap();
        cache.get_matching_requesters(our_features)
    }

    /// Get all requesters needing builders
    pub fn get_all_requesters(&self) -> Vec<RequesterInfo> {
        let cache = self.requester_cache.lock().unwrap();
        cache.all_requesters()
    }

    /// Get the gossip instance
    pub fn gossip(&self) -> &Gossip {
        &self.gossip
    }

    /// Get the topic ID
    pub fn topic_id(&self) -> TopicId {
        self.topic_id
    }

    /// Our endpoint ID
    pub fn our_id(&self) -> EndpointId {
        self.our_id
    }

    /// Add bootstrap peers
    pub async fn add_peers(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.sender
            .join_peers(peers)
            .await
            .map_err(|e| Error::Gossip(format!("join_peers failed: {}", e)))?;
        Ok(())
    }

    /// Shutdown the gossip service
    pub async fn shutdown(self) -> Result<()> {
        self.gossip
            .shutdown()
            .await
            .map_err(|e| Error::Gossip(format!("shutdown failed: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gossip_message_roundtrip() {
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
    fn test_need_builder_message() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let endpoint_id = secret_key.public();

        let msg = GossipMessage::NeedBuilder {
            requester: *endpoint_id.as_bytes(),
            system_features: vec!["x86_64-linux".to_string(), "kvm".to_string()],
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::NeedBuilder {
                requester,
                system_features,
            } => {
                assert_eq!(requester, *endpoint_id.as_bytes());
                assert_eq!(system_features.len(), 2);
                assert!(system_features.contains(&"x86_64-linux".to_string()));
                assert!(system_features.contains(&"kvm".to_string()));
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_provider_cache() {
        use iroh_base::SecretKey;

        let mut cache = ProviderCache::new();
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

        // Remove provider
        cache.remove_provider(&endpoint_id);
        assert_eq!(cache.get_providers(&hash).unwrap().len(), 0);
    }

    #[test]
    fn test_requester_cache() {
        use iroh_base::SecretKey;

        let mut cache = RequesterCache::new();
        let secret_key = SecretKey::generate(&mut rand::rng());
        let endpoint_id = secret_key.public();

        cache.add_requester(RequesterInfo {
            endpoint_id,
            system_features: vec!["x86_64-linux".to_string(), "kvm".to_string()],
        });

        // Match with all features
        let matching = cache.get_matching_requesters(&[
            "x86_64-linux".to_string(),
            "kvm".to_string(),
            "big-parallel".to_string(),
        ]);
        assert_eq!(matching.len(), 1);

        // No match if missing a feature
        let matching = cache.get_matching_requesters(&["x86_64-linux".to_string()]);
        assert_eq!(matching.len(), 0);

        // Remove requester
        cache.remove_requester(&endpoint_id);
        assert_eq!(cache.all_requesters().len(), 0);
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
            store_path: "/nix/store/abc123-package".to_string(),
            nar_size: 98765,
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::IHave {
                blake3,
                store_path,
                nar_size,
            } => {
                assert_eq!(blake3, [55u8; 32]);
                assert_eq!(store_path, "/nix/store/abc123-package");
                assert_eq!(nar_size, 98765);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_message_build_complete_roundtrip() {
        use crate::build::{BuildOutput, BuildResult, DrvHash, JobId};

        let result = BuildResult {
            job_id: JobId(42),
            drv_hash: DrvHash([77u8; 32]),
            builder: [1u8; 32],
            outputs: vec![
                BuildOutput {
                    store_path: "/nix/store/output1".to_string(),
                    blake3: [2u8; 32],
                    sha256: [3u8; 32],
                    nar_size: 1000,
                },
                BuildOutput {
                    store_path: "/nix/store/output2".to_string(),
                    blake3: [4u8; 32],
                    sha256: [5u8; 32],
                    nar_size: 2000,
                },
            ],
            signature_r: [6u8; 32],
            signature_s: [7u8; 32],
        };
        let msg = GossipMessage::BuildComplete(result);

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::BuildComplete(res) => {
                assert_eq!(res.drv_hash, DrvHash([77u8; 32]));
                assert_eq!(res.outputs.len(), 2);
                assert_eq!(res.outputs[0].store_path, "/nix/store/output1");
                assert_eq!(res.outputs[1].nar_size, 2000);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn test_gossip_message_gc_warning_roundtrip() {
        let msg = GossipMessage::GcWarning {
            blake3: [88u8; 32],
            store_path: "/nix/store/old-package".to_string(),
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = GossipMessage::from_bytes(&bytes).unwrap();

        match decoded {
            GossipMessage::GcWarning { blake3, store_path } => {
                assert_eq!(blake3, [88u8; 32]);
                assert_eq!(store_path, "/nix/store/old-package");
            }
            _ => panic!("wrong message type"),
        }
    }
}
