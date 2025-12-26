//! Environment dependency graph example.
//!
//! This example builds a best-effort dependency graph across all loaded binary sources and prints
//! a small summary (node count, edge counts, and a container-rooted closure).

use std::path::PathBuf;

use unity_asset::environment::{
    DependencyGraphBuildOptions, DependencyGraphTraversalOptions, Environment,
};

fn main() -> unity_asset::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples/char_118_yuki.ab"));
    let pattern = args.get(2).cloned().unwrap_or_else(|| "Assets/".to_string());

    let mut env = Environment::new();
    env.load_file(&input)?;

    let graph = env.build_dependency_graph(DependencyGraphBuildOptions::default());

    println!("nodes={}", graph.nodes().len());
    println!("internal_edges={}", graph.internal_edge_count());
    println!("external_edges={}", graph.external_edge_count());
    println!(
        "external_edges_resolved={}",
        graph.resolved_external_edge_count()
    );
    println!("warnings={}", graph.warnings().len());
    // Rebuilding should reuse cached scans internally.
    let _ = env.build_dependency_graph(DependencyGraphBuildOptions::default());

    let roots: Vec<_> = env.bundle_container_root_keys(&pattern, Some(16));

    if roots.is_empty() {
        println!("container_roots=0 (pattern={})", pattern);
        return Ok(());
    }

    println!("graph_roots_internal={}", graph.roots(false).len());
    println!("graph_leaves_internal={}", graph.leaves(false).len());
    println!("graph_roots_with_external={}", graph.roots(true).len());
    println!("graph_leaves_with_external={}", graph.leaves(true).len());
    println!("graph_cycles_internal={}", graph.cycles(50, false).len());
    println!(
        "graph_cycles_with_external={}",
        graph.cycles(50, true).len()
    );

    let closure = graph.internal_closure(&roots, Some(2), Some(50_000));
    println!(
        "container_roots={} closure_nodes={} (pattern={})",
        roots.len(),
        closure.len(),
        pattern
    );

    let closure_with_external = graph.closure_with_options(
        &roots,
        DependencyGraphTraversalOptions {
            max_depth: Some(2),
            max_nodes: Some(50_000),
            follow_resolved_external: true,
        },
    );
    println!(
        "closure_nodes_with_external={} (depth=2)",
        closure_with_external.len()
    );

    if args.get(3).map(|s| s.as_str()) == Some("dot") {
        print!("{}", graph.to_dot(50_000, true));
    }

    Ok(())
}
