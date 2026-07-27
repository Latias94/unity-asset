//! Errors produced while encoding or byte-preserving TypeTree values.

use std::collections::TryReserveError;

use thiserror::Error;
use unity_asset_core::BudgetError;

use crate::error::BinaryError;

/// Result type for TypeTree write operations.
pub type TypeTreeWriteResult<T> = std::result::Result<T, TypeTreeWriteError>;

/// Failure produced while validating, encoding, or rewriting a TypeTree value.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TypeTreeWriteError {
    /// A semantic object does not match the writable fields in its schema.
    #[error(
        "TypeTree object shape mismatch: schema expects {expected_fields} named fields, value contains {actual_fields}"
    )]
    Shape {
        expected_fields: usize,
        actual_fields: usize,
    },
    /// A schema node belongs to a different compiled TypeTree.
    #[error("TypeTree value node belongs to a different schema")]
    ForeignNode,
    /// A semantic value cannot be represented by its TypeTree node.
    #[error("{message}")]
    InvalidValue { message: String },
    /// A caller-owned resource budget rejected the operation.
    #[error("{operation}: {source}")]
    Budget {
        operation: &'static str,
        #[source]
        source: BudgetError,
    },
    /// A fallible allocation failed.
    #[error("{operation}: {source}")]
    Allocation {
        operation: &'static str,
        #[source]
        source: TryReserveError,
    },
    /// Existing template bytes are malformed for the selected field.
    #[non_exhaustive]
    #[error("Failed to {operation} for TypeTree template field '{field}': {source}")]
    MalformedTemplate {
        field: String,
        operation: &'static str,
        #[source]
        source: BinaryError,
    },
    /// A binary operation failed outside a field-specific template read.
    #[non_exhaustive]
    #[error("{operation}: {source}")]
    Binary {
        operation: &'static str,
        #[source]
        source: BinaryError,
    },
}

impl TypeTreeWriteError {
    #[must_use]
    pub const fn shape(expected_fields: usize, actual_fields: usize) -> Self {
        Self::Shape {
            expected_fields,
            actual_fields,
        }
    }

    #[must_use]
    pub const fn foreign_node() -> Self {
        Self::ForeignNode
    }

    pub fn invalid_value(message: impl Into<String>) -> Self {
        Self::InvalidValue {
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn budget(operation: &'static str, source: BudgetError) -> Self {
        Self::Budget { operation, source }
    }

    #[must_use]
    pub const fn allocation(operation: &'static str, source: TryReserveError) -> Self {
        Self::Allocation { operation, source }
    }

    pub fn malformed_template(
        field: impl Into<String>,
        operation: &'static str,
        source: BinaryError,
    ) -> Self {
        match source {
            BinaryError::Budget(source) => Self::budget(operation, source),
            source => Self::MalformedTemplate {
                field: field.into(),
                operation,
                source,
            },
        }
    }

    pub fn binary(operation: &'static str, source: BinaryError) -> Self {
        match source {
            BinaryError::Budget(source) => Self::budget(operation, source),
            source => Self::Binary { operation, source },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_budget_is_promoted() {
        let error = TypeTreeWriteError::binary(
            "read TypeTree input",
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 1,
                requested: 2,
            }),
        );

        assert!(matches!(
            error,
            TypeTreeWriteError::Budget {
                operation: "read TypeTree input",
                source: BudgetError::Exceeded {
                    resource: "bytes",
                    limit: 1,
                    requested: 2,
                },
            }
        ));
    }

    #[test]
    fn malformed_template_budget_is_promoted() {
        let error = TypeTreeWriteError::malformed_template(
            "m_Value",
            "compare value",
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 3,
                requested: 4,
            }),
        );

        assert!(matches!(
            error,
            TypeTreeWriteError::Budget {
                operation: "compare value",
                source: BudgetError::Exceeded {
                    resource: "entries",
                    limit: 3,
                    requested: 4,
                },
            }
        ));
    }
}
