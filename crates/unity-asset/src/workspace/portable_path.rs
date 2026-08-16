use std::path::Path;

use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use unity_asset_core::{AssetLoadBudget, BudgetError};

#[derive(Debug, Error)]
pub(crate) enum PortablePathError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("native path contains a non-UTF-8 component")]
    UnsupportedEncoding,
    #[error("failed to allocate {requested} bytes for a portable path key: {message}")]
    Allocation { requested: usize, message: String },
}

pub(crate) fn native_key(
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<String, PortablePathError> {
    let mut requested = 0_usize;
    for component in path.components() {
        let component = component
            .as_os_str()
            .to_str()
            .ok_or(PortablePathError::UnsupportedEncoding)?;
        requested = add_component_length(requested, component)?;
    }
    let mut key = allocate_key(requested, budget)?;
    for component in path.components() {
        let component = component
            .as_os_str()
            .to_str()
            .ok_or(PortablePathError::UnsupportedEncoding)?;
        push_component(&mut key, component);
    }
    budget.consume_bytes(usize_to_u64(key.capacity())?)?;
    Ok(key)
}

pub(crate) fn slash_key(
    path: &str,
    budget: &mut AssetLoadBudget,
) -> Result<String, PortablePathError> {
    let mut requested = 0_usize;
    for component in path.split('/') {
        requested = add_component_length(requested, component)?;
    }
    let mut key = allocate_key(requested, budget)?;
    for component in path.split('/') {
        push_component(&mut key, component);
    }
    budget.consume_bytes(usize_to_u64(key.capacity())?)?;
    Ok(key)
}

fn add_component_length(mut length: usize, component: &str) -> Result<usize, BudgetError> {
    length = length
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "portable destination path",
        })?;
    for character in component.nfkc().flat_map(char::to_lowercase) {
        length =
            length
                .checked_add(character.len_utf8())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "portable destination path",
                })?;
    }
    Ok(length)
}

fn allocate_key(requested: usize, budget: &AssetLoadBudget) -> Result<String, PortablePathError> {
    budget.check_bytes(usize_to_u64(requested)?)?;
    let mut key = String::new();
    key.try_reserve_exact(requested)
        .map_err(|error| PortablePathError::Allocation {
            requested,
            message: error.to_string(),
        })?;
    Ok(key)
}

fn push_component(key: &mut String, component: &str) {
    key.push('/');
    key.extend(component.nfkc().flat_map(char::to_lowercase));
}

fn usize_to_u64(value: usize) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "portable destination path",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    #[test]
    fn native_and_slash_keys_share_unicode_alias_policy() {
        let mut native_budget = AssetLoadBudget::default();
        let native = native_key(Path::new("Folder/\u{e9}.asset"), &mut native_budget).unwrap();
        let mut slash_budget = AssetLoadBudget::default();
        let slash = slash_key("folder/e\u{301}.ASSET", &mut slash_budget).unwrap();
        assert_eq!(native, slash);

        let usage = slash_budget.usage();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert_eq!(
            slash_key("folder/e\u{301}.ASSET", &mut exact).unwrap(),
            slash
        );
        assert_eq!(exact.usage().bytes, usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            slash_key("folder/e\u{301}.ASSET", &mut one_short),
            Err(PortablePathError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }
}
