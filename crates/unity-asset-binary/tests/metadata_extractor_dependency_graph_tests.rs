use std::path::PathBuf;

use unity_asset_binary::bundle::BundleParser;
use unity_asset_binary::metadata::MetadataExtractor;
use unity_asset_core::AssetLoadBudget;

fn sample_bundle_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/char_118_yuki.ab")
}

#[test]
fn metadata_extractor_populates_dependency_graph_nodes() {
    let path = sample_bundle_path();
    let bytes = std::fs::read(&path).expect("read sample bundle");
    let mut budget = AssetLoadBudget::default();
    let bundle =
        BundleParser::from_bytes_with_budget(bytes, &mut budget).expect("parse sample bundle");
    let asset = bundle
        .assets
        .first()
        .expect("bundle has at least one asset");

    let extractor = MetadataExtractor::new();
    let result = extractor
        .extract_from_asset(asset, &mut budget)
        .expect("extract metadata");

    assert!(
        !result
            .metadata
            .dependencies
            .dependency_graph
            .nodes
            .is_empty(),
        "dependency graph nodes should be populated (not placeholder)"
    );
    assert_eq!(
        result.metadata.dependencies.dependency_graph.nodes.len(),
        asset.objects().len(),
        "dependency graph nodes should cover analyzed objects"
    );
}
