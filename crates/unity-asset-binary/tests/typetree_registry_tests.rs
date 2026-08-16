use std::sync::Arc;

use unity_asset_binary::file::{UnityFile, load_unity_file_with_budget};
use unity_asset_binary::typetree::InMemoryTypeTreeRegistry;
use unity_asset_core::AssetLoadBudget;

#[test]
fn registry_can_restore_typetree_parsing_when_stripped() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/banner_1");
    let mut budget = AssetLoadBudget::default();
    let mut bundle =
        match load_unity_file_with_budget(&path, &mut budget).expect("load sample bundle") {
            UnityFile::AssetBundle(b) => b,
            other => panic!("expected AssetBundle, got {:?}", other.kind()),
        };

    assert!(!bundle.assets.is_empty(), "bundle has asset 0");
    let file = bundle.assets.remove(0);

    let original_tree = file
        .types()
        .iter()
        .find(|t| t.class_id == 28)
        .expect("bundle asset has Texture2D type tree")
        .type_tree
        .clone();

    let mut registry = InMemoryTypeTreeRegistry::default();
    registry.insert_any(28, original_tree);

    let mut types = file.types().to_vec();
    for t in &mut types {
        t.type_tree.clear();
    }
    let ref_types = file.ref_types().to_vec();
    let file = file
        .with_type_tables(types, ref_types)
        .without_embedded_type_trees()
        .with_type_tree_registry(Some(Arc::new(registry)));

    let handle = file
        .find_object_handle(-3875358842991402074)
        .expect("Texture2D object handle exists");

    let peek = handle.peek_name(&mut budget).expect("peek_name");
    assert_eq!(peek.as_deref(), Some("banner_1"));

    let obj = handle
        .read(&mut budget)
        .expect("read object via registry TypeTree");
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
    assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
    assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));
}
