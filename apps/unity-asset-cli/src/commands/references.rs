use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use unity_asset::AssetLoadBudget;
use unity_asset::reference::{
    ReferenceGraphBuildOptions, ReferenceProjectionFormat, ReferenceProjectionOptions,
};

use crate::cli::{ReferencesCommand, ReferencesSubcommand};
use crate::shared::AppContext;
use crate::workspace_loader::{DiscoveryPolicy, load_workspace};

pub(crate) fn run(command: ReferencesCommand, context: &AppContext) -> Result<()> {
    match command.command {
        ReferencesSubcommand::Graph {
            input,
            unity_project,
            max_facts,
        } => graph(input, unity_project, max_facts, context),
    }
}

fn graph(input: PathBuf, unity_project: bool, max_facts: u64, context: &AppContext) -> Result<()> {
    let mut budget = AssetLoadBudget::default();
    let policy = if unity_project {
        DiscoveryPolicy::UnityProject
    } else {
        DiscoveryPolicy::Generic
    };
    let workspace = load_workspace(&input, None, policy, None, context, &mut budget)?;
    let snapshot = workspace.snapshot();
    let graph = snapshot
        .reference_graph(ReferenceGraphBuildOptions::unbounded(), &mut budget)
        .context("Failed to build the revision-bound reference graph")?;
    let options = ReferenceProjectionOptions::new(ReferenceProjectionFormat::JsonV2)
        .with_max_facts(max_facts);
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    graph
        .write_projection(&mut output, options, &mut budget)
        .context("Failed to write the reference projection")?;
    output
        .flush()
        .context("Failed to flush the reference projection")
}
