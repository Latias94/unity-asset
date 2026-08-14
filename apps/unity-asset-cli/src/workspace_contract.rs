use std::path::PathBuf;

use serde::Serialize;
use unity_asset::extraction::{BUNDLE_CONTAINER_QUERY_VERSION, BUNDLE_CONTAINER_RESULT_VERSION};
use unity_asset::workspace::{
    COMMIT_REPORT_VERSION, MUTATION_PLAN_VERSION, PREPARE_REPORT_VERSION,
    RECOVERY_DISCOVERY_VERSION, RECOVERY_LOCATOR_VERSION, RECOVERY_OUTCOME_VERSION,
    WORKSPACE_OBJECT_INSPECTION_VERSION, WORKSPACE_SOURCE_INSPECTION_VERSION,
};

use crate::cli::{
    WorkspaceCommand, WorkspaceInspectSubcommand, WorkspacePlanSubcommand,
    WorkspaceRecoverSubcommand, WorkspaceSubcommand,
};

const WORKSPACE_CLI_CAPABILITIES_CONTRACT: &str = "unity_asset.workspace_cli_capabilities";
const WORKSPACE_CLI_CAPABILITIES_VERSION: u8 = 1;

const WORKSPACE_INPUT: WorkspaceRequirement = WorkspaceRequirement {
    argument: "--input",
    accepted: &[WorkspaceInputKind::File, WorkspaceInputKind::Directory],
};
const PUBLICATION_ROOT: PublicationRootRequirement = PublicationRootRequirement {
    argument: "--publication-root",
    must_exist: true,
    absolute: true,
};
const FILE_OR_STDIN: &[StructuredInputSource] =
    &[StructuredInputSource::File, StructuredInputSource::Stdin];

const NO_STRUCTURED_INPUTS: &[StructuredInputContract] = &[];
const OBJECT_ADDRESS_CONTRACT: StructuredInputContract = StructuredInputContract {
    argument: "--address-json",
    schema: "ObjectAddress",
    wire_version: None,
    sources: FILE_OR_STDIN,
};
const BUNDLE_CONTAINER_QUERY_CONTRACT: StructuredInputContract = StructuredInputContract {
    argument: "--query-json",
    schema: "BundleContainerQuery",
    wire_version: Some(BUNDLE_CONTAINER_QUERY_VERSION),
    sources: FILE_OR_STDIN,
};
const MUTATION_PLAN_CONTRACT: StructuredInputContract = StructuredInputContract {
    argument: "--plan",
    schema: "MutationPlan",
    wire_version: Some(MUTATION_PLAN_VERSION),
    sources: FILE_OR_STDIN,
};
const RECOVERY_LOCATOR_CONTRACT: StructuredInputContract = StructuredInputContract {
    argument: "--locator-json",
    schema: "RecoveryLocator",
    wire_version: Some(RECOVERY_LOCATOR_VERSION),
    sources: FILE_OR_STDIN,
};

const OBJECT_ADDRESS_INPUT: &[StructuredInputContract] = &[OBJECT_ADDRESS_CONTRACT];
const BUNDLE_CONTAINER_QUERY_INPUT: &[StructuredInputContract] = &[BUNDLE_CONTAINER_QUERY_CONTRACT];
const MUTATION_PLAN_INPUT: &[StructuredInputContract] = &[MUTATION_PLAN_CONTRACT];
const PREVIEW_INPUTS: &[StructuredInputContract] =
    &[MUTATION_PLAN_CONTRACT, OBJECT_ADDRESS_CONTRACT];
const RECOVERY_LOCATOR_INPUT: &[StructuredInputContract] = &[RECOVERY_LOCATOR_CONTRACT];

const CAPABILITIES_OUTPUT: OutputContract = OutputContract::object(
    "WorkspaceCliCapabilities",
    Some(WORKSPACE_CLI_CAPABILITIES_VERSION),
);
const SOURCE_INSPECTION_OUTPUT: OutputContract = OutputContract::array(
    "WorkspaceSourceInspection",
    Some(WORKSPACE_SOURCE_INSPECTION_VERSION),
);
const OBJECT_INSPECTION_OUTPUT: OutputContract = OutputContract::object(
    "WorkspaceObjectInspection",
    Some(WORKSPACE_OBJECT_INSPECTION_VERSION),
);
const OBJECT_INSPECTION_ARRAY_OUTPUT: OutputContract = OutputContract::array(
    "WorkspaceObjectInspection",
    Some(WORKSPACE_OBJECT_INSPECTION_VERSION),
);
const BUNDLE_CONTAINER_RESULT_OUTPUT: OutputContract = OutputContract::object(
    "BundleContainerResult",
    Some(BUNDLE_CONTAINER_RESULT_VERSION),
);
const MUTATION_PLAN_OUTPUT: OutputContract =
    OutputContract::object("MutationPlan", Some(MUTATION_PLAN_VERSION));
const PREPARE_REPORT_OUTPUT: OutputContract =
    OutputContract::object("PrepareReport", Some(PREPARE_REPORT_VERSION));
const COMMIT_REPORT_OUTPUT: OutputContract =
    OutputContract::object("CommitReport", Some(COMMIT_REPORT_VERSION));
const RECOVERY_DISCOVERY_OUTPUT: OutputContract =
    OutputContract::object("RecoveryDiscovery", Some(RECOVERY_DISCOVERY_VERSION));
const RECOVERY_OUTCOME_OUTPUT: OutputContract =
    OutputContract::object("RecoveryOutcome", Some(RECOVERY_OUTCOME_VERSION));

const WORKSPACE_OPERATIONS: &[WorkspaceOperationDescriptor] = &[
    WorkspaceOperationDescriptor {
        id: "capabilities",
        command: &["workspace", "capabilities"],
        workspace: None,
        publication_root: None,
        structured_inputs: NO_STRUCTURED_INPUTS,
        stdout: CAPABILITIES_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "inspect_sources",
        command: &["workspace", "inspect", "sources"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: NO_STRUCTURED_INPUTS,
        stdout: SOURCE_INSPECTION_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "inspect_objects",
        command: &["workspace", "inspect", "objects"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: NO_STRUCTURED_INPUTS,
        stdout: OBJECT_INSPECTION_ARRAY_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "inspect_object",
        command: &["workspace", "inspect", "object"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: OBJECT_ADDRESS_INPUT,
        stdout: OBJECT_INSPECTION_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "inspect_bundle_containers",
        command: &["workspace", "inspect", "bundle-containers"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: BUNDLE_CONTAINER_QUERY_INPUT,
        stdout: BUNDLE_CONTAINER_RESULT_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "plan_validate",
        command: &["workspace", "plan", "validate"],
        workspace: None,
        publication_root: None,
        structured_inputs: MUTATION_PLAN_INPUT,
        stdout: MUTATION_PLAN_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "prepare",
        command: &["workspace", "prepare"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: MUTATION_PLAN_INPUT,
        stdout: PREPARE_REPORT_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "preview",
        command: &["workspace", "preview"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: PREVIEW_INPUTS,
        stdout: OBJECT_INSPECTION_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "commit",
        command: &["workspace", "commit"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: Some(PUBLICATION_ROOT),
        structured_inputs: MUTATION_PLAN_INPUT,
        stdout: COMMIT_REPORT_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "recover_discover",
        command: &["workspace", "recover", "discover"],
        workspace: None,
        publication_root: Some(PUBLICATION_ROOT),
        structured_inputs: NO_STRUCTURED_INPUTS,
        stdout: RECOVERY_DISCOVERY_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "recover_resume",
        command: &["workspace", "recover", "resume"],
        workspace: None,
        publication_root: None,
        structured_inputs: RECOVERY_LOCATOR_INPUT,
        stdout: RECOVERY_OUTCOME_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "recover_abandon",
        command: &["workspace", "recover", "abandon"],
        workspace: None,
        publication_root: None,
        structured_inputs: RECOVERY_LOCATOR_INPUT,
        stdout: RECOVERY_OUTCOME_OUTPUT,
    },
    WorkspaceOperationDescriptor {
        id: "recover_finalize",
        command: &["workspace", "recover", "finalize"],
        workspace: Some(WORKSPACE_INPUT),
        publication_root: None,
        structured_inputs: RECOVERY_LOCATOR_INPUT,
        stdout: RECOVERY_OUTCOME_OUTPUT,
    },
];

#[derive(Serialize)]
pub(crate) struct WorkspaceCliCapabilities {
    contract: &'static str,
    version: u8,
    stdin_structured_inputs_max: u8,
    operations: &'static [WorkspaceOperationDescriptor],
}

pub(crate) const fn workspace_cli_capabilities() -> WorkspaceCliCapabilities {
    WorkspaceCliCapabilities {
        contract: WORKSPACE_CLI_CAPABILITIES_CONTRACT,
        version: WORKSPACE_CLI_CAPABILITIES_VERSION,
        stdin_structured_inputs_max: 1,
        operations: WORKSPACE_OPERATIONS,
    }
}

#[derive(Debug)]
pub(crate) enum WorkspaceOperation {
    Capabilities,
    InspectSources {
        input: PathBuf,
    },
    InspectObjects {
        input: PathBuf,
    },
    InspectObject {
        input: PathBuf,
        address_json: PathBuf,
    },
    InspectBundleContainers {
        input: PathBuf,
        query_json: PathBuf,
    },
    PlanValidate {
        plan: PathBuf,
    },
    Prepare {
        input: PathBuf,
        plan: PathBuf,
    },
    Preview {
        input: PathBuf,
        plan: PathBuf,
        address_json: PathBuf,
    },
    Commit {
        input: PathBuf,
        plan: PathBuf,
        publication_root: PathBuf,
    },
    RecoverDiscover {
        publication_root: PathBuf,
    },
    RecoverResume {
        locator_json: PathBuf,
    },
    RecoverAbandon {
        locator_json: PathBuf,
    },
    RecoverFinalize {
        input: PathBuf,
        locator_json: PathBuf,
    },
}

impl From<WorkspaceCommand> for WorkspaceOperation {
    fn from(command: WorkspaceCommand) -> Self {
        match command.command {
            WorkspaceSubcommand::Capabilities => Self::Capabilities,
            WorkspaceSubcommand::Inspect(command) => match command.command {
                WorkspaceInspectSubcommand::Sources { input } => Self::InspectSources { input },
                WorkspaceInspectSubcommand::Objects { input } => Self::InspectObjects { input },
                WorkspaceInspectSubcommand::Object {
                    input,
                    address_json,
                } => Self::InspectObject {
                    input,
                    address_json,
                },
                WorkspaceInspectSubcommand::BundleContainers { input, query_json } => {
                    Self::InspectBundleContainers { input, query_json }
                }
            },
            WorkspaceSubcommand::Plan(command) => match command.command {
                WorkspacePlanSubcommand::Validate { plan } => Self::PlanValidate { plan },
            },
            WorkspaceSubcommand::Prepare { input, plan } => Self::Prepare { input, plan },
            WorkspaceSubcommand::Preview {
                input,
                plan,
                address_json,
            } => Self::Preview {
                input,
                plan,
                address_json,
            },
            WorkspaceSubcommand::Commit {
                input,
                plan,
                publication_root,
            } => Self::Commit {
                input,
                plan,
                publication_root,
            },
            WorkspaceSubcommand::Recover(command) => match command.command {
                WorkspaceRecoverSubcommand::Discover { publication_root } => {
                    Self::RecoverDiscover { publication_root }
                }
                WorkspaceRecoverSubcommand::Resume { locator_json } => {
                    Self::RecoverResume { locator_json }
                }
                WorkspaceRecoverSubcommand::Abandon { locator_json } => {
                    Self::RecoverAbandon { locator_json }
                }
                WorkspaceRecoverSubcommand::Finalize {
                    input,
                    locator_json,
                } => Self::RecoverFinalize {
                    input,
                    locator_json,
                },
            },
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct WorkspaceOperationDescriptor {
    id: &'static str,
    command: &'static [&'static str],
    workspace: Option<WorkspaceRequirement>,
    publication_root: Option<PublicationRootRequirement>,
    structured_inputs: &'static [StructuredInputContract],
    stdout: OutputContract,
}

#[derive(Clone, Copy, Serialize)]
struct WorkspaceRequirement {
    argument: &'static str,
    accepted: &'static [WorkspaceInputKind],
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceInputKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Serialize)]
struct PublicationRootRequirement {
    argument: &'static str,
    must_exist: bool,
    absolute: bool,
}

#[derive(Clone, Copy, Serialize)]
struct StructuredInputContract {
    argument: &'static str,
    schema: &'static str,
    wire_version: Option<u8>,
    sources: &'static [StructuredInputSource],
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum StructuredInputSource {
    File,
    Stdin,
}

#[derive(Clone, Copy, Serialize)]
struct OutputContract {
    schema: &'static str,
    wire_version: Option<u8>,
    shape: OutputShape,
}

impl OutputContract {
    const fn object(schema: &'static str, wire_version: Option<u8>) -> Self {
        Self {
            schema,
            wire_version,
            shape: OutputShape::Object,
        }
    }

    const fn array(schema: &'static str, wire_version: Option<u8>) -> Self {
        Self {
            schema,
            wire_version,
            shape: OutputShape::Array,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputShape {
    Object,
    Array,
}

#[cfg(test)]
mod tests {
    use clap::{Command, CommandFactory};

    use super::WORKSPACE_OPERATIONS;
    use crate::cli::Cli;

    #[test]
    fn capability_inventory_matches_every_workspace_leaf_command() {
        let cli = Cli::command();
        let workspace = cli
            .get_subcommands()
            .find(|command| command.get_name() == "workspace")
            .expect("workspace command must exist");
        let mut routed = Vec::new();
        collect_leaf_paths(workspace, &mut vec!["workspace".to_owned()], &mut routed);

        let advertised = WORKSPACE_OPERATIONS
            .iter()
            .map(|operation| {
                operation
                    .command
                    .iter()
                    .map(|segment| (*segment).to_owned())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(advertised, routed);
    }

    fn collect_leaf_paths(
        command: &Command,
        path: &mut Vec<String>,
        output: &mut Vec<Vec<String>>,
    ) {
        let subcommands = command.get_subcommands().collect::<Vec<_>>();
        if subcommands.is_empty() {
            output.push(path.clone());
            return;
        }

        for subcommand in subcommands {
            path.push(subcommand.get_name().to_owned());
            collect_leaf_paths(subcommand, path, output);
            path.pop();
        }
    }
}
