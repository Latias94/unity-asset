use std::path::PathBuf;

use anyhow::{Context, Result};
use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{YamlSplitExecutor, YamlSplitPlanner};

use super::write_stdout;
use crate::cli::ExistingOutputArg;
use crate::shared::AppContext;
use crate::workspace_loader::load_full_workspace_excluding_output;

pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    existing_output: ExistingOutputArg,
    ctx: &AppContext,
) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let workspace = load_full_workspace_excluding_output(&input, &output, ctx, &mut budget)?;
    let snapshot = workspace.snapshot();
    let plan = YamlSplitPlanner::new()
        .plan(&snapshot, &mut budget)
        .context("Failed to plan YAML document splitting")?;
    let report = YamlSplitExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            existing_output.into_policy(),
            &mut budget,
        )
        .context("Failed to publish split YAML documents")?;

    write_stdout(
        |writer| {
            serde_json::to_writer(writer, &report).context("Failed to write the YAML split report")
        },
        "Failed to flush the YAML split report",
    )
}
