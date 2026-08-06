use std::path::PathBuf;

use anyhow::{Context, Result};
use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{
    ExtractionExecutionLimits, ExtractionExecutionOptions, ExtractionExecutor,
    ExtractionFailurePolicy, ExtractionPath, ExtractionPlanner, ExtractionRequest,
    ExtractionRunOptions,
};

use super::write_stdout;
use crate::cli::ExistingOutputArg;
use crate::cli_error::{mark_export_execution_error, mark_export_plan_error};
use crate::shared::AppContext;

use super::extraction_workspace;

pub(crate) fn run(
    input: PathBuf,
    output: PathBuf,
    existing_output: ExistingOutputArg,
    ctx: &AppContext,
) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let workspace = extraction_workspace::load(&input, &output, None, ctx, &mut budget)?;
    let snapshot = workspace.snapshot();
    let request = ExtractionRequest::yaml_documents()
        .with_prefix(ExtractionPath::new("documents").context("Invalid YAML output prefix")?);
    let plan = ExtractionPlanner::new(&snapshot)
        .plan(request, &mut budget)
        .map_err(mark_export_plan_error)
        .context("Failed to plan YAML document extraction")?;
    let execution = ExtractionExecutionOptions::new(
        ExtractionExecutionLimits::default(),
        existing_output.into_policy(),
        ExtractionFailurePolicy::CollectAll,
    )
    .map_err(mark_export_execution_error)
    .context("Invalid YAML extraction execution limits")?;
    let manifest_path = ExtractionPath::new("extraction-manifest.json")
        .context("Invalid YAML extraction manifest path")?;
    let report = ExtractionExecutor::new()
        .execute(
            &snapshot,
            &plan,
            &output,
            ExtractionRunOptions::new(execution).with_manifest_path(&manifest_path),
            &mut budget,
        )
        .map_err(mark_export_execution_error)
        .context("Failed to publish YAML extraction artifacts")?;

    write_stdout(
        |writer| {
            report
                .write_canonical_json(writer)
                .context("Failed to write the canonical extraction report")
        },
        "Failed to flush the YAML extraction report",
    )
}
