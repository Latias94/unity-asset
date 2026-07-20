use super::*;
use std::fs;
use std::path::Path;
use unity_asset_core::BudgetError;

fn canonicalize_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn budget_error_in_chain<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a BudgetError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(budget) = error.downcast_ref::<BudgetError>() {
            return Some(budget);
        }
        current = error.source();
    }
    None
}

fn link_or_copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(src, dst).map(|_| ()),
    }
}

fn sample_serialized_file_bytes() -> Vec<u8> {
    let bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(fs::read(bundle_path).unwrap())
            .unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|node| {
            node.is_file() && !node.name.ends_with(".resS") && !node.name.ends_with(".resource")
        })
        .expect("sample bundle contains a SerializedFile node");
    bundle.extract_node_data(node).unwrap()
}

#[test]
fn environment_loads_yaml_fixture() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../unity-asset-yaml/tests/fixtures/SingleDoc.asset"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(!env.yaml_documents().is_empty());
    assert!(env.yaml_objects().next().is_some());
    assert!(env.find_yaml_by_anchor("1").is_some());
}

#[test]
fn environment_can_find_binary_object_by_path_id_and_container_and_stream_info() {
    use unity_asset_binary::unity_version::UnityVersion;
    use unity_asset_decode::audio::AudioClipConverter;

    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(!env.bundles().is_empty());

    let first = env
        .bundles()
        .values()
        .next()
        .and_then(|b| b.assets.first())
        .and_then(|a| a.objects().first())
        .expect("bundle has at least one object");

    let found = env.find_binary_objects(first.path_id());
    assert!(!found.is_empty());

    // Disambiguation helpers should work on the same source path.
    assert!(
        env.find_binary_object_in_source(&path, first.path_id())
            .is_some()
    );
    let obj_ref = env
        .find_binary_object_in_bundle_asset(&path, 0, first.path_id())
        .expect("can find object in bundle asset 0");

    let key = obj_ref.key();
    assert_eq!(key.source, BinarySource::path(&path));
    assert_eq!(key.source_kind, BinarySourceKind::AssetBundle);
    assert_eq!(key.asset_index, Some(0));
    assert_eq!(key.path_id, first.path_id());

    let parsed = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(parsed.info.path_id(), first.path_id());

    let keys = env.find_binary_object_keys(first.path_id());
    assert!(!keys.is_empty());

    let keys_in_source = env.find_binary_object_keys_in_source(&path, first.path_id());
    assert!(keys_in_source.contains(&key));

    // PPtr resolution closure:
    // fileID=0 must resolve to the current serialized file (same source + asset_index).
    let pptr_key = env
        .resolve_binary_pptr(&obj_ref, 0, first.path_id())
        .expect("resolve PPtr with fileID=0");
    assert_eq!(pptr_key, key);

    let pptr_obj = env
        .read_binary_pptr(
            &obj_ref,
            0,
            first.path_id(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(pptr_obj.info.path_id(), first.path_id());

    // If externals are present, pick an out-of-range fileID; otherwise use 1.
    let invalid_file_id = if obj_ref.object.file().externals.is_empty() {
        1
    } else {
        (obj_ref.object.file().externals.len() as i32) + 1
    };
    assert!(
        env.resolve_binary_pptr(&obj_ref, invalid_file_id, first.path_id())
            .is_none()
    );

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&path))
        .expect("sample bundle loaded");
    let has_assetbundle_object = bundle
        .assets
        .iter()
        .any(|f| f.objects().iter().any(|o| o.class_id() == 142));
    assert!(
        has_assetbundle_object,
        "expected at least one AssetBundle (class id 142) object in sample bundle"
    );

    let mut budget = AssetLoadBudget::default();
    let entries = env.bundle_container_entries(&path, &mut budget).unwrap();
    assert!(
        !entries.is_empty(),
        "expected at least one m_Container entry in sample bundle"
    );
    assert!(entries.iter().any(|e| !e.asset_path.is_empty()));
    assert!(entries.iter().any(|e| e.key.is_some()));

    let found = env
        .find_bundle_container_entries(&entries[0].asset_path, &mut budget)
        .unwrap();
    assert!(!found.is_empty());

    let file_name = entries[0]
        .asset_path
        .rsplit('/')
        .next()
        .unwrap_or(&entries[0].asset_path);
    let glob = format!("*{}*", file_name);
    let found_glob = env
        .find_bundle_container_entries(&glob, &mut budget)
        .unwrap();
    assert!(
        !found_glob.is_empty(),
        "glob pattern should match at least one container entry"
    );

    let entries = env.bundle_container_entries(&path, &mut budget).unwrap();
    let cn_001 = entries
        .iter()
        .find(|e| e.asset_path.to_ascii_lowercase().ends_with("/cn_001.ogg"))
        .expect("sample bundle contains cn_001.ogg container entry");
    let key = cn_001
        .key
        .clone()
        .expect("cn_001.ogg container entry resolves to an object key");

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();

    let unity_version = env
        .bundles()
        .get(&BinarySource::path(&path))
        .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
        .and_then(|f| UnityVersion::parse_version(&f.unity_version).ok())
        .unwrap_or_default();

    let converter = AudioClipConverter::new(unity_version);
    let clip = converter.from_unity_object(&obj).unwrap();

    assert!(
        clip.data.is_empty(),
        "streamed clip should not embed audio bytes"
    );
    assert!(clip.is_streamed());
    assert_eq!(clip.stream_info.offset, 4096);
    assert_eq!(clip.stream_info.size, 17088);
    assert!(
        clip.stream_info
            .path
            .contains("CAB-8579bc75d50073df38987733a7cb3193")
    );

    let peek = env
        .peek_binary_object_name(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(peek, obj.name());
}

#[test]
fn environment_can_edit_binary_object_and_save_bundle() {
    use unity_asset_write::PackingPolicy;

    let tmp = tempfile::tempdir().unwrap();
    let in_path = tmp.path().join("char_118_yuki.ab");
    let out_dir = tmp.path().join("out");

    std::fs::write(
        &in_path,
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab"),
    )
    .unwrap();

    let in_path = canonicalize_path(in_path);

    let mut env = Environment::new();
    env.load_file(&in_path, &mut AssetLoadBudget::default())
        .unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&in_path))
        .expect("sample bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let mut name_budget = AssetLoadBudget::default();
    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| {
            h.peek_name(&mut name_budget)
                .ok()
                .flatten()
                .map(|n| (h.path_id(), n))
        })
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: BinarySource::path(&in_path),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let new_name = format!("RUST_ENV_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
        if let Some(v) = class.get_mut("m_Name") {
            *v = UnityValue::String(new_name.clone());
            return Ok(());
        }
        if let Some(v) = class.get_mut("name") {
            *v = UnityValue::String(new_name.clone());
            return Ok(());
        }
        Err(UnityAssetError::format("No m_Name/name field found"))
    })
    .unwrap();

    env.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_path = out_dir.join("char_118_yuki.ab");
    assert!(out_path.is_file());

    let saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(out_path).unwrap())
            .unwrap();
    let saved_sf = saved_bundle
        .assets
        .first()
        .expect("saved bundle has asset 0");
    let saved_obj = saved_sf
        .find_object_handle(path_id)
        .expect("edited object exists after save");
    let saved_name = saved_obj
        .peek_name(&mut AssetLoadBudget::default())
        .unwrap()
        .unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn failed_bundle_object_edits_leave_pending_state_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("char_118_yuki.ab");
    std::fs::write(
        &input,
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab"),
    )
    .unwrap();
    let input = canonicalize_path(input);
    let source = BinarySource::path(&input);
    let mut env = Environment::new();
    env.load_file(&input, &mut AssetLoadBudget::default())
        .unwrap();
    let file = &env.bundles().get(&source).unwrap().assets[0];
    let mut name_budget = AssetLoadBudget::default();
    let path_id = file
        .object_handles()
        .find(|handle| handle.peek_name(&mut name_budget).ok().flatten().is_some())
        .unwrap()
        .path_id();
    let key = BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let error = env
        .edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
            class.set(
                "m_Name".to_owned(),
                UnityValue::String("FAILED_CALLBACK".to_owned()),
            );
            Err(UnityAssetError::format("callback rejected edit"))
        })
        .unwrap_err();
    assert!(error.to_string().contains("callback rejected edit"));
    assert!(!env.has_pending_writes());

    env.edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
        class.set(
            "m_Name".to_owned(),
            UnityValue::String("COMMITTED_EDIT".to_owned()),
        );
        Ok(())
    })
    .unwrap();
    let state = &env.write_state.bundles.get(&source).unwrap().assets[&0];
    let committed_bytes = state.edits.object_bytes(path_id).unwrap().to_vec();
    assert_eq!(
        state.classes[&path_id].get("m_Name"),
        Some(&UnityValue::String("COMMITTED_EDIT".to_owned()))
    );

    let mut exhausted = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: 1,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    assert!(
        env.edit_binary_object_key(&key, &mut exhausted, |class| {
            class.set(
                "m_Name".to_owned(),
                UnityValue::String("FAILED_ENCODE".to_owned()),
            );
            Ok(())
        })
        .is_err()
    );

    let state = &env.write_state.bundles.get(&source).unwrap().assets[&0];
    assert_eq!(state.edits.object_bytes(path_id).unwrap(), committed_bytes);
    assert_eq!(
        state.classes[&path_id].get("m_Name"),
        Some(&UnityValue::String("COMMITTED_EDIT".to_owned()))
    );
}

#[test]
fn environment_edit_session_can_save_binary_object_class_and_save_bundle() {
    use unity_asset_write::PackingPolicy;

    let tmp = tempfile::tempdir().unwrap();
    let in_path = tmp.path().join("char_118_yuki.ab");
    let out_dir = tmp.path().join("out");

    std::fs::write(
        &in_path,
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab"),
    )
    .unwrap();

    let in_path = canonicalize_path(in_path);

    let mut env = Environment::new();
    env.load_file(&in_path, &mut AssetLoadBudget::default())
        .unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&in_path))
        .expect("sample bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let mut name_budget = AssetLoadBudget::default();
    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| {
            h.peek_name(&mut name_budget)
                .ok()
                .flatten()
                .map(|n| (h.path_id(), n))
        })
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: BinarySource::path(&in_path),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    let mut class = obj.as_unity_class().clone();
    let field_name = if class.get("m_Name").is_some() {
        "m_Name"
    } else if class.get("name").is_some() {
        "name"
    } else {
        return;
    };

    let new_name = format!("RUST_ENV_OBJ_SAVE_{}", old_name);
    *class.get_mut(field_name).unwrap() = UnityValue::String(new_name.clone());

    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    session.save_binary_object_class(&key, class).unwrap();
    session.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_path = out_dir.join("char_118_yuki.ab");
    assert!(out_path.is_file());

    let saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(out_path).unwrap())
            .unwrap();
    let saved_sf = saved_bundle
        .assets
        .first()
        .expect("saved bundle has asset 0");
    let saved_obj = saved_sf
        .find_object_handle(path_id)
        .expect("edited object exists after save");
    let saved_name = saved_obj
        .peek_name(&mut AssetLoadBudget::default())
        .unwrap()
        .unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_edit_session_can_set_binary_value_at_path_and_save_bundle() {
    use unity_asset_write::PackingPolicy;

    let tmp = tempfile::tempdir().unwrap();
    let in_path = tmp.path().join("char_118_yuki.ab");
    let out_dir = tmp.path().join("out");

    std::fs::write(
        &in_path,
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab"),
    )
    .unwrap();

    let in_path = canonicalize_path(in_path);

    let mut env = Environment::new();
    env.load_file(&in_path, &mut AssetLoadBudget::default())
        .unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&in_path))
        .expect("sample bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let mut name_budget = AssetLoadBudget::default();
    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| {
            h.peek_name(&mut name_budget)
                .ok()
                .flatten()
                .map(|n| (h.path_id(), n))
        })
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: BinarySource::path(&in_path),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    let class = obj.as_unity_class();
    let field_name = if class.get("m_Name").is_some() {
        "m_Name"
    } else if class.get("name").is_some() {
        "name"
    } else {
        return;
    };

    let new_name = format!("RUST_ENV_SET_PATH_{}", old_name);
    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    let before = session.get_binary_value_at_path(&key, field_name).unwrap();
    assert_eq!(
        before.and_then(|v| v.as_str().map(|s| s.to_string())),
        Some(old_name)
    );

    session
        .set_binary_value_at_path(&key, field_name, UnityValue::String(new_name.clone()))
        .unwrap();

    let after = session.get_binary_value_at_path(&key, field_name).unwrap();
    assert_eq!(
        after.and_then(|v| v.as_str().map(|s| s.to_string())),
        Some(new_name.clone())
    );

    session.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_path = out_dir.join("char_118_yuki.ab");
    assert!(out_path.is_file());

    let saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(out_path).unwrap())
            .unwrap();
    let saved_sf = saved_bundle
        .assets
        .first()
        .expect("saved bundle has asset 0");
    let saved_obj = saved_sf
        .find_object_handle(path_id)
        .expect("edited object exists after save");
    let saved_name = saved_obj
        .peek_name(&mut AssetLoadBudget::default())
        .unwrap()
        .unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_indexes_meta_guid_for_best_effort_external_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let asset_path = temp.path().join("MyAsset.asset");
    let meta_path = temp.path().join("MyAsset.asset.meta");

    std::fs::write(&asset_path, b"not a real asset").unwrap();
    std::fs::write(
        &meta_path,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    let mut env = Environment::new();
    env.load_file(&meta_path, &mut AssetLoadBudget::default())
        .unwrap();

    let expected_guid: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];

    let cached = env.asset_path_for_guid(expected_guid);
    assert_eq!(cached, Some(canonicalize_path(asset_path)));
}

#[test]
fn environment_index_meta_guids_in_directory_skips_library_and_indexes_nested() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let nested_dir = root.join("Assets/Nested");
    std::fs::create_dir_all(&nested_dir).unwrap();

    let asset_path = nested_dir.join("MyAsset.asset");
    let meta_path = nested_dir.join("MyAsset.asset.meta");
    std::fs::write(&asset_path, b"not a real asset").unwrap();
    std::fs::write(
        &meta_path,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    let skipped_dir = root.join("Library");
    std::fs::create_dir_all(&skipped_dir).unwrap();
    let skipped_asset = skipped_dir.join("Skip.asset");
    let skipped_meta = skipped_dir.join("Skip.asset.meta");
    std::fs::write(&skipped_asset, b"not a real asset").unwrap();
    std::fs::write(
        &skipped_meta,
        b"fileFormatVersion: 2\nguid: deadbeefdeadbeefdeadbeefdeadbeef\n",
    )
    .unwrap();

    let env = Environment::new();
    let stats = env.index_meta_guids_in_directory(root).unwrap();
    assert!(stats.meta_files_seen >= 1);
    assert!(stats.meta_guids_indexed >= 1);

    let expected_guid: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    assert_eq!(
        env.asset_path_for_guid(expected_guid),
        Some(canonicalize_path(asset_path))
    );

    let skipped_guid = super::meta_guid::parse_guid_32_hex("deadbeefdeadbeefdeadbeefdeadbeef")
        .expect("parse skipped guid");
    assert_eq!(env.asset_path_for_guid(skipped_guid), None);
}

#[test]
fn environment_load_project_binaries_only_indexes_meta_without_loading_meta_documents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let assets_dir = root.join("Assets");
    std::fs::create_dir_all(&assets_dir).unwrap();

    let meta_asset_path = assets_dir.join("X.asset");
    let meta_path = assets_dir.join("X.asset.meta");
    std::fs::write(&meta_asset_path, b"not a real asset").unwrap();
    std::fs::write(
        &meta_path,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    // A bundle under the project root should be discovered by fast sniffing.
    let sample_bundle = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_dst = root.join("Build/char_118_yuki.ab");
    link_or_copy_file(&sample_bundle, &bundle_dst).unwrap();

    let mut env = Environment::new();
    let mut options = ProjectLoadOptions::binaries_only();
    // Avoid machine-specific global ignore rules (e.g. global gitignore ignoring `Build/`),
    // which can make this test flaky across developer environments.
    options.respect_ignores = false;
    let stats = env
        .load_project(root, options, &mut AssetLoadBudget::default())
        .unwrap();

    assert!(stats.meta_files_seen >= 1);
    assert!(stats.meta_guids_indexed >= 1);
    assert!(stats.binary_loaded >= 1);

    let expected_guid: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ];
    assert_eq!(
        env.asset_path_for_guid(expected_guid),
        Some(canonicalize_path(meta_asset_path))
    );

    // `.meta` should not be stored as a YAML document under binaries_only().
    let meta_path = canonicalize_path(meta_path);
    assert!(
        !env.yaml_documents().contains_key(&meta_path),
        "expected .meta documents to be skipped under ProjectLoadOptions::binaries_only()"
    );
}

#[test]
fn environment_typetree_registry_json_restores_parsing_for_stripped_assets() {
    use serde::Serialize;

    #[derive(Debug, Serialize)]
    struct Dump {
        schema: u32,
        entries: Vec<Entry>,
    }

    #[derive(Debug, Serialize)]
    struct Entry {
        #[serde(skip_serializing_if = "Option::is_none")]
        unity_version: Option<String>,
        class_id: i32,
        type_tree: unity_asset_binary::typetree::TypeTree,
    }

    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let source = BinarySource::path(&path);
    let texture_path_id = -3875358842991402074i64;
    let key = BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id: texture_path_id,
    };

    let type_tree = {
        let bundle = env.bundles.get(&source).expect("sample bundle loaded");
        let file = bundle.assets.first().expect("bundle has asset 0");
        file.types()
            .iter()
            .find(|t| t.class_id == 28)
            .expect("bundle asset has Texture2D type tree")
            .type_tree
            .clone()
    };

    {
        let bundle = env
            .bundles
            .get_mut(&source)
            .expect("sample bundle loaded (mutable)");
        let file = bundle.assets.first_mut().expect("bundle has asset 0");
        file.set_type_tree_enabled(false);
        for t in file.types_mut().iter_mut() {
            t.type_tree.clear();
        }
        file.set_type_tree_registry(None);
    }

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(obj.name(), None, "expected no typetree without registry");

    let tmp = tempfile::tempdir().unwrap();
    let reg_path = tmp.path().join("typetree_registry.json");
    let dump = Dump {
        schema: 1,
        entries: vec![Entry {
            unity_version: None,
            class_id: 28,
            type_tree,
        }],
    };
    fs::write(&reg_path, serde_json::to_string_pretty(&dump).unwrap()).unwrap();

    env.set_type_tree_registry_from_paths(
        std::slice::from_ref(&reg_path),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
    assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
    assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));

    let standalone_path = tmp.path().join("attached.assets");
    fs::write(&standalone_path, sample_serialized_file_bytes()).unwrap();
    let standalone_path = canonicalize_path(standalone_path);
    let standalone_source = BinarySource::path(&standalone_path);
    env.load_file(&standalone_path, &mut AssetLoadBudget::default())
        .unwrap();

    let old_effective = env
        .type_tree_registry
        .as_ref()
        .expect("effective registry is installed")
        .clone();
    let old_attachment = env
        .bundles
        .get(&source)
        .unwrap()
        .assets
        .first()
        .unwrap()
        .type_tree_registry()
        .expect("loaded file has the effective registry")
        .clone();
    let old_standalone_attachment = env
        .binary_assets
        .get(&standalone_source)
        .unwrap()
        .type_tree_registry()
        .expect("loaded standalone file has the effective registry")
        .clone();
    assert!(std::sync::Arc::ptr_eq(&old_effective, &old_attachment));
    assert!(std::sync::Arc::ptr_eq(
        &old_effective,
        &old_standalone_attachment
    ));

    let invalid_path = tmp.path().join("invalid_registry.json");
    fs::write(&invalid_path, b"{").unwrap();
    let error = env
        .set_type_tree_registry_from_paths(
            &[reg_path.clone(), invalid_path],
            &mut AssetLoadBudget::default(),
        )
        .expect_err("the second invalid registry must reject the complete replacement");
    assert!(
        error
            .to_string()
            .contains("Failed to load TypeTree registry")
    );
    assert!(std::sync::Arc::ptr_eq(
        env.type_tree_registry
            .as_ref()
            .expect("failed replacement preserves the effective registry"),
        &old_effective
    ));
    let attachment_after_failure = env
        .bundles
        .get(&source)
        .unwrap()
        .assets
        .first()
        .unwrap()
        .type_tree_registry()
        .expect("failed replacement preserves loaded file attachments");
    assert!(std::sync::Arc::ptr_eq(
        attachment_after_failure,
        &old_attachment
    ));
    let standalone_after_failure = env
        .binary_assets
        .get(&standalone_source)
        .unwrap()
        .type_tree_registry()
        .expect("failed replacement preserves standalone attachments");
    assert!(std::sync::Arc::ptr_eq(
        standalone_after_failure,
        &old_standalone_attachment
    ));

    let mut single_registry_budget = AssetLoadBudget::default();
    unity_asset_binary::typetree::JsonTypeTreeRegistry::from_path(
        &reg_path,
        &mut single_registry_budget,
    )
    .unwrap();
    let entry_limit = single_registry_budget.usage().entries;
    let mut second_path_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_entries: entry_limit,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .set_type_tree_registry_from_paths(&[reg_path.clone(), reg_path], &mut second_path_budget)
        .expect_err("the second registry must exceed the shared entry budget");
    assert!(
        matches!(
            budget_error_in_chain(&error),
            Some(BudgetError::Exceeded {
                resource: "entries",
                limit,
                requested,
            }) if *limit == entry_limit && *requested == entry_limit + 1
        ),
        "error={error:?}, usage={:?}",
        second_path_budget.usage()
    );
    assert!(std::sync::Arc::ptr_eq(
        env.type_tree_registry
            .as_ref()
            .expect("budget failure preserves the effective registry"),
        &old_effective
    ));
    assert!(std::sync::Arc::ptr_eq(
        env.bundles
            .get(&source)
            .unwrap()
            .assets
            .first()
            .unwrap()
            .type_tree_registry()
            .unwrap(),
        &old_attachment
    ));
    assert!(std::sync::Arc::ptr_eq(
        env.binary_assets
            .get(&standalone_source)
            .unwrap()
            .type_tree_registry()
            .unwrap(),
        &old_standalone_attachment
    ));
    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
}

#[test]
fn environment_registry_reuses_single_arcs_and_prepares_script_base_composition_atomically() {
    use unity_asset_binary::typetree::{
        CompositeTypeTreeRegistry, InMemoryTypeTreeRegistry, ScriptTypeTreeGenerator, TypeTree,
        TypeTreeRegistry,
    };

    #[derive(Debug)]
    struct EmptyScriptGenerator;

    impl ScriptTypeTreeGenerator for EmptyScriptGenerator {
        fn generate(
            &self,
            _unity_version: &str,
            _class_id: i32,
            _script_id: [u8; 16],
        ) -> Option<TypeTree> {
            None
        }
    }

    let mut env = Environment::new();
    let original_base: Arc<dyn TypeTreeRegistry> = Arc::new(InMemoryTypeTreeRegistry::default());
    env.set_type_tree_registry(Some(original_base.clone()));
    assert!(Arc::ptr_eq(
        env.type_tree_registry.as_ref().unwrap(),
        &original_base
    ));

    env.set_type_tree_registry(None);
    assert!(env.type_tree_registry.is_none());
    env.set_script_type_tree_generator(Some(Arc::new(EmptyScriptGenerator)));
    let script = env.script_type_tree_registry.as_ref().unwrap().clone();
    assert!(Arc::ptr_eq(
        env.type_tree_registry.as_ref().unwrap(),
        &script
    ));

    env.set_type_tree_registry(Some(original_base.clone()));
    let old_effective = env.type_tree_registry.as_ref().unwrap().clone();
    let old_base = env.base_type_tree_registry.as_ref().unwrap().clone();
    assert!(!Arc::ptr_eq(&old_effective, &old_base));

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("replacement.json");
    fs::write(&path, br#"{"schema":1,"entries":[]}"#).unwrap();

    let mut path_probe = AssetLoadBudget::default();
    let replacement =
        CompositeTypeTreeRegistry::from_paths(std::slice::from_ref(&path), &mut path_probe)
            .unwrap()
            .unwrap();
    let mut compose_probe = AssetLoadBudget::default();
    CompositeTypeTreeRegistry::compose(&[script.clone(), replacement], &mut compose_probe).unwrap();
    let required_bytes = path_probe.usage().bytes + compose_probe.usage().bytes;

    let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: required_bytes - 1,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .set_type_tree_registry_from_paths(std::slice::from_ref(&path), &mut one_short)
        .expect_err("script/base composite allocation must obey the caller budget");
    assert!(
        matches!(
            budget_error_in_chain(&error),
            Some(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if *limit == required_bytes - 1 && *requested == required_bytes
        ),
        "error={error:?}, usage={:?}",
        one_short.usage()
    );
    assert!(Arc::ptr_eq(
        env.base_type_tree_registry
            .as_ref()
            .expect("failed preparation preserves the base registry"),
        &old_base
    ));
    assert!(Arc::ptr_eq(
        env.type_tree_registry
            .as_ref()
            .expect("failed preparation preserves the effective registry"),
        &old_effective
    ));
}

#[test]
fn environment_can_edit_and_save_stripped_assets_with_typetree_registry() {
    use serde::Serialize;
    use unity_asset_binary::typetree::JsonTypeTreeRegistry;
    use unity_asset_write::PackingPolicy;

    #[derive(Debug, Serialize)]
    struct Dump {
        schema: u32,
        entries: Vec<Entry>,
    }

    #[derive(Debug, Serialize)]
    struct Entry {
        #[serde(skip_serializing_if = "Option::is_none")]
        unity_version: Option<String>,
        class_id: i32,
        type_tree: unity_asset_binary::typetree::TypeTree,
    }

    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let source = BinarySource::path(&path);
    let texture_path_id = -3875358842991402074i64;
    let key = BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id: texture_path_id,
    };

    let type_tree = {
        let bundle = env.bundles.get(&source).expect("sample bundle loaded");
        let file = bundle.assets.first().expect("bundle has asset 0");
        file.types()
            .iter()
            .find(|t| t.class_id == 28)
            .expect("bundle asset has Texture2D type tree")
            .type_tree
            .clone()
    };

    {
        let bundle = env
            .bundles
            .get_mut(&source)
            .expect("sample bundle loaded (mutable)");
        let file = bundle.assets.first_mut().expect("bundle has asset 0");
        file.set_type_tree_enabled(false);
        for t in file.types_mut().iter_mut() {
            t.type_tree.clear();
        }
        file.set_type_tree_registry(None);
    }

    let obj = env
        .read_binary_object_key(&key, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(obj.name(), None, "expected no typetree without registry");

    let tmp = tempfile::tempdir().unwrap();
    let reg_path = tmp.path().join("typetree_registry.json");
    let dump = Dump {
        schema: 1,
        entries: vec![Entry {
            unity_version: None,
            class_id: 28,
            type_tree,
        }],
    };
    fs::write(&reg_path, serde_json::to_string_pretty(&dump).unwrap()).unwrap();

    env.set_type_tree_registry_from_paths(
        std::slice::from_ref(&reg_path),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    env.edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
        class.set(
            "m_Name".to_string(),
            UnityValue::String("banner_1_edited".to_string()),
        );
        Ok(())
    })
    .unwrap();

    let out_dir = tmp.path().join("out");
    env.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_path = out_dir.join("banner_1");
    assert!(out_path.is_file());

    let mut saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(&out_path).unwrap())
            .unwrap();
    let reg = std::sync::Arc::new(
        JsonTypeTreeRegistry::from_path(&reg_path, &mut AssetLoadBudget::default()).unwrap(),
    );

    let file = saved_bundle.assets.first_mut().expect("bundle has asset 0");
    file.set_type_tree_registry(Some(reg));

    let saved = file
        .find_object_handle(texture_path_id)
        .expect("edited object exists after save")
        .read(&mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(saved.name().as_deref(), Some("banner_1_edited"));
}

#[test]
fn environment_can_load_split_assetbundle() {
    let tmp = tempfile::tempdir().unwrap();
    let split0 = tmp.path().join("char_118_yuki.ab.split0");
    let split1 = tmp.path().join("char_118_yuki.ab.split1");

    let bytes = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let mid = bytes.len() / 2;
    std::fs::write(&split0, &bytes[..mid]).unwrap();
    std::fs::write(&split1, &bytes[mid..]).unwrap();

    let mut env = Environment::new();
    env.load_file(&split0, &mut AssetLoadBudget::default())
        .unwrap();

    let source = env
        .bundles()
        .keys()
        .find(|s| match s {
            BinarySource::Path(p) => p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "char_118_yuki.ab"),
            _ => false,
        })
        .cloned()
        .expect("expected split bundle to be loaded");

    let mut budget = AssetLoadBudget::default();
    let entries = env
        .bundle_container_entries_source(&source, &mut budget)
        .unwrap();
    assert!(!entries.is_empty());
}

#[test]
fn environment_resource_failed_binary_and_split_loads_preserve_base_path() {
    let sample_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let original_base_path = sample_path.join("original-base");
    let limits = unity_asset_core::AssetLoadLimits {
        max_bytes: 1,
        ..unity_asset_core::AssetLoadLimits::default()
    };

    let mut direct_env = Environment::new();
    direct_env.base_path = original_base_path.clone();
    direct_env
        .load_file(&sample_path, &mut AssetLoadBudget::new(limits).unwrap())
        .expect_err("the direct binary must exceed the byte budget");
    assert_eq!(direct_env.base_path, original_base_path);

    let temp = tempfile::tempdir().unwrap();
    let split_path = temp.path().join("char_118_yuki.ab.split0");
    fs::write(
        &split_path,
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab"),
    )
    .unwrap();
    let mut split_env = Environment::new();
    split_env.base_path = original_base_path.clone();
    split_env
        .load_file(&split_path, &mut AssetLoadBudget::new(limits).unwrap())
        .expect_err("the split binary must exceed the byte budget");
    assert_eq!(split_env.base_path, original_base_path);
}

#[test]
fn environment_direct_positive_signed_parse_error_preserves_state() {
    let sample_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let sample_source = BinarySource::path(&sample_path);
    let temp = tempfile::tempdir().unwrap();
    let corrupt_path = temp.path().join("corrupt.ab");
    fs::write(&corrupt_path, b"UnityFS\0").unwrap();

    let mut env = Environment::new();
    env.load_file(&sample_path, &mut AssetLoadBudget::default())
        .unwrap();
    env.bundle_container_cache
        .write()
        .unwrap()
        .insert(sample_source.clone(), Vec::new());
    let original_base_path = temp.path().join("original-base");
    env.base_path = original_base_path.clone();
    let bundle_count = env.bundles().len();
    let binary_count = env.binary_assets().len();
    let webfile_count = env.webfiles().len();

    let error = env
        .load_file(&corrupt_path, &mut AssetLoadBudget::default())
        .expect_err("a positive-signed direct binary parse error must propagate");

    assert!(error.to_string().contains("corrupt.ab"), "{error:?}");
    assert_eq!(env.base_path, original_base_path);
    assert_eq!(env.bundles().len(), bundle_count);
    assert_eq!(env.binary_assets().len(), binary_count);
    assert_eq!(env.webfiles().len(), webfile_count);
    assert!(
        env.bundle_container_cache
            .read()
            .unwrap()
            .contains_key(&sample_source)
    );
}

#[test]
fn environment_project_does_not_count_positive_signed_parse_error() {
    let temp = tempfile::tempdir().unwrap();
    let corrupt_path = temp.path().join("corrupt.ab");
    fs::write(&corrupt_path, b"UnityFS\0").unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let mut options = ProjectLoadOptions::binaries_only();
    options.respect_ignores = false;

    let stats = env
        .load_project(temp.path(), options, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(stats.files_visited, 1);
    assert_eq!(stats.files_loaded, 0);
    assert_eq!(stats.binary_loaded, 0);
    assert_eq!(env.base_path, original_base_path);
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
    assert!(env.webfiles().is_empty());
    assert!(env.bundle_container_cache.read().unwrap().is_empty());
}

#[test]
fn environment_directory_and_project_share_one_cumulative_load_budget() {
    let sample_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let mut probe_budget = AssetLoadBudget::default();
    Environment::new()
        .load_file(&sample_path, &mut probe_budget)
        .unwrap();
    let one_file_members = probe_budget.usage().members;
    assert!(one_file_members > 0);

    for use_project_loader in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        fs::copy(&sample_path, temp.path().join("first.ab")).unwrap();
        fs::copy(&sample_path, temp.path().join("second.ab")).unwrap();
        let mut env = Environment::new();
        let mut budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_members: one_file_members,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();

        let error = if use_project_loader {
            let mut options = ProjectLoadOptions::binaries_only();
            options.respect_ignores = false;
            env.load_project(temp.path(), options, &mut budget)
                .expect_err("the second project source must share the exhausted member budget")
        } else {
            env.load_directory(temp.path(), &mut budget)
                .expect_err("the second directory source must share the exhausted member budget")
        };

        assert!(super::pptr::is_resource_error(&error), "{error:?}");
        assert_eq!(env.bundles().len(), 1);
        assert!(env.binary_assets().is_empty());
        assert!(env.webfiles().is_empty());
    }
}

#[test]
fn environment_can_load_zip_assetbundle_entry() {
    use std::io::Write;
    use zip::write::FileOptions;

    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("samples.zip");

    let f = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(f);
    zip.start_file("inner/char_118_yuki.ab", FileOptions::default())
        .unwrap();
    zip.write_all(include_bytes!(
        "../../../../../tests/samples/char_118_yuki.ab"
    ))
    .unwrap();
    zip.finish().unwrap();

    let zip_path = canonicalize_path(zip_path);

    let mut env = Environment::new();
    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();

    let source = BinarySource::archive_entry(&zip_path, "inner/char_118_yuki.ab");

    let mut budget = AssetLoadBudget::default();
    let entries = env
        .bundle_container_entries_source(&source, &mut budget)
        .unwrap();
    assert!(!entries.is_empty());
}

#[test]
fn environment_budgeted_zip_load_is_atomic_and_retryable() {
    use std::io::Write;
    use zip::write::FileOptions;

    fn write_bundle_zip(path: &Path, entry_names: &[&str]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
        for entry_name in entry_names {
            zip.start_file(*entry_name, FileOptions::default()).unwrap();
            zip.write_all(bundle).unwrap();
        }
        zip.finish().unwrap();
    }

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("atomic.zip");
    let first_entry = "inner/first.ab";
    let second_entry = "inner/second.ab";

    write_bundle_zip(&zip_path, &[first_entry]);
    let zip_path = canonicalize_path(zip_path);
    let first_source = BinarySource::archive_entry(&zip_path, first_entry);
    let second_source = BinarySource::archive_entry(&zip_path, second_entry);

    let mut env = Environment::new();
    let mut single_entry_budget = AssetLoadBudget::default();
    env.load_file(&zip_path, &mut single_entry_budget).unwrap();
    let single_entry_members = single_entry_budget.usage().members;

    env.bundles
        .get_mut(&first_source)
        .expect("seed bundle loaded")
        .assets
        .clear();
    env.bundle_container_cache
        .write()
        .unwrap()
        .insert(first_source.clone(), Vec::new());
    let original_base_path = temp.path().join("original-base");
    env.base_path = original_base_path.clone();

    write_bundle_zip(&zip_path, &[first_entry, second_entry]);
    let mut constrained_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: single_entry_members,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .load_file(&zip_path, &mut constrained_budget)
        .expect_err("the second zip member must exceed the cumulative member budget");

    assert!(super::pptr::is_resource_error(&error), "{error:?}");
    assert_eq!(env.base_path, original_base_path);
    assert_eq!(env.bundles().len(), 1);
    assert!(env.bundles().contains_key(&first_source));
    assert!(
        env.bundles()[&first_source].assets.is_empty(),
        "the staged replacement must not overwrite the existing source"
    );
    assert!(!env.bundles().contains_key(&second_source));
    assert!(env.binary_assets().is_empty());
    assert!(env.webfiles().is_empty());
    assert!(
        env.bundle_container_cache
            .read()
            .unwrap()
            .contains_key(&first_source),
        "a failed archive load must not invalidate existing caches"
    );

    let mut retry_budget = AssetLoadBudget::default();
    env.load_file(&zip_path, &mut retry_budget)
        .expect("one retry with a fresh sufficient budget must succeed");
    assert_eq!(env.bundles().len(), 2);
    assert!(!env.bundles()[&first_source].assets.is_empty());
    assert!(env.bundles().contains_key(&second_source));
}

#[test]
fn environment_zip_member_preflight_counts_directories_before_parsing() {
    use std::io::Write;
    use zip::write::FileOptions;

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("directory-flood.zip");
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");

    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("seed.ab", FileOptions::default()).unwrap();
    zip.write_all(bundle).unwrap();
    zip.finish().unwrap();

    let zip_path = canonicalize_path(zip_path);
    let seed_source = BinarySource::archive_entry(&zip_path, "seed.ab");
    let mut env = Environment::new();
    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(env.bundles().contains_key(&seed_source));

    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for directory in ["a/", "b/", "c/"] {
        zip.add_directory(directory, FileOptions::default())
            .unwrap();
    }
    zip.finish().unwrap();

    let mut budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: 2,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .load_file(&zip_path, &mut budget)
        .expect_err("all directory occurrences must be rejected before ZipArchive allocation");

    assert!(error.to_string().contains("member budget"), "{error:?}");
    assert_eq!(budget.usage().members, 0);
    assert!(env.bundles().contains_key(&seed_source));
    assert!(env.binary_assets().is_empty());
}

#[test]
fn environment_zip_member_limit_precedes_central_header_scan() {
    use zip::write::FileOptions;

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("corrupt-over-limit.zip");
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    for directory in ["a/", "b/", "c/"] {
        zip.add_directory(directory, FileOptions::default())
            .unwrap();
    }
    zip.finish().unwrap();

    let mut bytes = fs::read(&zip_path).unwrap();
    let central_signature = [0x50, 0x4b, 0x01, 0x02];
    let central_offset = bytes
        .windows(central_signature.len())
        .position(|window| window == central_signature)
        .expect("test ZIP contains a central directory header");
    bytes[central_offset] ^= 0xff;
    fs::write(&zip_path, bytes).unwrap();

    let mut env = Environment::new();
    let mut over_limit = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: 2,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .load_file(&zip_path, &mut over_limit)
        .expect_err("member count must fail before the corrupt central header is scanned");
    assert!(error.to_string().contains("member budget"), "{error:?}");
    assert_eq!(over_limit.usage().members, 0);

    let mut within_limit = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: 3,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .load_file(&zip_path, &mut within_limit)
        .expect_err("within-limit archives must validate every central header");
    assert!(error.to_string().contains("central directory"), "{error:?}");
    assert_eq!(within_limit.usage().members, 0);
}

#[test]
fn environment_zip_reload_replaces_removed_members_and_changed_kinds() {
    use std::io::Write;
    use zip::write::FileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            zip.start_file(*name, FileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("replace.zip");
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let serialized = sample_serialized_file_bytes();
    write_zip(
        &zip_path,
        &[("changed.bin", bundle), ("removed.ab", bundle)],
    );
    let zip_path = canonicalize_path(zip_path);
    let changed = BinarySource::archive_entry(&zip_path, "changed.bin");
    let removed = BinarySource::archive_entry(&zip_path, "removed.ab");

    let mut env = Environment::new();
    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(env.bundles().contains_key(&changed));
    assert!(env.bundles().contains_key(&removed));
    let retained_archive_paths = env
        .bundles()
        .keys()
        .filter_map(|source| match source {
            BinarySource::ArchiveEntry { archive_path, .. } => Some(archive_path),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retained_archive_paths.len(), 2);
    assert!(Arc::ptr_eq(
        retained_archive_paths[0],
        retained_archive_paths[1]
    ));

    write_zip(&zip_path, &[("changed.bin", &serialized)]);
    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();

    assert!(!env.bundles().contains_key(&changed));
    assert!(!env.bundles().contains_key(&removed));
    assert!(env.binary_assets().contains_key(&changed));
    assert!(!env.binary_assets().contains_key(&removed));
}

#[test]
fn environment_zip_rejects_positive_signed_corrupt_entry_atomically() {
    use std::io::Write;
    use zip::write::FileOptions;

    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("corrupt-signed.zip");
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("a-valid.ab", FileOptions::default())
        .unwrap();
    zip.write_all(include_bytes!(
        "../../../../../tests/samples/char_118_yuki.ab"
    ))
    .unwrap();
    zip.start_file("z-corrupt.ab", FileOptions::default())
        .unwrap();
    zip.write_all(b"UnityFS\0").unwrap();
    zip.finish().unwrap();

    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let error = env
        .load_file(&zip_path, &mut AssetLoadBudget::default())
        .expect_err("a positive-signed corrupt zip entry must fail the whole archive");

    assert!(error.to_string().contains("z-corrupt.ab"), "{error:?}");
    assert_eq!(env.base_path, original_base_path);
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
    assert!(env.webfiles().is_empty());
}

#[test]
fn environment_zip_failure_discards_recursive_webfile_stage() {
    use std::io::Write;
    use zip::write::FileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (entry_name, bytes) in entries {
            zip.start_file(*entry_name, FileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let inner_web = build_uncompressed_webfile(vec![("nested.ab".to_string(), bundle.to_vec())]);
    let outer_web = build_uncompressed_webfile(vec![("nested.web".to_string(), inner_web)]);

    let temp = tempfile::tempdir().unwrap();
    let probe_path = temp.path().join("probe.zip");
    write_zip(&probe_path, &[("payload/outer.web", &outer_web)]);
    let mut probe_env = Environment::new();
    let mut probe_budget = AssetLoadBudget::default();
    probe_env.load_file(&probe_path, &mut probe_budget).unwrap();

    let zip_path = temp.path().join("recursive.zip");
    write_zip(
        &zip_path,
        &[
            ("payload/outer.web", &outer_web),
            ("payload/direct.ab", bundle),
        ],
    );
    let zip_path = canonicalize_path(zip_path);
    let original_base_path = temp.path().join("recursive-original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let mut constrained_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: probe_budget.usage().members,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();

    env.load_file(&zip_path, &mut constrained_budget)
        .expect_err("the direct bundle must exceed the remaining member budget");
    assert_eq!(env.base_path, original_base_path);
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());

    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();
    let outer_web_path = zip_path.join("payload/outer.web");
    let inner_web_path = outer_web_path.join("nested.web");
    assert!(env.webfiles().contains_key(&outer_web_path));
    assert!(env.webfiles().contains_key(&inner_web_path));
    assert!(env.bundles().contains_key(&BinarySource::WebEntry {
        web_path: Arc::new(inner_web_path),
        entry_name: "nested.ab".to_string(),
    }));
    assert!(
        env.bundles()
            .contains_key(&BinarySource::archive_entry(&zip_path, "payload/direct.ab"))
    );
}

#[test]
fn environment_can_edit_zip_assetbundle_entry_and_save() {
    use std::io::Write;
    use unity_asset_write::PackingPolicy;
    use zip::write::FileOptions;

    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("samples.zip");
    let out_dir = tmp.path().join("out");

    let f = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(f);
    zip.start_file("inner/char_118_yuki.ab", FileOptions::default())
        .unwrap();
    zip.write_all(include_bytes!(
        "../../../../../tests/samples/char_118_yuki.ab"
    ))
    .unwrap();
    zip.finish().unwrap();

    let zip_path = canonicalize_path(zip_path);

    let mut env = Environment::new();
    env.load_file(&zip_path, &mut AssetLoadBudget::default())
        .unwrap();

    let source = BinarySource::archive_entry(&zip_path, "inner/char_118_yuki.ab");

    let bundle = env.bundles().get(&source).expect("zip bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let mut name_budget = AssetLoadBudget::default();
    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| {
            h.peek_name(&mut name_budget)
                .ok()
                .flatten()
                .map(|n| (h.path_id(), n))
        })
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let new_name = format!("RUST_ZIP_ENV_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
        if let Some(v) = class.get_mut("m_Name") {
            *v = UnityValue::String(new_name.clone());
            return Ok(());
        }
        if let Some(v) = class.get_mut("name") {
            *v = UnityValue::String(new_name.clone());
            return Ok(());
        }
        Err(UnityAssetError::format("No m_Name/name field found"))
    })
    .unwrap();

    env.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_path = out_dir.join("char_118_yuki.ab");
    assert!(out_path.is_file());

    let saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(out_path).unwrap())
            .unwrap();
    let saved_sf = saved_bundle
        .assets
        .first()
        .expect("saved bundle has asset 0");
    let saved_obj = saved_sf
        .find_object_handle(path_id)
        .expect("edited object exists after save");
    let saved_name = saved_obj
        .peek_name(&mut AssetLoadBudget::default())
        .unwrap()
        .unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_assetbundle_container_raw_matches_typetree_when_stripped() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/xinzexi_2_n_tex"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let mut budget = AssetLoadBudget::default();
    let baseline = env.bundle_container_entries(&path, &mut budget).unwrap();
    assert!(
        !baseline.is_empty(),
        "expected at least one m_Container entry in sample bundle"
    );

    let mut fallback_env = Environment::new();
    fallback_env
        .load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let source = BinarySource::path(&path);
    {
        let bundle = fallback_env
            .bundles
            .get_mut(&source)
            .expect("sample bundle loaded (mutable)");
        for file in bundle.assets.iter_mut() {
            for t in file.types_mut().iter_mut() {
                if t.class_id == 142 {
                    t.type_tree.clear();
                    t.type_tree
                        .add_node(unity_asset_binary::typetree::TypeTreeNode::with_info(
                            "UInt8".to_owned(),
                            "m_InvalidRoot".to_owned(),
                            1,
                        ));
                }
            }
            file.set_type_tree_registry(None);
        }
    }
    fallback_env
        .bundle_container_cache
        .write()
        .unwrap()
        .remove(&source);

    let malformed_typetree_fallback = fallback_env
        .bundle_container_entries(&path, &mut budget)
        .unwrap();
    assert!(!malformed_typetree_fallback.is_empty());

    {
        let bundle = fallback_env
            .bundles
            .get_mut(&source)
            .expect("sample bundle loaded (mutable)");
        for file in bundle.assets.iter_mut() {
            file.set_type_tree_enabled(false);
            for t in file.types_mut().iter_mut() {
                t.type_tree.clear();
            }
        }
    }
    fallback_env
        .bundle_container_cache
        .write()
        .unwrap()
        .remove(&source);

    let stripped = fallback_env
        .bundle_container_entries(&path, &mut budget)
        .unwrap();
    assert!(
        !stripped.is_empty(),
        "expected container entries via raw fallback when TypeTree is stripped"
    );

    let mut a: Vec<(String, i32, i64)> = baseline
        .iter()
        .map(|e| (e.asset_path.clone(), e.file_id, e.path_id))
        .collect();
    a.sort();
    let mut b: Vec<(String, i32, i64)> = stripped
        .iter()
        .map(|e| (e.asset_path.clone(), e.file_id, e.path_id))
        .collect();
    b.sort();
    assert_eq!(a, b, "raw container entries mismatch typetree baseline");
}

#[test]
fn environment_loads_minimal_gameobject_transform_prefab_and_resolves_refs() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let game_object = env
        .find_yaml_by_anchor("1001")
        .expect("GameObject anchor exists");
    assert_eq!(game_object.class_id, 1);
    assert_eq!(game_object.name(), Some("TestGO"));

    let comps = game_object
        .get("m_Component")
        .expect("m_Component present")
        .as_array()
        .expect("m_Component array");
    assert_eq!(comps.len(), 2);

    let mut comp_ids: Vec<i64> = Vec::new();
    for comp in comps {
        let comp = comp.as_object().expect("component entry object");
        let pptr = comp
            .get("component")
            .expect("component key present")
            .as_object()
            .expect("component pptr object");
        let file_id = pptr
            .get("fileID")
            .and_then(|v| v.as_i64())
            .expect("component fileID int");
        comp_ids.push(file_id);
    }
    comp_ids.sort();
    assert_eq!(comp_ids, vec![1002, 1003]);

    let transform = env.find_yaml_by_anchor("1002").expect("Transform anchor");
    assert_eq!(transform.class_id, 4);
    let t_go = transform
        .get("m_GameObject")
        .expect("m_GameObject present")
        .as_object()
        .expect("m_GameObject object");
    assert_eq!(t_go.get("fileID").and_then(|v| v.as_i64()), Some(1001));

    let mb = env
        .find_yaml_by_anchor("1003")
        .expect("MonoBehaviour anchor");
    assert_eq!(mb.class_id, 114);
    let mb_go = mb
        .get("m_GameObject")
        .expect("m_GameObject present")
        .as_object()
        .expect("m_GameObject object");
    assert_eq!(mb_go.get("fileID").and_then(|v| v.as_i64()), Some(1001));
    let script = mb
        .get("m_Script")
        .expect("m_Script present")
        .as_object()
        .expect("m_Script object");
    assert_eq!(
        script.get("guid").and_then(|v| v.as_str()),
        Some("0123456789abcdef0123456789abcdef")
    );
}

#[test]
fn environment_can_parse_external_yaml_prefab_if_provided() {
    let mut env = Environment::new();
    let Ok(path) = std::env::var("UNITY_ASSET_YAML_PREFAB") else {
        return;
    };
    let path = PathBuf::from(path);
    if !path.exists() {
        return;
    }
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let go = env
        .yaml_objects()
        .find(|o| o.class_id == 1 && o.name().is_some())
        .expect("at least one GameObject with a name");

    let comps = go
        .get("m_Component")
        .expect("m_Component present")
        .as_array()
        .expect("m_Component array");
    assert!(comps.iter().any(|v| {
        v.as_object()
            .and_then(|o| o.get("component"))
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("fileID"))
            .and_then(|v| v.as_i64())
            .is_some()
    }));
}

#[test]
fn environment_reads_live_default_flag_bundle_resources_before_fallback() {
    let bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let cab = "8579bc75d50073df38987733a7cb3193";
    let stream_path = format!("archive:/CAB-{cab}/CAB-{cab}.resource");
    let mut env = Environment::new();
    env.load_file(&bundle_path, &mut AssetLoadBudget::default())
        .unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&bundle_path))
        .expect("bundle is loaded");
    let resource = bundle
        .nodes
        .iter()
        .find(|node| node.name == format!("CAB-{cab}.resource"))
        .expect("sample resource node");
    assert_eq!(resource.flags, 0);
    assert!(resource.is_file());

    assert_eq!(
        env.read_stream_data(
            &bundle_path,
            BinarySourceKind::AssetBundle,
            &stream_path,
            4096,
            4,
        )
        .unwrap(),
        b"FSB5"
    );
}

#[test]
fn environment_stream_data_falls_back_to_filesystem_for_bundles() {
    let temp = tempfile::tempdir().unwrap();
    let bundle_src = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_path = temp.path().join("char_118_yuki.ab");
    link_or_copy_file(&bundle_src, &bundle_path).unwrap();

    let cab = "00112233445566778899aabbccddeeff";
    let stream_path = format!("archive:/CAB-{cab}/CAB-{cab}.resource");
    let resource_dir = temp.path().join(format!("CAB-{cab}"));
    fs::create_dir_all(&resource_dir).unwrap();
    let resource_path = resource_dir.join(format!("CAB-{cab}.resource"));

    let mut bytes = vec![0u8; 4096 + 4];
    bytes[4096..4096 + 4].copy_from_slice(b"OggS");
    fs::write(&resource_path, bytes).unwrap();

    let mut env = Environment::new();
    env.load_file(&bundle_path, &mut AssetLoadBudget::default())
        .unwrap();

    let read = env
        .read_stream_data(
            &bundle_path,
            BinarySourceKind::AssetBundle,
            &stream_path,
            4096,
            4,
        )
        .unwrap();
    assert_eq!(read, b"OggS");

    // Common on-disk variant: `CAB-<hash>1.resource` (no folder).
    fs::remove_file(&resource_path).unwrap();
    fs::remove_dir_all(&resource_dir).unwrap();

    let resource_path = temp.path().join(format!("CAB-{cab}1.resource"));
    let mut bytes = vec![0u8; 4096 + 4];
    bytes[4096..4096 + 4].copy_from_slice(b"OggS");
    fs::write(&resource_path, bytes).unwrap();

    let read = env
        .read_stream_data(
            &bundle_path,
            BinarySourceKind::AssetBundle,
            &stream_path,
            4096,
            4,
        )
        .unwrap();
    assert_eq!(read, b"OggS");

    drop(env);
    fs::remove_file(&resource_path).unwrap();
    fs::remove_file(&bundle_path).unwrap();
}

fn build_uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";

    let entry_table_len: usize = entries
        .iter()
        .map(|(name, _)| 12usize.saturating_add(name.len()))
        .sum();
    let header_len: usize = signature
        .len()
        .saturating_add(std::mem::size_of::<i32>())
        .saturating_add(entry_table_len);

    let head_length_i32: i32 = header_len
        .try_into()
        .expect("header_len fits i32 for test webfile");

    let mut out: Vec<u8> = Vec::with_capacity(
        header_len.saturating_add(entries.iter().map(|(_, b)| b.len()).sum::<usize>()),
    );
    out.extend_from_slice(signature);
    out.extend_from_slice(&head_length_i32.to_le_bytes());

    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    let mut cursor = header_len;

    for (name, bytes) in entries {
        let offset_i32: i32 = cursor.try_into().expect("offset fits i32");
        let length_i32: i32 = bytes.len().try_into().expect("length fits i32");
        let name_len_i32: i32 = name.len().try_into().expect("name_len fits i32");

        out.extend_from_slice(&offset_i32.to_le_bytes());
        out.extend_from_slice(&length_i32.to_le_bytes());
        out.extend_from_slice(&name_len_i32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());

        cursor = cursor.saturating_add(bytes.len());
        payloads.push(bytes);
    }

    for payload in payloads {
        out.extend_from_slice(&payload);
    }

    out
}

#[test]
fn environment_explicitly_loads_writer_gzip_and_brotli_webfiles() {
    use unity_asset_write::webfile::{WebFileEdits, WebFilePackingPolicy, WebFileWriter};

    let entry_name = "embedded.ab";
    let raw = build_uncompressed_webfile(vec![(
        entry_name.to_string(),
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab").to_vec(),
    )]);
    let web = WebFile::from_bytes(raw).unwrap();
    let temp = tempfile::tempdir().unwrap();

    for (file_name, packer) in [
        ("payload.gzip", WebFilePackingPolicy::Gzip),
        ("payload.brotli", WebFilePackingPolicy::Brotli),
    ] {
        let path = temp.path().join(file_name);
        let encoded = WebFileWriter::save(&web, &WebFileEdits::default(), packer).unwrap();
        fs::write(&path, encoded).unwrap();
        let path = canonicalize_path(path);
        let source = BinarySource::web_entry(&path, entry_name);
        let mut env = Environment::new();

        env.load_file(&path, &mut AssetLoadBudget::default())
            .unwrap();

        assert!(env.webfiles().contains_key(&path));
        assert!(env.bundles().contains_key(&source));
    }
}

#[test]
fn environment_directory_scan_ignores_ordinary_gzip_and_loads_gzip_webfile() {
    use unity_asset_write::webfile::{WebFileEdits, WebFilePackingPolicy, WebFileWriter};

    const ORDINARY_GZIP: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
        0x07, 0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
    ];
    let entry_name = "embedded.ab";
    let raw = build_uncompressed_webfile(vec![(
        entry_name.to_string(),
        include_bytes!("../../../../../tests/samples/char_118_yuki.ab").to_vec(),
    )]);
    let web = WebFile::from_bytes(raw).unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("notes.gz"), ORDINARY_GZIP).unwrap();
    let web_path = temp.path().join("payload.gz");
    fs::write(
        &web_path,
        WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePackingPolicy::Gzip).unwrap(),
    )
    .unwrap();

    let mut env = Environment::new();
    let mut budget = AssetLoadBudget::default();
    env.load_directory(temp.path(), &mut budget).unwrap();
    let web_path = canonicalize_path(web_path);

    assert!(env.webfiles().contains_key(&web_path));
    assert!(
        env.bundles()
            .contains_key(&BinarySource::web_entry(&web_path, entry_name))
    );
}

#[test]
fn environment_budgeted_webfile_load_is_atomic_and_retryable() {
    let sample_bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let entry_name = "char_118_yuki.ab".to_string();
    let web_bytes = build_uncompressed_webfile(vec![(
        entry_name.clone(),
        fs::read(&sample_bundle_path).unwrap(),
    )]);
    let web_len = u64::try_from(web_bytes.len()).unwrap();

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("UnityWebData");
    fs::write(&web_path, web_bytes).unwrap();
    let web_path = canonicalize_path(web_path);

    let mut env = Environment::new();
    let original_base_path = temp.path().join("original-base");
    env.base_path = original_base_path.clone();
    let mut constrained_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: web_len.checked_add(4 * 1024).unwrap(),
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .load_file(&web_path, &mut constrained_budget)
        .expect_err("embedded bundle parsing must share the WebFile load budget");
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut is_resource_error = false;
    while let Some(current) = source {
        if current
            .downcast_ref::<unity_asset_binary::error::BinaryError>()
            .is_some_and(unity_asset_binary::error::BinaryError::is_resource_error)
        {
            is_resource_error = true;
            break;
        }
        source = current.source();
    }
    assert!(is_resource_error, "unexpected error chain: {error:?}");
    assert!(
        error
            .to_string()
            .contains("Failed to load recognized WebFile entry")
    );
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
    assert_eq!(env.base_path, original_base_path);

    env.load_file(&web_path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(env.webfiles().contains_key(&web_path));
    assert!(env.bundles().contains_key(&BinarySource::WebEntry {
        web_path: Arc::new(web_path),
        entry_name,
    }));
}

#[test]
fn environment_webfile_reload_replaces_removed_members_and_changed_kinds() {
    use unity_asset_write::webfile::{WebFileEdits, WebFilePackingPolicy, WebFileWriter};

    fn gzip_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        let web = WebFile::from_bytes(build_uncompressed_webfile(entries)).unwrap();
        WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePackingPolicy::Gzip).unwrap()
    }

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("replace.web");
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    fs::write(
        &web_path,
        gzip_webfile(vec![
            ("changed.bin".to_string(), bundle.to_vec()),
            ("removed.ab".to_string(), bundle.to_vec()),
        ]),
    )
    .unwrap();
    let web_path = canonicalize_path(web_path);
    let changed = BinarySource::web_entry(&web_path, "changed.bin");
    let removed = BinarySource::web_entry(&web_path, "removed.ab");
    let mut env = Environment::new();

    env.load_file(&web_path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(env.bundles().contains_key(&changed));
    assert!(env.bundles().contains_key(&removed));

    fs::write(
        &web_path,
        gzip_webfile(vec![(
            "changed.bin".to_string(),
            sample_serialized_file_bytes(),
        )]),
    )
    .unwrap();
    env.load_file(&web_path, &mut AssetLoadBudget::default())
        .unwrap();

    assert!(!env.bundles().contains_key(&changed));
    assert!(!env.bundles().contains_key(&removed));
    assert!(env.binary_assets().contains_key(&changed));
    assert!(!env.binary_assets().contains_key(&removed));
    assert_eq!(env.webfiles().len(), 1);
}

#[test]
fn environment_webfile_rejects_positive_signed_corrupt_entry_atomically() {
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let web_bytes = build_uncompressed_webfile(vec![
        ("a-valid.ab".to_string(), bundle.to_vec()),
        ("z-corrupt.ab".to_string(), b"UnityFS\0".to_vec()),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("corrupt-signed.web");
    fs::write(&web_path, web_bytes).unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();

    let error = env
        .load_file(&web_path, &mut AssetLoadBudget::default())
        .expect_err("a positive-signed corrupt WebFile entry must fail the whole container");

    assert!(error.to_string().contains("z-corrupt.ab"), "{error:?}");
    assert_eq!(env.base_path, original_base_path);
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
}

#[test]
fn environment_webfile_rejects_duplicate_occurrences_before_deduplication() {
    let web_bytes = build_uncompressed_webfile(vec![
        ("duplicate.bin".to_string(), b"first".to_vec()),
        ("duplicate.bin".to_string(), b"second".to_vec()),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("duplicate.web");
    fs::write(&web_path, web_bytes).unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let mut budget = AssetLoadBudget::default();

    let error = env
        .load_file(&web_path, &mut budget)
        .expect_err("duplicate names cannot be represented by the legacy source identity");

    assert!(error.to_string().contains("duplicate"), "{error:?}");
    assert_eq!(budget.usage().members, 2);
    assert_eq!(env.base_path, original_base_path);
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
}

#[test]
fn environment_webfile_member_limit_fails_before_staging_allocations() {
    let web_bytes = build_uncompressed_webfile(vec![
        ("first.bin".to_string(), b"first".to_vec()),
        ("second.bin".to_string(), b"second".to_vec()),
    ]);
    let encoded_len = u64::try_from(web_bytes.len()).unwrap();

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("member-limit.web");
    fs::write(&web_path, web_bytes).unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let mut budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_members: 1,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();

    let error = env
        .load_file(&web_path, &mut budget)
        .expect_err("the complete WebFile member count must be preflighted");

    assert!(error.to_string().contains("members"), "{error:?}");
    assert_eq!(
        budget.usage().bytes,
        encoded_len + u64::try_from("UnityWebData1.0".len()).unwrap()
    );
    assert_eq!(budget.usage().members, 0);
    assert_eq!(env.base_path, original_base_path);
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
    assert!(env.webfiles().is_empty());
}

#[test]
fn environment_webfile_rejects_collapsed_recursive_source_identity() {
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let nested_web = build_uncompressed_webfile(vec![("b".to_string(), bundle.to_vec())]);
    let outer_web = build_uncompressed_webfile(vec![
        ("a".to_string(), nested_web),
        ("a/b".to_string(), bundle.to_vec()),
    ]);
    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("collapsed.web");
    fs::write(&web_path, outer_web).unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();

    let error = env
        .load_file(&web_path, &mut AssetLoadBudget::default())
        .expect_err("flattened recursive sources must not silently alias");

    assert!(
        error.to_string().contains("source identity collision"),
        "{error:?}"
    );
    assert_eq!(env.base_path, original_base_path);
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
}

#[test]
fn environment_webfile_checks_child_depth_before_parsing_child() {
    let entry_name = "nested.ab";
    let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
    let web_bytes = build_uncompressed_webfile(vec![(entry_name.to_string(), bundle.to_vec())]);
    let mut parse_only_budget = AssetLoadBudget::default();
    unity_asset_binary::file::load_unity_file_from_memory_with_budget(
        web_bytes.clone(),
        &mut parse_only_budget,
    )
    .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("depth.web");
    fs::write(&web_path, web_bytes).unwrap();
    let loaded_path = canonicalize_path(web_path.clone());
    let expected_bytes = parse_only_budget
        .usage()
        .bytes
        .checked_add(std::mem::size_of::<usize>() as u64)
        .and_then(|bytes| {
            bytes.checked_add(
                u64::try_from(loaded_path.as_os_str().as_encoded_bytes().len()).unwrap(),
            )
        })
        .unwrap();
    let original_base_path = temp.path().join("original-base");
    let mut env = Environment::new();
    env.base_path = original_base_path.clone();
    let mut budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_depth: 1,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();

    let error = env
        .load_file(&web_path, &mut budget)
        .expect_err("the child container depth must be rejected");

    assert!(error.to_string().contains("recursion budget"), "{error:?}");
    assert_eq!(budget.usage().bytes, expected_bytes);
    assert_eq!(budget.usage().members, 1);
    assert_eq!(budget.usage().max_observed_depth, 1);
    assert_eq!(env.base_path, original_base_path);
    assert!(env.webfiles().is_empty());
    assert!(env.bundles().is_empty());
    assert!(env.binary_assets().is_empty());
}

#[test]
fn environment_loads_extless_webfile_entries_and_reads_resource_bytes() {
    let sample_bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_bytes = fs::read(&sample_bundle_path).unwrap();

    let cab = "00112233445566778899aabbccddeeff";
    let resource_name = format!("CAB-{cab}.resource");
    let mut resource_bytes = vec![0u8; 4096 + 4];
    resource_bytes[4096..4096 + 4].copy_from_slice(b"OggS");

    let entry_name = "char_118_yuki.ab".to_string();
    let web_bytes = build_uncompressed_webfile(vec![
        (entry_name.clone(), bundle_bytes),
        (resource_name.clone(), resource_bytes),
    ]);

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("UnityWebData");
    fs::write(&web_path, web_bytes).unwrap();

    let mut env = Environment::new();
    env.load_file(&web_path, &mut AssetLoadBudget::default())
        .unwrap();
    let web_path = canonicalize_path(web_path);
    assert!(env.webfiles().contains_key(&web_path));

    let bundle_source = BinarySource::WebEntry {
        web_path: Arc::new(web_path.clone()),
        entry_name,
    };
    assert!(env.bundles().contains_key(&bundle_source));

    let obj_ref = env
        .binary_object_infos()
        .find(|r| r.source == &bundle_source && r.source_kind == BinarySourceKind::AssetBundle)
        .expect("web bundle yields at least one object handle");

    let key = obj_ref.key();
    assert_eq!(key.source, bundle_source);

    let stream_path = format!("archive:/CAB-{cab}/{resource_name}");
    let read = env
        .read_stream_data_source(
            &key.source,
            BinarySourceKind::AssetBundle,
            &stream_path,
            4096,
            4,
        )
        .unwrap();
    assert_eq!(read, b"OggS");
}

#[test]
fn environment_save_repacks_webfile_after_editing_embedded_bundle() {
    let sample_bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_bytes = fs::read(&sample_bundle_path).unwrap();

    let entry_name = "char_118_yuki.ab".to_string();
    let web_bytes = build_uncompressed_webfile(vec![(entry_name.clone(), bundle_bytes)]);

    let temp = tempfile::tempdir().unwrap();
    let web_path = temp.path().join("UnityWebData");
    fs::write(&web_path, web_bytes).unwrap();

    let mut env = Environment::new();
    env.load_file(&web_path, &mut AssetLoadBudget::default())
        .unwrap();
    let web_path = canonicalize_path(web_path);

    let bundle_source = BinarySource::WebEntry {
        web_path: Arc::new(web_path.clone()),
        entry_name: entry_name.clone(),
    };

    // Pick a stable object inside the embedded bundle and patch its name.
    let mut chosen: Option<(BinaryObjectKey, String)> = None;
    let mut name_budget = AssetLoadBudget::default();
    for r in env.binary_object_infos() {
        if r.source != &bundle_source || r.source_kind != BinarySourceKind::AssetBundle {
            continue;
        }
        if let Ok(Some(name)) = r.object.peek_name(&mut name_budget)
            && !name.is_empty()
        {
            chosen = Some((r.key(), name));
            break;
        }
    }

    let (key, old_name) = chosen.expect("expected at least one object with a peekable name");
    let new_name = format!("RUST_WEBFILE_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, &mut AssetLoadBudget::default(), |class| {
        if let Some(v) = class.get_mut("m_Name") {
            *v = UnityValue::String(new_name.clone());
        } else if let Some(v) = class.get_mut("name") {
            *v = UnityValue::String(new_name.clone());
        } else {
            return Err(UnityAssetError::format(
                "Chosen object has peekable name but no m_Name/name field",
            ));
        }
        Ok(())
    })
    .unwrap();

    let out_dir = temp.path().join("out");
    env.save(unity_asset_write::PackingPolicy::Preserve, &out_dir)
        .unwrap();

    // UnityPy-style save should rebuild the container, not emit extracted entry files.
    let out_web_path = out_dir.join("UnityWebData");
    assert!(out_web_path.exists());
    assert!(!out_dir.join(&entry_name).exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_web_path, &mut AssetLoadBudget::default())
        .unwrap();
    let out_web_path = canonicalize_path(out_web_path);

    let out_bundle_source = BinarySource::WebEntry {
        web_path: Arc::new(out_web_path),
        entry_name,
    };

    let r2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source == &out_bundle_source
                && r.source_kind == BinarySourceKind::AssetBundle
                && r.asset_index == key.asset_index
                && r.object.path_id() == key.path_id
        })
        .expect("expected edited object handle in repacked webfile bundle");

    let observed = r2
        .object
        .peek_name(&mut AssetLoadBudget::default())
        .unwrap()
        .expect("edited object should still have a name");
    assert_eq!(observed, new_name);
}

#[test]
fn environment_resolve_pptr_path_key_resolves_sprite_texture() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let sprite_ref = env
        .binary_object_infos()
        .find(|r| r.source_kind == BinarySourceKind::AssetBundle && r.object.class_id() == 213)
        .expect("sample bundle contains at least one Sprite");
    let sprite_key = sprite_ref.key();
    let mut budget = AssetLoadBudget::default();

    let resolved = env
        .resolve_pptr_path_key(&sprite_key, "m_RD.texture", &mut budget)
        .unwrap()
        .expect("sprite should reference a texture via m_RD.texture");

    let sprite_obj = env
        .read_binary_object_key(&sprite_key, &mut budget)
        .unwrap();
    let v = super::pptr_path::get_value_at_path(sprite_obj.as_unity_class(), "m_RD.texture")
        .expect("m_RD.texture exists");
    let (_, expected_path_id) = super::pptr_path::read_pptr(v).expect("m_RD.texture is a PPtr");
    assert_eq!(resolved.path_id, expected_path_id);

    let texture = env.read_binary_object_key(&resolved, &mut budget).unwrap();
    assert_eq!(texture.class_id(), 28, "expected Texture2D target");
}

#[test]
fn environment_can_set_pptr_path_to_key_and_reload() {
    use unity_asset_write::PackingPolicy;

    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );
    env.load_file(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let sprite_key = env
        .binary_object_infos()
        .find(|r| r.source_kind == BinarySourceKind::AssetBundle && r.object.class_id() == 213)
        .expect("sample bundle contains at least one Sprite")
        .key();
    let atlas_key = env
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle && r.object.class_id() == 687078895
        })
        .expect("sample bundle contains a SpriteAtlas")
        .key();

    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("atlas_test"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path, &mut AssetLoadBudget::default())
        .unwrap();
    let sprite_ref = env2
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_obj = env2
        .read_binary_object_key(&sprite_ref.key(), &mut AssetLoadBudget::default())
        .unwrap();

    let atlas_ref =
        super::pptr_path::get_value_at_path(sprite_obj.as_unity_class(), "m_SpriteAtlas")
            .expect("m_SpriteAtlas present");
    let (file_id, path_id) =
        super::pptr_path::read_pptr(atlas_ref).expect("m_SpriteAtlas is a PPtr");
    assert_eq!(file_id, 0);
    assert_eq!(path_id, atlas_key.path_id);
}

#[test]
fn environment_set_pptr_path_to_key_adds_external_when_cross_source() {
    use unity_asset_write::PackingPolicy;

    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );

    env.load_file(&banner_path, &mut AssetLoadBudget::default())
        .unwrap();
    env.load_file(&atlas_path, &mut AssetLoadBudget::default())
        .unwrap();

    let sprite_key = env
        .binary_object_infos()
        .find(|r| r.source == &BinarySource::path(&banner_path) && r.object.class_id() == 213)
        .expect("banner_1 bundle contains a Sprite")
        .key();
    let atlas_key = env
        .binary_object_infos()
        .find(|r| r.source == &BinarySource::path(&atlas_path) && r.object.class_id() == 687078895)
        .expect("atlas_test bundle contains a SpriteAtlas")
        .key();

    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    let (file_id, _) = session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();
    assert!(file_id > 0);

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("banner_1"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path, &mut AssetLoadBudget::default())
        .unwrap();
    let sprite_ref = env2
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_obj = env2
        .read_binary_object_key(&sprite_ref.key(), &mut AssetLoadBudget::default())
        .unwrap();

    let atlas_ref =
        super::pptr_path::get_value_at_path(sprite_obj.as_unity_class(), "m_SpriteAtlas")
            .expect("m_SpriteAtlas present");
    let (saved_file_id, saved_path_id) =
        super::pptr_path::read_pptr(atlas_ref).expect("m_SpriteAtlas is a PPtr");
    assert_eq!(saved_file_id, file_id);
    assert_eq!(saved_path_id, atlas_key.path_id);

    let bundle = env2
        .bundles()
        .get(&BinarySource::path(&out_bundle_path))
        .expect("saved bundle loaded");
    let sf = bundle
        .assets
        .first()
        .expect("bundle has at least one asset");
    let external = sf
        .externals
        .iter()
        .find(|external| external.path == "atlas_test")
        .expect("expected added external entry for cross-source PPtr");
    assert_eq!(external.guid, [0; 16]);
    assert_eq!(external.type_, 0);
}

#[test]
fn environment_external_budget_failure_does_not_publish_edit_state() {
    fn fixture() -> (Environment, BinaryObjectKey, BinaryObjectKey) {
        let mut env = Environment::new();
        let banner_path = canonicalize_path(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
        );
        let atlas_path = canonicalize_path(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
        );
        env.load_file(&banner_path, &mut AssetLoadBudget::default())
            .unwrap();
        env.load_file(&atlas_path, &mut AssetLoadBudget::default())
            .unwrap();
        let context = env
            .binary_object_infos()
            .find(|reference| {
                reference.source == &BinarySource::path(&banner_path)
                    && reference.object.class_id() == 213
            })
            .unwrap()
            .key();
        let target = env
            .binary_object_infos()
            .find(|reference| {
                reference.source == &BinarySource::path(&atlas_path)
                    && reference.object.class_id() == 687078895
            })
            .unwrap()
            .key();
        (env, context, target)
    }

    let (mut probe, context, target) = fixture();
    let mut measured = AssetLoadBudget::default();
    probe
        .edit_session(&mut measured)
        .file_id_for_target(&context, &target)
        .unwrap();
    let required = measured.usage().bytes;
    assert!(required > 0);

    let (mut env, context, target) = fixture();
    let mut short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: required - 1,
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .edit_session(&mut short)
        .file_id_for_target(&context, &target)
        .unwrap_err();

    assert!(budget_error_in_chain(&error).is_some(), "{error:?}");
    assert!(env.write_state.is_empty());
}

#[test]
fn environment_reuses_path_only_external_metadata_without_publishing_empty_state() {
    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );
    env.load_file(&banner_path, &mut AssetLoadBudget::default())
        .unwrap();
    env.load_file(&atlas_path, &mut AssetLoadBudget::default())
        .unwrap();

    let context = env
        .binary_object_infos()
        .find(|reference| {
            reference.source == &BinarySource::path(&banner_path)
                && reference.object.class_id() == 213
        })
        .unwrap()
        .key();
    let target = env
        .binary_object_infos()
        .find(|reference| {
            reference.source == &BinarySource::path(&atlas_path)
                && reference.object.class_id() == 687078895
        })
        .unwrap()
        .key();

    let expected_file_id = {
        let file = match context.source_kind {
            BinarySourceKind::SerializedFile => env
                .binary_assets
                .get_mut(&context.source)
                .expect("standalone context file is loaded"),
            BinarySourceKind::AssetBundle => env
                .bundles
                .get_mut(&context.source)
                .and_then(|bundle| bundle.assets.get_mut(context.asset_index.unwrap()))
                .expect("bundle context file is loaded"),
        };
        file.externals
            .push(unity_asset_binary::asset::FileIdentifier {
                temp_empty: String::new(),
                guid: [0x7a; 16],
                type_: 1,
                path: "atlas_test".to_owned(),
            });
        i32::try_from(file.externals.len()).unwrap()
    };

    let file_id = env
        .edit_session(&mut AssetLoadBudget::default())
        .file_id_for_target(&context, &target)
        .unwrap();

    assert_eq!(file_id, expected_file_id);
    assert!(!env.has_pending_writes());
}

#[test]
fn environment_object_edit_failure_does_not_publish_external_state() {
    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );
    env.load_file(&banner_path, &mut AssetLoadBudget::default())
        .unwrap();
    env.load_file(&atlas_path, &mut AssetLoadBudget::default())
        .unwrap();
    let context = env
        .binary_object_infos()
        .find(|reference| {
            reference.source == &BinarySource::path(&banner_path)
                && reference.object.class_id() == 213
        })
        .unwrap()
        .key();
    let target = env
        .binary_object_infos()
        .find(|reference| {
            reference.source == &BinarySource::path(&atlas_path)
                && reference.object.class_id() == 687078895
        })
        .unwrap()
        .key();

    let error = env
        .set_pptr_path_to_key(
            &context,
            "invalid[path",
            &target,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

    assert!(error.to_string().contains("missing ']'"), "{error:?}");
    assert!(env.write_state.is_empty());
}

#[test]
fn environment_resolve_pptr_path_key_best_effort_loads_external_bundle_from_subdir() {
    use unity_asset_write::PackingPolicy;

    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );

    env.load_file(&banner_path, &mut AssetLoadBudget::default())
        .unwrap();
    env.load_file(&atlas_path, &mut AssetLoadBudget::default())
        .unwrap();

    let sprite_key = env
        .binary_object_infos()
        .find(|r| r.source == &BinarySource::path(&banner_path) && r.object.class_id() == 213)
        .expect("banner_1 bundle contains a Sprite")
        .key();
    let atlas_key = env
        .binary_object_infos()
        .find(|r| r.source == &BinarySource::path(&atlas_path) && r.object.class_id() == 687078895)
        .expect("atlas_test bundle contains a SpriteAtlas")
        .key();

    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    let (file_id, _) = session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();
    assert!(file_id > 0);

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session.save(PackingPolicy::Preserve, &out_dir).unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("banner_1"));
    assert!(out_bundle_path.exists());

    // Place the external dependency in a nested folder to force the `find_file`-style directory scan.
    let deps_dir = out_dir.join("deps");
    std::fs::create_dir_all(&deps_dir).unwrap();
    let atlas_copy_path = deps_dir.join("atlas_test");

    let mut missing_env = Environment::new();
    missing_env
        .load_file(&out_bundle_path, &mut AssetLoadBudget::default())
        .unwrap();

    let sprite_ref = missing_env
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_key2 = sprite_ref.key();

    let mut missing_budget = AssetLoadBudget::default();
    let mut missing_session = missing_env.edit_session(&mut missing_budget);
    assert!(
        missing_session
            .resolve_pptr_path_key(&sprite_key2, "m_SpriteAtlas")
            .unwrap()
            .is_none()
    );

    std::fs::copy(&atlas_path, &atlas_copy_path).unwrap();
    let atlas_copy_path = canonicalize_path(atlas_copy_path);
    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path, &mut AssetLoadBudget::default())
        .unwrap();
    let mut constrained_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: std::fs::metadata(&atlas_copy_path).unwrap().len(),
        ..unity_asset_core::AssetLoadLimits::default()
    })
    .unwrap();
    let mut constrained_session = env2.edit_session(&mut constrained_budget);
    let error = constrained_session
        .resolve_pptr_path_key(&sprite_key2, "m_SpriteAtlas")
        .expect_err("dependency loading must use the caller-owned budget");
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut is_resource_error = false;
    while let Some(current) = source {
        if current
            .downcast_ref::<unity_asset_binary::error::BinaryError>()
            .is_some_and(unity_asset_binary::error::BinaryError::is_resource_error)
        {
            is_resource_error = true;
            break;
        }
        source = current.source();
    }
    assert!(is_resource_error, "unexpected error chain: {error:?}");
    assert!(
        !env2
            .bundles()
            .contains_key(&BinarySource::path(&atlas_copy_path))
    );

    std::fs::write(&atlas_copy_path, b"not a Unity binary").unwrap();
    let warning_count = env2.warnings().len();
    let mut invalid_format_budget = AssetLoadBudget::default();
    let mut invalid_format_session = env2.edit_session(&mut invalid_format_budget);
    assert!(
        invalid_format_session
            .resolve_pptr_path_key(&sprite_key2, "m_SpriteAtlas")
            .unwrap()
            .is_none()
    );
    let warnings = env2.warnings();
    assert_eq!(warnings.len(), warning_count + 1);
    assert!(matches!(
        warnings.last(),
        Some(EnvironmentWarning::LoadFailed { path, .. }) if path == &atlas_copy_path
    ));

    std::fs::copy(&atlas_path, &atlas_copy_path).unwrap();
    let mut edit_budget2 = AssetLoadBudget::default();
    let mut session2 = env2.edit_session(&mut edit_budget2);
    let resolved = session2
        .resolve_pptr_path_key(&sprite_key2, "m_SpriteAtlas")
        .unwrap()
        .expect("sprite should reference a SpriteAtlas via external PPtr");

    assert_eq!(resolved.path_id, atlas_key.path_id);
    assert_eq!(resolved.source_kind, BinarySourceKind::AssetBundle);
    assert_eq!(resolved.source, BinarySource::path(&atlas_copy_path));
    assert!(
        env2.bundles()
            .contains_key(&BinarySource::path(&atlas_copy_path))
    );
}

#[test]
fn pptr_path_supports_array_indices() {
    let mut class = UnityClass::new(0, "Test".to_string(), "0".to_string());
    class.set("m_Materials".to_string(), UnityValue::Array(Vec::new()));

    super::pptr_path::write_pptr_at_path(&mut class, "m_Materials[1]", 0, 42).unwrap();

    let v = super::pptr_path::get_value_at_path(&class, "m_Materials[1]")
        .expect("m_Materials[1] exists");
    let (file_id, path_id) = super::pptr_path::read_pptr(v).expect("element is a PPtr");
    assert_eq!(file_id, 0);
    assert_eq!(path_id, 42);
}

#[test]
fn environment_can_edit_yaml_prefab_by_anchor_and_save() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Old
  m_Component:
  - component: {fileID: 100001}
--- !u!4 &100001
Transform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path, &mut AssetLoadBudget::default())
        .unwrap();

    let mut edit_budget = AssetLoadBudget::default();
    let mut session = env.edit_session(&mut edit_budget);
    session
        .set_yaml_value_at_path(
            &prefab_path,
            "100000",
            "m_Name",
            UnityValue::String("New".to_string()),
        )
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(unity_asset_write::PackingPolicy::Preserve, &out_dir)
        .unwrap();

    let out_prefab = out_dir.join("ui.prefab");
    assert!(out_prefab.exists());

    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();
    let go = doc.get(Some("GameObject"), Some(&["m_Name"])).unwrap();
    assert_eq!(go.get("m_Name").and_then(|v| v.as_str()), Some("New"));
}
