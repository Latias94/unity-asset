use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use unity_asset::extraction::{ExistingOutputPolicy, ExtractionFailurePolicy};

#[derive(Parser)]
#[command(name = "unity_asset")]
#[command(about = "Typed Unity asset workspace and extraction tools")]
#[command(version)]
pub(crate) struct Cli {
    /// Fail-fast TypeTree parsing.
    #[arg(long)]
    pub(crate) strict: bool,

    /// Emit non-fatal load warnings without corrupting structured failures.
    #[arg(long)]
    pub(crate) show_warnings: bool,

    /// External TypeTree registry JSON/TPK. Earlier registries take precedence.
    #[arg(long)]
    pub(crate) typetree_registry: Vec<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Inspect and mutate one revision-bound Asset Workspace.
    Workspace(WorkspaceCommand),

    /// Query revision-bound reference facts through structured projections.
    References(ReferencesCommand),

    /// Export one deterministic, revision-bound artifact set.
    Export(Box<ExportCommand>),

    /// Split Unity YAML documents through the safe artifact publisher.
    #[command(name = "split-yaml")]
    SplitYaml {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value_t = ExistingOutputArg::Error)]
        existing_output: ExistingOutputArg,
    },

    /// List physical AssetBundle nodes through the low-level binary adapter.
    #[command(name = "list-bundle")]
    ListBundle {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(long, default_value = "")]
        filter: String,
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ExistingOutputArg {
    Error,
    Skip,
    Replace,
}

impl ExistingOutputArg {
    pub(crate) const fn into_policy(self) -> ExistingOutputPolicy {
        match self {
            Self::Error => ExistingOutputPolicy::Error,
            Self::Skip => ExistingOutputPolicy::Skip,
            Self::Replace => ExistingOutputPolicy::Replace,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ExtractionFailureArg {
    CollectAll,
    StopInPlanOrder,
}

impl ExtractionFailureArg {
    pub(crate) const fn into_policy(self) -> ExtractionFailurePolicy {
        match self {
            Self::CollectAll => ExtractionFailurePolicy::CollectAll,
            Self::StopInPlanOrder => ExtractionFailurePolicy::StopInPlanOrder,
        }
    }
}

#[derive(Args)]
pub(crate) struct WorkspaceCommand {
    #[command(subcommand)]
    pub(crate) command: WorkspaceSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceSubcommand {
    /// Print the stable machine-readable capability catalog.
    Capabilities,

    /// Inspect immutable workspace state.
    Inspect(WorkspaceInspectCommand),

    /// Validate persisted mutation plans.
    Plan(WorkspacePlanCommand),

    /// Build and prove a zero-write candidate revision.
    Prepare {
        #[arg(short, long)]
        input: PathBuf,
        /// MutationPlan JSON file, or `-` for stdin.
        #[arg(long)]
        plan: PathBuf,
    },

    /// Inspect one object through the prepared read-your-writes view.
    Preview {
        #[arg(short, long)]
        input: PathBuf,
        /// MutationPlan JSON file, or `-` for stdin.
        #[arg(long)]
        plan: PathBuf,
        /// ObjectAddress JSON file, or `-` for stdin.
        #[arg(long)]
        address_json: PathBuf,
    },

    /// Re-prepare and durably publish one exact mutation plan.
    Commit {
        #[arg(short, long)]
        input: PathBuf,
        /// MutationPlan JSON file, or `-` for stdin.
        #[arg(long)]
        plan: PathBuf,
        /// Existing absolute containment root for publication and recovery.
        #[arg(long)]
        publication_root: PathBuf,
    },

    /// Discover, resume, abandon, or finalize durable recovery evidence.
    Recover(WorkspaceRecoverCommand),
}

#[derive(Args)]
pub(crate) struct WorkspaceInspectCommand {
    #[command(subcommand)]
    pub(crate) command: WorkspaceInspectSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceInspectSubcommand {
    /// Inspect all loaded sources without reparsing or reopening them.
    Sources {
        #[arg(short, long)]
        input: PathBuf,
    },

    /// Inspect all loaded objects in deterministic address order.
    Objects {
        #[arg(short, long)]
        input: PathBuf,
    },

    /// Inspect one object by a structured ObjectAddress.
    Object {
        #[arg(short, long)]
        input: PathBuf,
        /// ObjectAddress JSON file, or `-` for stdin.
        #[arg(long)]
        address_json: PathBuf,
    },

    /// Inspect every matching AssetBundle m_Container occurrence.
    #[command(name = "bundle-containers")]
    BundleContainers {
        #[arg(short, long)]
        input: PathBuf,
        /// BundleContainerQuery JSON file, or `-` for stdin.
        #[arg(long)]
        query_json: PathBuf,
    },
}

#[derive(Args)]
pub(crate) struct WorkspacePlanCommand {
    #[command(subcommand)]
    pub(crate) command: WorkspacePlanSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum WorkspacePlanSubcommand {
    /// Parse, validate, and emit the current canonical MutationPlan JSON.
    Validate {
        /// MutationPlan JSON file, or `-` for stdin.
        #[arg(long)]
        plan: PathBuf,
    },
}

#[derive(Args)]
pub(crate) struct WorkspaceRecoverCommand {
    #[command(subcommand)]
    pub(crate) command: WorkspaceRecoverSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum WorkspaceRecoverSubcommand {
    /// List canonical recovery candidates below one publication root.
    Discover {
        #[arg(long)]
        publication_root: PathBuf,
    },

    /// Resume filesystem publication without attaching a workspace baseline.
    Resume {
        /// RecoveryLocator JSON file, or `-` for stdin.
        #[arg(long)]
        locator_json: PathBuf,
    },

    /// Roll back an unfinished publication when the journal proves it is safe.
    Abandon {
        /// RecoveryLocator JSON file, or `-` for stdin.
        #[arg(long)]
        locator_json: PathBuf,
    },

    /// Reopen trusted sources and attach a recovered publication to the workspace.
    Finalize {
        #[arg(short, long)]
        input: PathBuf,
        /// RecoveryLocator JSON file, or `-` for stdin.
        #[arg(long)]
        locator_json: PathBuf,
    },
}

#[derive(Args)]
pub(crate) struct ReferencesCommand {
    #[command(subcommand)]
    pub(crate) command: ReferencesSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum ReferencesSubcommand {
    /// Emit one versioned JSON reference-graph projection.
    Graph {
        #[arg(short, long)]
        input: PathBuf,
        /// Apply Unity-project root filtering for generated directories.
        #[arg(long)]
        unity_project: bool,
        /// Maximum facts included in the projection.
        #[arg(long, default_value_t = 200_000)]
        max_facts: u64,
    },
}

#[derive(Args)]
pub(crate) struct ExportCommand {
    #[arg(short, long)]
    pub(crate) input: PathBuf,

    #[arg(short, long)]
    pub(crate) output: PathBuf,

    /// Execute a canonical ExtractionPlan JSON file, or `-` for stdin.
    #[arg(
        long,
        conflicts_with_all = ["request", "dry_run"],
        required_unless_present = "request"
    )]
    pub(crate) plan: Option<PathBuf>,

    /// Build a plan from a canonical ExtractionRequest JSON file, or `-` for stdin.
    #[arg(long, conflicts_with = "plan", required_unless_present = "plan")]
    pub(crate) request: Option<PathBuf>,

    /// Print the canonical ExtractionPlan without writing artifacts.
    #[arg(long, conflicts_with_all = ["resume", "plan", "manifest"])]
    pub(crate) dry_run: bool,

    /// Read a canonical extraction manifest and verify resumable artifacts.
    #[arg(long)]
    pub(crate) resume: Option<PathBuf>,

    /// Publish the canonical manifest at this relative path under `--output`.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) manifest: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = ExistingOutputArg::Error)]
    pub(crate) existing_output: ExistingOutputArg,

    #[arg(long, value_enum, default_value_t = ExtractionFailureArg::CollectAll)]
    pub(crate) failure: ExtractionFailureArg,

    #[arg(long)]
    pub(crate) workers: Option<usize>,

    #[arg(long)]
    pub(crate) max_in_flight_bytes: Option<u64>,

    /// Cap simultaneous open files; the safe publication minimum is 5.
    #[arg(long)]
    pub(crate) max_open_files: Option<usize>,

    /// Cap total bytes published by this execution.
    #[arg(long)]
    pub(crate) max_output_bytes: Option<u64>,

    /// Cap cumulative final-path bytes read to verify persisted evidence.
    #[arg(long)]
    pub(crate) max_evidence_verification_bytes: Option<u64>,

    #[arg(long)]
    pub(crate) max_report_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands, WorkspaceInspectSubcommand, WorkspaceSubcommand};

    #[test]
    fn workspace_inspection_uses_nested_typed_commands() {
        let cli = Cli::try_parse_from([
            "unity-asset",
            "workspace",
            "inspect",
            "object",
            "--input",
            "game.ab",
            "--address-json",
            "address.json",
        ])
        .unwrap();
        let Commands::Workspace(command) = cli.command else {
            panic!("workspace command expected");
        };
        let WorkspaceSubcommand::Inspect(command) = command.command else {
            panic!("workspace inspect command expected");
        };
        assert!(matches!(
            command.command,
            WorkspaceInspectSubcommand::Object { .. }
        ));
    }

    #[test]
    fn extraction_requires_exactly_one_typed_contract() {
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--request",
                "request.json",
                "--plan",
                "plan.json",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--request",
                "request.json",
                "--dry-run",
            ])
            .is_ok()
        );
    }

    #[test]
    fn superseded_commands_are_absent() {
        for command in [
            "extract",
            "find-object",
            "inspect-object",
            "list-objects",
            "scan-pptr",
            "stats",
            "stats-pathid",
            "dump-typetree-registry",
            "deps",
            "project-graph",
            "parse-yaml",
        ] {
            assert!(Cli::try_parse_from(["unity-asset", command]).is_err());
        }
    }
}
