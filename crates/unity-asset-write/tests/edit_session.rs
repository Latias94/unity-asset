use unity_asset_write::ChangeTracker;
use unity_asset_write::object::SerializedFileEditSession;
use unity_asset_write::serialized_file::SerializedFileWriter;

#[test]
fn serialized_file_edit_session_can_edit_name_and_roundtrip() {
    let bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes).unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
        .expect("expected at least one serialized file node in test sample");
    let node_bytes = bundle.extract_node_data(node).unwrap();
    let sf = unity_asset_binary::asset::SerializedFileParser::from_bytes(node_bytes).unwrap();

    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| h.peek_name().ok().flatten().map(|n| (h.path_id(), n)))
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name");

    let new_name = format!("RUST_SESSION_{}", old_name);

    let mut session = SerializedFileEditSession::new(&sf);
    session
        .edit_object(path_id, |class| {
            if let Some(v) = class.get_mut("m_Name") {
                *v = unity_asset_core::UnityValue::String(new_name.clone());
                return Ok(());
            }
            if let Some(v) = class.get_mut("name") {
                *v = unity_asset_core::UnityValue::String(new_name.clone());
                return Ok(());
            }
            Err(unity_asset_write::UnityAssetError::format(
                "No m_Name/name field found for rename test",
            ))
        })
        .unwrap();

    assert!(session.is_changed());
    assert!(!session.edits().is_empty());

    let saved = SerializedFileWriter::save(&sf, session.edits()).unwrap();
    let reparsed = unity_asset_binary::asset::SerializedFileParser::from_bytes(saved).unwrap();
    let reparsed_handle = reparsed
        .find_object_handle(path_id)
        .expect("edited object must exist after save");
    let reparsed_name = reparsed_handle.peek_name().unwrap().unwrap();
    assert_eq!(reparsed_name, new_name);
}
