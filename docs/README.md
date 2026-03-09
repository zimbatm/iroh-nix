# iroh-nix Documentation

iroh-nix is a P2P Nix binary cache using [iroh](https://iroh.computer/) for artifact distribution.

## What is iroh-nix?

iroh-nix enables multiple machines to share Nix store paths in a decentralized manner. Instead of relying on a central binary cache, nodes discover each other via gossip and transfer NAR data directly peer-to-peer.

## Key Features

- **P2P Distribution**: Transfer Nix artifacts directly between machines using QUIC with NAT traversal
- **Gossip Discovery**: Nodes announce what they have and discover providers automatically
- **HTTP Binary Cache**: Serve as a Nix substituter for seamless integration with `nix` CLI
- **Pull-Through Caching**: Local store falls through to upstream HTTP caches
- **Content Filtering**: Control which store paths are exposed (e.g., by trusted signing keys)
- **Content Addressing**: BLAKE3 hashes for fast lookups, SHA256 for Nix compatibility

## Documentation

| Document | Description |
|----------|-------------|
| [QUICKSTART.md](QUICKSTART.md) | Getting started guide |
| [COMMANDS.md](COMMANDS.md) | CLI command reference |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System design and components |
| [PROTOCOLS.md](PROTOCOLS.md) | Wire protocols and message formats |
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

Use as a Nix substituter:

```bash
# Start the HTTP binary cache
iroh-nix serve --bind 127.0.0.1:8080

# Use with nix
nix build --substituters http://127.0.0.1:8080 ./result
```

## Architecture Overview

```
+----------------+          gossip          +----------------+
|    Node A      |<------------------------>|    Node B      |
+-------+--------+                          +--------+-------+
        |                                            |
        |  Have / Want / IHave                       |
        |<------------------------------------------>|
        |                                            |
        |  direct NAR transfer (QUIC)                |
        |<------------------------------------------>|
        |                                            |
   +----v-----+                                 +----v-----+
   |  HTTP     |                                |  HTTP     |
   |  Binary   |                                |  Binary   |
   |  Cache    |                                |  Cache    |
   +----------+                                 +----------+
```

## Status

iroh-nix is experimental software. The protocols and APIs may change.

## License

See the repository root for license information.
