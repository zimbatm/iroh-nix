# CLI Command Reference

## Global Options

These options apply to all commands:

| Option | Description |
|--------|-------------|
| `--data-dir <PATH>` | Data directory (default: `.iroh-nix`) |
| `--relay-url <URL>` | Relay server URL for NAT traversal |
| `--network <ID>` | Network ID for gossip (enables gossip) |
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
- Participates in gossip (if `--network` enabled)
- Runs the HTTP binary cache server (substituter)
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
- Number of indexed entries

---

### add

Index a local store path.

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
3. Updates the hash index
4. Announces via gossip (if enabled)

---

### fetch

Fetch a NAR from the network.

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

List all indexed store paths.

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
- `GET /nar/<blake3>.nar` - NAR download

**Example:**
```bash
iroh-nix serve --bind 0.0.0.0:8080 --priority 30
```

Use with Nix:
```bash
nix build --substituters http://localhost:8080 ./result
```

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
