# Quick Start Guide

This guide walks you through setting up iroh-nix and performing basic operations.

## Installation

### Using Nix

```bash
nix build github:n0-computer/iroh-nix
./result/bin/iroh-nix --help
```

### From Source

```bash
git clone https://github.com/n0-computer/iroh-nix
cd iroh-nix
cargo build --release
./target/release/iroh-nix --help
```

## Basic Setup

### Data Directory

iroh-nix stores its data in a directory (default: `.iroh-nix` in current directory):

```
.iroh-nix/
  secret.key       # Node identity (Ed25519)
  hash_index.db    # SQLite index (BLAKE3 <-> SHA256 <-> store path)
```

Note: NAR data is generated on-demand from `/nix/store` paths, not stored separately.

Specify a custom location:

```bash
iroh-nix --data-dir /var/lib/iroh-nix daemon
```

### Start the Daemon

Basic standalone mode (no network):

```bash
iroh-nix daemon
```

With gossip networking:

```bash
iroh-nix --network my-cluster daemon
```

With relay for NAT traversal:

```bash
iroh-nix --network my-cluster --relay-url https://relay.example.com daemon
```

With bootstrap peers:

```bash
iroh-nix --network my-cluster \
  --peer <endpoint-id-1> \
  --peer <endpoint-id-2> \
  daemon
```

### HTTP Cache Fallback

By default, iroh-nix uses `cache.nixos.org` as a fallback when content isn't available locally. When a peer requests a store path that doesn't exist on disk, iroh-nix fetches it from the HTTP cache and streams it to the peer.

To use additional or different caches:

```bash
iroh-nix --substituter https://my-cache.example.com daemon
```

To disable HTTP cache fallback entirely:

```bash
iroh-nix --no-substituters daemon
```

## First Steps

### Check Node Info

```bash
iroh-nix info
```

Output:

```
Endpoint ID: abc123def456...
Gossip: enabled (network: my-cluster)
Cached entries: 0
```

### Index a Store Path

Index a local Nix store path to make it available for sharing:

```bash
iroh-nix add /nix/store/abc123-hello-2.10
```

Output:

```
Indexed /nix/store/abc123-hello-2.10
  BLAKE3: 1234abcd...
  SHA256: 5678efgh...
  Size: 12345 bytes
```

The path is now discoverable via gossip and can be served on-demand to peers.

### List Indexed Paths

```bash
iroh-nix list
```

### Query Providers

Find who has a specific hash (requires gossip):

```bash
iroh-nix query <blake3-hash>
```

### Fetch from Network

Fetch a NAR by hash (discovers providers via gossip):

```bash
iroh-nix fetch --hash <blake3-hash>
```

Fetch from a specific peer:

```bash
iroh-nix fetch --hash <blake3-hash> --from <endpoint-id>
```

## HTTP Binary Cache

Run iroh-nix as a Nix substituter:

```bash
iroh-nix serve --bind 127.0.0.1:8080
```

Configure Nix to use it:

```bash
nix build --substituters http://127.0.0.1:8080 <derivation>
```

Or in `nix.conf`:

```
substituters = http://127.0.0.1:8080 https://cache.nixos.org
```

## Common Workflows

### Sharing Store Paths Between Machines

On machine A (has the store path):

```bash
iroh-nix --network shared add /nix/store/result-path
```

On machine B (wants the store path):

```bash
iroh-nix --network shared fetch --hash <blake3-from-machine-a>
```

### Running as a Systemd Service

Example service file:

```ini
[Unit]
Description=iroh-nix daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/iroh-nix --data-dir /var/lib/iroh-nix --network production daemon
Restart=always
User=iroh-nix

[Install]
WantedBy=multi-user.target
```

## Troubleshooting

### Node won't connect to peers

- Check firewall allows UDP traffic
- Try adding `--relay-url` for NAT traversal
- Verify network ID matches on all nodes

### Fetch times out

- Query providers first: `iroh-nix query <hash>`
- Try direct fetch with `--from <endpoint-id>`
- Check if the provider is reachable

## Next Steps

- [COMMANDS.md](COMMANDS.md) -- Full CLI reference
- [ARCHITECTURE.md](ARCHITECTURE.md) -- System design
