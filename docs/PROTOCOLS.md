# Wire Protocols

iroh-nix uses two ALPN-negotiated protocols over QUIC connections.

## Protocol Identifiers

| ALPN | Purpose |
|------|---------|
| `/iroh-nix/nar/1` | NAR blob transfer |
| `/iroh-gossip/0` | Peer discovery (iroh-gossip) |

## Serialization

All structured messages use [postcard](https://crates.io/crates/postcard) -- a compact binary format based on serde. Length prefixes are 4-byte little-endian for the NAR protocol.

## NAR Transfer Protocol

Used for transferring NAR data between nodes.

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
   |                                   | generate NAR on-demand
   |  0x01 + header + NAR stream       |
   |<----------------------------------|
   |                                   |
   | verify BLAKE3                     |
   | import to nix store               |
```

### Limits

- Max request size: 1 KB
- Max NAR size: 10 GB
- Transfer buffer: 64 KB chunks

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

#### Want

"Who has this artifact?"

Sent when:
- Fetching a hash without known provider
- Discovering sources for dependencies

#### IHave

Response to Want -- "I have it."

Sent when:
- Receiving a Want for something in local index

#### GcWarning

"I'm about to delete this artifact."

Gives peers time to fetch before deletion.

### Message Flow Example

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

Returns raw NAR data with `Content-Type: application/x-nix-nar`.

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
   |                                        | generate NAR on-demand
   |              200 OK (NAR stream)       |
   |<---------------------------------------|
```

## Protocol Versioning

Protocols include version in ALPN:

- `/iroh-nix/nar/1` -- NAR v1

Future versions would use `/iroh-nix/nar/2`, etc. Nodes can support multiple versions and negotiate during connection.

## Error Handling

### NAR Protocol

- Unknown hash: Error response with "hash not found"
- BLAKE3 mismatch: Client-side error after verification

### Gossip

- Malformed messages: Silently dropped
- Unknown sender: Message processed anyway (open network)
