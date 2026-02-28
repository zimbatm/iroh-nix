# iroh-nix Documentation

iroh-nix is a distributed Nix build system using [iroh](https://iroh.computer/) for P2P artifact distribution.

## What is iroh-nix?

iroh-nix enables multiple machines to collaborate on building and sharing Nix packages in a decentralized manner. Instead of relying on a central binary cache, nodes discover each other via gossip and transfer NAR blobs directly peer-to-peer.

## Key Features

- **P2P Distribution**: Transfer Nix artifacts directly between machines using QUIC with NAT traversal
- **Distributed Builds**: Queue derivations and let remote builders execute them
- **Gossip Discovery**: Nodes announce what they have and discover providers automatically
- **HTTP Binary Cache**: Serve as a Nix substituter for seamless integration with `nix` CLI
- **Content Addressing**: BLAKE3 hashes for fast lookups, SHA256 for Nix compatibility
- **Replica-Aware GC**: Check network for copies before deleting local artifacts

## Documentation

| Document | Description |
|----------|-------------|
| [QUICKSTART.md](QUICKSTART.md) | Getting started guide |
| [COMMANDS.md](COMMANDS.md) | CLI command reference |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design and components |
| [PROTOCOLS.md](PROTOCOLS.md) | Wire protocols and message formats |
| [BUILD-SYSTEM.md](BUILD-SYSTEM.md) | Distributed build system |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Developer guide |

## Quick Example

Start a node and add a store path:

```bash
# Start the daemon with gossip enabled
iroh-nix --network my-cluster daemon &

# Add a local store path
iroh-nix add /nix/store/abc123-hello

# On another machine, fetch it
iroh-nix --network my-cluster fetch --hash <blake3-hash>
```

Run distributed builds:

```bash
# On the requester (has derivations)
iroh-nix --network my-cluster build-push /nix/store/xyz.drv

# On builders (execute builds)
iroh-nix --network my-cluster builder --features x86_64-linux,kvm
```

## Architecture Overview

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
        |  5. execute build                          |
        |                                    nix-store --realise
        |  6. complete with outputs                  |
        |<-------------------------------------------|
        |  7. fetch outputs (NAR)                    |
        |------------------------------------------->|
```

## Status

iroh-nix is experimental software. The protocols and APIs may change.

## License

See the repository root for license information.
