# System Architecture

## Overview

iroh-nix is a distributed Nix build system built on [iroh](https://iroh.computer/) for P2P networking. The system enables decentralized artifact distribution and coordinated remote builds.

## High-Level Architecture

```
+------------------------------------------------------------------+
|                         User / CLI                                |
|                        (main.rs)                                  |
+------------------+-------------------+----------------------------+
                   |                   |
        +----------v----------+   +----v--------------------+
        |        Node         |   |    GossipService        |
        |      (node.rs)      |   |     (gossip.rs)         |
        |                     |   |                         |
        | - Endpoint          |   | - Provider cache        |
        | - HashIndex         |   | - Requester cache       |
        | - BuildQueue        |   | - Topic subscription    |
        +----------+----------+   +----------+--------------+
                   |                         |
        +----------v-------------------------v--------------+
        |              iroh Endpoint                        |
        |         (QUIC + mDNS + relay)                     |
        +---+------------------+------------------+---------+
            |                  |                  |
     +------v------+    +------v------+    +------v---------+
     |    NAR      |    |   Gossip    |    | Build Queue    |
     |  Protocol   |    |  Protocol   |    |   Protocol     |
     | (transfer)  |    | (discovery) |    |  (builder)     |
     +-------------+    +-------------+    +----------------+
```

## Module Responsibilities

### Core Modules

#### `node.rs` - Node Daemon

The central component managing all node-level operations:

- **Endpoint Management**: Creates and manages the iroh QUIC endpoint with mDNS discovery
- **Hash Index**: Maintains SQLite database for hash lookups
- **On-Demand NAR**: Generates NAR data from `/nix/store` paths when requested
- **Protocol Routing**: Routes incoming connections by ALPN identifier
- **Build Queue**: Optional queue for distributed builds (when gossip enabled)

Key struct:
```rust
struct Node {
    endpoint: Endpoint,
    secret_key: SecretKey,
    hash_index: Arc<Mutex<HashIndex>>,
    gossip: Option<Arc<GossipService>>,
    build_queue: Option<Arc<BuildQueue>>,
}
```

Note: NAR data is generated on-demand from `/nix/store` paths, not stored in blob files.

#### `hash_index.rs` - Hash Translation

SQLite-backed bidirectional index:

```
BLAKE3 <--> SHA256 <--> Store Path
```

- Primary key: BLAKE3 hash (32 bytes)
- Unique constraints on SHA256 and store path
- Nix base32 encoding for SHA256 display
- Thread-safe via Mutex (rusqlite is not Send)

#### `nar.rs` - NAR Serialization

Implements Nix ARchive format with streaming hash computation:

- Recursive path serialization (files, directories, symlinks)
- Dual hashing: computes both BLAKE3 and SHA256 while writing
- Deterministic output (sorted directory entries)
- Preserves executable bit

#### `error.rs` - Error Types

Unified error handling with categorized variants:

- `Database`, `Io`, `Iroh` - Infrastructure errors
- `HashNotFound`, `StorePathNotFound` - Lookup failures
- `Protocol`, `Connection`, `Timeout` - Network errors
- `Build`, `Signing` - Build system errors
- `Internal` - Lock poisoning, unexpected state

### Networking Modules

#### `gossip.rs` - Peer Discovery

Gossip-based metadata dissemination via iroh-gossip:

- **Provider Cache**: Maps BLAKE3 hash -> list of providers
- **Requester Cache**: Maps endpoint ID -> builders needed
- **Message Types**: Have, Want, IHave, NeedBuilder, BuildComplete, GcWarning

No blob data through gossip - only metadata and coordination.

#### `transfer.rs` - NAR Streaming

P2P blob transfer protocol:

- Request: BLAKE3 hash (length-prefixed postcard)
- Response: Header + NAR stream
- Streaming prevents memory buffering
- BLAKE3 verification on receive
- Max NAR size: 10 GB

#### `substituter.rs` - HTTP Binary Cache Server

Nix-compatible HTTP server:

- `/nix-cache-info` - Cache metadata
- `/<hash>.narinfo` - Package info (SHA256 lookup)
- `/nar/<blake3>.nar` - NAR blob download

Bridges BLAKE3 internal addressing with Nix's SHA256 expectations.

#### `http_cache.rs` - HTTP Binary Cache Client

Client for fetching from HTTP binary caches (e.g., cache.nixos.org):

- **On-demand proxy**: Fetches content when not available locally
- **Compression support**: Handles xz, zstd, bzip2 compressed NARs
- **Hash translation**: Computes BLAKE3 while streaming from SHA256-addressed caches
- **Automatic indexing**: Adds fetched content to local hash index

Default substituter: `https://cache.nixos.org`

### Build System Modules

#### `build.rs` - Build Queue

Pull-based job queue on the requester side:

```rust
struct BuildQueue {
    pending: Mutex<VecDeque<BuildJob>>,
    leased: Mutex<HashMap<JobId, LeasedJob>>,
    completed: Mutex<HashMap<DrvHash, JobOutcome>>,
    // ...
}
```

Job lifecycle: pending -> leased -> completed/failed

#### `builder.rs` - Builder Worker

Builder-side implementation:

- Listens for NeedBuilder gossip
- Connects to requesters with matching features
- Pulls and executes jobs
- Signs results with Ed25519

#### `gc.rs` - Garbage Collection

Replica-aware cleanup:

1. List all local artifacts
2. Query gossip for replica counts
3. Announce GC warning
4. Wait grace period
5. Delete if still safe

### Utility Modules

#### `retry.rs` - Retry Logic

Exponential backoff with jitter:

- Classifies errors as retryable vs permanent
- Configurable max retries, delays, backoff
- Provider fallback support

#### `protocol.rs` - Wire Formats

Message definitions for all protocols:

- NAR request/response headers
- Build queue messages (Pull, Heartbeat, Complete, etc.)
- Gossip message enum
- Postcard serialization

## Data Flow

### Adding a Store Path

```
User: add /nix/store/abc123-hello
         |
         v
    +----+----+
    |  Node   |
    +----+----+
         |
         | 1. Serialize to NAR
         v
    +----+----+
    |   NAR   |
    | Writer  |
    +----+----+
         |
         | 2. Compute BLAKE3 + SHA256 (streaming to sink)
         v
    +----+----+
    | Hashing |
    | Writer  |
    +----+----+
         |
         | 3. Update index (no blob file stored)
         v
    +----+----+
    |  Hash   |
    |  Index  | --> hash_index.db
    +----+----+
         |
         | 4. Announce (if gossip)
         v
    +----+----+
    | Gossip  | --> GossipMessage::Have
    +---------+

NAR data is generated on-demand when peers request it.
```

### Fetching a NAR

```
User: fetch --hash <H>
         |
         v
    +----+----+           +----------+
    | Gossip  |  Want(H)  |  Peers   |
    | Query   |---------->|          |
    +----+----+           +----+-----+
         |                     |
         |<--------------------+
         | IHave(H, provider)
         v
    +----+----+
    | Connect |
    | to peer |
    +----+----+
         |
         | NarRequest(H)
         v
    +----+----+           +----------+
    |  NAR    |  stream   |  Remote  |
    | Receive |<----------|   Node   |
    +----+----+           +----------+
         |                      |
         | (Remote generates    |
         |  NAR on-demand from  |
         |  /nix/store path)    |
         |
         | Verify BLAKE3
         v
    +----+----+
    | Import  |
    | to Nix  | --> nix-store --restore
    +----+----+
         |
         v
    +----+----+
    | Update  |
    | Index   |
    +---------+
```

## Networking Layer

### Protocol Stack

```
+------------------------------------------+
|           Application Protocols          |
|  /iroh-nix/nar/1    (NAR transfer)      |
|  /iroh-nix/build-queue/1 (build jobs)   |
|  /iroh-gossip/0     (peer discovery)    |
+------------------------------------------+
|              iroh Endpoint               |
|  - QUIC transport                        |
|  - Connection multiplexing               |
|  - ALPN protocol negotiation             |
+------------------------------------------+
|              Discovery                   |
|  - mDNS (local network)                  |
|  - Relay server (NAT traversal)          |
+------------------------------------------+
|                 UDP                      |
+------------------------------------------+
```

### Connection Routing

Incoming connections are routed by ALPN:

```rust
match connection.alpn() {
    GOSSIP_ALPN => handle_gossip(connection),
    BUILD_QUEUE_PROTOCOL_ALPN => handle_build_queue(connection),
    _ => handle_nar(connection),  // default
}
```

## Security Model

### Identity

- Each node has an Ed25519 keypair
- Public key serves as endpoint ID
- Private key stored in `secret.key`

### Build Signatures

- Builders sign `BuildResult` with their secret key
- Signature includes: job ID, drv hash, outputs
- Requesters can verify builder authenticity

### Trust

Currently trust-on-first-use. Future work may include:

- Allowlists for builders
- Signature verification on fetch
- Reputation systems

## Concurrency Model

### Thread Safety

- `HashIndex`: Wrapped in `Arc<Mutex<_>>` (rusqlite is not Send)
- `BuildQueue`: Internal Mutex per field
- Async tasks spawned for each connection

### Lock Handling

Uses `MutexExt` trait for safe lock acquisition:

```rust
let index = self.hash_index.lock_or_err()?;
```

Converts `PoisonError` to `Error::Internal`.

## File Layout

```
.iroh-nix/
  secret.key           # Ed25519 private key (32 bytes)
  hash_index.db        # SQLite database (BLAKE3 <-> SHA256 <-> store path)
```

NAR data is generated on-demand from `/nix/store` paths when requested by peers.

## Configuration

No configuration file - all settings via CLI flags:

| Flag | Purpose |
|------|---------|
| `--data-dir` | Storage location |
| `--relay-url` | NAT traversal relay |
| `--network` | Gossip network ID |
| `--peer` | Bootstrap peers |

## Future Directions

Potential improvements:

- Persistent peer connections
- Multi-output derivation handling
- Build caching strategies
- Federation between networks
