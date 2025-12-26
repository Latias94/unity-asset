//! Environment project object graph example.
//!
//! This example scans a Unity project root (fast sniff + `.meta` GUID indexing), then builds a
//! best-effort object graph. By default, it loads binaries only (bundles/serialized/webfiles).

use std::path::PathBuf;

use unity_asset::environment::{
    Environment, ObjectGraphBuildOptions, ObjectGraphTraversalOptions, ProjectLoadOptions,
};

fn main() -> unity_asset::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("repo-ref/BoatAttack"));

    let load_yaml = args.get(2).map(|s| s.as_str()) == Some("yaml");
    let dot = args.get(3).map(|s| s.as_str()) == Some("dot")
        || (args.get(2).map(|s| s.as_str()) == Some("dot"));

    let options = if load_yaml {
        ProjectLoadOptions::everything()
    } else {
        ProjectLoadOptions::binaries_only()
    };

    let mut env = Environment::new();
    let stats = env.load_project(&root, options)?;

    eprintln!(
        "load_project: visited={} loaded={} yaml_loaded={} binary_loaded={} meta_seen={} meta_indexed={}",
        stats.files_visited,
        stats.files_loaded,
        stats.yaml_loaded,
        stats.binary_loaded,
        stats.meta_files_seen,
        stats.meta_guids_indexed
    );

    let graph = env.build_object_graph(ObjectGraphBuildOptions {
        include_yaml: load_yaml,
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

    println!("nodes={}", graph.nodes().len());
    println!("internal_edges={}", internal_edges);
    println!("external_edges={}", external_edges);
    println!("roots_internal={}", graph.roots(false).len());
    println!("leaves_internal={}", graph.leaves(false).len());
    println!("cycles_internal={}", graph.cycles(50, false).len());

    if let Some(root) = graph.nodes().first() {
        let closure = graph.closure_with_options(
            std::slice::from_ref(root),
            ObjectGraphTraversalOptions {
                max_depth: Some(2),
                max_nodes: Some(10_000),
                follow_resolved_external: true,
            },
        );
        println!("closure_nodes={} (depth=2, follow_external)", closure.len());
    }

    if dot {
        print!("{}", graph.to_dot(200_000, true));
    }

    Ok(())
}
