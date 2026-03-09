# Nix Smart Binary Cache Protocol

Version: 1 (draft)

## The problem

The standard Nix binary cache protocol requires two HTTP round-trips per store path: one `GET` for the `.narinfo` metadata and one `GET` for the NAR content. These requests are serial. Nix evaluates a derivation, produces a closure of store paths, then fetches narinfo for every path in that closure one at a time.

For a closure of 100 store paths where the client already has 80 locally, the current protocol ("dumb") looks like this:

| Step | Requests | Purpose |
|------|----------|---------|
| 1 | 100 serial GETs | Fetch `.narinfo` for every path in the closure |
| 2 | 20 GETs | Download NARs for the 20 missing paths |
| **Total** | **~120 round-trips** | |

The narinfo lookups dominate wall-clock time. Each one blocks on a full HTTP round-trip before the next can start. This is the N+1 query problem applied to Nix binary caches.

Git had the same bottleneck with its original "dumb" HTTP protocol and solved it with want/have negotiation and packfiles. This spec takes a similar approach for Nix caches.

## Design goals

- **Backward compatible.** Old clients ignore the new field in `/nix-cache-info`. Existing endpoints stay unchanged.
- **Transport-agnostic.** The same semantics work over HTTP (JSON), QUIC/iroh (postcard binary), and edge compute runtimes like Cloudflare Workers.
- **Server-side closure computation.** The server knows the reference graph. It can compute `closure(wants) - haves` in a single round-trip.
- **Stateless.** No streaming, no connection state. Pure request/response.

## Capability announcement

The server signals support by adding a `SmartProtocol` field to the `/nix-cache-info` response:

```
StoreDir: /nix/store
WantMassQuery: 1
Priority: 40
SmartProtocol: 1
```

Clients that don't recognize `SmartProtocol` ignore it. This is existing Nix behavior -- unknown fields in `nix-cache-info` are silently skipped. Smart-aware clients detect the field and use the new endpoints described below.

The version number (`1`) allows future protocol revisions without breaking older clients.

## Endpoints

All smart protocol endpoints live under the `/_nix/v1/` path prefix. This avoids conflicts with existing Nix cache paths (which use `/<hash>.narinfo` and `/nar/`).

### POST /_nix/v1/batch-narinfo

Look up multiple store hashes in a single request.

**When to use:** The client has a list of store path hashes and wants to know which ones the server has. Replaces N individual `GET /<hash>.narinfo` requests with one POST.

### POST /_nix/v1/closure

Compute the transitive closure of one or more store paths, minus what the client already has.

**When to use:** The client wants to install a derivation and knows which store paths it needs (wants) and which it already has locally (haves). The server walks the reference graph and returns only the delta.

## Wire format

Both endpoints accept and return the same logical types. The serialization format depends on the transport:

| Transport | Content-Type | Serialization |
|-----------|-------------|---------------|
| HTTP | `application/json` | JSON (human-debuggable, easy for Workers) |
| QUIC/iroh | n/a (binary stream) | postcard (compact, already used by iroh-nix) |

Both formats serialize the same Rust types. Implementations in other languages should treat the Rust definitions as the canonical schema.

## Request types

### SmartRequest

A tagged union with two variants:

```rust
enum SmartRequest {
    BatchNarInfo {
        store_hashes: Vec<String>,  // 32-char nix base32 prefixes
    },
    Closure {
        wants: Vec<String>,         // store hashes the client needs closures for
        haves: Vec<String>,         // store hashes the client already has
        limit: Option<u32>,         // max paths to return (pagination)
    },
}
```

**JSON representation:**

```json
{
  "BatchNarInfo": {
    "store_hashes": ["gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03", "1b9xa4mfnagfsiih33wfb0xniwbd6rv4"]
  }
}
```

```json
{
  "Closure": {
    "wants": ["gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03"],
    "haves": ["1b9xa4mfnagfsiih33wfb0xniwbd6rv4", "x74n8fmcns3pp40gak4hbklfjnhgyr0c"],
    "limit": 50
  }
}
```

The `limit` field is optional. When omitted, the server returns all matching paths. When present, the server returns at most `limit` paths and signals whether more are available via the `has_more` field in the response.

## Response types

### SmartResponse

A tagged union with three variants:

```rust
enum SmartResponse {
    BatchNarInfo {
        results: Vec<Option<StorePathInfoCompact>>,  // same order as request
    },
    PathSet {
        paths: Vec<StorePathInfoCompact>,   // paths the client needs
        has_more: bool,                      // true if pagination truncated results
    },
    Error {
        code: SmartErrorCode,
        message: String,
    },
}
```

**Endpoint-to-response mapping:**

| Endpoint | Success response | Error response |
|----------|-----------------|----------------|
| `batch-narinfo` | `BatchNarInfo` | `Error` |
| `closure` | `PathSet` | `Error` |

### StorePathInfoCompact

The wire representation of narinfo metadata. Uses basenames instead of full paths for references and deriver to reduce payload size.

```rust
struct StorePathInfoCompact {
    store_path: String,             // "/nix/store/abc123-name"
    nar_hash: String,               // "sha256:<nix32>"
    nar_size: u64,
    references: Vec<String>,        // basenames only, e.g. ["abc123-name", "def456-lib"]
    deriver: Option<String>,        // basename only, e.g. "xyz789-name.drv"
    signatures: Vec<String>,        // e.g. ["cache.nixos.org-1:base64sig..."]
    blake3: Option<String>,         // hex-encoded, present only if server knows it
}
```

Fields with `Option` type are omitted from JSON when `null` (using `skip_serializing_if`).

### SmartErrorCode

```rust
enum SmartErrorCode {
    BadRequest,      // malformed request
    NotFound,        // requested path does not exist
    Internal,        // server-side failure
    TooManyHashes,   // request exceeds server limits
}
```

## HTTP examples

### Batch narinfo request

```
POST /_nix/v1/batch-narinfo HTTP/1.1
Content-Type: application/json

{
  "BatchNarInfo": {
    "store_hashes": [
      "gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03",
      "1b9xa4mfnagfsiih33wfb0xniwbd6rv4",
      "nonexistenthash00000000000000000"
    ]
  }
}
```

```
HTTP/1.1 200 OK
Content-Type: application/json

{
  "BatchNarInfo": {
    "results": [
      {
        "store_path": "/nix/store/gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0",
        "nar_hash": "sha256:1ixr0v3r80hx3gnkp5n1mdfbsqmgzwfnm0ljn9ridca0cyd9wwc",
        "nar_size": 4718592,
        "references": [
          "gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0",
          "x74n8fmcns3pp40gak4hbklfjnhgyr0c-glibc-2.39"
        ],
        "signatures": ["cache.nixos.org-1:aBcDeFg..."],
        "blake3": "a3f2b8c901d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0"
      },
      {
        "store_path": "/nix/store/1b9xa4mfnagfsiih33wfb0xniwbd6rv4-openssl-3.3.0",
        "nar_hash": "sha256:0w3w9rqvziy8j9hqf7xixm7lmjpgxfqrfid9j5cv8w4a7p8a9kg",
        "nar_size": 2097152,
        "references": [],
        "signatures": [],
        "blake3": "b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5"
      },
      null
    ]
  }
}
```

The `results` array is positional. Each element corresponds to the request's `store_hashes` at the same index. A `null` entry means the server does not have that store path.

### Closure request

```
POST /_nix/v1/closure HTTP/1.1
Content-Type: application/json

{
  "Closure": {
    "wants": ["gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03"],
    "haves": [
      "x74n8fmcns3pp40gak4hbklfjnhgyr0c",
      "8bxkn7cqbhd0gfvxl3l0fmw53azj5rsa"
    ],
    "limit": 50
  }
}
```

```
HTTP/1.1 200 OK
Content-Type: application/json

{
  "PathSet": {
    "paths": [
      {
        "store_path": "/nix/store/gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0",
        "nar_hash": "sha256:1ixr0v3r80hx3gnkp5n1mdfbsqmgzwfnm0ljn9ridca0cyd9wwc",
        "nar_size": 4718592,
        "references": [
          "gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0",
          "x74n8fmcns3pp40gak4hbklfjnhgyr0c-glibc-2.39",
          "j9qa0z4m3bj31wrqhfyyyxhkgz09bq38-libsodium-1.0.20"
        ],
        "signatures": ["cache.nixos.org-1:aBcDeFg..."],
        "blake3": "a3f2b8c901d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0"
      },
      {
        "store_path": "/nix/store/j9qa0z4m3bj31wrqhfyyyxhkgz09bq38-libsodium-1.0.20",
        "nar_hash": "sha256:0xyzabc123def456789abcdef0123456789abcdef0123456789ab",
        "nar_size": 524288,
        "references": [],
        "signatures": ["cache.nixos.org-1:xYzAbC..."]
      }
    ],
    "has_more": false
  }
}
```

The server walked the reference graph starting from the `wants` hashes, excluded everything in `haves`, and returned the remaining paths. In this example `glibc` was in `haves` so it was excluded, but `libsodium` was not, so it appears in the response.

### Error response

```
HTTP/1.1 400 Bad Request
Content-Type: application/json

{
  "Error": {
    "code": "TooManyHashes",
    "message": "batch-narinfo limited to 500 hashes per request"
  }
}
```

## Closure algorithm

The server computes the closure delta using breadth-first search over the reference graph:

```
closure(wants, haves):
  needed = {}
  queue = wants
  while queue is not empty:
    hash = queue.pop_front()
    if hash in haves or hash in needed:
      continue
    info = lookup(hash)
    if info is None:
      continue      // unknown path, skip
    needed[hash] = info
    for ref in info.references:
      ref_hash = store_hash_of(ref)
      queue.push_back(ref_hash)
  return needed
```

When `limit` is set, the algorithm stops after collecting `limit` paths and sets `has_more = true` if the queue is non-empty. The client can issue follow-up requests, adding the paths it received so far to `haves`.

Paths that appear as references but are not known to the server are silently skipped. This handles the case where a cache has partial coverage of a closure.

## Performance comparison

For a typical build with a closure of 100 store paths, where the client already has 80:

| Protocol | Narinfo lookups | NAR downloads | Total round-trips |
|----------|----------------|---------------|-------------------|
| Dumb (current) | 100 serial GETs | 20 GETs | ~120 |
| Smart (batch-narinfo) | 1 POST (batch) | 20 GETs | ~21 |
| Smart (closure) | 1 POST (closure returns 20 paths) | 20 GETs | ~21 |

The closure endpoint has an additional advantage: the client doesn't need to know the full list of store hashes in the closure. It sends the top-level derivation outputs and the server computes the full dependency graph. This eliminates the need for the client to evaluate the closure locally before querying.

## Backward compatibility

The smart protocol is purely additive. Nothing about the existing ("dumb") binary cache protocol changes:

- `GET /nix-cache-info` still works. The `SmartProtocol` field is ignored by clients that don't recognize it.
- `GET /<hash>.narinfo` still works exactly as before.
- `GET /nar/<hash>.nar` still works exactly as before.
- The `/_nix/v1/` path prefix does not collide with any existing Nix cache path. Standard narinfo hashes are 32 characters of nix base32, and NAR paths use `/nar/`. Nothing in the current protocol starts with `/_nix/`.

A server can support both protocols simultaneously. A client can probe for `SmartProtocol: 1` in the cache-info response and fall back to individual narinfo GETs if the field is absent.

## Security considerations

### Denial of service

The closure endpoint walks the reference graph server-side. A malicious client could request the closure of a path with a very large dependency tree.

Mitigations:

- **Request size limits.** Servers SHOULD enforce a maximum on the number of hashes in `store_hashes`, `wants`, and `haves`. A reasonable default is 500 per field.
- **Closure depth limits.** Servers SHOULD cap the total number of paths traversed during BFS, independent of the `limit` parameter. This prevents pathological graphs from consuming unbounded server resources.
- **The `limit` parameter.** Clients use this for pagination. Servers can also enforce their own maximum below what the client requests.
- **The `TooManyHashes` error code.** Allows servers to reject oversized requests with a clear signal, so clients can retry with smaller batches.

### Authentication and authorization

This spec does not define authentication. Servers that require it should use standard HTTP mechanisms (Bearer tokens, mTLS) or transport-level authentication (iroh node identity). The smart endpoints carry the same trust model as the existing narinfo/NAR endpoints on the same server.

### Signature verification

The `signatures` field in `StorePathInfoCompact` carries Nix signatures. Clients SHOULD verify these the same way they verify signatures from individual `.narinfo` responses. The batch and closure endpoints do not change the trust model -- they change the transport, not the content.

## QUIC/iroh transport

Over iroh QUIC connections, the smart protocol uses postcard serialization instead of JSON. The flow is:

```
+----------------+------------------------+
| 4 bytes (LE)   | N bytes                |
| message length | postcard(SmartRequest) |
+----------------+------------------------+
```

```
+----------------+-------------------------+
| 4 bytes (LE)   | N bytes                 |
| message length | postcard(SmartResponse) |
+----------------+-------------------------+
```

This matches the existing iroh-nix NAR protocol framing (length-prefixed postcard messages). The ALPN for the smart protocol is `/iroh-nix/smart/1`.

## Upload protocol

The smart protocol includes an upload flow for pushing store paths to the cache. This is the write counterpart to the read-only batch/closure endpoints.

### Design constraints

- NARs can be hundreds of megabytes. They must not be buffered in memory or encoded inside JSON bodies.
- Narinfo metadata must not be visible to readers until its NAR content is fully uploaded (atomicity).
- Duplicate NARs (same blake3 hash) should be deduplicated.

### Upload flow (3 phases)

```
Phase 1: Init
  Client -> POST /_admin/v1/upload/init
  Body: { "paths": [StorePathInfoCompact, ...] }
  Response: { "session_id": "abc123", "need_upload": ["<blake3>", ...] }

Phase 2: Upload NARs
  For each blake3 in need_upload:
    Client -> PUT /_admin/v1/nar/<blake3>
    Body: raw NAR bytes (application/octet-stream)
    Response: 200 OK

Phase 3: Commit
  Client -> POST /_admin/v1/upload/commit
  Body: { "session_id": "abc123" }
  Response: { "committed": 5 }
```

### Phase 1: Init

The client sends all narinfo metadata in a single JSON POST. The server:

1. Generates a session ID.
2. Stages narinfos in a `pending_upload` table (not visible to readers).
3. Checks which NARs already exist in storage (deduplication via HEAD requests).
4. Returns the session ID and a list of blake3 hashes that need uploading.

If all NARs already exist (e.g., re-uploading after a failed commit), `need_upload` is empty and the client can skip to Phase 3.

### Phase 2: Upload NARs

Each NAR is uploaded as a separate PUT request with a raw binary body. The key is the blake3 hash, which is content-addressed and immutable. Uploading the same blake3 hash twice is idempotent.

The server streams the body to storage without buffering the full NAR in memory. Authentication uses the same Bearer token as the other admin endpoints.

### Phase 3: Commit

The client sends the session ID. The server:

1. Verifies all NARs exist in storage (HEAD requests for each blake3).
2. Atomically moves pending narinfos to the live table (single database transaction).
3. Deletes the pending records.

If any NAR is missing, the server returns HTTP 409 (Conflict) and the client can retry the upload.

### Atomicity guarantee

Narinfos are staged in a separate `pending_upload` table during the upload process. They are only moved to the live `narinfo` table in the commit step, which runs as a single database transaction. This means:

- Readers never see narinfo for a path whose NAR hasn't been uploaded yet.
- A failed upload session leaves no visible artifacts (pending records are cleaned up by a periodic garbage collection job).
- The commit is all-or-nothing: either all narinfos in the session become visible, or none do.

### Stale session cleanup

Pending uploads older than 1 hour are garbage-collected. The corresponding NARs in storage are orphaned but harmless (they are content-addressed and may be referenced by future uploads).

### Deduplication

Since NARs are keyed by blake3 hash, the same NAR content is stored exactly once regardless of how many store paths reference it. The init phase checks for existing NARs to avoid redundant uploads.

### Authentication

All upload endpoints require a Bearer token in the `Authorization` header. The token is compared against a server-configured secret using constant-time comparison.

```
Authorization: Bearer <admin-token>
```

## Future extensions

This section is non-normative.

- **Streaming NAR bundles.** A future version could return NARs inline with the closure response, eliminating the separate NAR download step entirely. This would reduce the 21-roundtrip smart protocol flow to a single request.
- **Delta encoding.** For paths that differ slightly (e.g., patch-level version bumps), the server could return binary diffs instead of full NARs.
- **Presigned URL uploads.** For deployments with S3-compatible storage (R2, S3, MinIO), the init phase could return presigned PUT URLs so clients upload NARs directly to storage, bypassing the server entirely. This eliminates the server as a data-plane bottleneck for large NARs.
- **Compression negotiation.** The smart response could include compressed NARs with a `compression` field, similar to the existing narinfo `Compression` field.
