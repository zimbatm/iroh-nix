//! Error types for nix-store

use std::sync::{Mutex, MutexGuard};
use thiserror::Error;

/// Result type alias using our Error type
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in nix-store operations
#[derive(Error, Debug)]
pub enum Error {
    /// Database error
    #[cfg(feature = "sqlite")]
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// IO error
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Hash not found in index
    #[error("hash not found: {0}")]
    HashNotFound(String),

    /// Store path not found
    #[error("store path not found: {0}")]
    StorePathNotFound(String),

    /// Invalid store path format
    #[error("invalid store path: {0}")]
    InvalidStorePath(String),

    /// NAR serialization error
    #[error("NAR error: {0}")]
    Nar(String),

    /// Protocol error (message encoding/decoding, unexpected responses)
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Signing error
    #[error("signing error: {0}")]
    Signing(String),

    /// Timeout error
    #[error("operation timed out: {0}")]
    Timeout(String),

    /// Authentication error
    #[error("authentication error: {0}")]
    Auth(String),

    /// Internal error (lock poisoning, unexpected state)
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Get a suggestion for how to fix this error, if available
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Error::HashNotFound(_) => Some(
                "Use 'iroh-nix query <hash>' to find providers, or 'iroh-nix add' to cache locally",
            ),
            Error::StorePathNotFound(_) => {
                Some("Run 'nix-store --realise <path>' to ensure the path exists locally")
            }
            Error::InvalidStorePath(_) => Some("Store paths should match /nix/store/<hash>-<name>"),
            Error::Timeout(_) => Some("Try increasing timeout or check network connectivity"),
            #[cfg(feature = "sqlite")]
            Error::Database(_) => Some("Try removing the data directory and restarting the daemon"),
            _ => None,
        }
    }

    /// Check if this error is likely transient and worth retrying
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Timeout(_))
    }

    /// Format the error with suggestion for CLI display
    pub fn display_with_suggestion(&self) -> String {
        let mut msg = self.to_string();
        if let Some(suggestion) = self.suggestion() {
            msg.push_str("\n    Hint: ");
            msg.push_str(suggestion);
        }
        msg
    }
}

/// Extension trait for Mutex to convert lock errors to our Error type
pub trait MutexExt<T> {
    /// Lock the mutex, converting poisoned errors to Error::Internal
    fn lock_or_err(&self) -> Result<MutexGuard<'_, T>>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_or_err(&self) -> Result<MutexGuard<'_, T>> {
        self.lock()
            .map_err(|_| Error::Internal("mutex poisoned".into()))
    }
}
