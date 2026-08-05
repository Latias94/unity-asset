use std::path::Path;

use anyhow::{Context, Result};
use unity_asset::extraction::{BundleContainerQuery, ExtractionPlanner};
use unity_asset::workspace::{
    AssetWorkspace, MutationPlan, PrepareOptions, PublicationTarget, RecoveryLocator,
    WorkspaceInspector, WorkspaceLookup, workspace_capabilities,
};
use unity_asset::{AssetLoadBudget, ContractJsonLimits, ContractJsonResourceModel, ObjectAddress};

use crate::cli::{
    WorkspaceCommand, WorkspaceInspectSubcommand, WorkspacePlanSubcommand,
    WorkspaceRecoverSubcommand, WorkspaceSubcommand,
};
use crate::cli_error::{
    mark_commit_error, mark_prepare_error, mark_publication_target_error,
    mark_recovery_discovery_error, mark_recovery_error, resolve_lookup,
};
use crate::json_io::{read_small_contract, with_contract_reader, write_canonical, write_json};
use crate::shared::AppContext;
use crate::workspace_loader::{load_full_workspace, load_full_workspace_with_workspace_id};

const OBJECT_ADDRESS_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.object_address",
    1024 * 1024,
    8,
    512,
    512,
    ContractJsonResourceModel::new(6, 4 * 1024, 2 * 1024, 512),
);
const RECOVERY_LOCATOR_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.recovery_locator",
    1024 * 1024,
    8,
    64,
    64,
    ContractJsonResourceModel::new(6, 4 * 1024, 2 * 1024, 512),
);

pub(crate) fn run(command: WorkspaceCommand, context: &AppContext) -> Result<()> {
    match command.command {
        WorkspaceSubcommand::Capabilities => write_json(&workspace_capabilities()),
        WorkspaceSubcommand::Inspect(command) => match command.command {
            WorkspaceInspectSubcommand::Sources { input } => inspect_sources(&input, context),
            WorkspaceInspectSubcommand::Objects { input } => inspect_objects(&input, context),
            WorkspaceInspectSubcommand::Object {
                input,
                address_json,
            } => inspect_object(&input, &address_json, context),
            WorkspaceInspectSubcommand::BundleContainers { input, query_json } => {
                inspect_bundle_containers(&input, &query_json, context)
            }
        },
        WorkspaceSubcommand::Plan(command) => match command.command {
            WorkspacePlanSubcommand::Validate { plan } => validate_plan(&plan),
        },
        WorkspaceSubcommand::Prepare { input, plan } => prepare(&input, &plan, context),
        WorkspaceSubcommand::Preview {
            input,
            plan,
            address_json,
        } => preview(&input, &plan, &address_json, context),
        WorkspaceSubcommand::Commit {
            input,
            plan,
            publication_root,
        } => commit(&input, &plan, &publication_root, context),
        WorkspaceSubcommand::Recover(command) => match command.command {
            WorkspaceRecoverSubcommand::Discover { publication_root } => {
                discover_recoveries(&publication_root)
            }
            WorkspaceRecoverSubcommand::Resume { locator_json } => resume_recovery(&locator_json),
            WorkspaceRecoverSubcommand::Abandon { locator_json } => abandon_recovery(&locator_json),
            WorkspaceRecoverSubcommand::Finalize {
                input,
                locator_json,
            } => finalize_recovery(&input, &locator_json, context),
        },
    }
}

fn inspect_sources(input: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let workspace = load_full_workspace(input, context, &mut budget)?;
    let snapshot = workspace.snapshot();
    let sources = WorkspaceInspector::new(&snapshot)
        .sources(&mut budget)
        .context("Failed to inspect workspace sources")?;
    write_json(&sources)
}

fn inspect_objects(input: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let workspace = load_full_workspace(input, context, &mut budget)?;
    let snapshot = workspace.snapshot();
    let mut objects = WorkspaceInspector::new(&snapshot)
        .objects(&mut budget)
        .context("Failed to inspect workspace objects")?;
    objects.sort_by(|left, right| left.address().cmp(right.address()));
    write_json(&objects)
}

fn inspect_object(input: &Path, address_json: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let address: ObjectAddress =
        read_small_contract(address_json, &mut budget, OBJECT_ADDRESS_JSON_LIMITS)?;
    let workspace = load_full_workspace(input, context, &mut budget)?;
    let snapshot = workspace.snapshot();
    let inspection = WorkspaceInspector::new(&snapshot)
        .object(&address, &mut budget)
        .context("Failed to inspect workspace object")?;
    write_resolved_object(inspection)
}

fn inspect_bundle_containers(input: &Path, query_json: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let query = with_contract_reader(query_json, |reader| {
        BundleContainerQuery::read_json(reader, &mut budget)
            .context("Invalid BundleContainerQuery JSON")
    })?;
    let workspace = load_full_workspace(input, context, &mut budget)?;
    let snapshot = workspace.snapshot();
    let result = ExtractionPlanner::new(&snapshot)
        .bundle_container_occurrences(query, &mut budget)
        .context("Failed to inspect AssetBundle container occurrences")?;
    write_canonical(|output| {
        result
            .write_canonical_json(output)
            .context("Failed to encode BundleContainerResult")
    })
}

fn validate_plan(path: &Path) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let plan = read_plan(path, &mut budget)?;
    write_canonical(|output| {
        plan.write_canonical_json(output)
            .context("Failed to encode canonical MutationPlan")
    })
}

fn prepare(input: &Path, plan_path: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let plan = read_plan(plan_path, &mut budget)?;
    let workspace =
        load_full_workspace_with_workspace_id(input, plan.workspace_id(), context, &mut budget)?;
    let prepared = workspace
        .prepare(plan, PrepareOptions::default(), &mut budget)
        .map_err(mark_prepare_error)?;
    write_json(prepared.report())
}

fn preview(
    input: &Path,
    plan_path: &Path,
    address_json: &Path,
    context: &AppContext,
) -> Result<()> {
    reject_shared_stdin(plan_path, address_json)?;
    let mut budget = AssetLoadBudget::default();
    let plan = read_plan(plan_path, &mut budget)?;
    let address: ObjectAddress =
        read_small_contract(address_json, &mut budget, OBJECT_ADDRESS_JSON_LIMITS)?;
    let workspace =
        load_full_workspace_with_workspace_id(input, plan.workspace_id(), context, &mut budget)?;
    let prepared = workspace
        .prepare(plan, PrepareOptions::default(), &mut budget)
        .map_err(mark_prepare_error)?;
    let view = prepared.view();
    let inspection = WorkspaceInspector::new(&view)
        .object(&address, &mut budget)
        .context("Failed to inspect prepared object")?;
    write_resolved_object(inspection)
}

fn commit(
    input: &Path,
    plan_path: &Path,
    publication_root: &Path,
    context: &AppContext,
) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let plan = read_plan(plan_path, &mut budget)?;
    let mut workspace =
        load_full_workspace_with_workspace_id(input, plan.workspace_id(), context, &mut budget)?;
    let prepared = workspace
        .prepare(plan, PrepareOptions::default(), &mut budget)
        .map_err(mark_prepare_error)?;
    let target =
        PublicationTarget::in_place(publication_root).map_err(mark_publication_target_error)?;
    let report = workspace
        .commit(prepared, target, &mut budget)
        .map_err(mark_commit_error)?;
    write_json(&report)
}

fn discover_recoveries(publication_root: &Path) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let target =
        PublicationTarget::in_place(publication_root).map_err(mark_publication_target_error)?;
    let discovery = target
        .discover_recoveries(&mut budget)
        .map_err(mark_recovery_discovery_error)?;
    write_json(&discovery)
}

fn resume_recovery(locator_json: &Path) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let locator: RecoveryLocator =
        read_small_contract(locator_json, &mut budget, RECOVERY_LOCATOR_JSON_LIMITS)?;
    let outcome = AssetWorkspace::recover_at(&locator, &mut budget).map_err(mark_recovery_error)?;
    write_json(&outcome)
}

fn abandon_recovery(locator_json: &Path) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let locator: RecoveryLocator =
        read_small_contract(locator_json, &mut budget, RECOVERY_LOCATOR_JSON_LIMITS)?;
    let outcome = AssetWorkspace::abandon_at(&locator, &mut budget).map_err(mark_recovery_error)?;
    write_json(&outcome)
}

fn finalize_recovery(input: &Path, locator_json: &Path, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let locator: RecoveryLocator =
        read_small_contract(locator_json, &mut budget, RECOVERY_LOCATOR_JSON_LIMITS)?;
    let detached =
        AssetWorkspace::recover_at(&locator, &mut budget).map_err(mark_recovery_error)?;
    if !detached.requires_workspace_finalization() {
        return write_json(&detached);
    }
    let workspace_id = detached
        .workspace_id()
        .context("Recoverable commit did not report its workspace identity")?;
    let mut workspace =
        load_full_workspace_with_workspace_id(input, workspace_id, context, &mut budget)?;
    let outcome = workspace
        .finalize_recovery_at(&locator, &mut budget)
        .map_err(mark_recovery_error)?;
    write_json(&outcome)
}

fn read_plan(path: &Path, budget: &mut AssetLoadBudget) -> Result<MutationPlan> {
    with_contract_reader(path, |reader| {
        MutationPlan::from_json_reader(reader, budget).context("Invalid MutationPlan JSON")
    })
}

fn write_resolved_object(
    lookup: WorkspaceLookup<unity_asset::workspace::WorkspaceObjectInspection>,
) -> Result<()> {
    write_json(&resolve_lookup(lookup)?)
}

fn reject_shared_stdin(first: &Path, second: &Path) -> Result<()> {
    if first == Path::new("-") && second == Path::new("-") {
        anyhow::bail!("Only one structured input may read from stdin in a command");
    }
    Ok(())
}
