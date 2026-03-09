# System Architecture

## Overview

iroh-nix is a P2P Nix binary cache built on [iroh](https://iroh.computer/) for networking. The system is structured as a Rust workspace with layered crates, keeping the core store abstractions independent of the transport layer.

## Workspace Structure

```
iroh-nix/
  Cargo.toml                # workspace root
  crates/
    nix-store/              # Core types, traits, NAR, hashing
    nix-store-http/         # HTTP binary cache client
    nix-substituter/        # HTTP binary cache server
    nix-store-iroh/         # Iroh transport layer
  iroh-nix/                 # Binary / CLI / daemon
```

## High-Level Architecture

```
+------------------------------------------------------------------+
|                         User / CLI                                |
|                     (iroh-nix/main.rs)                            |
+------------------+-------------------+----------------------------+
                   |                   |
        +----------v----------+   +----v--------------------+
        |        Node         |   |    GossipService        |
        |   (iroh-nix/node)   |   |   (nix-store-iroh)      |
        |                     |   |                         |
        | - Endpoint          |   | - Provider cache        |
        | - HashIndex         |   | - Topic subscription    |
        +----------+----------+   +----------+--------------+
                   |                         |
        +----------v-------------------------v--------------+
        |              iroh Endpoint                        |
        |         (QUIC + mDNS + relay)                     |
        +---+------------------+----------------------------+
            |                  |
     +------v------+    +------v------+
     |    NAR      |    |   Gossip    |
     |  Protocol   |    |  Protocol   |
     | (transfer)  |    | (discovery) |
     +-------------+    +-------------+
```

## Crate Responsibilities

### `nix-store` -- Foundation (no iroh dependency)

Core types, traits, and utilities shared by all other crates.

**Modules:**
- `hash_index.rs` -- SQLite-backed bidirectional index (BLAKE3 <-> SHA256 <-> store path)
- `nar.rs` -- NAR serialization with streaming dual-hash computation (SHA256 + BLAKE3)
- `store.rs` -- Core trait hierarchy and implementations
- `error.rs` -- Unified error types
- `gc.rs` -- Stale index cleanup
- `retry.rs` -- Exponential backoff with jitter
- `nix_info.rs` -- Nix path-info queries
- `nix_protocol.rs` -- Nix CommonProto binary format helpers

**Key Traits:**
```rust
/// Read-only store interface
trait NarInfoProvider: Send + Sync {
    async fn get_narinfo(&self, store_hash: &str) -> Result<Option<StorePathInfo>>;
    async fn get_nar(&self, store_path: &str) -> Result<Option<Vec<u8>>>;
}

/// Extends provider with write operations
trait NarInfoIndex: NarInfoProvider {
    async fn index_path(&self, info: &StorePathInfo) -> Result<()>;
    async fn remove_path(&self, store_hash: &str) -> Result<bool>;
}

/// Decides which store paths to expose
trait ContentFilter: Send + Sync {
    fn allow_narinfo(&self, info: &StorePathInfo) -> bool;
}
```

**Key Types:**
- `StorePathInfo` -- unified narinfo type (store path, blake3, sha256, nar_size, references, deriver, signatures)
- `LocalStore` -- wraps HashIndex + on-demand NAR generation, implements both `NarInfoProvider` and `NarInfoIndex`
- `PullThroughStore` -- checks local first, falls through to upstream providers
- `AllowAll` / `SignatureFilter` -- built-in `ContentFilter` implementations

### `nix-store-http` -- HTTP Cache Client

Implements `NarInfoProvider` by fetching from upstream HTTP binary caches (cache.nixos.org, etc.).

- Parses narinfo responses
- Handles NAR decompression (xz, zstd, bzip2)
- Computes BLAKE3 while streaming from SHA256-addressed caches

### `nix-substituter` -- HTTP Cache Server

Serves the standard Nix binary cache protocol over HTTP. Takes `Arc<dyn NarInfoProvider>` and `Arc<dyn ContentFilter>` -- no iroh dependency.

Routes:
- `GET /nix-cache-info` -- cache metadata
- `GET /<hash>.narinfo` -- package info
- `GET /nar/<blake3>.nar` -- NAR download

Can be deployed behind Cloudflare, nginx, etc.

### `nix-store-iroh` -- Iroh Transport

Iroh-specific networking layer. Implements P2P artifact transfer and gossip discovery.

- `GossipService` -- peer discovery via iroh-gossip (Have/Want/IHave/GcWarning messages)
- `handle_nar_accepted` -- serves NAR requests from peers using any `NarInfoProvider`
- `fetch_nar` / `fetch_nar_with_config` -- fetches NARs from peers
- Protocol types: `NarRequest`, `NarResponseHeader`, `GossipMessage`

### `iroh-nix` -- Binary / CLI

Composes the crates into a running daemon:

```
LocalStore -> PullThroughStore(local, [HttpStore, IrohStore])
                -> SubstituterServer (HTTP binary cache)
                -> IrohNarServer (P2P NAR serving)
```

**Modules:**
- `main.rs` -- CLI with subcommands (daemon, info, add, fetch, list, stats, query)
- `node.rs` -- daemon composition, endpoint management, protocol routing
- `cli.rs` -- output helpers, progress bars

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
         | 2. Compute BLAKE3 + SHA256 (streaming)
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
    _ => handle_nar(connection),  // default
}
```

## Security Model

### Identity

- Each node has an Ed25519 keypair
- Public key serves as endpoint ID
- Private key stored in `secret.key`

### Trust

Currently trust-on-first-use. Future work may include:

- Signature verification on fetch
- Content filtering by trusted signing keys (`SignatureFilter`)
- Allowlists for peers

## Concurrency Model

### Thread Safety

- `HashIndex`: Wrapped in `Arc<Mutex<_>>` (rusqlite is not Send)
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

No configuration file -- all settings via CLI flags:

| Flag | Purpose |
|------|---------|
| `--data-dir` | Storage location |
| `--relay-url` | NAT traversal relay |
| `--network` | Gossip network ID |
| `--peer` | Bootstrap peers |

## Future Directions

Potential improvements:

- Persistent peer connections
- Federation between networks
- Streaming NAR transfer (avoid full buffering)
- Compression support for P2P transfers
