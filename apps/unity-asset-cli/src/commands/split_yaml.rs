use std::path::PathBuf;

use anyhow::{Context, Result};
use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{YamlSplitExecutor, YamlSplitPlanner};

use super::deps;
use super::{parse_existing_output, write_stdout};
use crate::shared::AppContext;

pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    existing_output: String,
    ctx: &AppContext,
) -> Result<()> {
    let existing_output = parse_existing_output(&existing_output)?;
    let mut budget = AssetLoadBudget::default();
    let loaded = deps::load_full_workspace(&input, ctx, &mut budget)?;
    let snapshot = loaded.workspace.snapshot();
    let plan = YamlSplitPlanner::new()
        .plan(&snapshot, &mut budget)
        .context("Failed to plan YAML document splitting")?;
    let report = YamlSplitExecutor::new()
        .execute(&snapshot, &plan, &output, existing_output, &mut budget)
        .context("Failed to publish split YAML documents")?;

    write_stdout(
        |writer| {
            serde_json::to_writer(writer, &report).context("Failed to write the YAML split report")
        },
        "Failed to flush the YAML split report",
    )
}
