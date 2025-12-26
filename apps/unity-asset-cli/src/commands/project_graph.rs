use crate::shared::{AppContext, build_environment};
use anyhow::Result;
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use unity_asset::environment::{ExternalObjectEdge, ObjectGraphBuildOptions, ProjectLoadOptions};

#[derive(Debug, Serialize)]
struct JsonlEdge<'a> {
    kind: &'a str,
    from: String,
    to: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProjectGraphJson {
    scan: ProjectGraphJsonScan,
    graph: ProjectGraphJsonGraph,
}

#[derive(Debug, Serialize)]
struct ProjectGraphJsonScan {
    files_visited: usize,
    files_loaded: usize,
    yaml_loaded: usize,
    binary_loaded: usize,
    meta_files_seen: usize,
    meta_guids_indexed: usize,
}

#[derive(Debug, Serialize)]
struct ProjectGraphJsonGraph {
    nodes: usize,
    internal_edges: usize,
    external_edges: usize,
    external_edges_resolved: usize,
    roots_internal: usize,
    leaves_internal: usize,
    cycles_internal: usize,
    roots_with_external: usize,
    leaves_with_external: usize,
    cycles_with_external: usize,
}

fn open_output(output: Option<&PathBuf>) -> Result<Box<dyn Write>> {
    if let Some(path) = output {
        return Ok(Box::new(File::create(path)?));
    }
    Ok(Box::new(std::io::stdout()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    input: PathBuf,
    output: Option<PathBuf>,
    yaml: bool,
    format: String,
    max_files: Option<usize>,
    max_edges: usize,
    follow_external: bool,
    no_ignore: bool,
    follow_symlinks: bool,
    ctx: &AppContext,
) -> Result<()> {
    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;

    let mut options = if yaml {
        ProjectLoadOptions::everything()
    } else {
        ProjectLoadOptions::binaries_only()
    };
    options.max_files = max_files;
    options.respect_ignores = !no_ignore;
    options.follow_symlinks = follow_symlinks;

    let stats = env.load_project(&input, options)?;

    let graph = env.build_object_graph(ObjectGraphBuildOptions {
        include_yaml: yaml,
        include_binary: true,
        ..ObjectGraphBuildOptions::default()
    });

    let internal_edges: usize = graph
        .nodes()
        .iter()
        .map(|n| graph.internal_refs_from(n).len())
        .sum();
    let external_edges: usize = graph
        .nodes()
        .iter()
        .map(|n| graph.external_refs_from(n).len())
        .sum();
    let resolved_external_edges: usize = graph
        .nodes()
        .iter()
        .flat_map(|n| graph.external_refs_from(n))
        .filter(|e| match e {
            ExternalObjectEdge::Binary(b) => b.resolved.is_some(),
            ExternalObjectEdge::Yaml(y) => y.resolved.is_some(),
        })
        .count();

    let roots_with_external = graph.roots(follow_external).len();
    let leaves_with_external = graph.leaves(follow_external).len();
    let cycles_with_external = graph.cycles(50, follow_external).len();

    let fmt = format.to_ascii_lowercase();
    let mut out = open_output(output.as_ref())?;
    match fmt.as_str() {
        "summary" => {
            writeln!(
                out,
                "scan: visited={} loaded={} yaml_loaded={} binary_loaded={} meta_seen={} meta_indexed={}",
                stats.files_visited,
                stats.files_loaded,
                stats.yaml_loaded,
                stats.binary_loaded,
                stats.meta_files_seen,
                stats.meta_guids_indexed
            )?;
            writeln!(out, "nodes={}", graph.nodes().len())?;
            writeln!(out, "internal_edges={}", internal_edges)?;
            writeln!(out, "external_edges={}", external_edges)?;
            writeln!(out, "external_edges_resolved={}", resolved_external_edges)?;
            writeln!(out, "roots_internal={}", graph.roots(false).len())?;
            writeln!(out, "leaves_internal={}", graph.leaves(false).len())?;
            writeln!(out, "cycles_internal={}", graph.cycles(50, false).len())?;
            writeln!(out, "roots_with_external={}", roots_with_external)?;
            writeln!(out, "leaves_with_external={}", leaves_with_external)?;
            writeln!(out, "cycles_with_external={}", cycles_with_external)?;
        }
        "json" => {
            let scan = ProjectGraphJsonScan {
                files_visited: stats.files_visited,
                files_loaded: stats.files_loaded,
                yaml_loaded: stats.yaml_loaded,
                binary_loaded: stats.binary_loaded,
                meta_files_seen: stats.meta_files_seen,
                meta_guids_indexed: stats.meta_guids_indexed,
            };
            let payload = ProjectGraphJson {
                scan,
                graph: ProjectGraphJsonGraph {
                    nodes: graph.nodes().len(),
                    internal_edges,
                    external_edges,
                    external_edges_resolved: resolved_external_edges,
                    roots_internal: graph.roots(false).len(),
                    leaves_internal: graph.leaves(false).len(),
                    cycles_internal: graph.cycles(50, false).len(),
                    roots_with_external,
                    leaves_with_external,
                    cycles_with_external,
                },
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&payload)?)?;
        }
        "dot" => {
            write!(out, "{}", graph.to_dot(max_edges, follow_external))?;
        }
        "jsonl" => {
            let mut emitted = 0usize;

            for from in graph.nodes() {
                for to in graph.internal_refs_from(from) {
                    if emitted >= max_edges {
                        break;
                    }
                    let edge = JsonlEdge {
                        kind: "internal",
                        from: from.to_string(),
                        to: Some(to.to_string()),
                    };
                    writeln!(out, "{}", serde_json::to_string(&edge)?)?;
                    emitted += 1;
                }
                if emitted >= max_edges {
                    break;
                }

                for ext in graph.external_refs_from(from) {
                    if emitted >= max_edges {
                        break;
                    }
                    let resolved = match ext {
                        ExternalObjectEdge::Binary(b) => b.resolved.as_ref().map(|k| k.to_string()),
                        ExternalObjectEdge::Yaml(y) => y.resolved.as_ref().map(|k| k.to_string()),
                    };
                    if follow_external && resolved.is_some() {
                        let edge = JsonlEdge {
                            kind: "external_resolved",
                            from: from.to_string(),
                            to: resolved,
                        };
                        writeln!(out, "{}", serde_json::to_string(&edge)?)?;
                        emitted += 1;
                    } else if !follow_external {
                        let edge = JsonlEdge {
                            kind: "external",
                            from: from.to_string(),
                            to: resolved,
                        };
                        writeln!(out, "{}", serde_json::to_string(&edge)?)?;
                        emitted += 1;
                    }
                }
                if emitted >= max_edges {
                    break;
                }
            }

            if emitted >= max_edges {
                eprintln!("... (truncated: max_edges={})", max_edges);
            }
        }
        other => anyhow::bail!(
            "Invalid --format: {} (expected summary|dot|jsonl|json)",
            other
        ),
    }

    Ok(())
}
