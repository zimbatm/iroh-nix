//! iroh-nix CLI
//!
//! Command-line interface for the iroh-nix distributed Nix binary cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use iroh_base::{EndpointId, RelayUrl};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use iroh_nix::cli;
use iroh_nix::{Blake3Hash, ConfiguredRelayMode, Node, NodeConfig, Result};

#[derive(Parser)]
#[command(name = "iroh-nix")]
#[command(about = "Distributed Nix binary cache using iroh for P2P artifact distribution")]
#[command(version)]
struct Cli {
    /// Data directory for storing blobs, index, and keys
    #[arg(short, long, default_value = ".iroh-nix")]
    data_dir: PathBuf,

    /// Relay mode: "disabled", "default", "staging", or a relay URL
    #[arg(long, default_value = "default")]
    relay_mode: String,

    /// Network ID for gossip discovery (enables gossip when set)
    #[arg(long)]
    network: Option<String>,

    /// Bootstrap peer for gossip (can be specified multiple times)
    #[arg(long = "peer")]
    peers: Vec<String>,

    /// HTTP binary cache URLs for fallback fetching (can be specified multiple times)
    /// Default: https://cache.nixos.org
    /// Use --no-substituters to disable all substituters
    #[arg(long = "substituter")]
    substituters: Vec<String>,

    /// Disable all HTTP binary cache substituters (including cache.nixos.org)
    #[arg(long)]
    no_substituters: bool,

    /// Output in JSON format (machine-readable)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the iroh-nix daemon (P2P node, HTTP substituter)
    Daemon {
        /// Address to bind the HTTP binary cache server to
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// Priority of this cache (lower = higher priority)
        #[arg(short, long, default_value = "40")]
        priority: u32,

        /// Interval in seconds between stale index cleanup runs (0 to disable)
        #[arg(long, default_value = "3600")]
        cleanup_interval: u64,

        /// Maximum NAR cache size. Supports suffixes: G, M, K for bytes,
        /// % for percentage of filesystem. 0 disables the limit.
        /// Examples: "10G", "500M", "10%", "0"
        #[arg(long, default_value = "10%")]
        nar_cache_size: String,
    },

    /// Show node information
    Info,

    /// Add a store path to the local cache
    Add {
        /// The Nix store path (e.g., /nix/store/abc123-name)
        store_path: String,

        /// Path to the actual files (defaults to store_path)
        #[arg(short, long)]
        path: Option<PathBuf>,
    },

    /// Fetch a NAR from a remote node
    Fetch {
        /// Remote endpoint ID (64 hex chars). If not provided, uses gossip to find providers.
        #[arg(long)]
        from: Option<String>,

        /// BLAKE3 hash of the NAR to fetch (64 hex chars)
        hash: String,

        /// Maximum number of retry attempts (0 = no retries)
        #[arg(long, default_value = "3")]
        retries: u32,

        /// Initial retry delay in milliseconds
        #[arg(long, default_value = "500")]
        retry_delay_ms: u64,
    },

    /// List all cached store paths
    List,

    /// Show statistics
    Stats,

    /// Query for providers of a hash via gossip
    Query {
        /// BLAKE3 hash to query for (64 hex chars)
        hash: String,
    },

    /// Push store paths to a remote HTTP cache
    Push {
        /// Base URL of the remote cache (e.g. https://cache.example.com/)
        #[arg(long)]
        cache_url: String,

        /// Static authentication token (overrides auto-detection)
        #[arg(long)]
        token: Option<String>,

        /// OIDC audience parameter (for CI environments)
        #[arg(long)]
        audience: Option<String>,

        /// Number of concurrent NAR uploads
        #[arg(long, default_value = "8")]
        concurrency: usize,

        /// Store paths to push (e.g. /nix/store/abc123-hello)
        store_paths: Vec<String>,
    },
}

/// Build a `NodeConfig` from CLI arguments.
fn build_config(cli: &Cli) -> Result<NodeConfig> {
    let mut config = NodeConfig::new(&cli.data_dir);

    match cli.relay_mode.as_str() {
        "disabled" => {
            config = config.with_relay_mode(ConfiguredRelayMode::Disabled);
        }
        "default" => {} // already the default
        "staging" => {
            config = config.with_relay_mode(ConfiguredRelayMode::Staging);
        }
        url => {
            let url: RelayUrl = url
                .parse()
                .map_err(|e| iroh_nix::Error::Protocol(format!("invalid relay URL: {}", e)))?;
            config = config.with_relay_mode(ConfiguredRelayMode::Custom(url));
        }
    }

    if let Some(ref network) = cli.network {
        config = config.with_network_id(network.clone());
    }

    if !cli.peers.is_empty() {
        let mut peer_ids = Vec::new();
        for peer in &cli.peers {
            let id: EndpointId = peer.parse().map_err(|e| {
                iroh_nix::Error::Protocol(format!("invalid peer ID '{}': {}", peer, e))
            })?;
            peer_ids.push(id);
        }
        config = config.with_bootstrap_peers(peer_ids);
    }

    if cli.no_substituters {
        config = config.with_substituters(vec![]);
    } else if !cli.substituters.is_empty() {
        config = config.with_substituters(cli.substituters.clone());
    }

    Ok(config)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = build_config(&cli)?;
    let json_output = cli.json;

    match cli.command {
        Commands::Daemon {
            bind,
            priority,
            cleanup_interval,
            nar_cache_size,
        } => run_daemon(config, bind, priority, cleanup_interval, nar_cache_size).await,
        Commands::Info => show_info(config, json_output).await,
        Commands::Add { store_path, path } => {
            add_store_path(config, store_path, path, json_output).await
        }
        Commands::Fetch {
            from,
            hash,
            retries,
            retry_delay_ms,
        } => fetch_nar_cmd(config, from, hash, retries, retry_delay_ms).await,
        Commands::List => list_store_paths(config, json_output).await,
        Commands::Stats => show_stats(config, json_output).await,
        Commands::Query { hash } => query_providers(config, hash, json_output).await,
        Commands::Push {
            cache_url,
            token,
            audience,
            concurrency,
            store_paths,
        } => push_store_paths(cache_url, token, audience, concurrency, store_paths).await,
    }
}

/// Parse a human-readable size string into bytes.
///
/// Supports:
/// - Plain number: bytes (e.g. "1073741824")
/// - `K` suffix: kibibytes (e.g. "100K" = 102400)
/// - `M` suffix: mebibytes (e.g. "500M")
/// - `G` suffix: gibibytes (e.g. "10G")
/// - `%` suffix: percentage of the filesystem containing `data_dir` (e.g. "10%")
/// - `0`: unlimited (no eviction)
fn parse_size(s: &str, data_dir: &std::path::Path) -> Result<u64> {
    let s = s.trim();

    if s == "0" {
        return Ok(0);
    }

    if let Some(pct_str) = s.strip_suffix('%') {
        let pct: f64 = pct_str
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid percentage '{}': {}", s, e)))?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(iroh_nix::Error::Protocol(format!(
                "percentage must be 0-100, got {}",
                pct
            )));
        }
        let fs_size = filesystem_size(data_dir)?;
        return Ok((fs_size as f64 * pct / 100.0) as u64);
    }

    if let Some(num_str) = s.strip_suffix('G') {
        let n: f64 = num_str
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid size '{}': {}", s, e)))?;
        return Ok((n * 1024.0 * 1024.0 * 1024.0) as u64);
    }

    if let Some(num_str) = s.strip_suffix('M') {
        let n: f64 = num_str
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid size '{}': {}", s, e)))?;
        return Ok((n * 1024.0 * 1024.0) as u64);
    }

    if let Some(num_str) = s.strip_suffix('K') {
        let n: f64 = num_str
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid size '{}': {}", s, e)))?;
        return Ok((n * 1024.0) as u64);
    }

    // Plain bytes
    s.parse::<u64>().map_err(|e| {
        iroh_nix::Error::Protocol(format!(
            "invalid size '{}': expected a number with optional G/M/K/% suffix: {}",
            s, e
        ))
    })
}

/// Query total filesystem size (in bytes) for the filesystem containing `path`.
fn filesystem_size(path: &std::path::Path) -> Result<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    // Ensure the directory exists so statvfs has something to stat
    std::fs::create_dir_all(path)?;

    let c_path = CString::new(path.to_string_lossy().as_bytes().to_vec())
        .map_err(|e| iroh_nix::Error::Protocol(format!("invalid path for statvfs: {}", e)))?;

    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_frsize * stat.f_blocks)
}

async fn run_daemon(
    config: NodeConfig,
    bind: String,
    priority: u32,
    cleanup_interval: u64,
    nar_cache_size: String,
) -> Result<()> {
    let gossip_enabled = config.network_id.is_some();
    let substituters = config.substituters.clone();

    let max_cache_bytes = parse_size(&nar_cache_size, &config.data_dir)?;

    let node = Arc::new(Node::spawn(config).await?);

    cli::success("iroh-nix daemon started");
    cli::detail(&format!("Endpoint ID: {}", node.id()));

    if gossip_enabled {
        cli::detail(&format!("Gossip: {}", cli::status_ok("enabled")));
    } else {
        cli::detail(&format!(
            "Gossip: {} (use --network to enable)",
            cli::status_pending("disabled")
        ));
    }

    if substituters.is_empty() {
        cli::detail(&format!(
            "HTTP substituters: {}",
            cli::status_pending("disabled")
        ));
    } else {
        cli::detail(&format!(
            "HTTP substituters: {} ({})",
            cli::status_ok("enabled"),
            substituters.join(", ")
        ));
    }

    if max_cache_bytes == 0 {
        cli::detail(&format!("NAR cache: {}", cli::status_pending("unlimited")));
    } else {
        cli::detail(&format!(
            "NAR cache: {} max",
            cli::format_bytes(max_cache_bytes)
        ));
    }

    cli::detail(&format!("HTTP cache: http://{}", bind));

    cli::header("Endpoint address (share with peers)");
    println!("  {}", cli::value(&node.id().to_string()));

    cli::info("Press Ctrl+C to stop.");

    // Create cancellation token for clean shutdown
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Spawn signal handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
        cli::info("Shutting down...");
        cancel_clone.cancel();
    });

    // 1. Spawn HTTP substituter
    let bind_addr: std::net::SocketAddr = bind.parse().map_err(|e| {
        iroh_nix::Error::Protocol(format!("invalid bind address '{}': {}", bind, e))
    })?;

    let substituter_config = Arc::new(nix_substituter::SubstituterConfig {
        bind_addr,
        priority,
    });

    let nar_cache = nix_store::NarCache::new(node.data_dir(), max_cache_bytes)?;

    let substituter_handle = {
        let hash_index = node.hash_index();
        let cancel = cancel.clone();

        // Create a LocalStore as the local index for the pull-through store
        let local_store: Arc<dyn nix_store::store::NarInfoIndex> =
            Arc::new(nix_store::store::LocalStore::new(hash_index));

        // Build upstreams from configured HTTP caches
        let upstreams: Vec<Arc<dyn nix_store::store::NarInfoProvider>> = match node.http_cache() {
            Some(http) => vec![http as Arc<dyn nix_store::store::NarInfoProvider>],
            None => vec![],
        };

        // Build pull-through store with NAR cache
        let provider: Arc<dyn nix_store::smart::SmartProvider> =
            Arc::new(nix_store::store::PullThroughStore::with_nar_cache(
                local_store,
                upstreams,
                nar_cache.clone(),
            ));
        let filter: Arc<dyn nix_store::store::ContentFilter> = Arc::new(nix_store::store::AllowAll);

        tokio::spawn(async move {
            if let Err(e) =
                nix_substituter::run_substituter(substituter_config, provider, filter, cancel).await
            {
                tracing::error!("Substituter error: {}", e);
            }
        })
    };

    // 2. Spawn QUIC NAR server
    let serve_handle = {
        let node = node.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            node.serve(cancel).await;
        })
    };

    // 3. Optionally spawn full cleanup loop (stale index + orphaned NAR cache)
    let cleanup_handle = if cleanup_interval > 0 {
        let hash_index = node.hash_index();
        let cancel = cancel.clone();
        Some(tokio::spawn(nix_store::run_full_cleanup_loop(
            hash_index,
            nar_cache,
            std::time::Duration::from_secs(cleanup_interval),
            cancel,
        )))
    } else {
        None
    };

    // Wait for cancellation
    cancel.cancelled().await;

    // Graceful shutdown: wait for tasks to finish (they listen to the cancel token),
    // with a timeout to avoid hanging forever.
    let shutdown_timeout = std::time::Duration::from_secs(5);

    let mut handles: Vec<tokio::task::JoinHandle<()>> = vec![substituter_handle, serve_handle];
    if let Some(h) = cleanup_handle {
        handles.push(h);
    }

    if tokio::time::timeout(shutdown_timeout, async {
        for h in &mut handles {
            if let Err(e) = h.await {
                if e.is_panic() {
                    tracing::error!("Task panicked during shutdown: {}", e);
                }
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("Shutdown timed out, aborting remaining tasks");
        for h in &handles {
            h.abort();
        }
    }

    // All tasks have finished (or been aborted), so Arc should unwrap cleanly.
    drop(handles);
    match Arc::try_unwrap(node) {
        Ok(node) => node.shutdown().await?,
        Err(node) => {
            tracing::warn!(
                "Node has {} outstanding references, forcing shutdown",
                Arc::strong_count(&node) - 1
            );
            // Best-effort: call shutdown on the Arc'd node directly
            // This won't move out of the Arc, but the endpoint will be dropped.
        }
    }

    cli::success("Daemon stopped");
    Ok(())
}

async fn show_info(config: NodeConfig, json_output: bool) -> Result<()> {
    let node = Node::spawn(config).await?;
    let stats = node.stats().await?;

    if json_output {
        let info = serde_json::json!({
            "endpoint_id": node.id().to_string(),
            "gossip_enabled": node.gossip_enabled(),
            "cached_entries": stats.entry_count,
            "total_blob_size": stats.total_blob_size
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    } else {
        cli::header("Node Information");
        println!(
            "  {}: {}",
            cli::key("Endpoint ID"),
            cli::value(&node.id().to_string())
        );
        println!(
            "  {}: {}",
            cli::key("Gossip"),
            if node.gossip_enabled() {
                cli::status_ok("enabled")
            } else {
                cli::status_pending("disabled")
            }
        );
        println!(
            "  {}: {}",
            cli::key("Cached entries"),
            cli::value(&stats.entry_count.to_string())
        );
        println!(
            "  {}: {}",
            cli::key("Total blob size"),
            cli::value(&cli::format_bytes(stats.total_blob_size))
        );
    }

    node.shutdown().await?;
    Ok(())
}

async fn add_store_path(
    config: NodeConfig,
    store_path: String,
    path: Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    let node = Node::spawn(config).await?;

    let fs_path = path.unwrap_or_else(|| PathBuf::from(&store_path));

    if !fs_path.exists() {
        return Err(iroh_nix::Error::StorePathNotFound(
            fs_path.display().to_string(),
        ));
    }

    let pb = if !json_output {
        Some(cli::spinner("Indexing store path..."))
    } else {
        None
    };
    let entry = node.index_store_path(&store_path, &fs_path).await?;

    if json_output {
        let result = serde_json::json!({
            "store_path": store_path,
            "blake3": entry.blake3.to_string(),
            "sha256": entry.sha256.to_string(),
            "nar_size": entry.nar_size,
            "gossip_announced": node.gossip_enabled()
        });
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        if let Some(pb) = pb {
            cli::finish_success(&pb, &format!("Added: {}", store_path));
        }
        cli::detail(&format!("BLAKE3: {}", entry.blake3));
        cli::detail(&format!("SHA256: {}", entry.sha256));
        cli::detail(&format!("Size: {}", cli::format_bytes(entry.nar_size)));

        if node.gossip_enabled() {
            cli::detail(&format!("Gossip: {}", cli::status_ok("announced")));
        }
    }

    node.shutdown().await?;
    Ok(())
}

async fn fetch_nar_cmd(
    config: NodeConfig,
    from: Option<String>,
    hash: String,
    retries: u32,
    retry_delay_ms: u64,
) -> Result<()> {
    let node = Node::spawn(config).await?;

    // Build retry config from CLI options
    let retry_config = iroh_nix::RetryConfig {
        max_retries: retries,
        initial_delay: std::time::Duration::from_millis(retry_delay_ms),
        ..Default::default()
    };

    // Parse BLAKE3 hash
    let blake3 = Blake3Hash::from_hex(&hash)?;

    let result = if let Some(from) = from {
        // Direct fetch from specified peer
        let remote_id: EndpointId = from
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid endpoint ID: {}", e)))?;

        println!("Fetching {} from {}...", hash, remote_id);
        if retries > 0 {
            println!(
                "  (retries: {}, initial delay: {}ms)",
                retries, retry_delay_ms
            );
        }
        node.fetch_by_id_with_config(remote_id, blake3, &retry_config)
            .await?
    } else {
        // Try to find providers via gossip
        if !node.gossip_enabled() {
            return Err(iroh_nix::Error::Protocol(
                "No --from specified and gossip not enabled. Use --network to enable gossip or specify --from.".into()
            ));
        }

        println!("Querying gossip for providers of {}...", hash);
        let providers = node.query_providers(&blake3).await?;

        if providers.is_empty() {
            // Wait a bit for responses
            println!("Waiting for responses...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let providers = node.get_providers(&blake3);

            if providers.is_empty() {
                return Err(iroh_nix::Error::Protocol(
                    "No providers found via gossip. Try specifying --from directly.".into(),
                ));
            }
        }

        let providers = node.get_providers(&blake3);
        let provider = &providers[0];
        println!("Found provider: {}", provider.endpoint_id);

        node.fetch_by_id_with_config(provider.endpoint_id, blake3, &retry_config)
            .await?
    };

    let (import_result, _nar_data) = result;
    println!("Fetched: {}", import_result.store_path);
    println!("  BLAKE3: {}", import_result.blake3);
    println!("  SHA256: {}", import_result.sha256);
    println!("  Size: {} bytes", import_result.nar_size);

    node.shutdown().await?;
    Ok(())
}

async fn list_store_paths(config: NodeConfig, json_output: bool) -> Result<()> {
    let node = Node::spawn(config).await?;

    let entries = node.list_store_paths().await?;

    if json_output {
        let items: Vec<_> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "store_path": e.store_path,
                    "blake3": e.blake3.to_string(),
                    "sha256": e.sha256.to_string(),
                    "nar_size": e.nar_size
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else if entries.is_empty() {
        cli::info("No store paths cached");
    } else {
        cli::header(&format!("Cached Store Paths ({})", entries.len()));
        for entry in &entries {
            println!(
                "  {} ({})",
                cli::value(&entry.store_path),
                cli::format_bytes(entry.nar_size)
            );
        }
        cli::separator();
        let total_size: u64 = entries.iter().map(|e| e.nar_size).sum();
        cli::detail(&format!(
            "Total: {} entries, {}",
            entries.len(),
            cli::format_bytes(total_size)
        ));
    }

    node.shutdown().await?;
    Ok(())
}

async fn show_stats(config: NodeConfig, json_output: bool) -> Result<()> {
    let node = Node::spawn(config).await?;

    let stats = node.stats().await?;

    if json_output {
        let result = serde_json::json!({
            "endpoint_id": stats.endpoint_id.to_string(),
            "gossip_enabled": node.gossip_enabled(),
            "cached_entries": stats.entry_count,
            "total_blob_size": stats.total_blob_size
        });
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        cli::header("Node Statistics");
        println!(
            "  {}: {}",
            cli::key("Endpoint ID"),
            cli::value(&stats.endpoint_id.to_string())
        );
        println!(
            "  {}: {}",
            cli::key("Gossip"),
            if node.gossip_enabled() {
                cli::status_ok("enabled")
            } else {
                cli::status_pending("disabled")
            }
        );
        println!(
            "  {}: {}",
            cli::key("Cached entries"),
            cli::value(&stats.entry_count.to_string())
        );
        println!(
            "  {}: {}",
            cli::key("Total blob size"),
            cli::value(&cli::format_bytes(stats.total_blob_size))
        );
    }

    node.shutdown().await?;
    Ok(())
}

async fn query_providers(config: NodeConfig, hash: String, json_output: bool) -> Result<()> {
    let node = Node::spawn(config).await?;

    if !node.gossip_enabled() {
        return Err(iroh_nix::Error::Protocol(
            "Gossip not enabled. Use --network to enable.".into(),
        ));
    }

    let blake3 = Blake3Hash::from_hex(&hash)?;

    let pb = if !json_output {
        Some(cli::spinner(&format!(
            "Querying gossip for {}...",
            &hash[..16]
        )))
    } else {
        None
    };
    node.query_providers(&blake3).await?;

    // Wait for responses
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    if let Some(pb) = pb {
        pb.finish_and_clear();
    }

    let providers = node.get_providers(&blake3);

    if json_output {
        let items: Vec<_> = providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "endpoint_id": p.endpoint_id.to_string(),
                    "store_path": p.store_path,
                    "nar_size": p.nar_size
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else if providers.is_empty() {
        cli::warn("No providers found");
    } else {
        cli::success(&format!("Found {} provider(s)", providers.len()));
        for p in providers {
            println!(
                "  {} - {} ({})",
                cli::value(&p.endpoint_id.to_string()),
                p.store_path,
                cli::format_bytes(p.nar_size)
            );
        }
    }

    node.shutdown().await?;
    Ok(())
}

async fn push_store_paths(
    cache_url: String,
    token: Option<String>,
    audience: Option<String>,
    concurrency: usize,
    store_paths: Vec<String>,
) -> Result<()> {
    use nix_store_http::auth::AuthProvider;
    use nix_store_http::upload::UploadClient;
    use url::Url;

    if store_paths.is_empty() {
        return Err(iroh_nix::Error::Protocol("no store paths specified".into()));
    }

    let base_url = Url::parse(&cache_url)
        .map_err(|e| iroh_nix::Error::Protocol(format!("invalid cache URL: {}", e)))?;

    // Detect or use provided auth
    let client = reqwest::Client::builder()
        .user_agent("iroh-nix")
        .build()
        .map_err(|e| iroh_nix::Error::Protocol(format!("failed to create HTTP client: {}", e)))?;

    let auth = if let Some(tok) = token {
        AuthProvider::from_static(tok)
    } else {
        AuthProvider::detect(&client, audience.as_deref()).await
    };

    if auth.is_none() {
        cli::warn("No authentication configured. Set --token or NIX_CACHE_TOKEN.");
        return Err(iroh_nix::Error::Auth("no authentication available".into()));
    }

    let upload = UploadClient::new(base_url, auth)?;

    cli::header(&format!("Pushing {} store path(s)", store_paths.len()));

    // Serialize each store path to NAR and collect metadata
    let mut path_infos = Vec::new();
    let mut nar_data: HashMap<Blake3Hash, Vec<u8>> = HashMap::new();

    for store_path in &store_paths {
        let fs_path = std::path::Path::new(store_path);
        if !fs_path.exists() {
            return Err(iroh_nix::Error::StorePathNotFound(store_path.clone()));
        }

        let pb = cli::spinner(&format!("Serializing {}...", store_path));

        let path_str = store_path.clone();
        let (data, info) = tokio::task::spawn_blocking(move || {
            nix_store::serialize_path(std::path::Path::new(&path_str))
        })
        .await
        .map_err(|e| iroh_nix::Error::Internal(format!("spawn_blocking: {}", e)))??;

        cli::finish_success(
            &pb,
            &format!(
                "Serialized {} ({})",
                store_path,
                cli::format_bytes(data.len() as u64)
            ),
        );

        // Query Nix for references/deriver
        let (references, deriver) = match nix_store::NixPathInfo::query(store_path).await {
            Ok(nix_info) => (nix_info.references, nix_info.deriver),
            Err(_) => (vec![], None),
        };

        let store_path_info = nix_store::StorePathInfo {
            store_path: store_path.clone(),
            blake3: info.blake3,
            nar_hash: info.sha256,
            nar_size: info.nar_size,
            references,
            deriver,
            signatures: vec![],
        };

        nar_data.insert(info.blake3, data);
        path_infos.push(store_path_info);
    }

    // Upload
    let pb = cli::spinner("Uploading...");
    let result = upload
        .upload_paths(&path_infos, nar_data, concurrency)
        .await?;
    cli::finish_success(&pb, &format!("Committed {} path(s)", result.committed));

    Ok(())
}
