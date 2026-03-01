//! iroh-nix Node: Main daemon that runs on each machine
//!
//! The Node manages:
//! - iroh Endpoint for P2P connectivity with NAT traversal
//! - Hash index for BLAKE3 <-> SHA256 <-> store path translation
//! - Protocol handlers for NAR requests (on-demand NAR generation)
//! - Build queue (requester-side) for distributed builds

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use iroh::address_lookup::MdnsAddressLookup;
use iroh::endpoint::Builder;
use iroh::{Endpoint, RelayMode};
use iroh_base::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
use iroh_gossip::net::GOSSIP_ALPN;
use tracing::{debug, info, warn};

use crate::build::BuildQueue;
use crate::builder::handle_build_queue_accepted;
use crate::error::MutexExt;
use crate::gossip::{GossipService, ProviderInfo};
use crate::hash_index::{Blake3Hash, HashEntry, HashIndex};
use crate::http_cache::{HttpCacheClient, HttpCacheConfig};
use crate::nar::serialize_path_to_writer;
use crate::nix_info::NixPathInfo;
use crate::protocol::{BUILD_QUEUE_PROTOCOL_ALPN, NAR_PROTOCOL_ALPN};
use crate::transfer::handle_nar_accepted;
use crate::{Error, Result};

/// Configuration for an iroh-nix node
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Directory for storing data (blobs, index, etc.)
    pub data_dir: PathBuf,

    /// Secret key for node identity (generated if not provided)
    pub secret_key: Option<SecretKey>,

    /// Relay URL for NAT traversal
    pub relay_url: Option<RelayUrl>,

    /// Bind address for the QUIC endpoint
    pub bind_addr: Option<std::net::SocketAddr>,

    /// Network ID for gossip (nodes with same ID discover each other)
    pub network_id: Option<String>,

    /// Bootstrap peers for gossip
    pub bootstrap_peers: Vec<EndpointId>,

    /// HTTP binary cache URLs for fallback fetching
    pub substituters: Vec<String>,
}

/// Default NixOS binary cache
pub const DEFAULT_SUBSTITUTER: &str = "https://cache.nixos.org";

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(".iroh-nix"),
            secret_key: None,
            relay_url: None,
            bind_addr: None,
            network_id: None,
            bootstrap_peers: Vec::new(),
            substituters: vec![DEFAULT_SUBSTITUTER.to_string()],
        }
    }
}

impl NodeConfig {
    /// Create a new config with the given data directory
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            ..Default::default()
        }
    }

    /// Set the secret key
    pub fn with_secret_key(mut self, key: SecretKey) -> Self {
        self.secret_key = Some(key);
        self
    }

    /// Set the relay URL
    pub fn with_relay_url(mut self, url: RelayUrl) -> Self {
        self.relay_url = Some(url);
        self
    }

    /// Set the network ID for gossip discovery
    pub fn with_network_id(mut self, network_id: impl Into<String>) -> Self {
        self.network_id = Some(network_id.into());
        self
    }

    /// Add bootstrap peers for gossip
    pub fn with_bootstrap_peers(mut self, peers: Vec<EndpointId>) -> Self {
        self.bootstrap_peers = peers;
        self
    }

    /// Set HTTP binary cache URLs for fallback fetching
    pub fn with_substituters(mut self, substituters: Vec<String>) -> Self {
        self.substituters = substituters;
        self
    }
}

/// An iroh-nix node
pub struct Node {
    /// The iroh endpoint for P2P connectivity
    endpoint: Endpoint,

    /// Our secret key
    secret_key: SecretKey,

    /// Hash index for BLAKE3 <-> SHA256 <-> store path translation
    /// Uses std::sync::Mutex since rusqlite::Connection is not Send
    hash_index: Arc<Mutex<HashIndex>>,

    /// Gossip service for peer discovery (optional)
    gossip: Option<Arc<GossipService>>,

    /// Build queue for distributed builds (optional - requires gossip)
    build_queue: Option<Arc<BuildQueue>>,

    /// HTTP cache client for fallback fetching from binary caches
    http_cache: Option<Arc<HttpCacheClient>>,
}

impl Node {
    /// Create and start a new node
    pub async fn spawn(config: NodeConfig) -> Result<Self> {
        // Ensure data directory exists
        std::fs::create_dir_all(&config.data_dir)?;

        // Load or generate secret key
        let secret_key = match &config.secret_key {
            Some(key) => key.clone(),
            None => {
                let key_path = config.data_dir.join("secret.key");
                load_or_generate_key(&key_path)?
            }
        };

        // Build the iroh endpoint
        let relay_mode = if let Some(relay_url) = &config.relay_url {
            RelayMode::Custom(relay_url.clone().into())
        } else {
            RelayMode::Default
        };

        let mut builder = Builder::empty(relay_mode).secret_key(secret_key.clone());

        // Add our custom protocols and gossip
        builder = builder.alpns(vec![
            NAR_PROTOCOL_ALPN.to_vec(),
            BUILD_QUEUE_PROTOCOL_ALPN.to_vec(),
            GOSSIP_ALPN.to_vec(),
        ]);

        // Add mDNS address lookup for local network
        builder = builder.address_lookup(MdnsAddressLookup::builder());

        let endpoint = builder.bind().await.map_err(|e| Error::Iroh(e.into()))?;

        // Open the hash index
        let index_path = config.data_dir.join("hash_index.db");
        let hash_index = HashIndex::open(&index_path)?;

        // Create build queue if gossip will be enabled
        let build_queue = if config.network_id.is_some() {
            Some(Arc::new(BuildQueue::new(endpoint.id(), secret_key.clone())))
        } else {
            None
        };

        // Initialize gossip if network_id is configured
        let gossip = if let Some(ref network_id) = config.network_id {
            let gossip_service =
                GossipService::new(&endpoint, network_id, config.bootstrap_peers.clone()).await?;
            Some(Arc::new(gossip_service))
        } else {
            None
        };

        // Initialize HTTP cache client if substituters are configured
        let http_cache = if !config.substituters.is_empty() {
            let configs: Vec<HttpCacheConfig> = config
                .substituters
                .iter()
                .filter_map(|url| match HttpCacheConfig::new(url) {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        warn!("Invalid substituter URL '{}': {}", url, e);
                        None
                    }
                })
                .collect();

            if configs.is_empty() {
                None
            } else {
                info!(
                    "Configured {} HTTP substituter(s): {:?}",
                    configs.len(),
                    configs.iter().map(|c| c.url.as_str()).collect::<Vec<_>>()
                );
                Some(Arc::new(HttpCacheClient::new(configs)?))
            }
        } else {
            None
        };

        let node = Self {
            endpoint,
            secret_key,
            hash_index: Arc::new(Mutex::new(hash_index)),
            gossip,
            build_queue,
            http_cache,
        };

        info!("Node started with ID: {}", node.endpoint.id());

        Ok(node)
    }

    /// Get the node's public ID
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Get the iroh endpoint
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Get the secret key
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    /// Get the hash index
    pub fn hash_index(&self) -> Arc<Mutex<HashIndex>> {
        Arc::clone(&self.hash_index)
    }

    /// Get the build queue (if enabled)
    pub fn build_queue(&self) -> Option<Arc<BuildQueue>> {
        self.build_queue.clone()
    }

    /// Get the gossip service (if enabled)
    pub fn gossip(&self) -> Option<&Arc<GossipService>> {
        self.gossip.as_ref()
    }

    /// Get the HTTP cache client (if configured)
    pub fn http_cache(&self) -> Option<Arc<HttpCacheClient>> {
        self.http_cache.clone()
    }

    /// Get an Arc clone of the gossip service (if enabled)
    pub fn gossip_arc(&self) -> Option<Arc<GossipService>> {
        self.gossip.clone()
    }

    /// Index a store path (compute hashes for on-demand serving)
    ///
    /// This computes the NAR hashes and updates the hash index.
    /// No NAR blob is stored - NARs are generated on-demand when requested.
    pub async fn index_store_path(&self, store_path: &str, fs_path: &Path) -> Result<HashEntry> {
        // Try to get SHA256 and size from Nix first (faster for known paths)
        let (sha256, nar_size, blake3, references, deriver) =
            match NixPathInfo::query(store_path).await {
                Ok(nix_info) => {
                    // Got metadata from Nix, just need to compute BLAKE3
                    let sha256 = crate::hash_index::Sha256Hash(nix_info.sha256_bytes()?);
                    let nar_size = nix_info.nar_size;
                    let references = nix_info.references.clone();
                    let deriver = nix_info.deriver.clone();

                    // Compute BLAKE3 by serializing to sink (required, can't get from Nix)
                    let info = serialize_path_to_writer(fs_path, std::io::sink())?;
                    (sha256, nar_size, info.blake3, references, deriver)
                }
                Err(_) => {
                    // Fall back to computing everything by serializing
                    let info = serialize_path_to_writer(fs_path, std::io::sink())?;
                    (info.sha256, info.nar_size, info.blake3, vec![], None)
                }
            };

        // Create index entry
        let entry = HashEntry {
            blake3,
            sha256,
            store_path: store_path.to_string(),
            nar_size,
            references,
            deriver,
        };

        // Update hash index
        {
            let index = self.hash_index.lock_or_err()?;
            index.insert(&entry)?;
        }

        // Announce via gossip if enabled
        if let Some(ref gossip) = self.gossip {
            if let Err(e) = gossip
                .announce_have(entry.blake3, store_path, entry.nar_size)
                .await
            {
                warn!("Failed to announce via gossip: {}", e);
            }
        }

        info!(
            "Indexed store path {} (blake3: {}, size: {} bytes)",
            store_path,
            blake3.to_hex(),
            nar_size
        );

        Ok(entry)
    }

    /// Check if a store path exists in the index
    pub fn has_indexed(&self, blake3: &Blake3Hash) -> Result<bool> {
        let index = self.hash_index.lock_or_err()?;
        Ok(index.get_by_blake3(blake3)?.is_some())
    }

    /// Look up a store path in the index
    pub async fn get_by_store_path(&self, store_path: &str) -> Result<Option<HashEntry>> {
        let index = self.hash_index.lock_or_err()?;
        index.get_by_store_path(store_path)
    }

    /// Look up by BLAKE3 hash
    pub async fn get_by_blake3(&self, blake3: &Blake3Hash) -> Result<Option<HashEntry>> {
        let index = self.hash_index.lock_or_err()?;
        index.get_by_blake3(blake3)
    }

    /// List all store paths in the index
    pub async fn list_store_paths(&self) -> Result<Vec<HashEntry>> {
        let index = self.hash_index.lock_or_err()?;
        index.list_all()
    }

    /// Get node statistics
    pub async fn stats(&self) -> Result<NodeStats> {
        let index = self.hash_index.lock_or_err()?;
        let entries = index.list_all()?;

        let total_size: u64 = entries.iter().map(|e| e.nar_size).sum();

        Ok(NodeStats {
            endpoint_id: self.endpoint.id(),
            entry_count: entries.len() as u64,
            total_blob_size: total_size,
        })
    }

    /// Query providers for a hash via gossip
    pub async fn query_providers(&self, blake3: &Blake3Hash) -> Result<Vec<ProviderInfo>> {
        if let Some(ref gossip) = self.gossip {
            gossip.query_want(*blake3).await?;
            Ok(gossip.get_providers(blake3))
        } else {
            Err(Error::Protocol("Gossip not enabled".into()))
        }
    }

    /// Get cached providers for a hash
    pub fn get_providers(&self, blake3: &Blake3Hash) -> Vec<ProviderInfo> {
        if let Some(ref gossip) = self.gossip {
            gossip.get_providers(blake3)
        } else {
            vec![]
        }
    }

    /// Check if gossip is enabled
    pub fn gossip_enabled(&self) -> bool {
        self.gossip.is_some()
    }

    /// Check if build queue is enabled
    pub fn build_enabled(&self) -> bool {
        self.build_queue.is_some()
    }

    /// Push a build job to the queue
    ///
    /// Returns the job ID and whether a NeedBuilder announcement should be made
    pub async fn push_build(
        &self,
        drv_path: &str,
        system_features: Vec<String>,
        outputs: Vec<String>,
        input_paths: Vec<crate::build::InputPath>,
    ) -> Result<(crate::build::JobId, bool)> {
        let build_queue = self
            .build_queue
            .as_ref()
            .ok_or_else(|| Error::Protocol("Build queue not enabled".into()))?;

        let (job_id, should_announce) =
            build_queue.push(drv_path, system_features.clone(), outputs, input_paths);

        // Announce via gossip if we just started needing builders
        if should_announce {
            if let Some(ref gossip) = self.gossip {
                gossip.announce_need_builder(system_features).await?;
            }
        }

        Ok((job_id, should_announce))
    }

    /// Get the node's endpoint address for sharing with other nodes
    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Run the server loop to accept incoming connections
    ///
    /// This runs until the provided cancellation token is triggered.
    pub async fn serve(&self, cancel: tokio_util::sync::CancellationToken) {
        info!("Starting server on {}", self.id());

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Server shutting down");
                    break;
                }
                incoming = self.endpoint.accept() => {
                    match incoming {
                        Some(conn) => {
                            let hash_index = Arc::clone(&self.hash_index);
                            let build_queue = self.build_queue.clone();
                            let gossip = self.gossip.as_ref().map(|g| g.gossip().clone());
                            let http_cache = self.http_cache.clone();

                            tokio::spawn(async move {
                                // Accept the connection first to get the ALPN
                                let accepting = match conn.accept() {
                                    Ok(a) => a,
                                    Err(e) => {
                                        warn!("Failed to accept connection: {}", e);
                                        return;
                                    }
                                };

                                let connection = match accepting.await {
                                    Ok(c) => c,
                                    Err(e) => {
                                        warn!("Connection failed: {}", e);
                                        return;
                                    }
                                };

                                // Route based on ALPN
                                let alpn = connection.alpn();
                                if alpn == GOSSIP_ALPN {
                                    // Gossip protocol
                                    if let Some(gossip) = gossip {
                                        if let Err(e) = gossip.handle_connection(connection).await {
                                            warn!("Error handling gossip connection: {}", e);
                                        }
                                    } else {
                                        warn!("Gossip connection received but gossip not enabled");
                                    }
                                } else if alpn == BUILD_QUEUE_PROTOCOL_ALPN {
                                    // Build queue protocol
                                    if let Some(build_queue) = build_queue {
                                        if let Err(e) = handle_build_queue_accepted(connection, build_queue).await {
                                            warn!("Error handling build queue connection: {}", e);
                                        }
                                    } else {
                                        warn!("Build queue connection received but build queue not enabled");
                                    }
                                } else {
                                    // NAR protocol (default) - streams NAR on-demand from filesystem
                                    // Falls back to HTTP cache if configured and path not found locally
                                    if let Err(e) = handle_nar_accepted(connection, hash_index, http_cache).await {
                                        warn!("Error handling NAR connection: {}", e);
                                    }
                                }
                            });
                        }
                        None => {
                            debug!("Endpoint closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Fetch a NAR from a remote node
    ///
    /// Returns the import result and NAR data for import to nix store.
    pub async fn fetch_from(
        &self,
        remote: EndpointAddr,
        blake3: Blake3Hash,
    ) -> Result<(crate::transfer::ImportResult, bytes::Bytes)> {
        self.fetch_from_with_config(remote, blake3, &crate::retry::RetryConfig::default())
            .await
    }

    /// Fetch a NAR from a remote node and import it with custom retry config
    ///
    /// The NAR is fetched, indexed, and the data returned for import to nix store.
    /// No blob file is stored locally.
    pub async fn fetch_from_with_config(
        &self,
        remote: EndpointAddr,
        blake3: Blake3Hash,
        retry_config: &crate::retry::RetryConfig,
    ) -> Result<(crate::transfer::ImportResult, bytes::Bytes)> {
        // Fetch the NAR with retry
        let (header, nar_data) =
            crate::transfer::fetch_nar_with_config(&self.endpoint, remote, blake3, retry_config)
                .await?;

        // Try to get references/deriver if the path already exists locally
        let (references, deriver) = match NixPathInfo::query(&header.store_path).await {
            Ok(nix_info) => (nix_info.references, nix_info.deriver),
            Err(_) => (vec![], None),
        };

        // Create index entry (no blob file storage)
        let entry = HashEntry {
            blake3,
            sha256: header.sha256(),
            store_path: header.store_path.clone(),
            nar_size: header.size,
            references,
            deriver,
        };

        // Update hash index
        {
            let index = self.hash_index.lock_or_err()?;
            index.insert(&entry)?;
        }

        info!(
            "Fetched store path {} (blake3: {}, size: {} bytes)",
            entry.store_path, blake3, entry.nar_size
        );

        let result = crate::transfer::ImportResult {
            store_path: entry.store_path,
            blake3,
            sha256: entry.sha256,
            nar_size: entry.nar_size,
        };

        Ok((result, nar_data))
    }

    /// Fetch a NAR by connecting to a node by its endpoint ID
    ///
    /// This uses discovery to find the node's address.
    pub async fn fetch_by_id(
        &self,
        remote_id: EndpointId,
        blake3: Blake3Hash,
    ) -> Result<(crate::transfer::ImportResult, bytes::Bytes)> {
        // Create an EndpointAddr from just the ID (relies on discovery)
        let remote = EndpointAddr::from(remote_id);
        self.fetch_from(remote, blake3).await
    }

    /// Fetch a NAR by endpoint ID with custom retry config
    pub async fn fetch_by_id_with_config(
        &self,
        remote_id: EndpointId,
        blake3: Blake3Hash,
        retry_config: &crate::retry::RetryConfig,
    ) -> Result<(crate::transfer::ImportResult, bytes::Bytes)> {
        let remote = EndpointAddr::from(remote_id);
        self.fetch_from_with_config(remote, blake3, retry_config)
            .await
    }

    /// Shutdown the node
    pub async fn shutdown(self) -> Result<()> {
        if let Some(gossip) = self.gossip {
            // Try to get exclusive ownership for clean shutdown
            match Arc::try_unwrap(gossip) {
                Ok(g) => g.shutdown().await?,
                Err(_) => {
                    // Other references exist, just let them drop
                    warn!("Gossip service has other references, skipping clean shutdown");
                }
            }
        }
        self.endpoint.close().await;
        Ok(())
    }
}

/// Statistics about the node
#[derive(Debug, Clone)]
pub struct NodeStats {
    /// Endpoint ID
    pub endpoint_id: EndpointId,
    /// Number of entries in the hash index
    pub entry_count: u64,
    /// Total size of stored blobs
    pub total_blob_size: u64,
}

/// Load or generate a secret key
fn load_or_generate_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        let key_bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Protocol("invalid secret key length".into()))?;
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        std::fs::write(path, key.to_bytes())?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_config(temp_dir: &TempDir) -> NodeConfig {
        NodeConfig::new(temp_dir.path())
    }

    #[tokio::test]
    #[ignore] // Requires network access (mDNS binding)
    async fn test_node_spawn() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_test_config(&temp_dir);
        let node = Node::spawn(config).await.unwrap();

        assert!(!node.gossip_enabled());
        assert!(!node.build_enabled());

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore] // Requires network access (mDNS binding)
    async fn test_add_store_path() {
        let temp_dir = TempDir::new().unwrap();
        let config = create_test_config(&temp_dir);
        let node = Node::spawn(config).await.unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("test-file");
        {
            let mut f = std::fs::File::create(&test_file).unwrap();
            f.write_all(b"Hello, world!").unwrap();
        }

        // Index it
        let entry = node
            .index_store_path("/nix/store/test-hello", &test_file)
            .await
            .unwrap();

        assert_eq!(entry.store_path, "/nix/store/test-hello");
        assert!(entry.nar_size > 0);

        // Verify it's in the index
        let found = node
            .get_by_store_path("/nix/store/test-hello")
            .await
            .unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().blake3, entry.blake3);

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    #[ignore] // Requires network access (mDNS binding)
    async fn test_two_node_transfer() {
        use std::sync::Arc;

        // Create two nodes
        let temp_dir1 = TempDir::new().unwrap();
        let temp_dir2 = TempDir::new().unwrap();

        let node1 = Arc::new(Node::spawn(create_test_config(&temp_dir1)).await.unwrap());
        let node2 = Node::spawn(create_test_config(&temp_dir2)).await.unwrap();

        // Create and add a test file to node1
        let test_file = temp_dir1.path().join("test-file");
        {
            let mut f = std::fs::File::create(&test_file).unwrap();
            f.write_all(b"Test content for transfer").unwrap();
        }

        let entry = node1
            .index_store_path("/nix/store/transfer-test", &test_file)
            .await
            .unwrap();

        // Start node1's server in the background
        let cancel = tokio_util::sync::CancellationToken::new();
        let server_cancel = cancel.clone();
        let server_node = node1.clone();
        let server_handle = tokio::spawn(async move {
            server_node.serve(server_cancel).await;
        });

        // Node2 fetches from node1
        let (result, _nar_data) = node2
            .fetch_from(node1.endpoint_addr(), entry.blake3)
            .await
            .unwrap();

        assert_eq!(result.store_path, "/nix/store/transfer-test");
        assert_eq!(result.blake3, entry.blake3);
        assert_eq!(result.nar_size, entry.nar_size);

        // Stop the server
        cancel.cancel();
        server_handle.abort();

        node2.shutdown().await.unwrap();
        match Arc::try_unwrap(node1) {
            Ok(node) => node.shutdown().await.unwrap(),
            Err(_) => panic!("other references to node1 still exist"),
        }
    }

    #[test]
    fn test_node_config_default() {
        let config = NodeConfig::default();
        assert!(config.secret_key.is_none());
        assert!(config.relay_url.is_none());
        assert!(config.network_id.is_none());
        assert!(config.bootstrap_peers.is_empty());
        // cache.nixos.org is the default substituter
        assert_eq!(config.substituters, vec![DEFAULT_SUBSTITUTER.to_string()]);
    }

    #[test]
    fn test_node_config_new() {
        let config = NodeConfig::new("/custom/data");
        assert_eq!(config.data_dir, std::path::PathBuf::from("/custom/data"));
    }

    #[test]
    fn test_node_config_builder_chain() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let peer_key = SecretKey::generate(&mut rand::rng());

        let config = NodeConfig::new("/data")
            .with_secret_key(secret_key.clone())
            .with_network_id("test-network")
            .with_bootstrap_peers(vec![peer_key.public()]);

        assert!(config.secret_key.is_some());
        assert_eq!(config.network_id, Some("test-network".to_string()));
        assert_eq!(config.bootstrap_peers.len(), 1);
    }

    #[test]
    fn test_load_or_generate_key_creates_new() {
        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("secret.key");

        assert!(!key_path.exists());
        let key = load_or_generate_key(&key_path).unwrap();
        assert!(key_path.exists());

        // Key file should be 32 bytes
        let key_bytes = std::fs::read(&key_path).unwrap();
        assert_eq!(key_bytes.len(), 32);

        // Returned key should match file contents
        assert_eq!(key.to_bytes(), key_bytes.as_slice());
    }

    #[test]
    fn test_load_or_generate_key_loads_existing() {
        use iroh_base::SecretKey;

        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join("secret.key");

        // Create a key file first
        let original_key = SecretKey::generate(&mut rand::rng());
        std::fs::write(&key_path, original_key.to_bytes()).unwrap();

        // Load should return the same key
        let loaded_key = load_or_generate_key(&key_path).unwrap();
        assert_eq!(loaded_key.to_bytes(), original_key.to_bytes());
    }

    #[test]
    fn test_node_stats_struct() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let stats = NodeStats {
            endpoint_id: secret_key.public(),
            entry_count: 42,
            total_blob_size: 1024 * 1024,
        };

        assert_eq!(stats.entry_count, 42);
        assert_eq!(stats.total_blob_size, 1024 * 1024);
    }
}
