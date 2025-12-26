use unity_asset_binary::bundle::{BundleLoadOptions, BundleParser};

#[test]
fn unityfs_bundle_fast_mode_decompresses_on_demand() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/char_118_yuki.ab");
    let bytes = std::fs::read(&path).expect("read sample bundle");

    let bundle = BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::fast())
        .expect("parse bundle");

    assert_eq!(bundle.header.signature, "UnityFS");
    assert_eq!(
        bundle.assets.len(),
        0,
        "fast mode should not preload assets"
    );
    assert_eq!(
        bundle.data().len(),
        0,
        "fast mode should not eagerly decompress blocks"
    );
    assert!(
        bundle.size() > 0,
        "bundle reports expected decompressed size"
    );

    let node = bundle
        .nodes
        .iter()
        .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
        .expect("bundle contains at least one asset node");

    let bytes = bundle
        .extract_node_slice(node)
        .expect("extract triggers on-demand decompression");
    assert_eq!(bytes.len() as u64, node.size);
    assert!(
        !bundle.data().is_empty(),
        "bundle data becomes available after decompression"
    );
}
