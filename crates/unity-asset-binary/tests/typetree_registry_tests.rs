use std::sync::Arc;

use unity_asset_binary::file::{UnityFile, load_unity_file};
use unity_asset_binary::typetree::InMemoryTypeTreeRegistry;

#[test]
fn registry_can_restore_typetree_parsing_when_stripped() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/banner_1");
    let mut bundle = match load_unity_file(&path).expect("load sample bundle") {
        UnityFile::AssetBundle(b) => b,
        other => panic!("expected AssetBundle, got {:?}", other.kind()),
    };

    let file = bundle.assets.get_mut(0).expect("bundle has asset 0");

    let original_tree = file
        .types
        .iter()
        .find(|t| t.class_id == 28)
        .expect("bundle asset has Texture2D type tree")
        .type_tree
        .clone();

    let mut registry = InMemoryTypeTreeRegistry::default();
    registry.insert_any(28, original_tree);

    file.enable_type_tree = false;
    for t in file.types.iter_mut() {
        t.type_tree.clear();
    }
    file.set_type_tree_registry(Some(Arc::new(registry)));

    let handle = file
        .find_object_handle(-3875358842991402074)
        .expect("Texture2D object handle exists");

    let peek = handle.peek_name().expect("peek_name");
    assert_eq!(peek.as_deref(), Some("banner_1"));

    let obj = handle.read().expect("read object via registry TypeTree");
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
    assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
    assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));
}
