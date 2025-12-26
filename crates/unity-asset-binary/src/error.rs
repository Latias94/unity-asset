//! Error types for Unity binary parsing

use thiserror::Error;

/// Result type for Unity binary operations
pub type Result<T> = std::result::Result<T, BinaryError>;

/// Errors that can occur during Unity binary parsing
#[derive(Error, Debug)]
pub enum BinaryError {
    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid file format
    #[error("Invalid file format: {0}")]
    InvalidFormat(String),

    /// Unsupported file version
    #[error("Unsupported file version: {0}")]
    UnsupportedVersion(String),

    /// Unsupported compression format
    #[error("Unsupported compression: {0}")]
    UnsupportedCompression(String),

    /// Decompression failed
    #[error("Decompression failed: {0}")]
    DecompressionFailed(String),

    /// Invalid data
    #[error("Invalid data: {0}")]
    InvalidData(String),

    /// Parsing error
    #[error("Parse error: {0}")]
    ParseError(String),

    /// Not enough data
    #[error("Not enough data: expected {expected}, got {actual}")]
    NotEnoughData { expected: usize, actual: usize },

    /// Invalid signature
    #[error("Invalid signature: expected {expected}, got {actual}")]
    InvalidSignature { expected: String, actual: String },

    /// Unsupported feature
    #[error("Unsupported feature: {0}")]
    Unsupported(String),

    /// Memory allocation error
    #[error("Memory allocation error: {0}")]
    MemoryError(String),

    /// Timeout error
    #[error("Operation timed out: {0}")]
    Timeout(String),

    /// Resource limit exceeded
    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    /// Corrupted data
    #[error("Corrupted data detected: {0}")]
    CorruptedData(String),

    /// Version compatibility error
    #[error("Version compatibility error: {0}")]
    VersionCompatibility(String),

    /// Generic error with context
    #[error("Error: {0}")]
    Generic(String),
}

impl BinaryError {
    /// Create a new invalid format error
    pub fn invalid_format<S: Into<String>>(msg: S) -> Self {
        Self::InvalidFormat(msg.into())
    }

    /// Create a generic error (for compatibility)
    pub fn format<S: Into<String>>(msg: S) -> Self {
        Self::Generic(msg.into())
    }

    /// Create a new unsupported version error
    pub fn unsupported_version<S: Into<String>>(version: S) -> Self {
        Self::UnsupportedVersion(version.into())
    }

    /// Create a new unsupported compression error
    pub fn unsupported_compression<S: Into<String>>(compression: S) -> Self {
        Self::UnsupportedCompression(compression.into())
    }

    /// Create a new decompression failed error
    pub fn decompression_failed<S: Into<String>>(msg: S) -> Self {
        Self::DecompressionFailed(msg.into())
    }

    /// Create a new invalid data error
    pub fn invalid_data<S: Into<String>>(msg: S) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Create a new parse error
    pub fn parse_error<S: Into<String>>(msg: S) -> Self {
        Self::ParseError(msg.into())
    }

    /// Create a new not enough data error
    pub fn not_enough_data(expected: usize, actual: usize) -> Self {
        Self::NotEnoughData { expected, actual }
    }

    /// Create a new invalid signature error
    pub fn invalid_signature<S: Into<String>>(expected: S, actual: S) -> Self {
        Self::InvalidSignature {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Create a new unsupported feature error
    pub fn unsupported<S: Into<String>>(feature: S) -> Self {
        Self::Unsupported(feature.into())
    }

    /// Create a new generic error
    pub fn generic<S: Into<String>>(msg: S) -> Self {
        Self::Generic(msg.into())
    }

    /// Create a new I/O error (alias for generic)
    pub fn io_error<S: Into<String>>(msg: S) -> Self {
        Self::Generic(msg.into())
    }
}

// Conversion from other error types
impl From<lz4_flex::block::DecompressError> for BinaryError {
    fn from(err: lz4_flex::block::DecompressError) -> Self {
        Self::decompression_failed(format!("LZ4 decompression failed: {}", err))
    }
}

impl From<lz4_flex::frame::Error> for BinaryError {
    fn from(err: lz4_flex::frame::Error) -> Self {
        Self::decompression_failed(format!("LZ4 frame error: {}", err))
    }
}

impl From<std::string::FromUtf8Error> for BinaryError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Self::invalid_data(format!("Invalid UTF-8 string: {}", err))
    }
}

impl From<std::str::Utf8Error> for BinaryError {
    fn from(err: std::str::Utf8Error) -> Self {
        Self::invalid_data(format!("Invalid UTF-8 string: {}", err))
    }
}

/// Error severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorSeverity {
    /// Low severity - can be ignored
    Low,
    /// Medium severity - should be logged
    Medium,
    /// High severity - requires attention
    High,
    /// Critical severity - operation cannot continue
    Critical,
}

impl BinaryError {
    /// Create a memory error
    pub fn memory_error(msg: impl Into<String>) -> Self {
        BinaryError::MemoryError(msg.into())
    }

    /// Create a timeout error
    pub fn timeout(msg: impl Into<String>) -> Self {
        BinaryError::Timeout(msg.into())
    }

    /// Create a corrupted data error
    pub fn corrupted_data(msg: impl Into<String>) -> Self {
        BinaryError::CorruptedData(msg.into())
    }

    /// Create a version compatibility error
    pub fn version_compatibility(msg: impl Into<String>) -> Self {
        BinaryError::VersionCompatibility(msg.into())
    }

    /// Check if this error is recoverable
    pub fn is_recoverable(&self) -> bool {
        match self {
            BinaryError::Io(_) => false,
            BinaryError::InvalidFormat(_) => false,
            BinaryError::UnsupportedVersion(_) => false,
            BinaryError::UnsupportedCompression(_) => true, // Might try different compression
            BinaryError::DecompressionFailed(_) => true,    // Might retry or skip
            BinaryError::InvalidData(_) => true,            // Might skip corrupted object
            BinaryError::ParseError(_) => true,             // Might skip problematic object
            BinaryError::NotEnoughData { .. } => false,
            BinaryError::InvalidSignature { .. } => false,
            BinaryError::Unsupported(_) => true, // Might skip unsupported feature
            BinaryError::MemoryError(_) => false,
            BinaryError::Timeout(_) => true, // Might retry
            BinaryError::ResourceLimitExceeded(_) => true, // Might reduce limits
            BinaryError::CorruptedData(_) => true, // Might skip corrupted section
            BinaryError::VersionCompatibility(_) => true, // Might use compatibility mode
            BinaryError::Generic(_) => true, // Generic errors are usually recoverable
        }
    }

    /// Get error severity level
    pub fn severity(&self) -> ErrorSeverity {
        match self {
            BinaryError::Io(_) => ErrorSeverity::Critical,
            BinaryError::InvalidFormat(_) => ErrorSeverity::Critical,
            BinaryError::UnsupportedVersion(_) => ErrorSeverity::High,
            BinaryError::UnsupportedCompression(_) => ErrorSeverity::Medium,
            BinaryError::DecompressionFailed(_) => ErrorSeverity::Medium,
            BinaryError::InvalidData(_) => ErrorSeverity::Medium,
            BinaryError::ParseError(_) => ErrorSeverity::Medium,
            BinaryError::NotEnoughData { .. } => ErrorSeverity::High,
            BinaryError::InvalidSignature { .. } => ErrorSeverity::High,
            BinaryError::Unsupported(_) => ErrorSeverity::Low,
            BinaryError::MemoryError(_) => ErrorSeverity::Critical,
            BinaryError::Timeout(_) => ErrorSeverity::Medium,
            BinaryError::ResourceLimitExceeded(_) => ErrorSeverity::Medium,
            BinaryError::CorruptedData(_) => ErrorSeverity::Medium,
            BinaryError::VersionCompatibility(_) => ErrorSeverity::Low,
            BinaryError::Generic(_) => ErrorSeverity::Medium,
        }
    }

    /// Get suggested recovery action
    pub fn recovery_suggestion(&self) -> Option<&'static str> {
        match self {
            BinaryError::UnsupportedCompression(_) => Some("Try different compression method"),
            BinaryError::DecompressionFailed(_) => Some("Skip compressed section or retry"),
            BinaryError::InvalidData(_) => Some("Skip corrupted object and continue"),
            BinaryError::ParseError(_) => Some("Skip problematic object and continue"),
            BinaryError::Unsupported(_) => Some("Skip unsupported feature"),
            BinaryError::Timeout(_) => Some("Retry with longer timeout"),
            BinaryError::ResourceLimitExceeded(_) => Some("Reduce processing limits"),
            BinaryError::CorruptedData(_) => Some("Skip corrupted section"),
            BinaryError::VersionCompatibility(_) => Some("Enable compatibility mode"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_creation() {
        let err = BinaryError::invalid_format("test format");
        assert!(matches!(err, BinaryError::InvalidFormat(_)));
        assert_eq!(err.to_string(), "Invalid file format: test format");
    }

    #[test]
    fn test_not_enough_data_error() {
        let err = BinaryError::not_enough_data(100, 50);
        assert!(matches!(err, BinaryError::NotEnoughData { .. }));
        assert_eq!(err.to_string(), "Not enough data: expected 100, got 50");
    }

    #[test]
    fn test_invalid_signature_error() {
        let err = BinaryError::invalid_signature("UnityFS", "UnityWeb");
        assert!(matches!(err, BinaryError::InvalidSignature { .. }));
        assert_eq!(
            err.to_string(),
            "Invalid signature: expected UnityFS, got UnityWeb"
        );
    }
}
