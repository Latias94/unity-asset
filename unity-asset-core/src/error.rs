//! Error types for Unity asset parsing

use std::io;
use thiserror::Error;

/// Result type alias for Unity asset operations
pub type Result<T> = std::result::Result<T, UnityAssetError>;

/// Main error type for Unity asset parsing operations
#[derive(Error, Debug)]
pub enum UnityAssetError {
    /// IO errors when reading/writing files
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// Format parsing errors (YAML, binary, etc.)
    #[error("Format parsing error: {0}")]
    Format(String),

    /// Unity-specific format errors
    #[error("Unity format error: {message}")]
    UnityFormat { message: String },

    /// Class-related errors
    #[error("Class error: {message}")]
    Class { message: String },

    /// Unknown class ID encountered
    #[error("Unknown class ID: {class_id}")]
    UnknownClassId { class_id: String },

    /// Property access errors
    #[error("Property '{property}' not found in class '{class_name}'")]
    PropertyNotFound {
        property: String,
        class_name: String,
    },

    /// Type conversion errors
    #[error("Type conversion error: cannot convert {from} to {to}")]
    TypeConversion { from: String, to: String },

    /// Anchor-related errors
    #[error("Anchor error: {message}")]
    Anchor { message: String },

    /// Version compatibility errors
    #[error("Version error: {message}")]
    Version { message: String },

    /// Generic parsing errors
    #[error("Parse error: {message}")]
    Parse { message: String },
}

impl UnityAssetError {
    /// Create a format error
    pub fn format<S: Into<String>>(message: S) -> Self {
        Self::Format(message.into())
    }

    /// Create a Unity format error
    pub fn unity_format<S: Into<String>>(message: S) -> Self {
        Self::UnityFormat {
            message: message.into(),
        }
    }

    /// Create a class error
    pub fn class<S: Into<String>>(message: S) -> Self {
        Self::Class {
            message: message.into(),
        }
    }

    /// Create a property not found error
    pub fn property_not_found<S: Into<String>>(property: S, class_name: S) -> Self {
        Self::PropertyNotFound {
            property: property.into(),
            class_name: class_name.into(),
        }
    }

    /// Create a type conversion error
    pub fn type_conversion<S: Into<String>>(from: S, to: S) -> Self {
        Self::TypeConversion {
            from: from.into(),
            to: to.into(),
        }
    }

    /// Create an anchor error
    pub fn anchor<S: Into<String>>(message: S) -> Self {
        Self::Anchor {
            message: message.into(),
        }
    }

    /// Create a version error
    pub fn version<S: Into<String>>(message: S) -> Self {
        Self::Version {
            message: message.into(),
        }
    }

    /// Create a parse error
    pub fn parse<S: Into<String>>(message: S) -> Self {
        Self::Parse {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_creation() {
        let err = UnityAssetError::format("test message");
        assert!(matches!(err, UnityAssetError::Format(_)));
    }

    #[test]
    fn test_error_display() {
        let err = UnityAssetError::property_not_found("m_Name", "GameObject");
        let msg = format!("{}", err);
        assert!(msg.contains("m_Name"));
        assert!(msg.contains("GameObject"));
    }
}
