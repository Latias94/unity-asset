//! Environment object graph example (YAML + binary).
//!
//! This example builds a best-effort object graph across loaded sources and prints a small summary.

use std::path::PathBuf;

use unity_asset::environment::{Environment, ObjectGraphBuildOptions, ObjectGraphTraversalOptions};

fn main() -> unity_asset::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("crates/unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab")
    });

    let mut env = Environment::new();
    env.load_file(&input)?;

    let graph = env.build_object_graph(ObjectGraphBuildOptions::default());

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

    println!("nodes={}", graph.nodes().len());
    println!("internal_edges={}", internal_edges);
    println!("external_edges={}", external_edges);
    println!("roots_internal={}", graph.roots(false).len());
    println!("leaves_internal={}", graph.leaves(false).len());
    println!("cycles_internal={}", graph.cycles(50, false).len());

    // Demonstrate closure from the first node (if any).
    if let Some(root) = graph.nodes().first() {
        let closure = graph.closure_with_options(
            std::slice::from_ref(root),
            ObjectGraphTraversalOptions {
                max_depth: Some(2),
                max_nodes: Some(10_000),
                follow_resolved_external: false,
            },
        );
        println!("closure_nodes={} (depth=2)", closure.len());
    }

    if args.get(2).map(|s| s.as_str()) == Some("dot") {
        print!("{}", graph.to_dot(50_000, true));
    }

    Ok(())
}
