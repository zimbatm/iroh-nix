# CLI Command Reference

## Global Options

These options apply to all commands:

| Option | Description |
|--------|-------------|
| `--data-dir <PATH>` | Data directory (default: `.iroh-nix`) |
| `--relay-url <URL>` | Relay server URL for NAT traversal |
| `--network <ID>` | Network ID for gossip (enables gossip + build queue) |
| `--peer <ID>` | Bootstrap peer endpoint ID (repeatable) |
| `--substituter <URL>` | HTTP binary cache URL (repeatable, default: `https://cache.nixos.org`) |
| `--no-substituters` | Disable all HTTP binary cache substituters |

## Commands

### daemon

Start the iroh-nix node and listen for connections.

```bash
iroh-nix daemon
```

The daemon:
- Accepts incoming NAR transfer requests
- Handles build queue connections (if `--network` enabled)
- Participates in gossip (if `--network` enabled)
- Routes connections by ALPN protocol

Runs until interrupted (Ctrl+C).

---

### info

Display node information.

```bash
iroh-nix info
```

Output includes:
- Endpoint ID (public key)
- Gossip status and network ID
- Number of cached entries
- Total blob storage size

---

### add

Add a local store path to the blob store.

```bash
iroh-nix add <STORE_PATH>
```

**Arguments:**
- `<STORE_PATH>` - Path in `/nix/store/...`

**Example:**
```bash
iroh-nix add /nix/store/abc123-hello-2.10
```

This:
1. Serializes the path to NAR format
2. Computes BLAKE3 and SHA256 hashes
3. Stores the NAR blob in `blobs/`
4. Updates the hash index
5. Announces via gossip (if enabled)

---

### fetch

Fetch a NAR blob from the network.

```bash
iroh-nix fetch --hash <BLAKE3_HASH> [--from <ENDPOINT_ID>]
```

**Options:**
- `--hash <HASH>` - BLAKE3 hash to fetch (required)
- `--from <ID>` - Specific endpoint to fetch from (optional)

**Examples:**

Fetch via gossip discovery:
```bash
iroh-nix fetch --hash 1234abcd5678efgh...
```

Fetch from specific peer:
```bash
iroh-nix fetch --hash 1234abcd... --from abc123def456...
```

---

### list

List all cached store paths.

```bash
iroh-nix list
```

Output format:
```
/nix/store/abc123-hello  blake3:1234abcd...  size:12345
/nix/store/def456-world  blake3:5678efgh...  size:67890
```

---

### stats

Show node statistics.

```bash
iroh-nix stats
```

Output:
```
Endpoint ID: abc123...
Entries: 42
Total size: 1.23 GB
```

---

### query

Query the gossip network for providers of a hash.

```bash
iroh-nix query <BLAKE3_HASH>
```

**Arguments:**
- `<BLAKE3_HASH>` - Hash to query for

Waits for responses and displays providers:
```
Providers for 1234abcd...:
  - abc123... (announced 5s ago)
  - def456... (announced 12s ago)
```

Requires `--network` to be set.

---

### build-push

Queue a derivation for distributed building.

```bash
iroh-nix build-push <DRV_PATH>
```

**Arguments:**
- `<DRV_PATH>` - Path to `.drv` file

**What it does:**
1. Parses derivation with `nix derivation show --recursive`
2. Identifies all dependencies
3. Topologically sorts build order
4. Queues each derivation
5. Announces NeedBuilder via gossip
6. Waits for builders to complete
7. Fetches and imports outputs

**Example:**
```bash
iroh-nix --network cluster build-push /nix/store/xyz.drv
```

Requires `--network` to be set.

---

### build-queue

Show the current build queue status.

```bash
iroh-nix build-queue
```

Output:
```
Build Queue Status:
  Pending: 3 jobs
  Leased: 1 job (builder: abc123...)
  Completed: 5 jobs
```

---

### builder

Run as a builder worker.

```bash
iroh-nix builder [--features <FEATURES>]
```

**Options:**
- `--features <FEATURES>` - Comma-separated system features (e.g., `x86_64-linux,kvm`)

**Example:**
```bash
iroh-nix --network cluster builder --features x86_64-linux,kvm,big-parallel
```

The builder:
1. Listens for NeedBuilder gossip messages
2. Connects to requesters with matching features
3. Pulls jobs from their queues
4. Fetches input dependencies
5. Executes `nix-store --realise`
6. Streams logs back to requester
7. Signs and reports results

Runs until interrupted.

---

### serve

Run the HTTP binary cache server.

```bash
iroh-nix serve [--bind <ADDR>] [--priority <N>]
```

**Options:**
- `--bind <ADDR>` - Bind address (default: `127.0.0.1:8080`)
- `--priority <N>` - Cache priority (default: `40`, lower = higher priority)

**Endpoints:**
- `GET /nix-cache-info` - Cache metadata
- `GET /<hash>.narinfo` - Package info
- `GET /nar/<blake3>.nar` - NAR blob download

**Example:**
```bash
iroh-nix serve --bind 0.0.0.0:8080 --priority 30
```

Use with Nix:
```bash
nix build --substituters http://localhost:8080 ./result
```

---

### build-logs

Watch build logs from the queue.

```bash
iroh-nix build-logs [--job <JOB_ID>]
```

**Options:**
- `--job <ID>` - Watch specific job (default: all jobs)

Streams stdout/stderr from active builds in real-time.

---

### gc

Run garbage collection.

```bash
iroh-nix gc [OPTIONS]
```

**Options:**
- `--min-replicas <N>` - Minimum replicas before deleting (default: `1`)
- `--grace-period <SECS>` - Wait time after GC warning (default: `30`)
- `--max-delete <N>` - Max deletions per run (default: `100`)
- `--dry-run` - Preview without deleting

**Example:**
```bash
# Dry run first
iroh-nix gc --min-replicas 2 --dry-run

# Actually delete
iroh-nix gc --min-replicas 2
```

GC workflow:
1. List all local artifacts
2. Query gossip for replica counts
3. Mark candidates (replicas >= min_replicas)
4. Announce GC warning
5. Wait grace period
6. Delete blob and index entry

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Invalid arguments |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Logging level (e.g., `info`, `debug`, `iroh_nix=debug`) |

**Example:**
```bash
RUST_LOG=debug iroh-nix daemon
```
