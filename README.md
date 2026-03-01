# iroh-nix

Distributed Nix build system using [iroh](https://iroh.computer/) for P2P artifact distribution.

## Why

Sharing Nix build artifacts between machines is harder than it should be.
The official binary cache (cache.nixos.org) only serves what Hydra has built.
For everything else -- custom packages, private builds, CI outputs -- you
either stand up your own cache (S3, Cachix, attic) or rebuild from source on
every machine. Distributed builds exist but require SSH access, manual
`builders` configuration, and a central coordinator.

iroh-nix takes a different approach: machines discover each other over a gossip
network and exchange store paths directly, peer-to-peer. No central server, no
cloud storage, no SSH keys to distribute. Start a daemon on each machine, join
the same network name, and they share artifacts automatically. Builders pull
work from requesters instead of having jobs pushed to them, so the system
scales naturally without coordination.

## Features

- **P2P distribution** -- transfer Nix artifacts directly between machines
- **Distributed builds** -- queue derivations and let remote builders execute them (pull-based)
- **Gossip discovery** -- nodes announce what they have and find providers automatically
- **HTTP binary cache** -- serve as a Nix substituter, compatible with the standard `nix` CLI
- **Dual content addressing** -- BLAKE3 for fast internal lookups, SHA256 for Nix compatibility
- **Replica-aware GC** -- check the network for copies before deleting local artifacts

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

Run distributed builds:

```bash
# On the requester (has derivations to build)
iroh-nix --network my-cluster build-push /nix/store/xyz.drv

# On builders (execute builds)
iroh-nix --network my-cluster builder --features x86_64-linux,kvm
```

### NixOS module

```nix
{
  imports = [ iroh-nix.nixosModules.default ];

  services.iroh-nix = {
    daemon.enable = true;
    builder.enable = true;
    substituter.enable = true;
    network = "my-cluster";
  };
}
```

## How it works

```
+----------------+          gossip          +----------------+
|    Node A      |<------------------------>|    Node B      |
|  (requester)   |                          |   (builder)    |
+-------+--------+                          +--------+-------+
        |                                            |
        |  1. announce NeedBuilder                   |
        |<-------------------------------------------|
        |  2. builder connects                       |
        |<-------------------------------------------|
        |  3. pull job                               |
        |------------------------------------------->|
        |  4. fetch inputs (NAR)                     |
        |<-------------------------------------------|
        |  5. nix-store --realise                    |
        |                                            |
        |  6. complete with outputs                  |
        |<-------------------------------------------|
        |  7. fetch outputs (NAR)                    |
        |------------------------------------------->|
```

Builders pull work from requesters, fetch input NARs over direct QUIC
connections, execute `nix-store --realise`, and stream the outputs back.
NAR data is generated on-demand (not stored as blobs), keeping storage
requirements low.

## Documentation

| Document | Description |
|----------|-------------|
| [Quick start](docs/QUICKSTART.md) | Installation and first steps |
| [Commands](docs/COMMANDS.md) | CLI reference |
| [Architecture](docs/ARCHITECTURE.md) | System design and components |
| [Protocols](docs/PROTOCOLS.md) | Wire protocols and message formats |
| [Build system](docs/BUILD-SYSTEM.md) | Distributed build internals |
| [Contributing](docs/CONTRIBUTING.md) | Developer guide |

## Status

**This is a proof of concept.** The code is not ready for use or review. Protocols, APIs, and the overall architecture are subject to complete rewrite.

## License

MIT OR Apache-2.0
