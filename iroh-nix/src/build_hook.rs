//! Nix build-hook implementation
//!
//! This module implements the Nix build-hook protocol, allowing nix-daemon to
//! transparently offload builds to the iroh network.
//!
//! The build hook is invoked by nix-daemon for each derivation that needs building.
//! It communicates with the running iroh-nix daemon via a Unix socket to submit
//! builds, poll status, and stream logs.
//!
//! ## Nix Build-Hook Protocol (FD-based)
//!
//! FD 0 (stdin): Read settings and per-build requests from nix-daemon
//! FD 2 (stderr): Write accept/decline/postpone decisions
//! FD 4: Write build logs back to nix-daemon
//!
//! ### Initialization
//! 1. Read global settings: loop { read u32 flag; if 1: read key+value strings; if 0: stop }
//!
//! ### Per-build loop (reads from FD 0)
//! 1. Read string "try"
//! 2. Read u32 amWilling (whether local machine could build this)
//! 3. Read string neededSystem (e.g., "x86_64-linux")
//! 4. Read string drvPath (store path of .drv)
//! 5. Read StringSet requiredFeatures
//!
//! ### Decision (written to FD 2/stderr)
//! - "# accept\n" + machine-spec line: accept the build
//! - "# decline\n": decline (let nix build locally)
//! - "# postpone\n": try again later
//!
//! ### After accept
//! 1. Read StorePathSet from FD 0 (input paths)
//! 2. Read StringSet from FD 0 (missing output names)
//! 3. FD 0 closed
//! 4. Submit build via control socket, poll to completion
//! 5. Stream logs to FD 4
//! 6. Exit 0 on success

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::io::FromRawFd;
use std::path::Path;

use tracing::{debug, info, warn};

use crate::control::{BuildState, ControlClient, InputPathInfo};
use crate::nix_protocol;
use crate::Result;

/// Run the build-hook protocol
///
/// This function reads from FD 0 (stdin) and writes decisions to FD 2 (stderr).
/// It communicates with the daemon via the control socket.
pub async fn run_build_hook(socket_path: &Path) -> Result<()> {
    let mut stdin = io::stdin().lock();

    // Phase 1: Read global settings
    let settings = read_settings(&mut stdin)?;
    debug!("Build hook received {} settings", settings.len());
    for (k, v) in &settings {
        debug!("  setting: {} = {}", k, v);
    }

    // Connect to daemon control socket
    let mut client = ControlClient::connect(socket_path).await?;

    // Phase 2: Per-build loop
    loop {
        // Read the "try" string
        let try_str = match nix_protocol::read_string(&mut stdin) {
            Ok(s) => s,
            Err(_) => {
                // EOF or error means nix-daemon is done
                debug!("Build hook: stdin closed, exiting");
                break;
            }
        };

        if try_str != "try" {
            warn!("Build hook: expected 'try', got '{}'", try_str);
            break;
        }

        // Read amWilling (u32)
        let am_willing = nix_protocol::read_u32(&mut stdin)?;
        debug!("Build hook: amWilling = {}", am_willing);

        // Read neededSystem (string)
        let needed_system = nix_protocol::read_string(&mut stdin)?;
        debug!("Build hook: neededSystem = {}", needed_system);

        // Read drvPath (string)
        let drv_path = nix_protocol::read_string(&mut stdin)?;
        debug!("Build hook: drvPath = {}", drv_path);

        // Read requiredFeatures (StringSet)
        let required_features = nix_protocol::read_string_set(&mut stdin)?;
        debug!("Build hook: requiredFeatures = {:?}", required_features);

        // Decision: check if we have builders available
        let mut system_features = vec![needed_system.clone()];
        system_features.extend(required_features.clone());

        let (available, _count) = client.has_builders(system_features.clone()).await?;

        if !available {
            // No builders known - decline and let nix build locally
            // (or if am_willing is 0, postpone)
            if am_willing == 0 {
                // Nix can't build locally and we have no builders - postpone
                write_decision("# postpone\n")?;
            } else {
                // Nix can build locally - decline so it does
                write_decision("# decline\n")?;
            }
            continue;
        }

        // Accept the build
        // Write "# accept\n" followed by a machine spec line
        // The machine spec format is: <store-uri> <system> <ssh-key> <max-jobs> <speed-factor> <features> <mandatory-features> <ssh-public-key>
        // For our purposes, we use a dummy spec since we handle the build ourselves
        write_decision("# accept\n")?;

        // Write a machine spec line (nix-daemon expects this after accept)
        // Format: storeUri system sshKey maxJobs speedFactor supportedFeatures mandatoryFeatures publicKey
        let machine_spec = format!(
            "ssh://iroh-builder {} - 1 1 {} {} -\n",
            needed_system,
            system_features.join(","),
            required_features.join(","),
        );
        write_decision(&machine_spec)?;

        // After accept, read input paths and missing outputs
        let input_store_paths = nix_protocol::read_string_set(&mut stdin)?;
        debug!("Build hook: {} input store paths", input_store_paths.len());

        let missing_outputs = nix_protocol::read_string_set(&mut stdin)?;
        debug!("Build hook: {} missing outputs", missing_outputs.len());

        // Build input path infos (we don't have blake3 hashes here,
        // the daemon will compute them as needed)
        let input_paths: Vec<InputPathInfo> = input_store_paths
            .iter()
            .map(|sp| InputPathInfo {
                store_path: sp.clone(),
                blake3: [0u8; 32], // daemon will look up or compute
            })
            .collect();

        // Submit build to daemon
        let job_id = client
            .submit_build(
                &drv_path,
                &needed_system,
                required_features,
                input_paths,
                missing_outputs,
            )
            .await?;

        info!("Build hook: submitted job {} for {}", job_id, drv_path);

        // Open FD 4 for log output (if available)
        let mut log_fd = open_log_fd();

        // Poll until completion
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let status = client.poll_build(job_id).await?;

            match status {
                BuildState::Pending => {
                    // Still waiting for a builder to pick it up
                    continue;
                }
                BuildState::Building { builder_id } => {
                    // In progress
                    if let Some(ref mut fd) = log_fd {
                        let _ = writeln!(fd, "iroh-nix: building on remote builder {}", builder_id);
                    }
                    continue;
                }
                BuildState::Completed { outputs } => {
                    info!(
                        "Build hook: job {} completed with {} outputs",
                        job_id,
                        outputs.len()
                    );
                    if let Some(ref mut fd) = log_fd {
                        for output in &outputs {
                            let _ = writeln!(
                                fd,
                                "iroh-nix: output {} ({} bytes)",
                                output.store_path, output.nar_size
                            );
                        }
                    }
                    // Success - outputs should be in local store
                    // (the daemon fetches them as part of build completion)
                    break;
                }
                BuildState::Failed { error } => {
                    warn!("Build hook: job {} failed: {}", job_id, error);
                    if let Some(ref mut fd) = log_fd {
                        let _ = writeln!(fd, "iroh-nix: build failed: {}", error);
                    }
                    // Return error so nix-daemon knows the build failed
                    return Err(crate::Error::Build(format!(
                        "remote build failed: {}",
                        error
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Read settings from the build-hook initialization protocol
///
/// Format: loop { read u32 flag; if 1: read key string + value string; if 0: stop }
fn read_settings<R: Read>(reader: &mut R) -> Result<HashMap<String, String>> {
    let mut settings = HashMap::new();

    loop {
        let flag = nix_protocol::read_u32(reader)?;
        if flag == 0 {
            break;
        }
        if flag != 1 {
            return Err(crate::Error::Protocol(format!(
                "unexpected settings flag: {}",
                flag
            )));
        }

        let key = nix_protocol::read_string(reader)?;
        let value = nix_protocol::read_string(reader)?;
        settings.insert(key, value);
    }

    Ok(settings)
}

/// Write a decision string to stderr (FD 2)
fn write_decision(msg: &str) -> Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(msg.as_bytes())?;
    stderr.flush()?;
    Ok(())
}

/// Open FD 4 for log output (if available)
///
/// FD 4 is provided by nix-daemon for build log streaming.
/// Returns None if FD 4 is not available.
fn open_log_fd() -> Option<std::fs::File> {
    // Safety: FD 4 is provided by nix-daemon specifically for build hooks.
    // We take ownership of it here for writing logs. We duplicate the fd
    // first to check validity without accidentally closing it.
    unsafe {
        let dup_fd = libc_dup(4);
        if dup_fd < 0 {
            return None;
        }
        // Close the duplicate, use the original
        libc_close(dup_fd);
        Some(std::fs::File::from_raw_fd(4))
    }
}

// Minimal libc wrappers to avoid a full libc dependency
extern "C" {
    fn dup(fd: i32) -> i32;
    fn close(fd: i32) -> i32;
}

unsafe fn libc_dup(fd: i32) -> i32 {
    unsafe { dup(fd) }
}

unsafe fn libc_close(fd: i32) -> i32 {
    unsafe { close(fd) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_read_settings_empty() {
        // Just a terminating flag of 0
        let mut buf = Vec::new();
        nix_protocol::write_u32(&mut buf, 0).unwrap();

        let mut cursor = Cursor::new(&buf);
        let settings = read_settings(&mut cursor).unwrap();
        assert!(settings.is_empty());
    }

    #[test]
    fn test_read_settings_with_entries() {
        let mut buf = Vec::new();

        // Setting 1
        nix_protocol::write_u32(&mut buf, 1).unwrap();
        nix_protocol::write_string(&mut buf, "max-jobs").unwrap();
        nix_protocol::write_string(&mut buf, "4").unwrap();

        // Setting 2
        nix_protocol::write_u32(&mut buf, 1).unwrap();
        nix_protocol::write_string(&mut buf, "cores").unwrap();
        nix_protocol::write_string(&mut buf, "8").unwrap();

        // End
        nix_protocol::write_u32(&mut buf, 0).unwrap();

        let mut cursor = Cursor::new(&buf);
        let settings = read_settings(&mut cursor).unwrap();

        assert_eq!(settings.len(), 2);
        assert_eq!(settings.get("max-jobs").unwrap(), "4");
        assert_eq!(settings.get("cores").unwrap(), "8");
    }
}
