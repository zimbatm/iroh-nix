//! Retry utilities for network operations
//!
//! Provides configurable retry logic with exponential backoff for
//! transient failures in network operations.

use std::future::Future;
use std::time::Duration;

use tracing::{debug, warn};

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

/// Callback for retry progress updates
pub type RetryCallback = Box<dyn Fn(u32, u32, Duration) + Send + Sync>;

/// Execute an async operation with retries
///
/// The `is_retryable` predicate determines whether a given error should be retried.
pub async fn with_retry<F, Fut, T, E>(
    config: &RetryConfig,
    operation_name: &str,
    is_retryable: impl Fn(&E) -> bool,
    operation: F,
) -> std::result::Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    let (result, _stats) =
        with_retry_stats(config, operation_name, is_retryable, operation, None).await;
    result
}

/// Execute an async operation with retries and return statistics
///
/// The optional callback is called before each retry with (attempt, max_retries, delay).
pub async fn with_retry_stats<F, Fut, T, E>(
    config: &RetryConfig,
    operation_name: &str,
    is_retryable: impl Fn(&E) -> bool,
    mut operation: F,
    on_retry: Option<RetryCallback>,
) -> (std::result::Result<T, E>, RetryStats)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
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

    (Err(last_error.expect("at least one attempt")), stats)
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

    #[tokio::test]
    async fn test_with_retry_success_first_attempt() {
        let config = RetryConfig::default();
        let result: std::result::Result<i32, String> =
            with_retry(&config, "test", |_| false, || async { Ok(42) }).await;
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

        let result: std::result::Result<i32, String> = with_retry(
            &config,
            "test",
            |_| true, // all errors retryable
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt < 2 {
                        Err("transient failure".to_string())
                    } else {
                        Ok(42)
                    }
                }
            },
        )
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

        let result: std::result::Result<i32, String> = with_retry(
            &config,
            "test",
            |_| true,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err("always fails".to_string()) }
            },
        )
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

        let result: std::result::Result<i32, String> = with_retry(
            &config,
            "test",
            |_| false, // nothing is retryable
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err("permanent failure".to_string()) }
            },
        )
        .await;

        assert!(result.is_err());
        // Should not retry non-retryable errors
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
