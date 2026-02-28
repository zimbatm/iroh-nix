//! Retry utilities for network operations
//!
//! Provides configurable retry logic with exponential backoff for
//! transient failures in network operations.

use std::future::Future;
use std::time::Duration;

use tracing::{debug, warn};

use crate::{Error, Result};

/// Statistics about retry execution
#[derive(Debug, Clone, Default)]
pub struct RetryStats {
    /// Total number of attempts made
    pub attempts: u32,
    /// Total time spent waiting between retries
    pub total_delay: Duration,
    /// Whether the operation ultimately succeeded
    pub succeeded: bool,
}

impl RetryStats {
    /// Format stats for display
    pub fn display(&self) -> String {
        if self.attempts <= 1 {
            String::new()
        } else {
            format!(
                "{} attempts, {:.1}s total delay",
                self.attempts,
                self.total_delay.as_secs_f64()
            )
        }
    }
}

/// Configuration for retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries, just one attempt)
    pub max_retries: u32,
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Multiplier for exponential backoff (e.g., 2.0 doubles delay each retry)
    pub backoff_multiplier: f64,
    /// Add random jitter to delays (0.0 to 1.0, as fraction of delay)
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            jitter: 0.1,
        }
    }
}

impl RetryConfig {
    /// Create a config for no retries (single attempt)
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Default::default()
        }
    }

    /// Create a config for aggressive retries (useful for fetches)
    pub fn aggressive() -> Self {
        Self {
            max_retries: 5,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 1.5,
            jitter: 0.2,
        }
    }

    /// Create a config for patient retries (useful for peer connections)
    pub fn patient() -> Self {
        Self {
            max_retries: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_multiplier: 2.0,
            jitter: 0.25,
        }
    }

    /// Calculate delay for a given attempt number (0-indexed)
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }

        let base_delay =
            self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi((attempt - 1) as i32);
        let capped_delay = base_delay.min(self.max_delay.as_secs_f64());

        // Add jitter
        let jitter_range = capped_delay * self.jitter;
        let jitter = if jitter_range > 0.0 {
            (rand::random::<f64>() - 0.5) * 2.0 * jitter_range
        } else {
            0.0
        };

        let final_delay = (capped_delay + jitter).max(0.0);
        Duration::from_secs_f64(final_delay)
    }
}

/// Determines if an error is retryable
pub fn is_retryable(err: &Error) -> bool {
    match err {
        // Connection errors are usually transient
        Error::Connection(_) => true,
        // Timeouts should be retried
        Error::Timeout(_) => true,
        // Protocol errors might be transient (stream closed, etc.)
        Error::Protocol(msg) => {
            msg.contains("stream")
                || msg.contains("connection")
                || msg.contains("timeout")
                || msg.contains("reset")
        }
        // Remote errors depend on the message
        Error::Remote(msg) => {
            // "not found" is permanent, others might be transient
            !msg.contains("not found")
        }
        // IO errors can be transient
        Error::Io(e) => {
            use std::io::ErrorKind;
            matches!(
                e.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::TimedOut
                    | ErrorKind::Interrupted
                    | ErrorKind::WouldBlock
            )
        }
        // These are generally not retryable
        Error::HashNotFound(_) => false,
        Error::StorePathNotFound(_) => false,
        Error::InvalidStorePath(_) => false,
        Error::Database(_) => false,
        Error::Nar(_) => false,
        Error::Signing(_) => false,
        Error::Iroh(_) => true, // Iroh errors might be transient
        Error::Gossip(_) => true,
        Error::Build(_) => false,
        Error::JobNotFound(_) => false,
        Error::InvalidBuilder(_) => false,
        Error::Internal(_) => false, // Internal errors are not retryable
        Error::HttpCache(msg) => {
            // Most HTTP cache errors are transient network issues
            // "not found" errors from caches are permanent
            !msg.contains("not found") && !msg.contains("mismatch")
        }
    }
}

/// Callback for retry progress updates
pub type RetryCallback = Box<dyn Fn(u32, u32, Duration) + Send + Sync>;

/// Execute an async operation with retries
pub async fn with_retry<F, Fut, T>(
    config: &RetryConfig,
    operation_name: &str,
    operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let (result, _stats) = with_retry_stats(config, operation_name, operation, None).await;
    result
}

/// Execute an async operation with retries and return statistics
///
/// The optional callback is called before each retry with (attempt, max_retries, delay).
pub async fn with_retry_stats<F, Fut, T>(
    config: &RetryConfig,
    operation_name: &str,
    mut operation: F,
    on_retry: Option<RetryCallback>,
) -> (Result<T>, RetryStats)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;
    let mut stats = RetryStats::default();

    for attempt in 0..=config.max_retries {
        stats.attempts = attempt + 1;

        // Wait before retry (no wait on first attempt)
        let delay = config.delay_for_attempt(attempt);
        if !delay.is_zero() {
            // Call progress callback if provided
            if let Some(ref callback) = on_retry {
                callback(attempt, config.max_retries, delay);
            }

            debug!(
                "{}: retry attempt {} after {:?}",
                operation_name, attempt, delay
            );
            stats.total_delay += delay;
            tokio::time::sleep(delay).await;
        }

        match operation().await {
            Ok(result) => {
                if attempt > 0 {
                    debug!("{}: succeeded on attempt {}", operation_name, attempt + 1);
                }
                stats.succeeded = true;
                return (Ok(result), stats);
            }
            Err(e) => {
                let is_last = attempt >= config.max_retries;
                let should_retry = !is_last && is_retryable(&e);

                if should_retry {
                    warn!(
                        "{}: attempt {} failed (will retry): {}",
                        operation_name,
                        attempt + 1,
                        e
                    );
                } else if is_last {
                    warn!(
                        "{}: attempt {} failed (no more retries): {}",
                        operation_name,
                        attempt + 1,
                        e
                    );
                } else {
                    // Non-retryable error
                    warn!(
                        "{}: attempt {} failed (not retryable): {}",
                        operation_name,
                        attempt + 1,
                        e
                    );
                    return (Err(e), stats);
                }

                last_error = Some(e);
            }
        }
    }

    (
        Err(last_error.unwrap_or_else(|| Error::Timeout("all retries exhausted".into()))),
        stats,
    )
}

/// Execute an async operation with retries, using multiple providers
///
/// Tries each provider in order, with retries per provider.
pub async fn with_retry_providers<F, Fut, T, P>(
    config: &RetryConfig,
    operation_name: &str,
    providers: &[P],
    mut operation: F,
) -> Result<T>
where
    F: FnMut(&P) -> Fut,
    Fut: Future<Output = Result<T>>,
    P: std::fmt::Debug,
{
    if providers.is_empty() {
        return Err(Error::Protocol("no providers available".into()));
    }

    let mut last_error = None;

    for (provider_idx, provider) in providers.iter().enumerate() {
        debug!(
            "{}: trying provider {}/{}: {:?}",
            operation_name,
            provider_idx + 1,
            providers.len(),
            provider
        );

        // Use a reduced retry count per provider when we have multiple
        let provider_config = if providers.len() > 1 {
            RetryConfig {
                max_retries: config.max_retries.min(2),
                ..config.clone()
            }
        } else {
            config.clone()
        };

        match with_retry(&provider_config, operation_name, || operation(provider)).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                warn!("{}: provider {:?} failed: {}", operation_name, provider, e);
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| Error::Protocol("all providers failed".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_delay_calculation() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 2.0,
            jitter: 0.0, // No jitter for predictable testing
        };

        assert_eq!(config.delay_for_attempt(0), Duration::ZERO);
        assert_eq!(config.delay_for_attempt(1), Duration::from_secs(1));
        assert_eq!(config.delay_for_attempt(2), Duration::from_secs(2));
        assert_eq!(config.delay_for_attempt(3), Duration::from_secs(4));
        assert_eq!(config.delay_for_attempt(4), Duration::from_secs(8));
        // Should be capped at max_delay
        assert_eq!(config.delay_for_attempt(5), Duration::from_secs(10));
        assert_eq!(config.delay_for_attempt(10), Duration::from_secs(10));
    }

    #[test]
    fn test_is_retryable() {
        assert!(is_retryable(&Error::Connection("timeout".into())));
        assert!(is_retryable(&Error::Timeout("timed out".into())));
        assert!(!is_retryable(&Error::Remote("hash not found".into())));
        assert!(!is_retryable(&Error::StorePathNotFound(
            "/nix/store/x".into()
        )));
    }

    #[tokio::test]
    async fn test_with_retry_success_first_attempt() {
        let config = RetryConfig::default();
        let result: Result<i32> = with_retry(&config, "test", || async { Ok(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_with_retry_success_after_failures() {
        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            jitter: 0.0,
        };

        let attempts = AtomicU32::new(0);

        let result: Result<i32> = with_retry(&config, "test", || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    Err(Error::Connection("transient failure".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_with_retry_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            jitter: 0.0,
        };

        let attempts = AtomicU32::new(0);

        let result: Result<i32> = with_retry(&config, "test", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::Connection("always fails".into())) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3); // 1 + 2 retries
    }

    #[tokio::test]
    async fn test_with_retry_non_retryable_error() {
        let config = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_millis(10),
            ..Default::default()
        };

        let attempts = AtomicU32::new(0);

        let result: Result<i32> = with_retry(&config, "test", || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(Error::StorePathNotFound("/nix/store/x".into())) }
        })
        .await;

        assert!(result.is_err());
        // Should not retry non-retryable errors
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
