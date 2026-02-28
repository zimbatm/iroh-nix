# Wire Protocols

iroh-nix uses three ALPN-negotiated protocols over QUIC connections.

## Protocol Identifiers

| ALPN | Purpose |
|------|---------|
| `/iroh-nix/nar/1` | NAR blob transfer |
| `/iroh-nix/build-queue/1` | Build job coordination |
| `/iroh-gossip/0` | Peer discovery (iroh-gossip) |

## Serialization

All structured messages use [postcard](https://crates.io/crates/postcard) - a compact binary format based on serde. Length prefixes are 4-byte little-endian for NAR protocol and big-endian for build queue protocol.

## NAR Transfer Protocol

Used for transferring NAR blobs between nodes.

### Request

```
+----------------+----------------------+
| 4 bytes (LE)   | N bytes              |
| message length | postcard(NarRequest) |
+----------------+----------------------+
```

```rust
struct NarRequest {
    blake3: [u8; 32],  // BLAKE3 hash of the NAR
}
```

### Response (Success)

```
+--------+----------------+---------------------------+-------------+
| 1 byte | 4 bytes (LE)   | N bytes                   | M bytes     |
| 0x01   | header length  | postcard(NarResponseHeader)| NAR stream |
+--------+----------------+---------------------------+-------------+
```

```rust
struct NarResponseHeader {
    size: u64,           // NAR size in bytes
    sha256: [u8; 32],    // SHA256 hash
    store_path: String,  // e.g., "/nix/store/abc123-hello"
}
```

### Response (Error)

```
+--------+----------------+-----------+
| 1 byte | 4 bytes (LE)   | N bytes   |
| 0x00   | message length | error msg |
+--------+----------------+-----------+
```

### Flow

```
Client                              Server
   |                                   |
   |  NarRequest(blake3)               |
   |---------------------------------->|
   |                                   | lookup hash_index
   |                                   | open blob file
   |  0x01 + header + NAR stream       |
   |<----------------------------------|
   |                                   |
   | verify BLAKE3                     |
   | store blob                        |
```

### Limits

- Max request size: 1 KB
- Max NAR size: 10 GB
- Transfer buffer: 64 KB chunks

## Build Queue Protocol

Used for builder-requester communication.

### Message Framing

```
+----------------+--------------------+
| 4 bytes (BE)   | N bytes            |
| message length | postcard(message)  |
+----------------+--------------------+
```

### Request Types

#### Pull

Request next available job matching builder's features.

```rust
BuildQueueRequest::Pull {
    system_features: Vec<String>,  // e.g., ["x86_64-linux", "kvm"]
    stream_logs: bool,             // whether to receive log streaming
}
```

#### Heartbeat

Keep job lease alive and report status.

```rust
BuildQueueRequest::Heartbeat {
    job_id: u64,
    status: String,  // e.g., "building", "fetching inputs"
}
```

#### BuildLog

Stream build output back to requester.

```rust
BuildQueueRequest::BuildLog {
    job_id: u64,
    line: String,     // log line content
    is_stderr: bool,  // stdout vs stderr
}
```

#### Complete

Report successful build completion.

```rust
BuildQueueRequest::Complete {
    job_id: u64,
    outputs: Vec<BuildOutputProto>,
    signature_r: [u8; 32],  // Ed25519 signature (r component)
    signature_s: [u8; 32],  // Ed25519 signature (s component)
}

struct BuildOutputProto {
    store_path: String,
    blake3: [u8; 32],
    sha256: [u8; 32],
    nar_size: u64,
}
```

#### Fail

Report build failure.

```rust
BuildQueueRequest::Fail {
    job_id: u64,
    error: String,
}
```

### Response Types

#### Job

Job assigned to builder.

```rust
BuildQueueResponse::Job {
    job_id: u64,
    drv_hash: [u8; 32],
    drv_path: String,
    outputs: Vec<String>,
    input_paths: Vec<InputPathProto>,
}

struct InputPathProto {
    store_path: String,
    blake3: [u8; 32],
}
```

#### NoJob

No matching job available.

```rust
BuildQueueResponse::NoJob
```

#### HeartbeatAck / CompleteAck / FailAck / LogAck

Acknowledgments for respective requests.

#### InvalidJob

Job ID not found or wrong builder.

```rust
BuildQueueResponse::InvalidJob
```

### Connection Flow

```
Builder                             Requester
   |                                    |
   | connect(/iroh-nix/build-queue/1)   |
   |----------------------------------->|
   |                                    |
   | Pull(features, stream_logs)        |
   |----------------------------------->|
   |                                    | match features to jobs
   |             Job(id, drv, inputs)   |
   |<-----------------------------------|
   |                                    |
   | [fetch inputs via NAR protocol]    |
   |                                    |
   | Heartbeat(id, "building")          |  every 10s
   |----------------------------------->|
   |                      HeartbeatAck  |
   |<-----------------------------------|
   |                                    |
   | BuildLog(id, "...", stderr)        |  if stream_logs
   |----------------------------------->|
   |                           LogAck   |
   |<-----------------------------------|
   |                                    |
   | [nix-store --realise completes]    |
   |                                    |
   | Complete(id, outputs, signature)   |
   |----------------------------------->|
   |                      CompleteAck   |
   |<-----------------------------------|
   |                                    |
   | Pull(features, stream_logs)        |  next job
   |----------------------------------->|
```

### Timeouts

- Job lease: 60 seconds without heartbeat
- Builder idle: 30 seconds without job
- Heartbeat interval: 10 seconds

## Gossip Protocol

Uses iroh-gossip for pub/sub messaging. Messages are broadcast to all peers subscribed to the same topic.

### Topic Derivation

```rust
let topic_id = blake3::hash(network_id.as_bytes());
```

All nodes with the same `--network` ID share a topic.

### Message Types

```rust
enum GossipMessage {
    Have {
        blake3: [u8; 32],
        store_path: String,
        nar_size: u64,
    },
    Want {
        blake3: [u8; 32],
    },
    IHave {
        blake3: [u8; 32],
        store_path: String,
        nar_size: u64,
    },
    NeedBuilder {
        system_features: Vec<String>,
    },
    BuildComplete {
        drv_hash: [u8; 32],
        outputs: Vec<BuildOutputInfo>,
    },
    GcWarning {
        blake3: [u8; 32],
        store_path: String,
    },
}
```

### Message Semantics

#### Have

"I have this artifact available."

Sent when:
- Adding a new store path
- Completing a build (for outputs)

#### Want

"Who has this artifact?"

Sent when:
- Fetching a hash without known provider
- Discovering sources for dependencies

#### IHave

Response to Want - "I have it."

Sent when:
- Receiving a Want for something in local index

#### NeedBuilder

"I need builders with these features."

Sent when:
- Queuing new builds
- Periodically while jobs are pending

#### BuildComplete

"I finished building this derivation."

Includes all output paths with hashes.

#### GcWarning

"I'm about to delete this artifact."

Gives peers time to fetch before deletion.

### Message Flow Examples

#### Discovery

```
Node A                   Gossip Topic                   Node B
   |                          |                            |
   | Want(hash)               |                            |
   |------------------------->|--------------------------->|
   |                          |                            | check index
   |                          |             IHave(hash)    |
   |<-------------------------|<---------------------------|
   |                          |                            |
   | [connect directly for NAR transfer]                   |
```

#### Build Coordination

```
Requester                Gossip Topic                   Builder
   |                          |                            |
   | NeedBuilder([features])  |                            |
   |------------------------->|--------------------------->|
   |                          |                            | features match?
   |                          |                            |
   | [builder connects directly to requester]              |
```

### Limits

- Max message size: 8 KB (metadata only, not blobs)
- Messages are best-effort (gossip semantics)

## HTTP Binary Cache Protocol

For Nix substituter compatibility.

### Endpoints

#### GET /nix-cache-info

```
StoreDir: /nix/store
WantMassQuery: 1
Priority: 40
```

#### GET /{hash}.narinfo

Where `{hash}` is the store path hash (first component after `/nix/store/`).

```
StorePath: /nix/store/abc123-hello
URL: nar/1234abcd5678efgh.nar
Compression: none
NarHash: sha256:base32encodedsha256hash
NarSize: 12345
```

#### GET /nar/{blake3}.nar

Returns raw NAR blob with `Content-Type: application/x-nix-nar`.

### Flow

```
nix build                              iroh-nix serve
   |                                        |
   | GET /abc123.narinfo                    |
   |--------------------------------------->|
   |                                        | lookup by store path hash
   |              200 OK (narinfo)          |
   |<---------------------------------------|
   |                                        |
   | GET /nar/blake3hash.nar                |
   |--------------------------------------->|
   |                                        | stream blob file
   |              200 OK (NAR stream)       |
   |<---------------------------------------|
```

## Protocol Versioning

All protocols include version in ALPN:

- `/iroh-nix/nar/1` - NAR v1
- `/iroh-nix/build-queue/1` - Build queue v1

Future versions would use `/iroh-nix/nar/2`, etc. Nodes can support multiple versions and negotiate during connection.

## Error Handling

### NAR Protocol

- Unknown hash: Error response with "hash not found"
- Missing blob: Error response with "blob not found"
- BLAKE3 mismatch: Client-side error after verification

### Build Queue Protocol

- Unknown job ID: `InvalidJob` response
- Wrong builder: `InvalidJob` response
- Lease expired: Job returns to pending queue

### Gossip

- Malformed messages: Silently dropped
- Unknown sender: Message processed anyway (open network)
