# Distributed Build System

iroh-nix implements a pull-based distributed build system where builders actively pull work from requesters.

## Design Philosophy

### Pull vs Push

Traditional distributed build systems push jobs to workers. iroh-nix uses a **pull model**:

- Requesters announce they need builders
- Builders connect to requesters
- Builders pull jobs they can handle
- Natural load balancing - fast builders pull more

Benefits:
- No central coordinator needed
- Builders only take what they can handle
- Graceful handling of builder capacity
- Works with heterogeneous builder pools

## Roles

### Requester

A node that has derivations to build:

- Maintains a build queue (`BuildQueue`)
- Announces `NeedBuilder` via gossip
- Accepts builder connections
- Tracks job status and leases
- Fetches outputs from builders

### Builder

A node that executes builds:

- Advertises available features
- Listens for `NeedBuilder` messages
- Connects to matching requesters
- Pulls and executes jobs
- Reports results with signatures

A single node can be both requester and builder.

## Job Lifecycle

```
                    +-------------+
                    |  Submitted  |
                    +------+------+
                           |
                           | push to queue
                           v
+----------+         +-----+------+
| Timeout  |<--------|   Pending  |
| (return) |         +-----+------+
+----------+               |
     ^                     | builder pulls
     |                     v
     |               +-----+------+
     +---------------|   Leased   |
       no heartbeat  +-----+------+
                           |
              +------------+------------+
              |                         |
              v                         v
        +-----+------+           +------+-----+
        |  Completed |           |   Failed   |
        +------------+           +------------+
```

### 1. Submission

```rust
// On requester
let (job_id, should_announce) = node.push_build(
    "/nix/store/xyz.drv",
    vec!["x86_64-linux", "kvm"],  // required features
    vec!["/nix/store/output"],    // expected outputs
    input_paths,                   // dependencies with hashes
);

// Announce if this is first pending job
if should_announce {
    gossip.announce_need_builder(system_features).await;
}
```

### 2. Discovery

Builder receives gossip:

```rust
GossipMessage::NeedBuilder {
    system_features: ["x86_64-linux"],
}
```

Builder checks if it has all required features:

```rust
// Builder has: ["x86_64-linux", "kvm", "big-parallel"]
// Required:    ["x86_64-linux"]
// Match: yes (required is subset of available)
```

### 3. Pull

Builder connects and requests a job:

```rust
BuildQueueRequest::Pull {
    system_features: vec!["x86_64-linux", "kvm", "big-parallel"],
    stream_logs: true,
}
```

Requester finds matching job:

```rust
// Job requires: ["x86_64-linux", "kvm"]
// Builder has:  ["x86_64-linux", "kvm", "big-parallel"]
// Match: yes

BuildQueueResponse::Job {
    job_id: 1,
    drv_hash: [...],
    drv_path: "/nix/store/xyz.drv",
    outputs: [...],
    input_paths: [...],
}
```

### 4. Preparation

Builder fetches input dependencies:

```rust
for input in job.input_paths {
    // Check if we have it locally
    if !have_blob(&input.blake3) {
        // Fetch from requester via NAR protocol
        fetch_nar(&endpoint, requester_addr, input.blake3).await?;
    }
}
```

### 5. Execution

Builder runs the build:

```bash
nix-store --realise /nix/store/xyz.drv
```

During execution:
- Heartbeats sent every 10 seconds
- Logs streamed if requested

```rust
// Heartbeat
BuildQueueRequest::Heartbeat {
    job_id: 1,
    status: "building".to_string(),
}

// Log streaming
BuildQueueRequest::BuildLog {
    job_id: 1,
    line: "configure: checking for gcc...".to_string(),
    is_stderr: false,
}
```

### 6. Completion

Builder serializes outputs to NAR and signs result:

```rust
let result = sign_build_result(
    &secret_key,
    job_id,
    drv_hash,
    vec![BuildOutput {
        store_path: "/nix/store/output",
        blake3: [...],
        sha256: [...],
        nar_size: 12345,
    }],
);

BuildQueueRequest::Complete {
    job_id: 1,
    outputs: result.outputs,
    signature_r: result.signature_r,
    signature_s: result.signature_s,
}
```

### 7. Output Fetching

Requester fetches outputs from builder:

```rust
for output in completed_job.outputs {
    // Fetch NAR from builder
    let (header, nar_data) = fetch_nar(&endpoint, builder_addr, output.blake3).await?;

    // Store locally
    store_blob(&output.blake3, &nar_data)?;

    // Import into Nix store
    run("nix-store", ["--restore", output.store_path]).await?;
}
```

## Feature Matching

Features represent builder capabilities:

| Feature | Meaning |
|---------|---------|
| `x86_64-linux` | Linux x86_64 architecture |
| `aarch64-linux` | Linux ARM64 architecture |
| `kvm` | Hardware virtualization available |
| `big-parallel` | Many cores for parallel builds |
| `nixos-test` | Can run NixOS VM tests |

### Matching Algorithm

A builder can handle a job if it has **all** required features:

```rust
fn can_handle(job_features: &[String], builder_features: &[String]) -> bool {
    job_features.iter().all(|f| builder_features.contains(f))
}
```

Examples:

| Job Requires | Builder Has | Match? |
|-------------|-------------|--------|
| `[x86_64-linux]` | `[x86_64-linux, kvm]` | Yes |
| `[x86_64-linux, kvm]` | `[x86_64-linux]` | No |
| `[]` | `[x86_64-linux]` | Yes |
| `[kvm]` | `[x86_64-linux, kvm]` | Yes |

### Queue Selection

When a builder pulls, the queue returns the first matching job:

```rust
pub fn pull(&self, builder: EndpointId, builder_features: &[String]) -> Option<BuildJob> {
    let mut pending = self.pending.lock()?;

    // Find first job where builder has all required features
    let job_index = pending.iter().position(|job| {
        job.system_features.iter().all(|f| builder_features.contains(f))
    })?;

    pending.remove(job_index)
}
```

## Timeouts and Retries

### Job Lease Timeout

Jobs are leased for 60 seconds. Without heartbeats, they return to queue:

```rust
const JOB_LEASE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn check_timeouts(&self) -> Vec<JobId> {
    let now = Instant::now();
    let mut timed_out = Vec::new();

    for (job_id, lease) in self.leased.iter() {
        if now.duration_since(lease.last_heartbeat) > JOB_LEASE_TIMEOUT {
            timed_out.push(*job_id);
        }
    }

    // Return timed out jobs to pending queue
    for job_id in &timed_out {
        if let Some(lease) = self.leased.remove(job_id) {
            self.pending.push_back(lease.job);
        }
    }

    timed_out
}
```

### Builder Idle Timeout

Builders disconnect after 30 seconds without a job:

```rust
const BUILDER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
```

### Heartbeat Interval

Builders send heartbeats every 10 seconds:

```rust
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
```

## Signatures

Build results are signed for authenticity:

### Signing

```rust
pub fn sign_build_result(
    secret_key: &SecretKey,
    job_id: JobId,
    drv_hash: DrvHash,
    outputs: Vec<BuildOutput>,
) -> BuildResult {
    // Create message to sign
    let mut message = Vec::new();
    message.extend_from_slice(&job_id.0.to_le_bytes());
    message.extend_from_slice(drv_hash.as_bytes());
    for output in &outputs {
        message.extend_from_slice(&output.blake3);
    }

    // Sign with Ed25519
    let signature = secret_key.sign(&message);

    BuildResult {
        job_id,
        drv_hash,
        outputs,
        signature_r: signature.r_bytes(),
        signature_s: signature.s_bytes(),
        builder_id: secret_key.public(),
    }
}
```

### Verification

```rust
pub fn verify_build_result(result: &BuildResult) -> bool {
    let mut message = Vec::new();
    message.extend_from_slice(&result.job_id.0.to_le_bytes());
    message.extend_from_slice(result.drv_hash.as_bytes());
    for output in &result.outputs {
        message.extend_from_slice(&output.blake3);
    }

    let signature = Signature::from_components(result.signature_r, result.signature_s);
    result.builder_id.verify(&message, &signature).is_ok()
}
```

## Dependency Resolution

When queuing a derivation, dependencies are resolved first:

### Workflow

```rust
// 1. Parse derivation and all dependencies
let drv_json = run("nix", ["derivation", "show", "--recursive", drv_path]).await?;
let drvs: HashMap<String, DrvInfo> = serde_json::from_str(&drv_json)?;

// 2. Topological sort (Kahn's algorithm)
let build_order = topological_sort(&drvs)?;

// 3. Queue in dependency order
for drv_path in build_order {
    let drv = &drvs[&drv_path];

    // Collect input paths with hashes
    let input_paths = collect_inputs(drv)?;

    node.push_build(
        &drv_path,
        drv.system_features.clone(),
        drv.outputs.clone(),
        input_paths,
    ).await?;
}
```

### Topological Sort

Ensures dependencies are built before dependents:

```
A depends on B, C
B depends on D
C depends on D

Build order: D, B, C, A
```

## Log Streaming

Builders can stream build output to requesters:

### Builder Side

```rust
// Capture stdout/stderr during build
let mut child = Command::new("nix-store")
    .arg("--realise")
    .arg(&drv_path)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// Stream stdout
while let Some(line) = stdout.next_line().await? {
    send_request(BuildQueueRequest::BuildLog {
        job_id,
        line,
        is_stderr: false,
    }).await?;
}
```

### Requester Side

```rust
// Subscribe to logs
let rx = build_queue.subscribe_logs(Some(job_id));

// Or subscribe to all logs
let rx = build_queue.subscribe_logs(None);

while let Some(entry) = rx.recv().await {
    println!("[{}] {}", entry.job_id, entry.line);
}
```

## Example Session

```
# Terminal 1: Requester
$ iroh-nix --network cluster daemon &
$ iroh-nix --network cluster build-push ./result.drv
Queuing build: /nix/store/abc123.drv (requires: x86_64-linux)
Queuing build: /nix/store/def456.drv (requires: x86_64-linux, kvm)
Announcing NeedBuilder...
Waiting for builders...
Job 1 leased to builder xyz789...
Job 1 completed, fetching outputs...
Job 2 leased to builder xyz789...
Job 2 completed, fetching outputs...
Build complete!

# Terminal 2: Builder
$ iroh-nix --network cluster builder --features x86_64-linux,kvm
Listening for build requests...
Connecting to requester abc123...
Pulled job 1: /nix/store/abc123.drv
Fetching inputs...
Building...
  configure: checking for gcc... yes
  make: Building target 'all'
Build complete, reporting...
Pulled job 2: /nix/store/def456.drv
...
```

## Error Handling

### Build Failure

```rust
BuildQueueRequest::Fail {
    job_id: 1,
    error: "build failed: exit code 1".to_string(),
}
```

Failed jobs are recorded but not re-queued automatically.

### Connection Loss

- Builder disconnects: Leased jobs timeout and return to queue
- Requester disconnects: Builder reconnects to other requesters

### Invalid Results

- Signature verification failure: Result rejected
- Missing outputs: Result rejected
- Hash mismatch: Result rejected
