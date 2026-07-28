use anyhow::{Context, Result};
use unity_asset::AssetLoadBudget;
use unity_asset::extraction::{
    ExtractionExecutionLimits, ExtractionExecutionOptions, ExtractionExecutor, ExtractionManifest,
    ExtractionPath, ExtractionPlan, ExtractionPlanner, ExtractionRequest, ExtractionRunOptions,
    ExtractionSelection,
};
use unity_asset::reference::ReferenceGraphBuildOptions;

use super::write_stdout;
use crate::cli::ExportCommand;
use crate::cli_error::{
    ExportManifestPathErrorKind, mark_export_execution_error, mark_export_manifest_path_error,
    mark_export_plan_error, mark_export_reference_graph_error, mark_export_shared_stdin_error,
    mark_export_workspace_load_error,
};
use crate::json_io::with_contract_reader;
use crate::shared::AppContext;
use crate::workspace_loader::{
    load_full_workspace_excluding_output, load_full_workspace_with_workspace_id_excluding_output,
};

pub(crate) fn run(command: ExportCommand, ctx: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    reject_shared_stdin(&command)?;
    let manifest_path = command
        .manifest
        .as_deref()
        .map(parse_manifest_path)
        .transpose()?;
    let resume = command
        .resume
        .as_deref()
        .map(|path| read_manifest(path, &mut budget))
        .transpose()?;
    let saved_plan = command
        .plan
        .as_deref()
        .map(|path| read_plan(path, &mut budget))
        .transpose()?;
    let request = command
        .request
        .as_deref()
        .map(|path| read_request(path, &mut budget))
        .transpose()?;
    let workspace_id = saved_plan
        .as_ref()
        .map(ExtractionPlan::workspace_id)
        .or_else(|| resume.as_ref().map(ExtractionManifest::workspace_id));
    let workspace = match workspace_id {
        Some(workspace_id) => load_full_workspace_with_workspace_id_excluding_output(
            &command.input,
            &command.output,
            workspace_id,
            ctx,
            &mut budget,
        ),
        None => {
            load_full_workspace_excluding_output(&command.input, &command.output, ctx, &mut budget)
        }
    }
    .map_err(|error| mark_export_workspace_load_error(error, &command.input))?;
    let snapshot = workspace.snapshot();
    let plan = match saved_plan {
        Some(plan) => plan,
        None => {
            let request = request.context("ExtractionRequest is required when --plan is absent")?;
            let references = matches!(
                request.selection(),
                ExtractionSelection::ReferenceTraversal { .. }
            )
            .then(|| {
                snapshot
                    .reference_graph(ReferenceGraphBuildOptions::unbounded(), &mut budget)
                    .map_err(mark_export_reference_graph_error)
                    .context("Failed to build the extraction reference graph")
            })
            .transpose()?;
            let planner = ExtractionPlanner::new(&snapshot);
            let planner = match references.as_ref() {
                Some(references) => planner.with_reference_graph(references),
                None => planner,
            };
            planner
                .plan(request, &mut budget)
                .map_err(mark_export_plan_error)
                .context("Failed to plan extraction")?
        }
    };

    if command.dry_run {
        return write_stdout(
            |output| {
                plan.write_canonical_json(output)
                    .context("Failed to write the canonical extraction plan")
            },
            "Failed to flush the extraction plan",
        );
    }

    let options = ExtractionExecutionOptions::new(
        execution_limits(&command)?,
        command.existing_output.into_policy(),
        command.failure.into_policy(),
    )
    .map_err(mark_export_execution_error)?;
    let executor = ExtractionExecutor::new();
    let mut run = ExtractionRunOptions::new(options);
    if let Some(resume) = resume.as_ref() {
        run = run.with_resume(resume);
    }
    if let Some(manifest_path) = manifest_path.as_ref() {
        run = run.with_manifest_path(manifest_path);
    }
    let report = executor
        .execute(&snapshot, &plan, &command.output, run, &mut budget)
        .map_err(mark_export_execution_error)
        .context("Extraction execution failed")?;

    write_stdout(
        |output| {
            report
                .write_canonical_json(output)
                .context("Failed to write the canonical extraction report")
        },
        "Failed to flush the extraction report",
    )
}

fn parse_manifest_path(path: &std::path::Path) -> Result<ExtractionPath> {
    let value = path.to_str().ok_or_else(|| {
        mark_export_manifest_path_error(
            anyhow::anyhow!("--manifest must be a valid UTF-8 relative path"),
            path,
            ExportManifestPathErrorKind::NonUtf8,
        )
    })?;
    ExtractionPath::new(value).map_err(|error| {
        mark_export_manifest_path_error(error, path, ExportManifestPathErrorKind::Invalid)
    })
}

fn read_manifest(
    path: &std::path::Path,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionManifest> {
    with_contract_reader(path, |reader| {
        ExtractionManifest::read_json(reader, budget)
            .with_context(|| format!("Invalid --resume manifest {}", path.display()))
    })
}

fn read_plan(path: &std::path::Path, budget: &mut AssetLoadBudget) -> Result<ExtractionPlan> {
    with_contract_reader(path, |reader| {
        ExtractionPlan::read_json(reader, budget)
            .with_context(|| format!("Invalid --plan file {}", path.display()))
    })
}

fn read_request(path: &std::path::Path, budget: &mut AssetLoadBudget) -> Result<ExtractionRequest> {
    with_contract_reader(path, |reader| {
        ExtractionRequest::read_json(reader, budget)
            .with_context(|| format!("Invalid --request file {}", path.display()))
    })
}

fn execution_limits(command: &ExportCommand) -> Result<ExtractionExecutionLimits> {
    let defaults = ExtractionExecutionLimits::default();
    ExtractionExecutionLimits::new(
        command.workers.unwrap_or(defaults.workers()),
        command
            .max_in_flight_bytes
            .unwrap_or(defaults.max_in_flight_bytes()),
        command.max_open_files.unwrap_or(defaults.max_open_files()),
        command
            .max_output_bytes
            .unwrap_or(defaults.max_output_bytes()),
        command
            .max_report_bytes
            .unwrap_or(defaults.max_report_bytes()),
    )
    .map_err(mark_export_execution_error)
    .context("Invalid extraction execution limits")
}

fn reject_shared_stdin(command: &ExportCommand) -> Result<()> {
    let inputs = [
        ("--plan", command.plan.as_deref()),
        ("--request", command.request.as_deref()),
        ("--resume", command.resume.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, path)| (path == Some(std::path::Path::new("-"))).then_some(name))
    .collect::<Vec<_>>();
    if inputs.len() > 1 {
        return Err(mark_export_shared_stdin_error(&inputs));
    }
    Ok(())
}
