use std::path::Path;

use anyhow::{Context, Result};
use unity_asset::extraction::ExtractionExecutor;
use unity_asset::workspace::AssetWorkspace;
use unity_asset::{AssetLoadBudget, WorkspaceId};

use crate::cli_error::{mark_export_execution_error, mark_export_workspace_load_error};
use crate::shared::AppContext;
use crate::workspace_loader::load_full_workspace_excluding_output;

pub(super) fn load(
    input: &Path,
    output: &Path,
    persisted_workspace_id: Option<WorkspaceId>,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    let workspace_id = match persisted_workspace_id {
        Some(workspace_id) => Some(workspace_id),
        None => ExtractionExecutor::publication_workspace_id(output, budget)
            .map_err(mark_export_execution_error)
            .context("Failed to inspect extraction publication recovery")?,
    };
    load_full_workspace_excluding_output(input, output, workspace_id, ctx, budget)
        .map_err(|error| mark_export_workspace_load_error(error, input))
}
