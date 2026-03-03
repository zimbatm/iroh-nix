//! Pull-based build queue for distributed Nix builds
//!
//! Design:
//! - Requesters maintain their own FIFO queue of derivations to build
//! - Requesters broadcast "NeedBuilder" via gossip with required system features
//! - Builders with matching features connect to requester's RPC
//! - Builders pull jobs from the queue (work-stealing style)
//! - Builders stream status updates back while building
//! - Jobs timeout and return to queue if builder stops sending heartbeats

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use iroh_base::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{info, warn};

use crate::{Error, Result};

/// How long before a job lease expires without heartbeat
pub const JOB_LEASE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a builder waits for new jobs before disconnecting
pub const BUILDER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Heartbeat interval for builders
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Unique identifier for a derivation (BLAKE3 hash of drv path)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DrvHash(pub [u8; 32]);

impl DrvHash {
    /// Create from a derivation path by hashing it
    pub fn from_drv_path(path: &str) -> Self {
        Self(*blake3::hash(path.as_bytes()).as_bytes())
    }

    /// Create from raw bytes
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hex string
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Display for DrvHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Unique job ID for tracking leased jobs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Input path with its BLAKE3 hash for fetching
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputPath {
    /// Store path (e.g., /nix/store/xxx-name)
    pub store_path: String,
    /// BLAKE3 hash of the NAR (for fetching from peers)
    pub blake3: [u8; 32],
}

/// A build job in the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildJob {
    /// Unique job ID
    pub id: JobId,
    /// Hash of the derivation
    pub drv_hash: DrvHash,
    /// Path to the derivation file
    pub drv_path: String,
    /// Required system features (e.g., "x86_64-linux", "kvm", "big-parallel")
    pub system_features: Vec<String>,
    /// Expected output paths (optional)
    pub outputs: Vec<String>,
    /// Input store paths needed for the build (with BLAKE3 hashes)
    pub input_paths: Vec<InputPath>,
    /// When the job was submitted
    pub submitted_at: u64,
}

/// State of a leased job
#[derive(Debug, Clone)]
pub struct LeasedJob {
    /// The job
    pub job: BuildJob,
    /// Builder that has the lease
    pub builder: EndpointId,
    /// When the lease was granted
    pub leased_at: Instant,
    /// Last heartbeat received
    pub last_heartbeat: Instant,
    /// Current status message
    pub status: String,
}

/// A single build output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutput {
    /// Store path (e.g., /nix/store/xxx-name)
    pub store_path: String,
    /// BLAKE3 hash of the NAR
    pub blake3: [u8; 32],
    /// SHA256 hash of the NAR (for Nix compatibility)
    pub sha256: [u8; 32],
    /// NAR size in bytes
    pub nar_size: u64,
}

/// Result of a completed build
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    /// Job ID
    pub job_id: JobId,
    /// Derivation hash
    pub drv_hash: DrvHash,
    /// The builder that completed it
    pub builder: [u8; 32],
    /// Output store paths with their hashes
    pub outputs: Vec<BuildOutput>,
    /// Signature over the outputs (Ed25519) - stored as two 32-byte arrays
    pub signature_r: [u8; 32],
    pub signature_s: [u8; 32],
}

/// State of a completed/failed job (for history)
#[derive(Debug, Clone)]
pub enum JobOutcome {
    Completed(BuildResult),
    Failed { builder: EndpointId, error: String },
}

/// A log line received from a builder
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Job ID
    pub job_id: u64,
    /// Log data (UTF-8)
    pub data: Vec<u8>,
    /// Stream: 1 = stdout, 2 = stderr
    pub stream: u8,
}

/// The build queue (requester-side)
///
/// Each requester maintains their own queue. Builders connect via RPC to pull jobs.
pub struct BuildQueue {
    /// Our endpoint ID
    our_id: EndpointId,
    /// Next job ID
    next_job_id: Mutex<u64>,
    /// Pending jobs (FIFO queue)
    pending: Mutex<VecDeque<BuildJob>>,
    /// Currently leased jobs (being built)
    leased: Mutex<HashMap<JobId, LeasedJob>>,
    /// Completed jobs (recent history)
    completed: Mutex<HashMap<DrvHash, JobOutcome>>,
    /// Completed builds that need output fetching
    pending_fetch: Mutex<Vec<BuildResult>>,
    /// Current system features we need (for gossip)
    needed_features: Mutex<Option<Vec<String>>>,
    /// Log subscribers (job_id -> list of senders)
    log_subscribers: Mutex<HashMap<u64, Vec<tokio::sync::mpsc::Sender<LogEntry>>>>,
}

impl BuildQueue {
    /// Create a new build queue
    pub fn new(our_id: EndpointId, _secret_key: SecretKey) -> Self {
        Self {
            our_id,
            next_job_id: Mutex::new(1),
            pending: Mutex::new(VecDeque::new()),
            leased: Mutex::new(HashMap::new()),
            completed: Mutex::new(HashMap::new()),
            pending_fetch: Mutex::new(Vec::new()),
            needed_features: Mutex::new(None),
            log_subscribers: Mutex::new(HashMap::new()),
        }
    }

    /// Push a new job onto the queue
    ///
    /// Returns the job ID and whether we need to announce (if features changed)
    pub fn push(
        &self,
        drv_path: &str,
        system_features: Vec<String>,
        outputs: Vec<String>,
        input_paths: Vec<InputPath>,
    ) -> (JobId, bool) {
        let job_id = {
            let mut next = self.next_job_id.lock().unwrap();
            let id = JobId(*next);
            *next += 1;
            id
        };

        let job = BuildJob {
            id: job_id,
            drv_hash: DrvHash::from_drv_path(drv_path),
            drv_path: drv_path.to_string(),
            system_features: system_features.clone(),
            outputs,
            input_paths,
            submitted_at: current_timestamp(),
        };

        let should_announce = {
            let mut pending = self.pending.lock().unwrap();
            let was_empty = pending.is_empty();
            pending.push_back(job);

            // Update needed features
            let mut needed = self.needed_features.lock().unwrap();
            if was_empty || needed.is_none() {
                *needed = Some(system_features);
                true
            } else {
                false
            }
        };

        info!("Queued build job {}: {}", job_id, drv_path);
        (job_id, should_announce)
    }

    /// Pull the next job from the queue (for builders)
    ///
    /// Returns None if queue is empty or no matching job exists.
    /// The job is moved to "leased" state.
    ///
    /// A job matches if the builder has all of the job's required system features.
    pub fn pull(&self, builder: EndpointId, builder_features: &[String]) -> Option<BuildJob> {
        let mut pending = self.pending.lock().unwrap();

        // Find the first job whose required features are a subset of builder's features
        let job_index = pending.iter().position(|job| {
            job.system_features
                .iter()
                .all(|required| builder_features.contains(required))
        })?;

        // Remove the matching job from the queue
        let job = pending.remove(job_index)?;

        let leased_job = LeasedJob {
            job: job.clone(),
            builder,
            leased_at: Instant::now(),
            last_heartbeat: Instant::now(),
            status: "starting".to_string(),
        };

        let mut leased = self.leased.lock().unwrap();
        leased.insert(job.id, leased_job);

        info!(
            "Job {} leased to builder {} (required features: {:?})",
            job.id, builder, job.system_features
        );
        Some(job)
    }

    /// Update heartbeat for a leased job
    pub fn heartbeat(&self, job_id: JobId, builder: EndpointId, status: String) -> bool {
        let mut leased = self.leased.lock().unwrap();
        if let Some(job) = leased.get_mut(&job_id) {
            if job.builder == builder {
                job.last_heartbeat = Instant::now();
                job.status = status;
                return true;
            }
        }
        false
    }

    /// Mark a job as completed
    pub fn complete(&self, job_id: JobId, builder: EndpointId, result: BuildResult) -> bool {
        let mut leased = self.leased.lock().unwrap();
        if let Some(job) = leased.remove(&job_id) {
            if job.builder == builder {
                let drv_hash = job.job.drv_hash.clone();
                info!("Job {} completed by {}", job_id, builder);

                // Add to pending fetch queue
                {
                    let mut pending_fetch = self.pending_fetch.lock().unwrap();
                    pending_fetch.push(result.clone());
                }

                let mut completed = self.completed.lock().unwrap();
                completed.insert(drv_hash, JobOutcome::Completed(result));
                return true;
            } else {
                // Wrong builder, put it back
                leased.insert(job_id, job);
            }
        }
        false
    }

    /// Mark a job as failed
    pub fn fail(&self, job_id: JobId, builder: EndpointId, error: String) -> bool {
        let mut leased = self.leased.lock().unwrap();
        if let Some(job) = leased.remove(&job_id) {
            if job.builder == builder {
                let drv_hash = job.job.drv_hash.clone();
                warn!("Job {} failed: {}", job_id, error);

                let mut completed = self.completed.lock().unwrap();
                completed.insert(drv_hash, JobOutcome::Failed { builder, error });
                return true;
            } else {
                leased.insert(job_id, job);
            }
        }
        false
    }

    /// Check for timed-out leases and return jobs to queue
    pub fn check_timeouts(&self) -> Vec<JobId> {
        let mut leased = self.leased.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();

        let now = Instant::now();
        let mut timed_out = Vec::new();

        leased.retain(|job_id, leased_job| {
            if now.duration_since(leased_job.last_heartbeat) > JOB_LEASE_TIMEOUT {
                warn!(
                    "Job {} timed out (builder {}), returning to queue",
                    job_id, leased_job.builder
                );
                // Return to front of queue
                pending.push_front(leased_job.job.clone());
                timed_out.push(*job_id);
                false
            } else {
                true
            }
        });

        timed_out
    }

    /// Check if we have pending work
    pub fn has_pending_work(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    /// Get the system features we currently need builders for
    pub fn needed_features(&self) -> Option<Vec<String>> {
        self.needed_features.lock().unwrap().clone()
    }

    /// Clear needed features (when queue becomes empty)
    pub fn clear_needed_features(&self) {
        *self.needed_features.lock().unwrap() = None;
    }

    /// Get queue statistics
    pub fn stats(&self) -> QueueStats {
        QueueStats {
            pending_count: self.pending.lock().unwrap().len(),
            leased_count: self.leased.lock().unwrap().len(),
            completed_count: self.completed.lock().unwrap().len(),
        }
    }

    /// Get the outcome of a build by drv hash
    pub fn get_outcome(&self, drv_hash: &DrvHash) -> Option<JobOutcome> {
        self.completed.lock().unwrap().get(drv_hash).cloned()
    }

    /// List all pending jobs
    pub fn list_pending(&self) -> Vec<BuildJob> {
        self.pending.lock().unwrap().iter().cloned().collect()
    }

    /// List all leased jobs
    pub fn list_leased(&self) -> Vec<(JobId, LeasedJob)> {
        self.leased
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    /// Our endpoint ID
    pub fn our_id(&self) -> EndpointId {
        self.our_id
    }

    /// Take all pending fetch results (clears the queue)
    ///
    /// Returns build results that have completed but whose outputs haven't been fetched yet.
    pub fn take_pending_fetches(&self) -> Vec<BuildResult> {
        let mut pending_fetch = self.pending_fetch.lock().unwrap();
        std::mem::take(&mut *pending_fetch)
    }

    /// Check if there are pending fetches
    pub fn has_pending_fetches(&self) -> bool {
        !self.pending_fetch.lock().unwrap().is_empty()
    }

    /// Subscribe to logs for a specific job
    ///
    /// Returns a receiver that will get log entries as they arrive.
    pub fn subscribe_logs(&self, job_id: u64) -> tokio::sync::mpsc::Receiver<LogEntry> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let mut subs = self.log_subscribers.lock().unwrap();
        subs.entry(job_id).or_default().push(tx);
        rx
    }

    /// Subscribe to logs for all jobs
    ///
    /// Returns a receiver that will get log entries from all jobs.
    pub fn subscribe_all_logs(&self) -> tokio::sync::mpsc::Receiver<LogEntry> {
        // Use job_id 0 as a wildcard for "all jobs"
        self.subscribe_logs(0)
    }

    /// Broadcast a log entry to all subscribers
    pub fn broadcast_log(&self, job_id: u64, data: Vec<u8>, stream: u8) {
        let entry = LogEntry {
            job_id,
            data,
            stream,
        };

        let mut subs = self.log_subscribers.lock().unwrap();

        // Send to job-specific subscribers
        if let Some(subscribers) = subs.get_mut(&job_id) {
            subscribers.retain(|tx| tx.try_send(entry.clone()).is_ok());
        }

        // Send to wildcard subscribers (job_id 0)
        if let Some(subscribers) = subs.get_mut(&0) {
            subscribers.retain(|tx| tx.try_send(entry.clone()).is_ok());
        }
    }

    /// Unsubscribe from logs (cleanup dead senders)
    pub fn cleanup_log_subscribers(&self) {
        let mut subs = self.log_subscribers.lock().unwrap();
        subs.retain(|_, v| {
            v.retain(|tx| !tx.is_closed());
            !v.is_empty()
        });
    }
}

/// Queue statistics
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub pending_count: usize,
    pub leased_count: usize,
    pub completed_count: usize,
}

/// Builder-side: execute a build using nix-store
///
/// Uses `--option build-hook ""` to disable the build hook for this invocation,
/// preventing infinite recursion when the same machine is both builder and requester.
pub async fn execute_build(drv_path: &str) -> Result<Vec<PathBuf>> {
    info!("Executing build: {}", drv_path);

    let output = Command::new("nix-store")
        .args(["--realise", "--option", "build-hook", "", drv_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| Error::Build(format!("failed to run nix-store: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Build(stderr.into_owned()));
    }

    // Parse output paths from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<PathBuf> = stdout
        .lines()
        .filter(|line| line.starts_with("/nix/store/"))
        .map(PathBuf::from)
        .collect();

    info!("Build completed: {} outputs", paths.len());
    Ok(paths)
}

/// Log line from a build
#[derive(Debug, Clone)]
pub struct BuildLogLine {
    /// Job ID this log belongs to
    pub job_id: u64,
    /// Log data (UTF-8)
    pub data: Vec<u8>,
    /// Stream: 1 = stdout, 2 = stderr
    pub stream: u8,
}

/// Builder-side: execute a build with log streaming
///
/// The `log_tx` channel receives log lines as they're produced.
/// Returns the output paths on success.
///
/// Uses `--option build-hook ""` to disable the build hook for this invocation,
/// preventing infinite recursion when the same machine is both builder and requester.
pub async fn execute_build_with_logs(
    drv_path: &str,
    job_id: u64,
    log_tx: tokio::sync::mpsc::Sender<BuildLogLine>,
) -> Result<Vec<PathBuf>> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    info!("Executing build with log streaming: {}", drv_path);

    let mut child = Command::new("nix-store")
        .args(["--realise", "--option", "build-hook", "", drv_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Build(format!("failed to spawn nix-store: {}", e)))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let log_tx_stdout = log_tx.clone();
    let log_tx_stderr = log_tx;

    // Capture stdout for parsing output paths
    let stdout_capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stdout_capture_clone = stdout_capture.clone();

    // Stream stdout
    let stdout_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            // Capture for parsing
            stdout_capture_clone.lock().unwrap().push(line.clone());
            // Send to log channel
            let _ = log_tx_stdout
                .send(BuildLogLine {
                    job_id,
                    data: line.into_bytes(),
                    stream: 1,
                })
                .await;
        }
    });

    // Stream stderr
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = log_tx_stderr
                .send(BuildLogLine {
                    job_id,
                    data: line.into_bytes(),
                    stream: 2,
                })
                .await;
        }
    });

    // Wait for the process
    let status = child
        .wait()
        .await
        .map_err(|e| Error::Build(format!("failed to wait for nix-store: {}", e)))?;

    // Wait for log readers to finish
    let _ = stdout_handle.await;
    let _ = stderr_handle.await;

    if !status.success() {
        return Err(Error::Build(format!(
            "build failed with exit code: {:?}",
            status.code()
        )));
    }

    // Parse output paths from captured stdout
    let stdout_lines = stdout_capture.lock().unwrap();
    let paths: Vec<PathBuf> = stdout_lines
        .iter()
        .filter(|line| line.starts_with("/nix/store/"))
        .map(PathBuf::from)
        .collect();

    info!("Build completed: {} outputs", paths.len());
    Ok(paths)
}

/// Create a signed build result
pub fn sign_build_result(
    secret_key: &SecretKey,
    job_id: JobId,
    drv_hash: DrvHash,
    outputs: Vec<BuildOutput>,
) -> BuildResult {
    let builder = *secret_key.public().as_bytes();

    let mut result = BuildResult {
        job_id,
        drv_hash,
        builder,
        outputs,
        signature_r: [0u8; 32],
        signature_s: [0u8; 32],
    };

    // Create data to sign
    let data = result_to_sign_data(&result);
    let signature = secret_key.sign(&data);
    let sig_bytes = signature.to_bytes();
    result.signature_r.copy_from_slice(&sig_bytes[..32]);
    result.signature_s.copy_from_slice(&sig_bytes[32..]);

    result
}

/// Verify a build result signature
pub fn verify_build_result(result: &BuildResult) -> bool {
    let Ok(builder_id) = EndpointId::from_bytes(&result.builder) else {
        return false;
    };

    let data = result_to_sign_data(result);

    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(&result.signature_r);
    sig_bytes[32..].copy_from_slice(&result.signature_s);
    let signature = iroh_base::Signature::from_bytes(&sig_bytes);

    builder_id.verify(&data, &signature).is_ok()
}

/// Create data to sign for a build result
fn result_to_sign_data(result: &BuildResult) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&result.job_id.0.to_le_bytes());
    data.extend_from_slice(&result.drv_hash.0);
    data.extend_from_slice(&result.builder);
    for output in &result.outputs {
        data.extend_from_slice(output.store_path.as_bytes());
        data.extend_from_slice(&output.blake3);
        data.extend_from_slice(&output.sha256);
        data.extend_from_slice(&output.nar_size.to_le_bytes());
    }
    data
}

/// Get current timestamp in milliseconds
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_queue() -> BuildQueue {
        let secret_key = SecretKey::generate(&mut rand::rng());
        let endpoint_id = secret_key.public();
        BuildQueue::new(endpoint_id, secret_key)
    }

    #[test]
    fn test_drv_hash() {
        let hash1 = DrvHash::from_drv_path("/nix/store/abc123.drv");
        let hash2 = DrvHash::from_drv_path("/nix/store/abc123.drv");
        let hash3 = DrvHash::from_drv_path("/nix/store/def456.drv");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_queue_push_pull() {
        let queue = make_queue();
        let builder = SecretKey::generate(&mut rand::rng()).public();

        // Push a job
        let (job_id, should_announce) = queue.push(
            "/nix/store/test.drv",
            vec!["x86_64-linux".to_string()],
            vec![],
            vec![], // no input paths
        );
        assert!(should_announce);
        assert_eq!(queue.stats().pending_count, 1);

        // Pull the job (builder has matching features)
        let builder_features = vec!["x86_64-linux".to_string()];
        let job = queue.pull(builder, &builder_features).unwrap();
        assert_eq!(job.id, job_id);
        assert_eq!(queue.stats().pending_count, 0);
        assert_eq!(queue.stats().leased_count, 1);

        // Queue should be empty now
        assert!(queue.pull(builder, &builder_features).is_none());
    }

    #[test]
    fn test_queue_completion() {
        let queue = make_queue();
        let secret_key = SecretKey::generate(&mut rand::rng());
        let builder = secret_key.public();

        let (job_id, _) = queue.push("/nix/store/test.drv", vec![], vec![], vec![]);
        let job = queue.pull(builder, &[]).unwrap();

        // Complete the job
        let result = sign_build_result(
            &secret_key,
            job.id,
            job.drv_hash.clone(),
            vec![BuildOutput {
                store_path: "/nix/store/output".to_string(),
                blake3: [1u8; 32],
                sha256: [2u8; 32],
                nar_size: 100,
            }],
        );

        assert!(verify_build_result(&result));
        assert!(queue.complete(job_id, builder, result));
        assert_eq!(queue.stats().leased_count, 0);
        assert_eq!(queue.stats().completed_count, 1);
    }

    #[test]
    fn test_queue_timeout() {
        let queue = make_queue();
        let builder = SecretKey::generate(&mut rand::rng()).public();

        queue.push("/nix/store/test.drv", vec![], vec![], vec![]);
        let job = queue.pull(builder, &[]).unwrap();

        // Manually expire the lease
        {
            let mut leased = queue.leased.lock().unwrap();
            if let Some(lease) = leased.get_mut(&job.id) {
                lease.last_heartbeat = Instant::now() - JOB_LEASE_TIMEOUT - Duration::from_secs(1);
            }
        }

        // Check timeouts
        let timed_out = queue.check_timeouts();
        assert_eq!(timed_out.len(), 1);
        assert_eq!(timed_out[0], job.id);

        // Job should be back in pending queue
        assert_eq!(queue.stats().pending_count, 1);
        assert_eq!(queue.stats().leased_count, 0);
    }

    #[test]
    fn test_feature_filtering() {
        let queue = make_queue();
        let builder = SecretKey::generate(&mut rand::rng()).public();

        // Push job requiring "kvm" feature
        let (job_id, _) = queue.push(
            "/nix/store/kvm-job.drv",
            vec!["x86_64-linux".to_string(), "kvm".to_string()],
            vec![],
            vec![],
        );

        // Builder without kvm feature can't pull the job
        let no_kvm_features = vec!["x86_64-linux".to_string()];
        assert!(queue.pull(builder, &no_kvm_features).is_none());
        assert_eq!(queue.stats().pending_count, 1); // Job still pending

        // Builder with kvm feature can pull the job
        let with_kvm_features = vec!["x86_64-linux".to_string(), "kvm".to_string()];
        let job = queue.pull(builder, &with_kvm_features).unwrap();
        assert_eq!(job.id, job_id);
        assert_eq!(queue.stats().pending_count, 0);
    }

    #[test]
    fn test_feature_filtering_multiple_jobs() {
        let queue = make_queue();
        let builder = SecretKey::generate(&mut rand::rng()).public();

        // Push two jobs: one needs kvm, one doesn't
        let (simple_id, _) = queue.push(
            "/nix/store/simple.drv",
            vec!["x86_64-linux".to_string()],
            vec![],
            vec![],
        );
        let (_kvm_id, _) = queue.push(
            "/nix/store/kvm-job.drv",
            vec!["x86_64-linux".to_string(), "kvm".to_string()],
            vec![],
            vec![],
        );

        // Builder without kvm pulls the simple job first
        let no_kvm_features = vec!["x86_64-linux".to_string()];
        let job = queue.pull(builder, &no_kvm_features).unwrap();
        assert_eq!(job.id, simple_id); // Gets the simple job, skipping the kvm job

        // kvm job is still pending
        assert_eq!(queue.stats().pending_count, 1);
    }
}
