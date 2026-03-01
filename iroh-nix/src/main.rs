//! iroh-nix CLI
//!
//! Command-line interface for the iroh-nix distributed Nix build system.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use iroh_base::{EndpointId, RelayUrl};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use iroh_nix::cli;
use iroh_nix::{Blake3Hash, BuilderConfig, BuilderWorker, Node, NodeConfig, Result};

#[derive(Parser)]
#[command(name = "iroh-nix")]
#[command(about = "Distributed Nix build system using iroh for P2P artifact distribution")]
#[command(version)]
struct Cli {
    /// Data directory for storing blobs, index, and keys
    #[arg(short, long, default_value = ".iroh-nix")]
    data_dir: PathBuf,

    /// Relay URL for NAT traversal
    #[arg(long)]
    relay_url: Option<String>,

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
    /// Start the iroh-nix daemon (P2P node, HTTP substituter, optional builder)
    Daemon {
        /// Address to bind the HTTP binary cache server to
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// Priority of this cache (lower = higher priority)
        #[arg(short, long, default_value = "40")]
        priority: u32,

        /// Enable builder mode (connect to requesters and pull jobs)
        #[arg(long)]
        builder: bool,

        /// System features this builder supports (e.g., kvm, big-parallel). Implies --builder.
        #[arg(short, long = "feature")]
        features: Vec<String>,

        /// Stream build logs back to requester (builder mode only)
        #[arg(long)]
        stream_logs: bool,
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

    /// Submit a derivation to the build queue
    BuildPush {
        /// Path to the derivation file (e.g., /nix/store/xxx.drv)
        drv_path: String,

        /// System features required (e.g., x86_64-linux, kvm)
        #[arg(short, long)]
        features: Vec<String>,
    },

    /// Show build queue status
    BuildQueue,

    /// Watch build logs (stream logs from active builds)
    BuildLogs {
        /// Watch logs for a specific job ID (default: all jobs)
        #[arg(long)]
        job: Option<u64>,
    },
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

    // Build config
    let mut config = NodeConfig::new(&cli.data_dir);

    if let Some(relay_url) = cli.relay_url {
        let url: RelayUrl = relay_url
            .parse()
            .map_err(|e| iroh_nix::Error::Protocol(format!("invalid relay URL: {}", e)))?;
        config = config.with_relay_url(url);
    }

    if let Some(network) = cli.network {
        config = config.with_network_id(network);
    }

    if !cli.peers.is_empty() {
        let mut peer_ids = Vec::new();
        for peer in cli.peers {
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
        // CLI substituters replace defaults
        config = config.with_substituters(cli.substituters);
    }
    // Otherwise, use defaults (cache.nixos.org)

    let json_output = cli.json;

    match cli.command {
        Commands::Daemon {
            bind,
            priority,
            builder,
            features,
            stream_logs,
        } => run_daemon(config, bind, priority, builder, features, stream_logs).await,
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
        Commands::BuildPush { drv_path, features } => build_push(config, drv_path, features).await,
        Commands::BuildQueue => show_build_queue(config).await,
        Commands::BuildLogs { job } => watch_build_logs(config, job).await,
    }
}

async fn run_daemon(
    config: NodeConfig,
    bind: String,
    priority: u32,
    builder: bool,
    features: Vec<String>,
    stream_logs: bool,
) -> Result<()> {
    let gossip_enabled = config.network_id.is_some();
    let substituters = config.substituters.clone();
    // --feature implies --builder
    let builder_enabled = builder || !features.is_empty();

    if builder_enabled && !gossip_enabled {
        return Err(iroh_nix::Error::Protocol(
            "Builder mode requires gossip. Use --network to enable gossip.".into(),
        ));
    }

    let node = Arc::new(Node::spawn(config).await?);

    cli::success("iroh-nix daemon started");
    cli::detail(&format!("Endpoint ID: {}", node.id()));

    if gossip_enabled {
        cli::detail(&format!("Gossip: {}", cli::status_ok("enabled")));
        cli::detail(&format!("Build queue: {}", cli::status_ok("enabled")));
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

    cli::detail(&format!("HTTP cache: http://{}", bind));

    if builder_enabled {
        let feat_display = if features.is_empty() {
            "x86_64-linux".to_string()
        } else {
            features.join(", ")
        };
        cli::detail(&format!(
            "Builder: {} (features: {})",
            cli::status_ok("enabled"),
            feat_display
        ));
    } else {
        cli::detail(&format!(
            "Builder: {} (use --builder to enable)",
            cli::status_pending("disabled")
        ));
    }

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

    let substituter_config = iroh_nix::substituter::SubstituterConfig {
        bind_addr,
        priority,
    };

    let substituter_handle = {
        let hash_index = node.hash_index();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                iroh_nix::substituter::run_substituter(substituter_config, hash_index, cancel).await
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

    // 3. Optionally spawn builder worker
    let builder_handle = if builder_enabled {
        let builder_features = if features.is_empty() {
            vec!["x86_64-linux".to_string()]
        } else {
            features
        };

        let builder_config = BuilderConfig {
            system_features: builder_features,
            stream_logs,
        };

        let worker = BuilderWorker::new(
            node.endpoint().clone(),
            node.secret_key().clone(),
            builder_config,
            node.hash_index(),
        );

        let gossip = node
            .gossip()
            .expect("gossip required for builder")
            .clone();
        let cancel = cancel.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = worker.run(&gossip, cancel).await {
                tracing::error!("Builder error: {}", e);
            }
        }))
    } else {
        None
    };

    // 4. Spawn stale index cleanup loop (every hour)
    let cleanup_handle = {
        let hash_index = node.hash_index();
        let cancel = cancel.clone();
        tokio::spawn(iroh_nix::run_stale_cleanup_loop(
            hash_index,
            std::time::Duration::from_secs(3600),
            cancel,
        ))
    };

    // Wait for cancellation
    cancel.cancelled().await;

    // Abort spawned tasks
    substituter_handle.abort();
    serve_handle.abort();
    cleanup_handle.abort();
    if let Some(h) = builder_handle {
        h.abort();
    }

    // Shutdown
    match Arc::try_unwrap(node) {
        Ok(node) => node.shutdown().await?,
        Err(_) => {
            tracing::warn!("Could not cleanly shutdown node (other references exist)");
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
            "build_enabled": node.build_enabled(),
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
            cli::key("Build queue"),
            if node.build_enabled() {
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
            "build_enabled": node.build_enabled(),
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
            cli::key("Build queue"),
            if node.build_enabled() {
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
        return Err(iroh_nix::Error::Gossip(
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

/// Information extracted from a .drv file
#[derive(Debug, Clone)]
struct DrvInfo {
    drv_path: String,
    system: String,
    required_features: Vec<String>,
    outputs: Vec<String>,
    /// Direct input derivations with their required outputs (drv_path -> [output_names])
    input_drvs: std::collections::HashMap<String, Vec<String>>,
    /// Direct input source paths (inputSrcs)
    input_srcs: Vec<String>,
}

/// Result of analyzing a derivation and its dependencies
struct BuildPlan {
    /// Derivations that need to be built, in dependency order (build first items first)
    to_build: Vec<DrvInfo>,
    /// Store paths that already exist and should be added to blob store
    existing_paths: Vec<String>,
}

/// Parse a single derivation from JSON object
fn parse_drv_from_json(drv_path: &str, drv_obj: &serde_json::Value) -> Result<DrvInfo> {
    // Extract system (required)
    let system = drv_obj
        .get("system")
        .and_then(|v| v.as_str())
        .ok_or_else(|| iroh_nix::Error::Protocol(format!("{}: missing 'system' field", drv_path)))?
        .to_string();

    // Extract requiredSystemFeatures from env
    let required_features: Vec<String> = drv_obj
        .get("env")
        .and_then(|env| env.get("requiredSystemFeatures"))
        .and_then(|v| v.as_str())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    // Extract output paths
    let outputs: Vec<String> = drv_obj
        .get("outputs")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.values()
                .filter_map(|v| v.get("path").and_then(|p| p.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Extract input derivations with their output names
    let input_drvs: std::collections::HashMap<String, Vec<String>> = drv_obj
        .get("inputDrvs")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(drv, info)| {
                    let outputs: Vec<String> = info
                        .get("outputs")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_else(|| vec!["out".to_string()]);
                    (drv.clone(), outputs)
                })
                .collect()
        })
        .unwrap_or_default();

    // Extract input source paths
    let input_srcs: Vec<String> = drv_obj
        .get("inputSrcs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(DrvInfo {
        drv_path: drv_path.to_string(),
        system,
        required_features,
        outputs,
        input_drvs,
        input_srcs,
    })
}

/// Parse all derivations recursively using `nix derivation show --recursive`
async fn parse_drvs_recursive(
    drv_path: &str,
) -> Result<std::collections::HashMap<String, DrvInfo>> {
    let output = tokio::process::Command::new("nix")
        .args(["derivation", "show", "--recursive", drv_path])
        .output()
        .await
        .map_err(|e| {
            iroh_nix::Error::Protocol(format!("failed to run 'nix derivation show': {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(iroh_nix::Error::Protocol(format!(
            "nix derivation show failed: {}",
            stderr
        )));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
        iroh_nix::Error::Protocol(format!("failed to parse derivation JSON: {}", e))
    })?;

    let mut drvs = std::collections::HashMap::new();

    if let Some(obj) = json.as_object() {
        for (path, drv_obj) in obj {
            let info = parse_drv_from_json(path, drv_obj)?;
            drvs.insert(path.clone(), info);
        }
    }

    Ok(drvs)
}

/// Check which store paths exist locally
async fn check_existing_paths(paths: &[String]) -> Result<std::collections::HashSet<String>> {
    if paths.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let output = tokio::process::Command::new("nix")
        .arg("path-info")
        .args(paths)
        .output()
        .await
        .map_err(|e| iroh_nix::Error::Protocol(format!("failed to run 'nix path-info': {}", e)))?;

    // nix path-info returns the paths that exist, one per line
    // It exits with error if any path doesn't exist, so we ignore the exit code
    let stdout = String::from_utf8_lossy(&output.stdout);
    let existing: std::collections::HashSet<String> =
        stdout.lines().map(|s| s.trim().to_string()).collect();

    Ok(existing)
}

/// Topological sort of derivations (dependencies first)
fn topological_sort(
    drvs: &std::collections::HashMap<String, DrvInfo>,
    to_build: &std::collections::HashSet<String>,
) -> Vec<String> {
    use std::collections::{HashMap, VecDeque};

    // Build in-degree map (only for derivations we need to build)
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for drv_path in to_build {
        in_degree.entry(drv_path.as_str()).or_insert(0);
        if let Some(info) = drvs.get(drv_path) {
            for dep in info.input_drvs.keys() {
                if to_build.contains(dep) {
                    *in_degree.entry(drv_path.as_str()).or_insert(0) += 1;
                    dependents
                        .entry(dep.as_str())
                        .or_default()
                        .push(drv_path.as_str());
                }
            }
        }
    }

    // Kahn's algorithm
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&k, _)| k)
        .collect();

    let mut result = Vec::new();

    while let Some(node) = queue.pop_front() {
        result.push(node.to_string());

        if let Some(deps) = dependents.get(node) {
            for &dep in deps {
                if let Some(deg) = in_degree.get_mut(dep) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dep);
                    }
                }
            }
        }
    }

    result
}

/// Analyze a derivation and create a build plan
async fn create_build_plan(drv_path: &str) -> Result<BuildPlan> {
    println!("Analyzing derivation dependencies...");

    // Get all derivations recursively
    let drvs = parse_drvs_recursive(drv_path).await?;
    println!("  Found {} derivations in closure", drvs.len());

    // Collect all output paths
    let all_outputs: Vec<String> = drvs.values().flat_map(|d| d.outputs.clone()).collect();

    // Check which outputs already exist
    let existing = check_existing_paths(&all_outputs).await?;
    println!("  {} outputs already exist locally", existing.len());

    // Determine which derivations need to be built
    // A derivation needs building if ANY of its outputs don't exist
    let mut to_build_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (drv_path, info) in &drvs {
        let needs_build = info.outputs.iter().any(|out| !existing.contains(out));
        if needs_build {
            to_build_set.insert(drv_path.clone());
        }
    }

    println!("  {} derivations need to be built", to_build_set.len());

    // Topological sort to get build order
    let build_order = topological_sort(&drvs, &to_build_set);

    // Convert to DrvInfo list
    let to_build: Vec<DrvInfo> = build_order
        .iter()
        .filter_map(|path| drvs.get(path).cloned())
        .collect();

    // Existing paths that should be added to blob store
    let existing_paths: Vec<String> = existing.into_iter().collect();

    Ok(BuildPlan {
        to_build,
        existing_paths,
    })
}

async fn build_push(config: NodeConfig, drv_path: String, features: Vec<String>) -> Result<()> {
    let node = Arc::new(Node::spawn(config).await?);

    if !node.build_enabled() {
        return Err(iroh_nix::Error::Protocol(
            "Build queue not enabled. Use --network to enable gossip and the build queue.".into(),
        ));
    }

    // Analyze the derivation and its dependencies
    let plan = create_build_plan(&drv_path).await?;

    if plan.to_build.is_empty() {
        println!("Nothing to build - all outputs already exist locally");
        Arc::try_unwrap(node)
            .map_err(|_| iroh_nix::Error::Protocol("failed to unwrap node".into()))?
            .shutdown()
            .await?;
        return Ok(());
    }

    // Add existing paths to blob store so builders can access them
    if !plan.existing_paths.is_empty() {
        println!("\nIndexing {} existing paths...", plan.existing_paths.len());
        let mut indexed = 0;
        for path in &plan.existing_paths {
            // Check if path exists on filesystem
            let fs_path = std::path::Path::new(path);
            if fs_path.exists() {
                match node.index_store_path(path, fs_path).await {
                    Ok(_) => indexed += 1,
                    Err(e) => {
                        // Log but don't fail - some paths might be special
                        eprintln!("  Warning: failed to index {}: {}", path, e);
                    }
                }
            }
        }
        println!("  Indexed {} paths", indexed);
    }

    // Queue builds in dependency order
    println!(
        "\nQueueing {} builds in dependency order:",
        plan.to_build.len()
    );

    // Get the hash_index for looking up BLAKE3 hashes
    let hash_index = node.hash_index();

    let mut announced_any = false;
    for drv_info in &plan.to_build {
        // Combine system with required features (excluding "builtin" which is not a real feature)
        let mut drv_features = vec![drv_info.system.clone()];
        drv_features.extend(
            drv_info
                .required_features
                .iter()
                .filter(|f| *f != "builtin")
                .cloned(),
        );

        // User-specified features override if provided
        let build_features = if !features.is_empty() {
            features.clone()
        } else {
            drv_features
        };

        // Collect input paths for this derivation
        let mut input_paths: Vec<iroh_nix::build::InputPath> = Vec::new();

        // Add input sources
        for src_path in &drv_info.input_srcs {
            if let Ok(Some(entry)) = hash_index.lock().unwrap().get_by_store_path(src_path) {
                input_paths.push(iroh_nix::build::InputPath {
                    store_path: src_path.clone(),
                    blake3: entry.blake3.0,
                });
            }
        }

        // Add input derivation outputs
        for dep_drv in drv_info.input_drvs.keys() {
            // Find the drv in the plan to get its output paths
            if let Some(dep_info) = plan.to_build.iter().find(|d| &d.drv_path == dep_drv) {
                // For simplicity, add all outputs (we don't track per-output paths precisely yet)
                for output_path in &dep_info.outputs {
                    if let Ok(Some(entry)) =
                        hash_index.lock().unwrap().get_by_store_path(output_path)
                    {
                        input_paths.push(iroh_nix::build::InputPath {
                            store_path: output_path.clone(),
                            blake3: entry.blake3.0,
                        });
                    }
                }
            } else {
                // Dependency is not being built, it must already exist
                // Look it up in the hash_index if we have it
                // The outputs should have been added to the blob store earlier
                // For now, we skip - the builder will try to fetch from the requester
                // which should have these paths in its blob store
            }
        }

        let (job_id, announced) = node
            .push_build(
                &drv_info.drv_path,
                build_features.clone(),
                drv_info.outputs.clone(),
                input_paths,
            )
            .await?;

        if announced {
            announced_any = true;
        }

        // Extract just the name from the drv path for display
        let name = drv_info
            .drv_path
            .rsplit('/')
            .next()
            .unwrap_or(&drv_info.drv_path);

        println!("  [{}] {} ({})", job_id, name, build_features.join(", "));
    }

    if announced_any {
        println!("\nAnnounced need for builders via gossip");
    }

    // Keep running to serve the build queue and NAR blobs
    println!("\nWaiting for builds to complete. Press Ctrl+C to stop.");

    // Create cancellation token for clean shutdown
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Spawn signal handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
        println!("\nShutting down...");
        cancel_clone.cancel();
    });

    // Start serving incoming connections
    let serve_handle = {
        let node = node.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            node.serve(cancel).await;
        })
    };

    // Monitor build progress and re-announce NeedBuilder periodically
    let build_queue = node.build_queue().unwrap();
    let gossip = node.gossip().unwrap();
    let total_jobs = plan.to_build.len();
    let mut announcement_interval = tokio::time::interval(std::time::Duration::from_secs(10));
    announcement_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Collect unique system features from all jobs (excluding "builtin" which is not a real feature)
    let all_features: std::collections::HashSet<String> = plan
        .to_build
        .iter()
        .flat_map(|d| {
            let mut f = vec![d.system.clone()];
            f.extend(d.required_features.clone());
            f
        })
        .filter(|f| f != "builtin")
        .collect();
    let features_vec: Vec<String> = all_features.into_iter().collect();

    let mut fetched_outputs = 0usize;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            _ = announcement_interval.tick() => {
                let stats = build_queue.stats();
                // Re-announce NeedBuilder if there are still pending jobs
                if stats.pending_count > 0 {
                    if let Err(e) = gossip.announce_need_builder(features_vec.clone()).await {
                        eprintln!("Warning: failed to re-announce NeedBuilder: {}", e);
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                // Fetch outputs from completed builds
                let pending_fetches = build_queue.take_pending_fetches();
                for result in pending_fetches {
                    let builder_id = iroh_base::EndpointId::from_bytes(&result.builder)
                        .expect("invalid builder ID");
                    println!("Fetching {} outputs from builder {}...", result.outputs.len(), builder_id.fmt_short());

                    for output in &result.outputs {
                        let blake3 = iroh_nix::hash_index::Blake3Hash(output.blake3);
                        match node.fetch_by_id(builder_id, blake3).await {
                            Ok((import_result, nar_data)) => {
                                println!("  Fetched: {} ({} bytes)", import_result.store_path, import_result.nar_size);
                                fetched_outputs += 1;

                                // Import into Nix store
                                if let Err(e) = import_nar_to_nix_store(&nar_data, &output.store_path).await {
                                    eprintln!("  Warning: failed to import to Nix store: {}", e);
                                }
                            }
                            Err(e) => {
                                eprintln!("  Failed to fetch {}: {}", output.store_path, e);
                            }
                        }
                    }
                }

                let stats = build_queue.stats();

                // Check if all builds completed
                if stats.pending_count == 0 && stats.leased_count == 0 && !build_queue.has_pending_fetches() {
                    println!("\nAll {} builds completed! Fetched {} outputs.", total_jobs, fetched_outputs);
                    break;
                }

                // Print progress
                let completed = total_jobs - stats.pending_count - stats.leased_count;
                println!(
                    "Progress: {}/{} complete, {} pending, {} in progress",
                    completed, total_jobs, stats.pending_count, stats.leased_count
                );
            }
        }
    }

    cancel.cancel();
    serve_handle.abort();

    // Shutdown the node - need to unwrap from Arc
    match Arc::try_unwrap(node) {
        Ok(node) => node.shutdown().await?,
        Err(_) => {
            // Other references still exist, just let them drop
            eprintln!("Warning: could not cleanly shutdown node");
        }
    }
    Ok(())
}

async fn show_build_queue(config: NodeConfig) -> Result<()> {
    let node = Node::spawn(config).await?;

    if !node.build_enabled() {
        return Err(iroh_nix::Error::Protocol(
            "Build queue not enabled. Use --network to enable gossip and the build queue.".into(),
        ));
    }

    let build_queue = node.build_queue().unwrap();
    let stats = build_queue.stats();

    println!("Build Queue Status");
    println!("==================");
    println!("Pending jobs: {}", stats.pending_count);
    println!("Leased jobs: {}", stats.leased_count);
    println!("Completed jobs: {}", stats.completed_count);

    if stats.pending_count > 0 {
        println!("\nPending:");
        for job in build_queue.list_pending() {
            println!(
                "  [{}] {} (features: {})",
                job.id,
                job.drv_path,
                job.system_features.join(", ")
            );
        }
    }

    if stats.leased_count > 0 {
        println!("\nIn Progress:");
        for (job_id, leased) in build_queue.list_leased() {
            println!(
                "  [{}] {} (builder: {}, status: {})",
                job_id, leased.job.drv_path, leased.builder, leased.status
            );
        }
    }

    node.shutdown().await?;
    Ok(())
}

async fn watch_build_logs(config: NodeConfig, job: Option<u64>) -> Result<()> {
    use std::io::Write;

    let node = Arc::new(Node::spawn(config).await?);

    if !node.build_enabled() {
        return Err(iroh_nix::Error::Protocol(
            "Build queue not enabled. Use --network to enable gossip and the build queue.".into(),
        ));
    }

    let build_queue = node.build_queue().unwrap();

    // Subscribe to logs
    let mut log_rx = if let Some(job_id) = job {
        println!("Watching logs for job {}...", job_id);
        build_queue.subscribe_logs(job_id)
    } else {
        println!("Watching logs for all jobs...");
        build_queue.subscribe_all_logs()
    };
    println!("Press Ctrl+C to stop.\n");

    // Create cancellation token for clean shutdown
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Spawn signal handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
        cancel_clone.cancel();
    });

    // Also serve connections so we can receive logs from builders
    let serve_handle = {
        let node = node.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            node.serve(cancel).await;
        })
    };

    // Process log entries
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            Some(entry) = log_rx.recv() => {
                let stream_name = if entry.stream == 1 { "stdout" } else { "stderr" };
                let text = String::from_utf8_lossy(&entry.data);
                print!("[job {}][{}] {}", entry.job_id, stream_name, text);
                if !text.ends_with('\n') {
                    println!();
                }
                std::io::stdout().flush().ok();
            }
        }
    }

    println!("\nStopping log watch...");
    cancel.cancel();
    serve_handle.abort();

    // Shutdown the node
    match Arc::try_unwrap(node) {
        Ok(node) => node.shutdown().await?,
        Err(_) => {
            eprintln!("Warning: could not cleanly shutdown node");
        }
    }

    Ok(())
}

/// Import NAR data into the Nix store using nix-store --restore
async fn import_nar_to_nix_store(nar_data: &[u8], store_path: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    // Import using nix-store --restore
    let mut child = Command::new("nix-store")
        .args(["--restore", store_path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| iroh_nix::Error::Build(format!("failed to spawn nix-store: {}", e)))?;

    // Write NAR data to stdin
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| iroh_nix::Error::Build("failed to get nix-store stdin".to_string()))?;
        stdin.write_all(nar_data).await?;
    }

    // Wait for completion
    let result = child.wait_with_output().await?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(iroh_nix::Error::Build(format!(
            "failed to import {}: {}",
            store_path, stderr
        )));
    }

    Ok(())
}

