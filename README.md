# iroh-nix

P2P Nix binary cache using [iroh](https://iroh.computer/) for artifact distribution.

## Why

Sharing Nix build artifacts between machines is harder than it should be.
The official binary cache (cache.nixos.org) only serves what Hydra has built.
For everything else -- custom packages, private builds, CI outputs -- you
either stand up your own cache (S3, Cachix, attic) or rebuild from source on
every machine.

iroh-nix takes a different approach: machines discover each other over a gossip
network and exchange store paths directly, peer-to-peer. No central server, no
cloud storage, no SSH keys to distribute. Start a daemon on each machine, join
the same network name, and they share artifacts automatically.

## Features

- **P2P distribution** -- transfer Nix artifacts directly between machines via QUIC
- **Gossip discovery** -- nodes announce what they have and find providers automatically
- **HTTP binary cache** -- serve as a Nix substituter, compatible with the standard `nix` CLI
- **Pull-through caching** -- local store falls through to upstream HTTP caches
- **Content filtering** -- control which store paths are exposed (e.g., by signature)
- **Dual content addressing** -- BLAKE3 for fast internal lookups, SHA256 for Nix compatibility

## Quick start

### Install

With Nix:

```bash
nix run github:n0-computer/iroh-nix
```

Or build from source:

```bash
nix build
./result/bin/iroh-nix --help
```

### Usage

Start a node and share a store path:

```bash
# Start the daemon with gossip enabled
iroh-nix --network my-cluster daemon &

# Index a local store path
iroh-nix add /nix/store/abc123-hello

# On another machine, fetch it
iroh-nix --network my-cluster fetch --hash <blake3-hash>
```

Use as a Nix substituter:

```bash
# Start the HTTP binary cache
iroh-nix serve --bind 127.0.0.1:8080

# Use with nix build
nix build --substituters http://127.0.0.1:8080 ./result
```

### NixOS module

```nix
{
  imports = [ iroh-nix.nixosModules.default ];

  services.iroh-nix = {
    daemon.enable = true;
    substituter.enable = true;
    network = "my-cluster";
  };
}
```

## How it works

```
+----------------+          gossip          +----------------+
|    Node A      |<------------------------>|    Node B      |
+-------+--------+                          +--------+-------+
        |                                            |
        |  1. Have(hash, store_path)                 |
        |------------------------------------------->|
        |                                            |
        |  2. Want(hash)                             |
        |<-------------------------------------------|
        |  3. IHave(hash, store_path)                |
        |------------------------------------------->|
        |                                            |
        |  4. direct QUIC connection                 |
        |<-------------------------------------------|
        |  5. stream NAR data                        |
        |------------------------------------------->|
```

Nodes discover each other via gossip, then transfer NAR data over direct QUIC
connections. NAR data is generated on-demand from `/nix/store` paths (not stored
as blobs), keeping storage requirements low. The HTTP binary cache server bridges
iroh-nix with the standard Nix substituter protocol.

## Documentation

| Document | Description |
|----------|-------------|
| [Quick start](docs/QUICKSTART.md) | Installation and first steps |
| [Commands](docs/COMMANDS.md) | CLI reference |
| [Architecture](docs/ARCHITECTURE.md) | System design and components |
| [Protocols](docs/PROTOCOLS.md) | Wire protocols and message formats |
| [Contributing](docs/CONTRIBUTING.md) | Developer guide |

## Status

**This is a proof of concept.** The code is not ready for use or review. Protocols, APIs, and the overall architecture are subject to complete rewrite.

## License

MIT OR Apache-2.0
