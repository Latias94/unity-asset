use crate::cli::Commands;
use crate::shared::AppContext;
use anyhow::{Context, Result};
use std::io::Write;

mod extract;
mod list_bundle;
mod references;
mod split_yaml;
mod workspace;

pub(crate) fn write_stdout(
    write: impl FnOnce(&mut dyn Write) -> Result<()>,
    flush_context: &'static str,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    write(&mut output)?;
    output.flush().context(flush_context)
}

pub(crate) fn run(command: Commands, ctx: &AppContext) -> Result<()> {
    match command {
        Commands::Workspace(command) => workspace::run(command, ctx),
        Commands::References(command) => references::run(command, ctx),
        Commands::Extract(command) => extract::run(*command, ctx),
        Commands::SplitYaml {
            input,
            output,
            existing_output,
        } => split_yaml::run(input, output, existing_output, ctx),
        Commands::ListBundle {
            input,
            filter,
            verbose,
        } => list_bundle::run(input, filter, verbose, ctx),
    }
}
