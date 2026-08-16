//! Revision-bound workspace reference graph example.
//!
//! Run:
//! `cargo run -p unity-asset --example workspace_reference_graph -- <path> [json|jsonl|dot]`
//!
//! The input is the only source loaded by this example. Reference resolution never probes or
//! loads additional files. A deterministic projection is written to stdout; coverage and query
//! summaries are written to stderr.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::reference::{
    ReferenceDirection, ReferenceGraphBuildOptions, ReferenceProjectionFormat,
    ReferenceProjectionOptions, ReferenceTraversalLimits,
};
use unity_asset::workspace::AssetWorkspace;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (input, projection_format) = arguments()?;
    let mut budget = AssetLoadBudget::default();

    let mut workspace = AssetWorkspace::new()?;
    workspace.load_path(&input, &mut budget)?;

    let snapshot = workspace.snapshot();
    let graph = snapshot.reference_graph(ReferenceGraphBuildOptions::unbounded(), &mut budget)?;
    let coverage = graph.coverage();

    eprintln!(
        "workspace={} revision={} complete={}",
        graph.workspace_id(),
        graph.revision(),
        graph.is_complete()
    );
    eprintln!(
        "coverage sources={}/{} nodes={}/{} facts={} diagnostics={} truncations={}",
        coverage.scanned_sources(),
        coverage.total_sources(),
        coverage.indexed_nodes(),
        coverage.total_nodes(),
        coverage.fact_count(),
        graph.diagnostics().len(),
        coverage.truncations().len()
    );
    eprintln!(
        "build graph_cache_hit={} source_occurrence_cache_hits={}",
        graph.build_stats().graph_cache_hit(),
        graph.build_stats().source_occurrence_cache_hits()
    );
    eprintln!(
        "topology nodes={} roots={} leaves={}",
        graph.nodes().len(),
        graph.roots().count(),
        graph.leaves().count()
    );

    if let Some(object) = graph.nodes().first() {
        let outgoing = graph.outgoing(object)?.len();
        let incoming = graph.incoming(object)?.len();
        let closure = graph.closure(
            std::slice::from_ref(object),
            ReferenceDirection::Outgoing,
            ReferenceTraversalLimits::unbounded(),
            &mut budget,
        )?;
        eprintln!(
            "sample object={:?} outgoing={} incoming={} closure_nodes={} closure_complete={}",
            object.object(),
            outgoing,
            incoming,
            closure.len(),
            closure.is_complete()
        );
    } else {
        eprintln!("sample object=none outgoing=0 incoming=0 closure_nodes=0");
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let report = graph.write_projection(
        &mut output,
        ReferenceProjectionOptions::new(projection_format),
        &mut budget,
    )?;
    output.flush()?;
    drop(output);

    let counts = report.resolution_counts();
    eprintln!(
        "resolution null={} resolved={} unloaded={} missing={} ambiguous={} invalid={}",
        counts.null(),
        counts.resolved(),
        counts.unloaded(),
        counts.missing(),
        counts.ambiguous(),
        counts.invalid()
    );
    eprintln!(
        "projection format={} nodes={} facts={} resolved_edges={} bytes={} complete={}",
        projection_format,
        report.nodes_written(),
        report.facts_written(),
        report.resolved_edges_written(),
        report.bytes_written(),
        report.is_complete()
    );

    Ok(())
}

fn arguments() -> io::Result<(PathBuf, ReferenceProjectionFormat)> {
    let mut arguments = std::env::args_os().skip(1);
    let input = arguments.next().map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("crates/unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab")
    });
    let format = arguments
        .next()
        .as_deref()
        .map(parse_projection_format)
        .transpose()?
        .unwrap_or(ReferenceProjectionFormat::JsonLinesV2);
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: workspace_reference_graph [path] [json|jsonl|dot]",
        ));
    }
    Ok((input, format))
}

fn parse_projection_format(value: &OsStr) -> io::Result<ReferenceProjectionFormat> {
    match value.to_str() {
        Some("json") => Ok(ReferenceProjectionFormat::JsonV2),
        Some("jsonl") => Ok(ReferenceProjectionFormat::JsonLinesV2),
        Some("dot") => Ok(ReferenceProjectionFormat::DotV2),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection format must be json, jsonl, or dot",
        )),
    }
}
