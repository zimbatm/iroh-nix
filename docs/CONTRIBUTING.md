# Contributing Guide

## Building from Source

### Prerequisites

- Rust 1.70+ (or use Nix)
- Nix (for testing store operations)

### With Nix

```bash
nix develop  # Enter dev shell
cargo build
cargo test
```

### With Cargo

```bash
cargo build --release
cargo test
```

## Project Structure

```
iroh-nix/
  Cargo.toml          # Workspace root
  crates/
    nix-store/         # Core types, traits, NAR, hashing (no iroh dep)
      src/
        lib.rs         # Crate entry, re-exports
        error.rs       # Unified error types
        hash_index.rs  # BLAKE3/SHA256/store-path SQLite index
        nar.rs         # NAR serialization with dual hashing
        store.rs       # NarInfoProvider, NarInfoIndex, ContentFilter traits
        gc.rs          # Stale index cleanup
        retry.rs       # Exponential backoff with jitter
        nix_info.rs    # Nix path-info queries
        nix_protocol.rs # Nix binary protocol helpers
    nix-store-http/    # HTTP binary cache client
      src/
        lib.rs         # HttpCacheClient (implements NarInfoProvider)
    nix-substituter/   # HTTP binary cache server
      src/
        lib.rs         # Nix binary cache HTTP server
    nix-store-iroh/    # Iroh transport layer
      src/
        lib.rs         # Gossip, NAR transfer, protocol types
  iroh-nix/            # Binary / CLI / daemon
    src/
      lib.rs           # Crate entry
      main.rs          # CLI implementation
      node.rs          # Daemon composition
      cli.rs           # Output helpers
  docs/                # Documentation
  flake.nix            # Nix flake
  module.nix           # NixOS module
```

## Code Style

### Rust Conventions

- Use `rustfmt` defaults
- Prefer `?` over `.unwrap()` in fallible code
- Use `MutexExt::lock_or_err()` for mutex locking
- Document public APIs with rustdoc

### Error Handling

Use the `Error` enum from `nix-store/src/error.rs`:

```rust
use nix_store::{Error, Result};

fn my_function() -> Result<()> {
    // Use ? for propagation
    let data = std::fs::read("file")?;

    // Custom errors
    if data.is_empty() {
        return Err(Error::Protocol("empty data".into()));
    }

    Ok(())
}
```

For mutex locks:

```rust
use nix_store::MutexExt;

let guard = self.data.lock_or_err()?;
```

### Logging

Use `tracing` macros:

```rust
use tracing::{debug, info, warn, error};

info!("Starting operation");
debug!("Details: {:?}", value);
warn!("Something unusual: {}", msg);
error!("Failed: {}", err);
```

## Testing

### Run All Tests

```bash
cargo test --workspace
```

### Run Specific Crate Tests

```bash
cargo test -p nix-store
cargo test -p nix-store-http
cargo test -p nix-substituter
cargo test -p nix-store-iroh
cargo test -p iroh-nix
```

### Run Specific Tests

```bash
cargo test test_name
cargo test module::tests
```

### Ignored Tests

Some tests require network access:

```bash
cargo test --workspace -- --ignored
```

### Test Patterns

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_operation() {
        // Synchronous test
    }

    #[tokio::test]
    async fn test_async_operation() {
        // Async test
    }

    #[tokio::test]
    #[ignore]  // Requires network
    async fn test_network_operation() {
        // Network test
    }
}
```

## Adding a New Command

### 1. Add to CLI enum in `iroh-nix/src/main.rs`

```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing commands ...

    /// Description of new command
    NewCommand {
        /// Argument description
        #[arg(long)]
        some_arg: String,
    },
}
```

### 2. Implement handler

```rust
async fn cmd_new_command(args: &Args, some_arg: &str) -> Result<()> {
    let node = create_node(args).await?;

    // Implementation

    Ok(())
}
```

### 3. Add to match in main

```rust
match &args.command {
    // ... existing ...
    Commands::NewCommand { some_arg } => {
        cmd_new_command(&args, some_arg).await?;
    }
}
```

### 4. Add documentation

Update `docs/COMMANDS.md` with the new command.

## Adding a Protocol Message

### 1. Define in `nix-store-iroh/src/lib.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMessage {
    pub field: String,
}
```

### 2. Add to enum if needed

```rust
pub enum GossipMessage {
    // ... existing ...
    NewMessage(NewMessage),
}
```

### 3. Add handler

In the appropriate crate module:

```rust
fn handle_new_message(&self, msg: NewMessage) -> Result<()> {
    // Implementation
}
```

### 4. Add test

```rust
#[test]
fn test_new_message_roundtrip() {
    let msg = NewMessage { field: "test".into() };
    let bytes = postcard::to_allocvec(&msg).unwrap();
    let decoded: NewMessage = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.field, "test");
}
```

## Architecture Decisions

### Why BLAKE3 + SHA256?

- BLAKE3: Fast, modern, used internally
- SHA256: Nix compatibility
- Dual hashing during NAR creation

### Why Gossip for Discovery?

- Decentralized
- Works without central registry
- Natural fit for P2P networks

### Why postcard Serialization?

- Compact binary format
- Fast serialization
- serde compatible
- No schema files needed

### Why a Workspace?

- `nix-store` has no iroh dependency -- can be used standalone
- `nix-substituter` has no iroh dependency -- can deploy as plain HTTP cache
- Transport layer (`nix-store-iroh`) is pluggable
- Each crate has focused, testable responsibilities

## Debugging

### Enable Debug Logging

```bash
RUST_LOG=debug iroh-nix daemon
RUST_LOG=iroh_nix=debug iroh-nix daemon
RUST_LOG=nix_store_iroh=trace iroh-nix daemon
```

### Common Issues

**"mutex poisoned"**
- A panic occurred while holding a lock
- Check for `.unwrap()` calls in locked sections

**"connection failed"**
- Check firewall settings
- Try adding `--relay-url`
- Verify peer endpoint ID

**"hash not found"**
- Artifact not in local index
- Try `iroh-nix query <hash>` to find providers

## Pull Request Guidelines

### Before Submitting

1. Run tests: `cargo test --workspace`
2. Check formatting: `cargo fmt --check`
3. Run clippy: `cargo clippy --workspace`
4. Update documentation if needed

### PR Description

Include:
- What the change does
- Why it's needed
- How to test it
- Breaking changes (if any)

### Commit Messages

Format:
```
component: short description

Longer explanation if needed.

Fixes #123
```

Examples:
```
nix-store: add deriver field to StorePathInfo
nix-store-iroh: fix gossip message serialization for empty lists
docs: update ARCHITECTURE.md for workspace layout
```

## Resources

- [iroh documentation](https://iroh.computer/docs)
- [Nix manual](https://nixos.org/manual/nix/stable/)
- [NAR format](https://nixos.org/manual/nix/stable/protocols/nix-archive.html)
