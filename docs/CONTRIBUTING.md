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
  iroh-nix/
    src/
      lib.rs          # Crate entry, re-exports
      main.rs         # CLI implementation
      node.rs         # Core daemon
      hash_index.rs   # BLAKE3/SHA256 index
      nar.rs          # NAR serialization
      gossip.rs       # Peer discovery
      transfer.rs     # NAR streaming
      substituter.rs  # HTTP binary cache
      build.rs        # Build queue
      builder.rs      # Builder worker
      gc.rs           # Garbage collection
      retry.rs        # Retry logic
      protocol.rs     # Wire formats
      error.rs        # Error types
    Cargo.toml
  docs/               # Documentation
  flake.nix           # Nix flake
```

## Code Style

### Rust Conventions

- Use `rustfmt` defaults
- Prefer `?` over `.unwrap()` in fallible code
- Use `MutexExt::lock_or_err()` for mutex locking
- Document public APIs with rustdoc

### Error Handling

Use the `Error` enum from `error.rs`:

```rust
use crate::{Error, Result};

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
use crate::error::MutexExt;

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
cargo test
```

### Run Specific Tests

```bash
cargo test test_name
cargo test module::tests
```

### Ignored Tests

Some tests require network access:

```bash
cargo test -- --ignored
```

### Test Patterns

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

### 1. Add to CLI enum in `main.rs`

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

### 1. Define in `protocol.rs`

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

In the appropriate module (gossip.rs, builder.rs, etc.):

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

## Error Handling Patterns

### Adding a New Error Variant

In `error.rs`:

```rust
#[derive(Error, Debug)]
pub enum Error {
    // ... existing ...

    /// Description of error
    #[error("new error: {0}")]
    NewError(String),
}
```

### Retryable vs Non-Retryable

In `retry.rs`, update `is_retryable()`:

```rust
pub fn is_retryable(error: &Error) -> bool {
    match error {
        // Transient errors - retry
        Error::Connection(_) => true,
        Error::Timeout(_) => true,

        // Permanent errors - don't retry
        Error::NewError(_) => false,  // Add your error

        // ...
    }
}
```

## Pull Request Guidelines

### Before Submitting

1. Run tests: `cargo test`
2. Check formatting: `cargo fmt --check`
3. Run clippy: `cargo clippy`
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
build: add feature filtering to job queue
gossip: fix message serialization for empty lists
docs: add BUILD-SYSTEM.md
```

## Architecture Decisions

### Why Pull-Based Builds?

- No central coordinator needed
- Natural load balancing
- Builders only take what they can handle
- Works with heterogeneous pools

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

## Debugging

### Enable Debug Logging

```bash
RUST_LOG=debug iroh-nix daemon
RUST_LOG=iroh_nix=debug iroh-nix daemon
RUST_LOG=iroh_nix::gossip=trace iroh-nix daemon
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

## Resources

- [iroh documentation](https://iroh.computer/docs)
- [Nix manual](https://nixos.org/manual/nix/stable/)
- [NAR format](https://nixos.org/manual/nix/stable/protocols/nix-archive.html)
