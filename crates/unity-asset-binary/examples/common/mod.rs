use unity_asset_binary::object::ObjectHandle;
use unity_asset_binary::{BinaryError, Result};
use unity_asset_core::AssetLoadBudget;

fn is_skippable_object_error(error: &BinaryError) -> bool {
    matches!(
        error,
        BinaryError::InvalidFormat(_)
            | BinaryError::UnsupportedVersion(_)
            | BinaryError::InvalidData(_)
            | BinaryError::ParseError(_)
            | BinaryError::NotEnoughData { .. }
            | BinaryError::InvalidSignature { .. }
            | BinaryError::Unsupported(_)
            | BinaryError::CorruptedData(_)
            | BinaryError::VersionCompatibility(_)
    )
}

pub(crate) fn peek_name_best_effort(
    handle: &ObjectHandle<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>> {
    match handle.peek_name(budget) {
        Ok(name) => Ok(name),
        Err(error) if is_skippable_object_error(&error) => {
            eprintln!(
                "warning: path_id={} has no readable name: {error}",
                handle.path_id()
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
