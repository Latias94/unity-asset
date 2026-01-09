use std::collections::HashSet;
use std::path::PathBuf;

use unity_asset_binary::bundle::{AssetBundle, BundleLoadOptions, BundleLoader, BundleProcessor};
use unity_asset_binary::compression::CompressionType;
use unity_asset_binary::metadata::MetadataExtractor;

fn sample_bundle_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/char_118_yuki.ab")
}

fn collect_present_type_ids(bundle: &AssetBundle) -> HashSet<i32> {
    let mut ids = HashSet::new();
    for asset in &bundle.assets {
        for obj in &asset.objects {
            ids.insert(obj.type_id);
        }
    }
    ids
}

fn expected_bundle_compression_summary(bundle: &AssetBundle) -> String {
    if bundle.blocks.is_empty() {
        return bundle
            .header
            .compression_type()
            .map(|v| v.name().to_string())
            .unwrap_or_else(|_| "Unknown".to_string());
    }

    let mut types: Vec<String> = bundle
        .blocks
        .iter()
        .map(|b| {
            let raw = (b.flags as u32) & 0x3F;
            CompressionType::from_flags(raw)
                .map(|v| v.name().to_string())
                .unwrap_or_else(|_| format!("Unknown({})", raw))
        })
        .collect();

    types.sort();
    types.dedup();

    if types.len() == 1 {
        return types
            .into_iter()
            .next()
            .unwrap_or_else(|| "Unknown".to_string());
    }

    format!("Mixed({})", types.join("+"))
}

#[test]
fn bundle_loader_find_assets_by_type_filters_results() {
    let path = sample_bundle_path();
    let expected_bundle_name = path.to_string_lossy().to_string();

    let mut loader = BundleLoader::with_options(BundleLoadOptions::default());
    loader.load_from_file(&path).expect("load sample bundle");

    let bundle = loader
        .get_bundle(&expected_bundle_name)
        .expect("bundle is cached under path name");

    let present = collect_present_type_ids(bundle);
    let absent_type_id = 1_234_567;
    assert!(
        !present.contains(&absent_type_id),
        "absent type id should not exist in fixture"
    );

    assert!(
        loader.find_assets_by_type(absent_type_id).is_empty(),
        "should return empty list for absent type id"
    );

    let present_type_id = *present
        .iter()
        .next()
        .expect("fixture has at least one type id");
    let expected_count = bundle
        .assets
        .iter()
        .filter(|asset| !asset.objects_of_type(present_type_id).is_empty())
        .count();

    let results = loader.find_assets_by_type(present_type_id);
    assert_eq!(results.len(), expected_count);
    assert!(
        results
            .iter()
            .all(|(name, _)| *name == expected_bundle_name)
    );
    assert!(
        results
            .iter()
            .all(|(_name, asset)| !asset.objects_of_type(present_type_id).is_empty())
    );
}

#[test]
fn bundle_processor_extract_assets_by_type_filters_results() {
    let path = sample_bundle_path();
    let bundle_name = path.to_string_lossy().to_string();

    let mut processor = BundleProcessor::new();
    processor
        .process_file(&path)
        .expect("process sample bundle");

    let bundle = processor
        .loader()
        .get_bundle(&bundle_name)
        .expect("bundle is cached under path name");
    let present = collect_present_type_ids(bundle);

    let absent_type_id = 1_234_567;
    let absent = processor
        .extract_assets_by_type(&bundle_name, absent_type_id)
        .expect("bundle exists");
    assert!(absent.is_empty());

    let present_type_id = *present
        .iter()
        .next()
        .expect("fixture has at least one type id");
    let expected_count = bundle
        .assets
        .iter()
        .filter(|asset| !asset.objects_of_type(present_type_id).is_empty())
        .count();
    let filtered = processor
        .extract_assets_by_type(&bundle_name, present_type_id)
        .expect("bundle exists");
    assert_eq!(filtered.len(), expected_count);
    assert!(
        filtered
            .iter()
            .all(|asset| !asset.objects_of_type(present_type_id).is_empty())
    );
}

#[test]
fn metadata_extractor_from_bundle_sets_bundle_compression_type() {
    let path = sample_bundle_path();
    let bytes = std::fs::read(&path).expect("read sample bundle");
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes).expect("parse bundle");

    let expected = expected_bundle_compression_summary(&bundle);

    let extractor = MetadataExtractor::new();
    let results = extractor
        .extract_from_bundle(&bundle)
        .expect("extract metadata");
    assert!(
        !results.is_empty(),
        "fixture should yield at least one serialized file"
    );
    assert!(
        results
            .iter()
            .all(|r| r.metadata.file_info.compression_type == expected)
    );
}

#[test]
fn bundle_loader_find_assets_by_name_matches_embedded_asset_names() {
    let path = sample_bundle_path();
    let expected_bundle_name = path.to_string_lossy().to_string();

    let mut loader = BundleLoader::with_options(BundleLoadOptions::default());
    loader.load_from_file(&path).expect("load sample bundle");

    let bundle = loader
        .get_bundle(&expected_bundle_name)
        .expect("bundle is cached under path name");
    assert!(
        !bundle.asset_names.is_empty(),
        "fixture should have embedded asset file names"
    );

    let needle = bundle.asset_names[0].chars().take(8).collect::<String>();
    let expected_count = bundle
        .asset_names
        .iter()
        .filter(|n| n.contains(&needle))
        .count();

    let matches = loader.find_assets_by_name(&needle);
    assert_eq!(matches.len(), expected_count);
    assert!(
        matches
            .iter()
            .all(|(name, _asset)| *name == expected_bundle_name)
    );

    assert!(
        loader
            .find_assets_by_name("__this_name_should_not_exist__")
            .is_empty(),
        "absent needle should yield no results"
    );
}
