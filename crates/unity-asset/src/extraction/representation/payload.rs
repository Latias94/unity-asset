//! Caller-budgeted materialization for workspace-backed representation payloads.

use std::collections::TryReserveError;
use std::io;

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};
use unity_asset_decode::media::BudgetedMediaBytes;

use crate::workspace::WorkspaceByteRange;

pub(super) fn copy_workspace_range(
    range: &WorkspaceByteRange,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedMediaBytes, WorkspacePayloadError> {
    let requested = usize::try_from(range.len())
        .map_err(|_| WorkspacePayloadError::LengthOverflow { resource })?;
    budget.check_bytes(range.len())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(requested)
        .map_err(|source| WorkspacePayloadError::Allocation {
            resource,
            requested,
            source,
        })?;
    let retained = u64::try_from(bytes.capacity())
        .map_err(|_| WorkspacePayloadError::LengthOverflow { resource })?;
    budget.check_bytes(retained)?;
    if let Err(source) = range.copy_to(&mut bytes) {
        // The failed read still allocated temporary storage, so the monotonic ledger owns it.
        budget.consume_bytes(retained)?;
        return Err(WorkspacePayloadError::Read { resource, source });
    }
    BudgetedMediaBytes::from_vec(bytes, resource, budget).map_err(Into::into)
}

#[derive(Debug, Error)]
pub(super) enum WorkspacePayloadError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("workspace payload length exceeds usize for {resource}")]
    LengthOverflow { resource: &'static str },
    #[error("failed to allocate {requested} bytes for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("failed to read {resource}: {source}")]
    Read {
        resource: &'static str,
        #[source]
        source: io::Error,
    },
}
