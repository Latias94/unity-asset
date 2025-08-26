//! Error handling system
//!
//! Comprehensive error handling for async Unity asset processing with recovery mechanisms.

use std::time::Duration;
use thiserror::Error;
use tokio::time::{sleep, Instant};
use tracing::{error, info, warn};

/// Result type for operations
pub type Result<T> = std::result::Result<T, UnityAssetError>;

/// Main error type for Unity Asset Parser V2
#[derive(Error, Debug, Clone)]
pub enum UnityAssetError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {message} at position {position}")]
    Parse { message: String, position: u64 },

    #[error("Unsupported format: {format}")]
    UnsupportedFormat { format: String },

    #[error("Compression error: {0}")]
    Compression(String),

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("Unexpected end of file")]
    UnexpectedEof,

    #[error("Timeout error: operation took longer than {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("Concurrency error: {0}")]
    Concurrency(String),

    #[error("Memory error: {0}")]
    Memory(String),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Validation error: {field}: {message}")]
    Validation { field: String, message: String },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Task join error: {0}")]
    TaskJoin(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Custom error: {0}")]
    Custom(String),
}

impl UnityAssetError {
    /// Create a parse error with position
    pub fn parse_error<S: Into<String>>(message: S, position: u64) -> Self {
        Self::Parse {
            message: message.into(),
            position,
        }
    }

    /// Create an unsupported format error
    pub fn unsupported_format<S: Into<String>>(format: S) -> Self {
        Self::UnsupportedFormat {
            format: format.into(),
        }
    }

    /// Create a timeout error
    pub fn timeout(duration: Duration) -> Self {
        Self::Timeout {
            timeout_ms: duration.as_millis() as u64,
        }
    }

    /// Create a validation error
    pub fn validation<F, M>(field: F, message: M) -> Self
    where
        F: Into<String>,
        M: Into<String>,
    {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Check if this error is recoverable
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Io(_) => true,
            Self::Timeout { .. } => true,
            Self::Concurrency(_) => true,
            Self::Memory(_) => false, // Memory errors are typically not recoverable
            Self::Cache(_) => true,
            _ => false,
        }
    }

    /// Check if this error suggests retrying
    pub fn should_retry(&self) -> bool {
        match self {
            Self::Io(_) => true,
            Self::Timeout { .. } => true,
            Self::Concurrency(_) => true,
            _ => false,
        }
    }
}

impl From<std::io::Error> for UnityAssetError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<tokio::task::JoinError> for UnityAssetError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::TaskJoin(error.to_string())
    }
}

impl From<serde_yaml::Error> for UnityAssetError {
    fn from(error: serde_yaml::Error) -> Self {
        Self::Serialization(error.to_string())
    }
}

/// Retry configuration for error recovery
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: usize,
    /// Base delay between retries
    pub base_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Backoff multiplier (exponential backoff)
    pub backoff_factor: f64,
    /// Jitter range (0.0 - 1.0) to avoid thundering herd
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            backoff_factor: 2.0,
            jitter: 0.1,
        }
    }
}

impl RetryConfig {
    /// Create a retry config for I/O operations
    pub fn for_io() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter: 0.2,
        }
    }

    /// Create a retry config for network operations
    pub fn for_network() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.5,
            jitter: 0.3,
        }
    }

    /// Calculate delay for attempt number
    pub fn delay_for_attempt(&self, attempt: usize) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }

        let base_ms = self.base_delay.as_millis() as f64;
        let delay_ms = base_ms * self.backoff_factor.powi((attempt - 1) as i32);
        let max_ms = self.max_delay.as_millis() as f64;

        let clamped_delay_ms = delay_ms.min(max_ms);

        // Add jitter
        let jitter_range = clamped_delay_ms * self.jitter;
        let jitter = (rand::random::<f64>() - 0.5) * 2.0 * jitter_range;

        let final_delay_ms = (clamped_delay_ms + jitter).max(0.0);
        Duration::from_millis(final_delay_ms as u64)
    }
}

/// Error recovery utility for async operations
pub struct ErrorRecovery {
    config: RetryConfig,
}

impl ErrorRecovery {
    /// Create new error recovery with config
    pub fn new(config: RetryConfig) -> Self {
        Self { config }
    }

    /// Create with default config
    pub fn default() -> Self {
        Self::new(RetryConfig::default())
    }

    /// Retry async operation with exponential backoff
    pub async fn retry_async<F, Fut, T>(&self, mut operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last_error = None;

        for attempt in 0..self.config.max_attempts {
            let start_time = Instant::now();

            match operation().await {
                Ok(result) => {
                    if attempt > 0 {
                        info!(
                            "Operation succeeded after {} attempts, took {:?}",
                            attempt + 1,
                            start_time.elapsed()
                        );
                    }
                    return Ok(result);
                }
                Err(error) => {
                    last_error = Some(error.clone());

                    if attempt < self.config.max_attempts - 1 {
                        let delay = self.config.delay_for_attempt(attempt + 1);

                        warn!(
                            "Attempt {} failed: {:?}, retrying in {:?}",
                            attempt + 1,
                            error,
                            delay
                        );

                        sleep(delay).await;
                    } else {
                        error!(
                            "All {} attempts failed, final error: {:?}",
                            self.config.max_attempts, error
                        );
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    /// Retry with conditional check
    pub async fn retry_if<F, Fut, T, P>(&self, mut operation: F, mut should_retry: P) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
        P: FnMut(&UnityAssetError) -> bool,
    {
        let mut last_error = None;

        for attempt in 0..self.config.max_attempts {
            match operation().await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    if attempt < self.config.max_attempts - 1 && should_retry(&error) {
                        let delay = self.config.delay_for_attempt(attempt + 1);
                        warn!(
                            "Retryable error on attempt {}: {:?}, retrying in {:?}",
                            attempt + 1,
                            error,
                            delay
                        );
                        sleep(delay).await;
                        last_error = Some(error);
                    } else {
                        return Err(error);
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }
}

/// Convenient retry macro for Unity asset errors
#[macro_export]
macro_rules! retry_async {
    ($operation:expr) => {
        ErrorRecovery::default().retry_async(|| $operation).await
    };

    ($config:expr, $operation:expr) => {
        ErrorRecovery::new($config).retry_async(|| $operation).await
    };
}

/// Error context trait for better error reporting
pub trait ErrorContext<T> {
    /// Add context to error
    fn with_context<C>(self, context: C) -> Result<T>
    where
        C: Into<String>;

    /// Add context with format
    fn with_context_fmt(self, args: std::fmt::Arguments<'_>) -> Result<T>;
}

impl<T, E> ErrorContext<T> for std::result::Result<T, E>
where
    E: Into<UnityAssetError>,
{
    fn with_context<C>(self, context: C) -> Result<T>
    where
        C: Into<String>,
    {
        self.map_err(|e| {
            let base_error = e.into();
            UnityAssetError::Custom(format!("{}: {}", context.into(), base_error))
        })
    }

    fn with_context_fmt(self, args: std::fmt::Arguments<'_>) -> Result<T> {
        self.with_context(format!("{}", args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio_test;

    #[tokio::test]
    async fn test_retry_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let config = RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_factor: 2.0,
            jitter: 0.0,
        };

        let recovery = ErrorRecovery::new(config);

        let result = recovery
            .retry_async(|| async {
                let count = counter_clone.fetch_add(1, Ordering::SeqCst);
                if count < 2 {
                    Err(UnityAssetError::Custom("temporary failure".to_string()))
                } else {
                    Ok("success")
                }
            })
            .await;

        assert_eq!(result.unwrap(), "success");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_failure() {
        let config = RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_factor: 2.0,
            jitter: 0.0,
        };

        let recovery = ErrorRecovery::new(config);

        let result = recovery
            .retry_async(|| async {
                Err::<String, _>(UnityAssetError::Custom("permanent failure".to_string()))
            })
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_error_is_recoverable() {
        let io_err = UnityAssetError::Io("interrupted".to_string());
        assert!(io_err.is_recoverable());

        let parse_err = UnityAssetError::parse_error("invalid data", 100);
        assert!(!parse_err.is_recoverable());

        let timeout_err = UnityAssetError::timeout(Duration::from_secs(30));
        assert!(timeout_err.is_recoverable());
    }

    #[test]
    fn test_retry_config_delay() {
        let config = RetryConfig::default();

        assert_eq!(config.delay_for_attempt(0), Duration::ZERO);

        let delay1 = config.delay_for_attempt(1);
        let delay2 = config.delay_for_attempt(2);

        // Second delay should be longer (exponential backoff)
        assert!(delay2 > delay1);
    }
}
