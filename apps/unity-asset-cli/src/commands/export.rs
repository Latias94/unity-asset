use std::fs::File;
use std::str::FromStr;

use anyhow::{Context, Result};
use unity_asset::extraction::{
    ExtractionExecutionLimits, ExtractionExecutionOptions, ExtractionExecutor,
    ExtractionFailurePolicy, ExtractionFilter, ExtractionManifest, ExtractionPath, ExtractionPlan,
    ExtractionPlanner, ExtractionRepresentationPolicy, ExtractionRequest,
};
use unity_asset::reference::ReferenceGraphBuildOptions;
use unity_asset::workspace::WorkspaceSnapshot;
use unity_asset::{AssetLoadBudget, ObjectAddress, SourceLocator};

use super::deps;
use super::{parse_existing_output, write_stdout};
use crate::cli::ExportCommand;
use crate::shared::AppContext;

#[derive(Clone)]
struct RequestOptions {
    representation: ExtractionRepresentationPolicy,
    filter: ExtractionFilter,
    prefix: Option<ExtractionPath>,
}

pub(crate) fn run(command: ExportCommand, ctx: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
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
    let workspace_id = saved_plan
        .as_ref()
        .map(ExtractionPlan::workspace_id)
        .or_else(|| resume.as_ref().map(ExtractionManifest::workspace_id));
    let loaded = match workspace_id {
        Some(workspace_id) => deps::load_full_workspace_with_workspace_id(
            &command.input,
            workspace_id,
            ctx,
            &mut budget,
        )?,
        None => deps::load_full_workspace(&command.input, ctx, &mut budget)?,
    };
    let snapshot = loaded.workspace.snapshot();
    let plan = match saved_plan {
        Some(plan) => plan,
        None => build_plan(&command, &snapshot, &mut budget)?,
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
        parse_existing_output(&command.existing_output)?,
        parse_failure_policy(&command.failure)?,
    )?;
    let executor = ExtractionExecutor::new();
    let report = match manifest_path.as_ref() {
        Some(manifest_path) => executor.execute_with_manifest(
            &snapshot,
            &plan,
            &command.output,
            manifest_path,
            &options,
            resume.as_ref(),
            &mut budget,
        ),
        None => executor.execute(
            &snapshot,
            &plan,
            &command.output,
            &options,
            resume.as_ref(),
            &mut budget,
        ),
    }
    .context("Extraction execution failed")?;

    write_stdout(
        |output| {
            report
                .write_canonical_manifest_json(output)
                .context("Failed to write the canonical extraction manifest")
        },
        "Failed to flush the extraction manifest",
    )
}

fn parse_manifest_path(path: &std::path::Path) -> Result<ExtractionPath> {
    let value = path
        .to_str()
        .context("--manifest must be a valid UTF-8 relative path")?;
    ExtractionPath::new(value).context("Invalid --manifest path")
}

fn build_plan(
    command: &ExportCommand,
    snapshot: &WorkspaceSnapshot,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionPlan> {
    let request_options = RequestOptions {
        representation: parse_representation(&command.representation)?,
        filter: ExtractionFilter::new(
            command.class_id.iter().copied(),
            command.class_name.clone(),
            command.name.clone(),
            command.limit,
        )
        .context("Invalid extraction filter")?,
        prefix: command
            .prefix
            .as_deref()
            .map(ExtractionPath::new)
            .transpose()
            .context("Invalid --prefix")?,
    };
    match command.bundle_container.as_deref() {
        Some(pattern) => {
            let graph = snapshot
                .reference_graph(ReferenceGraphBuildOptions::unbounded(), budget)
                .context("Failed to build the reference graph for --bundle-container")?;
            let planner = ExtractionPlanner::new(snapshot).with_reference_graph(&graph);
            let addresses = planner
                .bundle_container_addresses(pattern, budget)
                .context("Failed to resolve AssetBundle m_Container entries")?;
            let request = request_with_options(
                ExtractionRequest::bundle_container(
                    pattern.to_owned(),
                    addresses,
                    request_options.representation,
                )
                .context("Invalid --bundle-container request")?,
                &request_options,
            );
            planner
                .plan(request, budget)
                .context("Failed to plan bundle-container extraction")
        }
        None => {
            let request = build_standard_request(command, &request_options)?;
            ExtractionPlanner::new(snapshot)
                .plan(request, budget)
                .context("Failed to plan extraction")
        }
    }
}

fn build_standard_request(
    command: &ExportCommand,
    options: &RequestOptions,
) -> Result<ExtractionRequest> {
    let request = if command.address.is_empty() {
        if command.source.is_empty() {
            ExtractionRequest::all(options.representation)
        } else {
            let sources = command
                .source
                .iter()
                .map(|source| {
                    SourceLocator::path(source).with_context(|| {
                        format!("Invalid --source alias {source:?}; use a portable root alias")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            ExtractionRequest::sources(sources, options.representation)
        }
    } else {
        let addresses = command
            .address
            .iter()
            .map(|address| {
                ObjectAddress::from_str(address)
                    .with_context(|| format!("Invalid --address {address:?}"))
            })
            .collect::<Result<Vec<_>>>()?;
        ExtractionRequest::addresses(addresses, options.representation)
            .context("Invalid explicit object-address selection")?
    };
    Ok(request_with_options(request, options))
}

fn request_with_options(request: ExtractionRequest, options: &RequestOptions) -> ExtractionRequest {
    let request = request.with_filter(options.filter.clone());
    match &options.prefix {
        Some(prefix) => request.with_prefix(prefix.clone()),
        None => request,
    }
}

fn read_manifest(
    path: &std::path::Path,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionManifest> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open --resume manifest {}", path.display()))?;
    ExtractionManifest::read_json(file, budget)
        .with_context(|| format!("Invalid --resume manifest {}", path.display()))
}

fn read_plan(path: &std::path::Path, budget: &mut AssetLoadBudget) -> Result<ExtractionPlan> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open --plan file {}", path.display()))?;
    ExtractionPlan::read_json(file, budget)
        .with_context(|| format!("Invalid --plan file {}", path.display()))
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
    .context("Invalid extraction execution limits")
}

fn parse_representation(value: &str) -> Result<ExtractionRepresentationPolicy> {
    match value {
        "raw" => Ok(ExtractionRepresentationPolicy::RawOnly),
        "prefer-decoded" => Ok(ExtractionRepresentationPolicy::PreferDecoded),
        "require-decoded" => Ok(ExtractionRepresentationPolicy::RequireDecoded),
        _ => anyhow::bail!(
            "Invalid --representation {value:?}; expected raw|prefer-decoded|require-decoded"
        ),
    }
}

fn parse_failure_policy(value: &str) -> Result<ExtractionFailurePolicy> {
    match value {
        "collect-all" => Ok(ExtractionFailurePolicy::CollectAll),
        "stop-in-plan-order" => Ok(ExtractionFailurePolicy::StopInPlanOrder),
        _ => anyhow::bail!("Invalid --failure {value:?}; expected collect-all|stop-in-plan-order"),
    }
}
