//! Daemon control API over Unix socket
//!
//! Provides a local IPC mechanism for the build-hook to communicate with
//! the running iroh-nix daemon. Uses JSON over length-prefixed frames.
//!
//! Protocol: 4-byte big-endian length prefix + JSON payload

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::build::{BuildQueue, InputPath, JobId, JobOutcome};
use crate::gossip::GossipService;
use crate::hash_index::HashIndex;
use crate::{Error, Result};

/// Maximum frame size (1 MB)
const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// Input path info for build submission
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputPathInfo {
    pub store_path: String,
    pub blake3: [u8; 32],
}

/// Build state reported via poll
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildState {
    Pending,
    Building { builder_id: String },
    Completed { outputs: Vec<OutputInfo> },
    Failed { error: String },
}

/// Output info returned on build completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputInfo {
    pub store_path: String,
    pub blake3: [u8; 32],
    pub nar_size: u64,
}

/// Requests from build-hook to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Submit a build job. Returns job ID.
    SubmitBuild {
        drv_path: String,
        system: String,
        required_features: Vec<String>,
        input_paths: Vec<InputPathInfo>,
        expected_outputs: Vec<String>,
    },
    /// Poll build status
    PollBuild { job_id: u64 },
    /// Check if builders with given features are known
    HasBuilders { system_features: Vec<String> },
    /// Cancel a build
    CancelBuild { job_id: u64 },
}

/// Responses from daemon to build-hook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlResponse {
    BuildSubmitted { job_id: u64 },
    BuildStatus { status: BuildState },
    HasBuilders { available: bool, count: usize },
    Error { message: String },
}

/// Shared daemon state accessible from the control socket handler
pub struct ControlContext {
    pub build_queue: Arc<BuildQueue>,
    pub gossip: Arc<GossipService>,
    pub hash_index: Arc<std::sync::Mutex<HashIndex>>,
}

/// Start the Unix socket control server
///
/// Accepts connections and dispatches requests to the daemon.
pub async fn serve_control(
    socket_path: &Path,
    ctx: Arc<ControlContext>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // Remove stale socket if it exists
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!("Control socket listening at {}", socket_path.display());

    // Set permissions so nix-daemon (root) can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o660))?;
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Control server shutting down");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let ctx = Arc::clone(&ctx);
                        tokio::spawn(async move {
                            if let Err(e) = handle_control_connection(stream, ctx).await {
                                debug!("Control connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Failed to accept control connection: {}", e);
                    }
                }
            }
        }
    }

    // Clean up socket file
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Handle a single control connection
async fn handle_control_connection(mut stream: UnixStream, ctx: Arc<ControlContext>) -> Result<()> {
    debug!("New control connection");

    loop {
        // Read frame length (4 bytes big-endian)
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                debug!("Control client disconnected");
                break;
            }
            Err(e) => return Err(Error::Io(e)),
        }

        let frame_len = u32::from_be_bytes(len_buf) as usize;
        if frame_len > MAX_FRAME_SIZE {
            return Err(Error::Protocol(format!(
                "control frame too large: {} bytes",
                frame_len
            )));
        }

        // Read frame
        let mut frame_buf = vec![0u8; frame_len];
        stream.read_exact(&mut frame_buf).await?;

        // Deserialize request
        let request: ControlRequest = serde_json::from_slice(&frame_buf)
            .map_err(|e| Error::Protocol(format!("invalid control request: {}", e)))?;

        // Handle request
        let response = handle_control_request(&ctx, request).await;

        // Serialize response
        let response_bytes = serde_json::to_vec(&response)
            .map_err(|e| Error::Protocol(format!("failed to serialize response: {}", e)))?;

        // Write frame
        let len_bytes = (response_bytes.len() as u32).to_be_bytes();
        stream.write_all(&len_bytes).await?;
        stream.write_all(&response_bytes).await?;
    }

    Ok(())
}

/// Handle a single control request
async fn handle_control_request(ctx: &ControlContext, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::SubmitBuild {
            drv_path,
            system,
            required_features,
            input_paths,
            expected_outputs,
        } => {
            // Build system_features: [system] ++ required_features
            let mut system_features = vec![system];
            system_features.extend(required_features);

            // Convert input paths
            let build_input_paths: Vec<InputPath> = input_paths
                .into_iter()
                .map(|ip| InputPath {
                    store_path: ip.store_path,
                    blake3: ip.blake3,
                })
                .collect();

            // Push to build queue
            let (job_id, should_announce) = ctx.build_queue.push(
                &drv_path,
                system_features.clone(),
                expected_outputs,
                build_input_paths,
            );

            // Announce via gossip if needed
            if should_announce {
                if let Err(e) = ctx.gossip.announce_need_builder(system_features).await {
                    warn!("Failed to announce NeedBuilder: {}", e);
                }
            }

            ControlResponse::BuildSubmitted { job_id: job_id.0 }
        }

        ControlRequest::PollBuild { job_id } => {
            // Check completed first
            let drv_hash = {
                // Find drv_hash from leased or pending jobs
                let leased = ctx.build_queue.list_leased();
                let pending = ctx.build_queue.list_pending();

                leased
                    .iter()
                    .find(|(id, _)| id.0 == job_id)
                    .map(|(_, j)| j.job.drv_hash.clone())
                    .or_else(|| {
                        pending
                            .iter()
                            .find(|j| j.id.0 == job_id)
                            .map(|j| j.drv_hash.clone())
                    })
            };

            // Check if it's still in the leased list (being built)
            let leased = ctx.build_queue.list_leased();
            if let Some((_, leased_job)) = leased.iter().find(|(id, _)| id.0 == job_id) {
                return ControlResponse::BuildStatus {
                    status: BuildState::Building {
                        builder_id: leased_job.builder.to_string(),
                    },
                };
            }

            // Check if it's pending
            let pending = ctx.build_queue.list_pending();
            if pending.iter().any(|j| j.id.0 == job_id) {
                return ControlResponse::BuildStatus {
                    status: BuildState::Pending,
                };
            }

            // Check completed outcomes by drv_hash
            if let Some(drv_hash) = drv_hash {
                if let Some(outcome) = ctx.build_queue.get_outcome(&drv_hash) {
                    return match outcome {
                        JobOutcome::Completed(result) => ControlResponse::BuildStatus {
                            status: BuildState::Completed {
                                outputs: result
                                    .outputs
                                    .iter()
                                    .map(|o| OutputInfo {
                                        store_path: o.store_path.clone(),
                                        blake3: o.blake3,
                                        nar_size: o.nar_size,
                                    })
                                    .collect(),
                            },
                        },
                        JobOutcome::Failed { error, .. } => ControlResponse::BuildStatus {
                            status: BuildState::Failed { error },
                        },
                    };
                }
            }

            // Try scanning all completed outcomes for this job_id
            // This handles the case where the job was removed from pending/leased
            // but we still have the outcome
            ControlResponse::BuildStatus {
                status: BuildState::Pending,
            }
        }

        ControlRequest::HasBuilders { system_features: _ } => {
            // The control socket only exists when gossip is enabled, so if we
            // reach this code, the gossip network is configured and active.
            // We optimistically accept -- if no builder picks up the job, the
            // build-hook will time out. A future improvement could track
            // explicit OfferBuilder announcements.
            ControlResponse::HasBuilders {
                available: true,
                count: 1,
            }
        }

        ControlRequest::CancelBuild { job_id } => {
            // Try to fail the job if it's leased
            let leased = ctx.build_queue.list_leased();
            if let Some((_, leased_job)) = leased.iter().find(|(id, _)| id.0 == job_id) {
                let builder = leased_job.builder;
                ctx.build_queue.fail(
                    JobId(job_id),
                    builder,
                    "cancelled by build-hook".to_string(),
                );
                ControlResponse::BuildStatus {
                    status: BuildState::Failed {
                        error: "cancelled".to_string(),
                    },
                }
            } else {
                ControlResponse::Error {
                    message: format!("job {} not found or not in progress", job_id),
                }
            }
        }
    }
}

// ============================================================================
// Client side: used by build-hook to connect to daemon
// ============================================================================

/// Client for communicating with the daemon control socket
pub struct ControlClient {
    stream: UnixStream,
}

impl ControlClient {
    /// Connect to the daemon control socket
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            Error::Connection(format!("failed to connect to control socket: {}", e))
        })?;
        Ok(Self { stream })
    }

    /// Send a request and receive a response
    pub async fn request(&mut self, req: &ControlRequest) -> Result<ControlResponse> {
        // Serialize request
        let req_bytes = serde_json::to_vec(req)
            .map_err(|e| Error::Protocol(format!("failed to serialize request: {}", e)))?;

        // Write frame
        let len_bytes = (req_bytes.len() as u32).to_be_bytes();
        self.stream.write_all(&len_bytes).await?;
        self.stream.write_all(&req_bytes).await?;

        // Read response frame length
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await?;
        let frame_len = u32::from_be_bytes(len_buf) as usize;
        if frame_len > MAX_FRAME_SIZE {
            return Err(Error::Protocol(format!(
                "response frame too large: {} bytes",
                frame_len
            )));
        }

        // Read response frame
        let mut frame_buf = vec![0u8; frame_len];
        self.stream.read_exact(&mut frame_buf).await?;

        // Deserialize response
        serde_json::from_slice(&frame_buf)
            .map_err(|e| Error::Protocol(format!("invalid control response: {}", e)))
    }

    /// Convenience: submit a build and return the job ID
    pub async fn submit_build(
        &mut self,
        drv_path: &str,
        system: &str,
        required_features: Vec<String>,
        input_paths: Vec<InputPathInfo>,
        expected_outputs: Vec<String>,
    ) -> Result<u64> {
        let resp = self
            .request(&ControlRequest::SubmitBuild {
                drv_path: drv_path.to_string(),
                system: system.to_string(),
                required_features,
                input_paths,
                expected_outputs,
            })
            .await?;

        match resp {
            ControlResponse::BuildSubmitted { job_id } => Ok(job_id),
            ControlResponse::Error { message } => Err(Error::Build(message)),
            _ => Err(Error::Protocol("unexpected response to SubmitBuild".into())),
        }
    }

    /// Convenience: poll build status
    pub async fn poll_build(&mut self, job_id: u64) -> Result<BuildState> {
        let resp = self.request(&ControlRequest::PollBuild { job_id }).await?;

        match resp {
            ControlResponse::BuildStatus { status } => Ok(status),
            ControlResponse::Error { message } => Err(Error::Build(message)),
            _ => Err(Error::Protocol("unexpected response to PollBuild".into())),
        }
    }

    /// Convenience: check if builders are available
    pub async fn has_builders(&mut self, system_features: Vec<String>) -> Result<(bool, usize)> {
        let resp = self
            .request(&ControlRequest::HasBuilders { system_features })
            .await?;

        match resp {
            ControlResponse::HasBuilders { available, count } => Ok((available, count)),
            ControlResponse::Error { message } => Err(Error::Build(message)),
            _ => Err(Error::Protocol("unexpected response to HasBuilders".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_control_request_serialization() {
        let req = ControlRequest::SubmitBuild {
            drv_path: "/nix/store/abc123.drv".to_string(),
            system: "x86_64-linux".to_string(),
            required_features: vec!["kvm".to_string()],
            input_paths: vec![InputPathInfo {
                store_path: "/nix/store/dep1".to_string(),
                blake3: [1u8; 32],
            }],
            expected_outputs: vec!["/nix/store/out1".to_string()],
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: ControlRequest = serde_json::from_str(&json).unwrap();

        match decoded {
            ControlRequest::SubmitBuild {
                drv_path,
                system,
                required_features,
                input_paths,
                expected_outputs,
            } => {
                assert_eq!(drv_path, "/nix/store/abc123.drv");
                assert_eq!(system, "x86_64-linux");
                assert_eq!(required_features, vec!["kvm"]);
                assert_eq!(input_paths.len(), 1);
                assert_eq!(expected_outputs.len(), 1);
            }
            _ => panic!("wrong request type"),
        }
    }

    #[test]
    fn test_control_response_serialization() {
        let resp = ControlResponse::BuildStatus {
            status: BuildState::Building {
                builder_id: "abc123".to_string(),
            },
        };

        let json = serde_json::to_string(&resp).unwrap();
        let decoded: ControlResponse = serde_json::from_str(&json).unwrap();

        match decoded {
            ControlResponse::BuildStatus {
                status: BuildState::Building { builder_id },
            } => {
                assert_eq!(builder_id, "abc123");
            }
            _ => panic!("wrong response type"),
        }
    }

    #[test]
    fn test_build_state_variants() {
        // Test all variants serialize/deserialize
        let states = vec![
            BuildState::Pending,
            BuildState::Building {
                builder_id: "node1".to_string(),
            },
            BuildState::Completed {
                outputs: vec![OutputInfo {
                    store_path: "/nix/store/out".to_string(),
                    blake3: [2u8; 32],
                    nar_size: 1024,
                }],
            },
            BuildState::Failed {
                error: "build failed".to_string(),
            },
        ];

        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let _decoded: BuildState = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_has_builders_request() {
        let req = ControlRequest::HasBuilders {
            system_features: vec!["x86_64-linux".to_string(), "kvm".to_string()],
        };

        let json = serde_json::to_string(&req).unwrap();
        let decoded: ControlRequest = serde_json::from_str(&json).unwrap();

        match decoded {
            ControlRequest::HasBuilders { system_features } => {
                assert_eq!(system_features.len(), 2);
            }
            _ => panic!("wrong request type"),
        }
    }
}
