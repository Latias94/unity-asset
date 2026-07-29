use thiserror::Error;

pub trait ValidateContract {
    fn validate(&self) -> Result<(), ContractValidationError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContractValidationError {
    #[error("unsupported {contract} version {actual}; expected {expected}")]
    UnsupportedVersion {
        contract: &'static str,
        actual: u16,
        expected: u16,
    },
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} contains {actual} entries; maximum is {maximum}")]
    EntryLimit {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} must be strictly increasing without duplicates")]
    NotStrictlyIncreasing { field: &'static str },
    #[error("{field} is inconsistent with related fields")]
    Inconsistent { field: &'static str },
    #[error("{field} has value {actual}; maximum is {maximum}")]
    NumericLimit {
        field: &'static str,
        actual: u64,
        maximum: u64,
    },
    #[error("{field} contains {actual} UTF-8 bytes; maximum is {maximum}")]
    ByteLimit {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
}

pub(crate) fn ensure_version(
    contract: &'static str,
    actual: u16,
    expected: u16,
) -> Result<(), ContractValidationError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ContractValidationError::UnsupportedVersion {
            contract,
            actual,
            expected,
        })
    }
}
