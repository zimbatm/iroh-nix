//! Builder worker that connects to requesters and pulls jobs
//!
//! This module provides:
//! - A build queue handler for requesters (accepting builder connections)
//! - A builder worker that connects to requesters and executes builds

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{RecvStream, SendStream};
use iroh::Endpoint;
use iroh_base::{EndpointAddr, EndpointId, SecretKey};
use tracing::{info, warn};

use crate::build::{
    execute_build, execute_build_with_logs, sign_build_result, BuildLogLine, BuildOutput,
    BuildQueue, DrvHash, JobId, BUILDER_IDLE_TIMEOUT, HEARTBEAT_INTERVAL,
};
use crate::error::MutexExt;
use crate::gossip::GossipService;
use crate::hash_index::{Blake3Hash, HashIndex};
use crate::nar::serialize_path_to_writer;
use crate::nix_info::NixPathInfo;
use crate::protocol::{
    BuildOutputProto, BuildQueueRequest, BuildQueueResponse, InputPathProto,
    BUILD_QUEUE_PROTOCOL_ALPN,
};
use crate::transfer::fetch_nar_with_config;
use crate::{Error, Result};

/// Maximum message size for build queue protocol
const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1 MB

/// Context for executing a build job (groups parameters to reduce function arguments)
struct JobContext<'a> {
    job_id: u64,
    drv_hash: DrvHash,
    drv_path: &'a str,
    input_paths: &'a [InputPathProto],
    requester_id: EndpointId,
    stream_logs: bool,
}

// ============================================================================
// Requester-side: Handle incoming builder connections
// ============================================================================

/// Handle an accepted build queue connection from a builder
pub async fn handle_build_queue_accepted(
    connection: iroh::endpoint::Connection,
    build_queue: Arc<BuildQueue>,
) -> Result<()> {
    let builder_id = connection.remote_id();
    info!("Builder {} connected to build queue", builder_id);

    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| Error::Protocol(format!("failed to accept bi stream: {}", e)))?;

    // Handle requests in a loop
    loop {
        // Read message length (4 bytes, big-endian)
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(_) => {
                // Any error likely means disconnection
                info!("Builder {} disconnected", builder_id);
                break;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len > MAX_MESSAGE_SIZE {
            return Err(Error::Protocol(format!(
                "message too large: {} bytes",
                msg_len
            )));
        }

        // Read message
        let mut msg_buf = vec![0u8; msg_len];
        recv.read_exact(&mut msg_buf)
            .await
            .map_err(|e| Error::Protocol(format!("failed to read message: {}", e)))?;

        let request = BuildQueueRequest::from_bytes(&msg_buf)
            .map_err(|e| Error::Protocol(format!("failed to decode request: {}", e)))?;

        let response = handle_build_queue_request(&build_queue, builder_id, request);

        // Send response
        let response_bytes = response
            .to_bytes()
            .map_err(|e| Error::Protocol(format!("failed to encode response: {}", e)))?;

        let len_bytes = (response_bytes.len() as u32).to_be_bytes();
        send.write_all(&len_bytes)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write response length: {}", e)))?;
        send.write_all(&response_bytes)
            .await
            .map_err(|e| Error::Protocol(format!("failed to write response: {}", e)))?;
    }

    Ok(())
}

/// Handle a single build queue request
fn handle_build_queue_request(
    build_queue: &BuildQueue,
    builder_id: EndpointId,
    request: BuildQueueRequest,
) -> BuildQueueResponse {
    match request {
        BuildQueueRequest::Pull {
            system_features,
            stream_logs: _,
        } => match build_queue.pull(builder_id, &system_features) {
            Some(job) => BuildQueueResponse::Job {
                job_id: job.id.0,
                drv_hash: *job.drv_hash.as_bytes(),
                drv_path: job.drv_path,
                outputs: job.outputs,
                input_paths: job
                    .input_paths
                    .iter()
                    .map(|ip| InputPathProto {
                        store_path: ip.store_path.clone(),
                        blake3: ip.blake3,
                    })
                    .collect(),
            },
            None => BuildQueueResponse::NoJob,
        },

        BuildQueueRequest::Heartbeat { job_id, status } => {
            if build_queue.heartbeat(JobId(job_id), builder_id, status) {
                BuildQueueResponse::HeartbeatAck
            } else {
                BuildQueueResponse::Error {
                    message: "invalid job or builder".to_string(),
                }
            }
        }

        BuildQueueRequest::Complete {
            job_id,
            outputs,
            signature_r,
            signature_s,
        } => {
            let build_outputs: Vec<BuildOutput> = outputs
                .into_iter()
                .map(|o| BuildOutput {
                    store_path: o.store_path,
                    blake3: o.blake3,
                    sha256: o.sha256,
                    nar_size: o.nar_size,
                })
                .collect();

            // Find the job to get its drv_hash
            let leased = build_queue.list_leased();
            let job_info = leased.iter().find(|(id, _)| id.0 == job_id);

            if let Some((_, leased_job)) = job_info {
                let result = crate::build::BuildResult {
                    job_id: JobId(job_id),
                    drv_hash: leased_job.job.drv_hash.clone(),
                    builder: *builder_id.as_bytes(),
                    outputs: build_outputs,
                    signature_r,
                    signature_s,
                };

                if build_queue.complete(JobId(job_id), builder_id, result) {
                    BuildQueueResponse::CompleteAck
                } else {
                    BuildQueueResponse::Error {
                        message: "failed to complete job".to_string(),
                    }
                }
            } else {
                BuildQueueResponse::Error {
                    message: "job not found".to_string(),
                }
            }
        }

        BuildQueueRequest::Fail { job_id, error } => {
            if build_queue.fail(JobId(job_id), builder_id, error) {
                BuildQueueResponse::FailAck
            } else {
                BuildQueueResponse::Error {
                    message: "invalid job or builder".to_string(),
                }
            }
        }

        BuildQueueRequest::BuildLog {
            job_id,
            data,
            stream,
        } => {
            // Forward log to any subscribed clients
            build_queue.broadcast_log(job_id, data, stream);
            BuildQueueResponse::LogAck
        }
    }
}

// ============================================================================
// Builder-side: Connect to requesters and pull jobs
// ============================================================================

/// Configuration for a builder worker
#[derive(Debug, Clone)]
pub struct BuilderConfig {
    /// System features this builder supports
    pub system_features: Vec<String>,
    /// Whether to stream logs back to requesters (when they request it)
    pub stream_logs: bool,
}

/// A builder worker that connects to requesters and executes builds
pub struct BuilderWorker {
    endpoint: Endpoint,
    secret_key: SecretKey,
    config: BuilderConfig,
    hash_index: Arc<std::sync::Mutex<HashIndex>>,
}

impl BuilderWorker {
    /// Create a new builder worker
    pub fn new(
        endpoint: Endpoint,
        secret_key: SecretKey,
        config: BuilderConfig,
        hash_index: Arc<std::sync::Mutex<HashIndex>>,
    ) -> Self {
        Self {
            endpoint,
            secret_key,
            config,
            hash_index,
        }
    }

    /// Run the builder worker, connecting to requesters from gossip
    ///
    /// This listens for NeedBuilder gossip messages and connects to matching requesters.
    pub async fn run(
        &self,
        gossip: &GossipService,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!(
            "Builder worker started with features: {:?}",
            self.config.system_features
        );

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Builder worker shutting down");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    // Check for requesters that need builders
                    let requesters = gossip.get_matching_requesters(&self.config.system_features);

                    for requester_info in requesters {
                        let requester_id = requester_info.endpoint_id;
                        info!("Found requester needing builder: {}", requester_id);

                        // Connect and process jobs
                        if let Err(e) = self.connect_and_work(requester_id, cancel.clone()).await {
                            warn!("Error working with requester {}: {}", requester_id, e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Fetch missing dependencies from the requester
    async fn fetch_dependencies(
        &self,
        input_paths: &[InputPathProto],
        requester_id: EndpointId,
    ) -> Result<()> {
        // Check which paths already exist locally
        let missing_paths: Vec<&InputPathProto> = input_paths
            .iter()
            .filter(|ip| !std::path::Path::new(&ip.store_path).exists())
            .collect();

        if missing_paths.is_empty() {
            info!("All {} dependencies already present", input_paths.len());
            return Ok(());
        }

        info!(
            "Fetching {} missing dependencies (of {} total)",
            missing_paths.len(),
            input_paths.len()
        );

        let requester_addr = EndpointAddr::from(requester_id);

        // Use aggressive retry for dependency fetching
        let retry_config = crate::retry::RetryConfig::aggressive();

        for input in missing_paths {
            let blake3 = Blake3Hash(input.blake3);
            info!("Fetching dependency: {}", input.store_path);

            // Fetch NAR from requester with retry
            let (header, nar_data) = fetch_nar_with_config(
                &self.endpoint,
                requester_addr.clone(),
                blake3,
                &retry_config,
            )
            .await?;

            // Import into Nix store using nix-store --restore
            // Stream NAR data directly to nix-store stdin
            let mut child = tokio::process::Command::new("nix-store")
                .args(["--restore", &input.store_path])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| Error::Build(format!("failed to spawn nix-store: {}", e)))?;

            // Write NAR data to stdin
            {
                let mut stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| Error::Build("failed to get nix-store stdin".to_string()))?;

                use tokio::io::AsyncWriteExt;
                stdin.write_all(&nar_data).await?;
            }

            // Wait for completion
            let result = child.wait_with_output().await?;

            if !result.status.success() {
                let stderr = String::from_utf8_lossy(&result.stderr);
                return Err(Error::Build(format!(
                    "failed to import {}: {}",
                    input.store_path, stderr
                )));
            }

            // Query Nix for references/deriver now that the path is imported
            let (references, deriver) = match NixPathInfo::query(&input.store_path).await {
                Ok(nix_info) => (nix_info.references, nix_info.deriver),
                Err(_) => (vec![], None),
            };

            // Store in hash index for future reference
            let entry = crate::hash_index::HashEntry {
                blake3,
                sha256: crate::hash_index::Sha256Hash(header.sha256),
                store_path: input.store_path.clone(),
                nar_size: header.size,
                references,
                deriver,
            };
            {
                let index = self.hash_index.lock_or_err()?;
                index.insert(&entry)?;
            }

            info!("Imported dependency: {}", input.store_path);
        }

        Ok(())
    }

    /// Connect to a requester and process jobs
    async fn connect_and_work(
        &self,
        requester_id: EndpointId,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        info!("Connecting to requester {} for build jobs", requester_id);

        let addr = EndpointAddr::from(requester_id);

        // Use retry logic for connection
        let retry_config = crate::retry::RetryConfig::patient();
        let endpoint = &self.endpoint;

        let connection = crate::retry::with_retry(
            &retry_config,
            &format!("connect({})", requester_id.fmt_short()),
            || async {
                endpoint
                    .connect(addr.clone(), BUILD_QUEUE_PROTOCOL_ALPN)
                    .await
                    .map_err(|e| {
                        Error::Connection(format!("failed to connect to requester: {}", e))
                    })
            },
        )
        .await?;

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| Error::Protocol(format!("failed to open bi stream: {}", e)))?;

        let mut idle_start = std::time::Instant::now();

        loop {
            if cancel.is_cancelled() {
                break;
            }

            // Request a job
            let pull_request = BuildQueueRequest::Pull {
                system_features: self.config.system_features.clone(),
                stream_logs: self.config.stream_logs,
            };

            send_request(&mut send, &pull_request).await?;
            let response = recv_response(&mut recv).await?;

            match response {
                BuildQueueResponse::Job {
                    job_id,
                    drv_hash,
                    drv_path,
                    outputs: _,
                    input_paths,
                } => {
                    idle_start = std::time::Instant::now();
                    info!(
                        "Got job {} to build: {} ({} inputs)",
                        job_id,
                        drv_path,
                        input_paths.len()
                    );

                    // Execute the build with heartbeats
                    let ctx = JobContext {
                        job_id,
                        drv_hash: DrvHash::from_bytes(drv_hash),
                        drv_path: &drv_path,
                        input_paths: &input_paths,
                        requester_id,
                        stream_logs: self.config.stream_logs,
                    };
                    match self
                        .execute_job_with_heartbeat(&mut send, &mut recv, &ctx)
                        .await
                    {
                        Ok(_) => info!("Job {} completed successfully", job_id),
                        Err(e) => {
                            warn!("Job {} failed: {}", job_id, e);
                            // Report failure
                            let fail_req = BuildQueueRequest::Fail {
                                job_id,
                                error: e.to_string(),
                            };
                            let _ = send_request(&mut send, &fail_req).await;
                            let _ = recv_response(&mut recv).await;
                        }
                    }
                }

                BuildQueueResponse::NoJob => {
                    // No job available
                    if idle_start.elapsed() > BUILDER_IDLE_TIMEOUT {
                        info!(
                            "No jobs for {:?}, disconnecting from requester",
                            BUILDER_IDLE_TIMEOUT
                        );
                        break;
                    }
                    // Wait a bit before asking again
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }

                BuildQueueResponse::Error { message } => {
                    warn!("Error from requester: {}", message);
                    break;
                }

                _ => {
                    warn!("Unexpected response type");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Execute a build job, sending heartbeats during execution
    async fn execute_job_with_heartbeat(
        &self,
        send: &mut SendStream,
        recv: &mut RecvStream,
        ctx: &JobContext<'_>,
    ) -> Result<()> {
        // First, fetch any missing dependencies
        if !ctx.input_paths.is_empty() {
            info!(
                "Fetching {} dependencies for job {}",
                ctx.input_paths.len(),
                ctx.job_id
            );

            // Send initial heartbeat
            let heartbeat = BuildQueueRequest::Heartbeat {
                job_id: ctx.job_id,
                status: "fetching dependencies".to_string(),
            };
            send_request(send, &heartbeat).await?;
            let _ = recv_response(recv).await?;

            // Fetch dependencies from requester
            self.fetch_dependencies(ctx.input_paths, ctx.requester_id)
                .await?;

            info!("Dependencies fetched for job {}", ctx.job_id);
        }

        // Spawn a task to send heartbeats
        let heartbeat_cancel = tokio_util::sync::CancellationToken::new();
        let heartbeat_cancel_clone = heartbeat_cancel.clone();

        // We need to use a channel to coordinate heartbeats since we can't share the stream
        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<String>(16);

        // Optionally receive log lines from the build
        let (log_tx, mut log_rx) = tokio::sync::mpsc::channel::<BuildLogLine>(256);

        // Spawn the build in a separate task
        let drv_path_owned = ctx.drv_path.to_string();
        let job_id = ctx.job_id;
        let build_handle = if ctx.stream_logs {
            // Use log-streaming version
            tokio::spawn(async move {
                let _ = heartbeat_tx.send("building".to_string()).await;
                let result = execute_build_with_logs(&drv_path_owned, job_id, log_tx).await;
                heartbeat_cancel_clone.cancel();
                result
            })
        } else {
            // Use regular version (drop log_tx immediately)
            drop(log_tx);
            tokio::spawn(async move {
                let _ = heartbeat_tx.send("building".to_string()).await;
                let result = execute_build(&drv_path_owned).await;
                heartbeat_cancel_clone.cancel();
                result
            })
        };

        // Process heartbeats and logs while building
        let mut heartbeat_interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut current_status = "starting".to_string();

        loop {
            tokio::select! {
                _ = heartbeat_cancel.cancelled() => {
                    break;
                }
                _ = heartbeat_interval.tick() => {
                    // Send heartbeat
                    let heartbeat = BuildQueueRequest::Heartbeat {
                        job_id: ctx.job_id,
                        status: current_status.clone(),
                    };
                    send_request(send, &heartbeat).await?;
                    let _ = recv_response(recv).await?; // HeartbeatAck
                }
                Some(status) = heartbeat_rx.recv() => {
                    current_status = status;
                }
                Some(log_line) = log_rx.recv() => {
                    // Send log line to requester
                    let log_req = BuildQueueRequest::BuildLog {
                        job_id: log_line.job_id,
                        data: log_line.data,
                        stream: log_line.stream,
                    };
                    send_request(send, &log_req).await?;
                    let _ = recv_response(recv).await?; // LogAck
                }
            }
        }

        // Get build result
        let output_paths = build_handle
            .await
            .map_err(|e| Error::Protocol(format!("build task panicked: {}", e)))??;

        // Index outputs and collect build outputs (NAR generated on-demand when requested)
        let mut build_outputs = Vec::new();

        for output_path in output_paths {
            let store_path_str = output_path.display().to_string();

            // Compute hashes by serializing to sink (no blob file stored)
            let info = serialize_path_to_writer(&output_path, std::io::sink())?;

            // Query Nix for references/deriver (build just completed, data is available)
            let (references, deriver) = match NixPathInfo::query(&store_path_str).await {
                Ok(nix_info) => (nix_info.references, nix_info.deriver),
                Err(_) => (vec![], None),
            };

            // Store in hash index for on-demand NAR generation
            let entry = crate::hash_index::HashEntry {
                blake3: info.blake3,
                sha256: info.sha256,
                store_path: store_path_str.clone(),
                nar_size: info.nar_size,
                references,
                deriver,
            };
            {
                let index = self.hash_index.lock_or_err()?;
                index.insert(&entry)?;
            }

            build_outputs.push(BuildOutput {
                store_path: store_path_str,
                blake3: info.blake3.0,
                sha256: info.sha256.0,
                nar_size: info.nar_size,
            });
        }

        // Sign and send completion
        let result = sign_build_result(
            &self.secret_key,
            JobId(ctx.job_id),
            ctx.drv_hash.clone(),
            build_outputs,
        );

        let complete_req = BuildQueueRequest::Complete {
            job_id: ctx.job_id,
            outputs: result
                .outputs
                .iter()
                .map(|o| BuildOutputProto {
                    store_path: o.store_path.clone(),
                    blake3: o.blake3,
                    sha256: o.sha256,
                    nar_size: o.nar_size,
                })
                .collect(),
            signature_r: result.signature_r,
            signature_s: result.signature_s,
        };

        send_request(send, &complete_req).await?;
        let response = recv_response(recv).await?;

        match response {
            BuildQueueResponse::CompleteAck => Ok(()),
            BuildQueueResponse::Error { message } => {
                Err(Error::Protocol(format!("completion rejected: {}", message)))
            }
            _ => Err(Error::Protocol(
                "unexpected response to Complete".to_string(),
            )),
        }
    }
}

/// Send a build queue request
async fn send_request(send: &mut SendStream, request: &BuildQueueRequest) -> Result<()> {
    let bytes = request
        .to_bytes()
        .map_err(|e| Error::Protocol(format!("failed to encode request: {}", e)))?;

    let len_bytes = (bytes.len() as u32).to_be_bytes();
    send.write_all(&len_bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write request length: {}", e)))?;
    send.write_all(&bytes)
        .await
        .map_err(|e| Error::Protocol(format!("failed to write request: {}", e)))?;

    Ok(())
}

/// Receive a build queue response
async fn recv_response(recv: &mut RecvStream) -> Result<BuildQueueResponse> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read response length: {}", e)))?;

    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len > MAX_MESSAGE_SIZE {
        return Err(Error::Protocol(format!(
            "response too large: {} bytes",
            msg_len
        )));
    }

    let mut msg_buf = vec![0u8; msg_len];
    recv.read_exact(&mut msg_buf)
        .await
        .map_err(|e| Error::Protocol(format!("failed to read response: {}", e)))?;

    BuildQueueResponse::from_bytes(&msg_buf)
        .map_err(|e| Error::Protocol(format!("failed to decode response: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_queue_request_roundtrip() {
        let request = BuildQueueRequest::Pull {
            system_features: vec!["x86_64-linux".to_string(), "kvm".to_string()],
            stream_logs: true,
        };
        let bytes = request.to_bytes().unwrap();
        let decoded = BuildQueueRequest::from_bytes(&bytes).unwrap();

        match decoded {
            BuildQueueRequest::Pull {
                system_features,
                stream_logs,
            } => {
                assert_eq!(system_features.len(), 2);
                assert_eq!(system_features[0], "x86_64-linux");
                assert!(stream_logs);
            }
            _ => panic!("wrong request type"),
        }
    }

    #[test]
    fn test_build_queue_response_roundtrip() {
        let response = BuildQueueResponse::Job {
            job_id: 42,
            drv_hash: [1u8; 32],
            drv_path: "/nix/store/abc.drv".to_string(),
            outputs: vec!["/nix/store/output".to_string()],
            input_paths: vec![InputPathProto {
                store_path: "/nix/store/dep".to_string(),
                blake3: [2u8; 32],
            }],
        };
        let bytes = response.to_bytes().unwrap();
        let decoded = BuildQueueResponse::from_bytes(&bytes).unwrap();

        match decoded {
            BuildQueueResponse::Job {
                job_id,
                drv_path,
                outputs,
                input_paths,
                ..
            } => {
                assert_eq!(job_id, 42);
                assert_eq!(drv_path, "/nix/store/abc.drv");
                assert_eq!(outputs.len(), 1);
                assert_eq!(input_paths.len(), 1);
                assert_eq!(input_paths[0].store_path, "/nix/store/dep");
            }
            _ => panic!("wrong response type"),
        }
    }

    #[test]
    fn test_builder_config_fields() {
        let config = BuilderConfig {
            system_features: vec!["x86_64-linux".to_string(), "kvm".to_string()],
            stream_logs: true,
        };

        assert_eq!(config.system_features.len(), 2);
        assert!(config.system_features.contains(&"x86_64-linux".to_string()));
        assert!(config.system_features.contains(&"kvm".to_string()));
        assert!(config.stream_logs);
    }

    #[test]
    fn test_handle_build_queue_request_pull_empty_queue() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let queue = Arc::new(crate::build::BuildQueue::new(
            secret_key.public(),
            secret_key.clone(),
        ));

        let builder_id = SecretKey::generate(&mut rand::rng()).public();
        let request = BuildQueueRequest::Pull {
            system_features: vec!["x86_64-linux".to_string()],
            stream_logs: false,
        };

        let response = handle_build_queue_request(&queue, builder_id, request);
        assert!(matches!(response, BuildQueueResponse::NoJob));
    }

    #[test]
    fn test_handle_build_queue_request_pull_with_job() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let queue = Arc::new(crate::build::BuildQueue::new(
            secret_key.public(),
            secret_key.clone(),
        ));

        // Add a job to the queue
        queue.push(
            "/nix/store/test.drv",
            vec!["x86_64-linux".to_string()],
            vec!["/nix/store/output".to_string()],
            vec![],
        );

        let builder_id = SecretKey::generate(&mut rand::rng()).public();
        let request = BuildQueueRequest::Pull {
            system_features: vec!["x86_64-linux".to_string()],
            stream_logs: false,
        };

        let response = handle_build_queue_request(&queue, builder_id, request);
        match response {
            BuildQueueResponse::Job { drv_path, .. } => {
                assert_eq!(drv_path, "/nix/store/test.drv");
            }
            _ => panic!("expected Job response"),
        }
    }

    #[test]
    fn test_handle_build_queue_request_heartbeat_invalid() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let queue = Arc::new(crate::build::BuildQueue::new(
            secret_key.public(),
            secret_key.clone(),
        ));

        let builder_id = SecretKey::generate(&mut rand::rng()).public();
        let request = BuildQueueRequest::Heartbeat {
            job_id: 9999, // Non-existent job
            status: "building".to_string(),
        };

        let response = handle_build_queue_request(&queue, builder_id, request);
        // Invalid job_id returns Error with message
        match response {
            BuildQueueResponse::Error { message } => {
                assert!(message.contains("invalid"));
            }
            _ => panic!("expected Error response, got {:?}", response),
        }
    }

    #[test]
    fn test_handle_build_queue_request_heartbeat_valid() {
        use iroh_base::SecretKey;

        let secret_key = SecretKey::generate(&mut rand::rng());
        let queue = Arc::new(crate::build::BuildQueue::new(
            secret_key.public(),
            secret_key.clone(),
        ));

        // Add and pull a job
        queue.push("/nix/store/test.drv", vec![], vec![], vec![]);
        let builder_id = SecretKey::generate(&mut rand::rng()).public();
        let job = queue.pull(builder_id, &[]).unwrap();

        let request = BuildQueueRequest::Heartbeat {
            job_id: job.id.0,
            status: "building".to_string(),
        };

        let response = handle_build_queue_request(&queue, builder_id, request);
        assert!(matches!(response, BuildQueueResponse::HeartbeatAck));
    }
}
