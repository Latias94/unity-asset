use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::bounded::BoundedString;
use crate::{FieldPath, ObjectAddress};

const MAX_DIAGNOSTIC_CODE_BYTES: usize = 128;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 64 * 1024;

/// Current wire version for diagnostics that may carry a versioned object address.
pub const DIAGNOSTIC_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    severity: DiagnosticSeverity,
    code: String,
    message: String,
    address: Option<ObjectAddress>,
    field_path: Option<FieldPath>,
}

#[derive(Serialize)]
struct DiagnosticRef<'a> {
    version: u8,
    severity: DiagnosticSeverity,
    code: &'a str,
    message: &'a str,
    address: &'a Option<ObjectAddress>,
    field_path: &'a Option<FieldPath>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticWire {
    version: u8,
    severity: DiagnosticSeverity,
    code: BoundedString<MAX_DIAGNOSTIC_CODE_BYTES>,
    message: BoundedString<MAX_DIAGNOSTIC_MESSAGE_BYTES>,
    address: Option<ObjectAddress>,
    field_path: Option<FieldPath>,
}

impl Serialize for Diagnostic {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        DiagnosticRef {
            version: DIAGNOSTIC_VERSION,
            severity: self.severity,
            code: &self.code,
            message: &self.message,
            address: &self.address,
            field_path: &self.field_path,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Diagnostic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = DiagnosticWire::deserialize(deserializer)?;
        if wire.version != DIAGNOSTIC_VERSION {
            return Err(serde::de::Error::custom(
                DiagnosticError::UnsupportedVersion(wire.version),
            ));
        }
        let mut diagnostic = Self::new(
            wire.severity,
            wire.code.into_string(),
            wire.message.into_string(),
        )
        .map_err(serde::de::Error::custom)?;
        diagnostic.address = wire.address;
        diagnostic.field_path = wire.field_path;
        Ok(diagnostic)
    }
}

impl Diagnostic {
    pub fn new(
        severity: DiagnosticSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, DiagnosticError> {
        let code = code.into();
        let message = message.into();
        if code.len() > MAX_DIAGNOSTIC_CODE_BYTES {
            return Err(DiagnosticError::CodeTooLong);
        }
        if code.is_empty()
            || !code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(DiagnosticError::InvalidCode(code));
        }
        if message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
            return Err(DiagnosticError::MessageTooLong);
        }
        if message.is_empty() {
            return Err(DiagnosticError::EmptyMessage);
        }
        Ok(Self {
            severity,
            code,
            message,
            address: None,
            field_path: None,
        })
    }

    #[must_use]
    pub fn at_address(mut self, address: ObjectAddress) -> Self {
        self.address = Some(address);
        self
    }

    #[must_use]
    pub fn at_field(mut self, field_path: FieldPath) -> Self {
        self.field_path = Some(field_path);
        self
    }

    #[must_use]
    pub const fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn address(&self) -> Option<&ObjectAddress> {
        self.address.as_ref()
    }

    #[must_use]
    pub const fn field_path(&self) -> Option<&FieldPath> {
        self.field_path.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DiagnosticError {
    #[error("diagnostic version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("diagnostic code exceeds the maximum encoded length")]
    CodeTooLong,
    #[error("diagnostic code must contain only ASCII uppercase letters, digits, or underscores")]
    InvalidCode(String),
    #[error("diagnostic message exceeds the maximum encoded length")]
    MessageTooLong,
    #[error("diagnostic message must not be empty")]
    EmptyMessage,
}
