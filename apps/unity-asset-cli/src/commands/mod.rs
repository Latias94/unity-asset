use crate::cli::Commands;
use crate::shared::AppContext;
use anyhow::{Context, Result};
use std::io::Write;
use unity_asset::extraction::ExistingOutputPolicy;

mod deps;
mod dump_typetree_registry;
mod export;
mod find_object;
mod inspect_object;
mod list_bundle;
mod list_objects;
mod parse_yaml;
mod project_graph;
mod scan_pptr;
mod split_yaml;
mod stats;
mod stats_pathid;

pub(crate) fn parse_existing_output(value: &str) -> Result<ExistingOutputPolicy> {
    match value {
        "error" => Ok(ExistingOutputPolicy::Error),
        "skip" => Ok(ExistingOutputPolicy::Skip),
        "replace" => Ok(ExistingOutputPolicy::Replace),
        _ => anyhow::bail!("Invalid --existing-output {value:?}; expected error|skip|replace"),
    }
}

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
        Commands::ParseYaml {
            input,
            format,
            preserve_types,
        } => parse_yaml::run(input, format, preserve_types, ctx),
        Commands::Export(command) => export::run(*command, ctx),
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
        Commands::ListObjects {
            input,
            kind,
            source,
            asset_index,
            class_id,
            class_name,
            name,
            limit,
            json,
        } => list_objects::run(
            input,
            kind,
            source,
            asset_index,
            class_id,
            class_name,
            name,
            limit,
            json,
            ctx,
        ),
        Commands::Stats {
            input,
            kind,
            limit,
            summary,
            json,
        } => stats::run(&input, kind.as_str(), &limit, &summary, &json, ctx),
        Commands::StatsPathId {
            input,
            kind,
            limit,
            check_duplicates,
            json,
        } => stats_pathid::run(input, kind, limit, check_duplicates, json, ctx),
        Commands::FindObject {
            input,
            pattern,
            name,
            class_id,
            class_name,
            limit,
            include_unresolved,
            verbose,
        } => find_object::run(
            input,
            pattern,
            name,
            class_id,
            class_name,
            limit,
            include_unresolved,
            verbose,
            ctx,
        ),
        Commands::InspectObject {
            input,
            address,
            source,
            kind,
            asset_index,
            path_id,
            max_depth,
            max_items,
            max_array,
            filter,
        } => inspect_object::run(
            input,
            address,
            source,
            kind,
            asset_index,
            path_id,
            max_depth,
            max_items,
            max_array,
            filter,
            ctx,
        ),
        Commands::DumpTypeTreeRegistry {
            input,
            output,
            class_id,
            version_prefix,
            overwrite,
        } => dump_typetree_registry::run(input, output, class_id, version_prefix, overwrite, ctx),
        Commands::ScanPPtr {
            input,
            kind,
            source,
            asset_index,
            class_id,
            name,
            limit,
            include_no_typetree,
            json,
        } => scan_pptr::run(
            input,
            kind,
            source,
            asset_index,
            class_id,
            name,
            limit,
            include_no_typetree,
            json,
            ctx,
        ),
        Commands::Deps {
            input,
            format,
            max_edges,
        } => deps::run(input, format, max_edges, ctx),
        Commands::ProjectGraph {
            input,
            output,
            yaml,
            format,
            max_files,
            max_edges,
        } => project_graph::run(input, output, yaml, format, max_files, max_edges, ctx),
    }
}
