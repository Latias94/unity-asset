use unity_asset_binary::bundle::{BundleLoadOptions, BundleParser};
use unity_asset_binary::error::BinaryError;
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

fn sample_bundle_bytes() -> Vec<u8> {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/char_118_yuki.ab");
    std::fs::read(path).expect("read sample bundle")
}

fn first_asset_node(
    bundle: &unity_asset_binary::bundle::AssetBundle,
) -> &unity_asset_binary::bundle::DirectoryNode {
    bundle
        .nodes
        .iter()
        .find(|node| {
            node.is_file() && !node.name.ends_with(".resS") && !node.name.ends_with(".resource")
        })
        .expect("bundle contains at least one asset node")
}

#[test]
fn unityfs_bundle_lazy_mode_decompresses_on_demand() {
    let bytes = sample_bundle_bytes();

    let bundle = BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy())
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

    let node = first_asset_node(&bundle);

    let bytes = bundle
        .extract_node_slice(node)
        .expect("extract triggers on-demand decompression");
    assert_eq!(bytes.len() as u64, node.size);
    assert!(
        !bundle.data().is_empty(),
        "bundle data becomes available after decompression"
    );
}

#[test]
fn lazy_extraction_uses_the_caller_owned_decompression_budget() {
    let bytes = sample_bundle_bytes();
    let mut probe_budget = AssetLoadBudget::default();
    let _probe = BundleParser::from_bytes_with_options_and_budget(
        bytes.clone(),
        BundleLoadOptions::lazy(),
        &mut probe_budget,
    )
    .expect("parse sample bundle");
    let metadata_decompressed = probe_budget.usage().decompressed_bytes;
    assert!(metadata_decompressed > 0);

    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: metadata_decompressed,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let bundle = BundleParser::from_bytes_with_options_and_budget(
        bytes,
        BundleLoadOptions::lazy(),
        &mut budget,
    )
    .expect("metadata fits the exact decompression budget");
    let node = first_asset_node(&bundle);

    let error = bundle
        .extract_node_data_with_budget(node, &mut budget)
        .unwrap_err();
    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            ..
        })
    ));
    assert_eq!(budget.usage().decompressed_bytes, metadata_decompressed);
}

#[test]
fn lazy_extraction_charges_output_before_allocating_it() {
    let bytes = sample_bundle_bytes();
    let bundle = BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy())
        .expect("parse sample bundle");
    let node = first_asset_node(&bundle);
    let limit = node.size.checked_sub(1).expect("sample node is non-empty");
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: limit,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = bundle
        .extract_node_data_with_budget(node, &mut budget)
        .unwrap_err();
    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            requested,
            ..
        }) if requested == node.size
    ));
    assert_eq!(budget.usage().bytes, 0);
    assert_eq!(budget.usage().decompressed_bytes, 0);
}
