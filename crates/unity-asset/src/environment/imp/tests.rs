use super::*;
use std::fs;
use std::path::Path;
use std::process::Command;

fn canonicalize_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
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

#[test]
fn environment_loads_yaml_fixture() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../unity-asset-yaml/tests/fixtures/SingleDoc.asset"),
    );
    env.load_file(&path).unwrap();
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
    env.load_file(&path).unwrap();
    assert!(!env.bundles().is_empty());

    let first = env
        .bundles()
        .values()
        .next()
        .and_then(|b| b.assets.first())
        .and_then(|a| a.objects.first())
        .expect("bundle has at least one object");

    let found = env.find_binary_objects(first.path_id);
    assert!(!found.is_empty());

    // Disambiguation helpers should work on the same source path.
    assert!(
        env.find_binary_object_in_source(&path, first.path_id)
            .is_some()
    );
    let obj_ref = env
        .find_binary_object_in_bundle_asset(&path, 0, first.path_id)
        .expect("can find object in bundle asset 0");

    let key = obj_ref.key();
    assert_eq!(key.source, BinarySource::path(&path));
    assert_eq!(key.source_kind, BinarySourceKind::AssetBundle);
    assert_eq!(key.asset_index, Some(0));
    assert_eq!(key.path_id, first.path_id);

    let key_str = key.to_string();
    let parsed: BinaryObjectKey = key_str.parse().expect("BinaryObjectKey parse");
    assert_eq!(parsed, key);

    let parsed = env.read_binary_object_key(&key).unwrap();
    assert_eq!(parsed.info.path_id, first.path_id);

    let keys = env.find_binary_object_keys(first.path_id);
    assert!(!keys.is_empty());

    let keys_in_source = env.find_binary_object_keys_in_source(&path, first.path_id);
    assert!(keys_in_source.contains(&key));

    // PPtr resolution closure:
    // fileID=0 must resolve to the current serialized file (same source + asset_index).
    let pptr_key = env
        .resolve_binary_pptr(&obj_ref, 0, first.path_id)
        .expect("resolve PPtr with fileID=0");
    assert_eq!(pptr_key, key);

    let pptr_obj = env.read_binary_pptr(&obj_ref, 0, first.path_id).unwrap();
    assert_eq!(pptr_obj.info.path_id, first.path_id);

    // If externals are present, pick an out-of-range fileID; otherwise use 1.
    let invalid_file_id = if obj_ref.object.file().externals.is_empty() {
        1
    } else {
        (obj_ref.object.file().externals.len() as i32) + 1
    };
    assert!(
        env.resolve_binary_pptr(&obj_ref, invalid_file_id, first.path_id)
            .is_none()
    );

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&path))
        .expect("sample bundle loaded");
    let has_assetbundle_object = bundle
        .assets
        .iter()
        .any(|f| f.objects.iter().any(|o| o.type_id == 142));
    assert!(
        has_assetbundle_object,
        "expected at least one AssetBundle (class id 142) object in sample bundle"
    );

    let entries = env.bundle_container_entries(&path).unwrap();
    assert!(
        !entries.is_empty(),
        "expected at least one m_Container entry in sample bundle"
    );
    assert!(entries.iter().any(|e| !e.asset_path.is_empty()));
    assert!(entries.iter().any(|e| e.key.is_some()));

    let found = env.find_bundle_container_entries(&entries[0].asset_path);
    assert!(!found.is_empty());

    let file_name = entries[0]
        .asset_path
        .rsplit('/')
        .next()
        .unwrap_or(&entries[0].asset_path);
    let glob = format!("*{}*", file_name);
    let found_glob = env.find_bundle_container_entries(&glob);
    assert!(
        !found_glob.is_empty(),
        "glob pattern should match at least one container entry"
    );

    let entries = env.bundle_container_entries(&path).unwrap();
    let cn_001 = entries
        .iter()
        .find(|e| e.asset_path.to_ascii_lowercase().ends_with("/cn_001.ogg"))
        .expect("sample bundle contains cn_001.ogg container entry");
    let key = cn_001
        .key
        .clone()
        .expect("cn_001.ogg container entry resolves to an object key");

    let obj = env.read_binary_object_key(&key).unwrap();

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

    let peek = env.peek_binary_object_name(&key).unwrap();
    assert_eq!(peek, obj.name());
}

#[test]
fn environment_can_edit_binary_object_and_save_bundle() {
    use unity_asset_write::{PackerOptions, UnityPyPacker};

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
    env.load_file(&in_path).unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&in_path))
        .expect("sample bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| h.peek_name().ok().flatten().map(|n| (h.path_id(), n)))
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: BinarySource::path(&in_path),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let new_name = format!("RUST_ENV_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, |class| {
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

    env.save(
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
        &out_dir,
    )
    .unwrap();

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
    let saved_name = saved_obj.peek_name().unwrap().unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_edit_session_can_set_binary_value_at_path_and_save_bundle() {
    use unity_asset_write::{PackerOptions, UnityPyPacker};

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
    env.load_file(&in_path).unwrap();

    let bundle = env
        .bundles()
        .get(&BinarySource::path(&in_path))
        .expect("sample bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| h.peek_name().ok().flatten().map(|n| (h.path_id(), n)))
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: BinarySource::path(&in_path),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let obj = env.read_binary_object_key(&key).unwrap();
    let class = obj.as_unity_class();
    let field_name = if class.get("m_Name").is_some() {
        "m_Name"
    } else if class.get("name").is_some() {
        "name"
    } else {
        return;
    };

    let new_name = format!("RUST_ENV_SET_PATH_{}", old_name);
    let mut session = env.edit_session();
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

    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

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
    let saved_name = saved_obj.peek_name().unwrap().unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_dependency_graph_builds_and_closure_from_container_is_non_empty() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    env.load_file(&path).unwrap();

    let graph = env.build_dependency_graph(DependencyGraphBuildOptions::default());
    assert!(!graph.nodes().is_empty());

    let entries = env.bundle_container_entries(&path).unwrap();
    let roots: Vec<_> = entries.into_iter().filter_map(|e| e.key).take(8).collect();
    assert!(!roots.is_empty());

    let roots_from_helper = env.bundle_container_root_keys("Assets/", Some(8));
    assert!(!roots_from_helper.is_empty());

    let closure = graph.internal_closure(&roots, Some(2), Some(10_000));
    assert!(
        !closure.is_empty(),
        "expected at least one reachable node from container roots"
    );

    let closure = graph.closure_with_options(
        &roots,
        DependencyGraphTraversalOptions {
            max_depth: Some(2),
            max_nodes: Some(10_000),
            follow_resolved_external: true,
        },
    );
    assert!(!closure.is_empty());

    let dot = graph.to_dot(10_000, true);
    assert!(dot.contains("digraph"));
}

#[test]
fn environment_dependency_graph_can_rebuild_single_source_subgraph() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    env.load_file(&path).unwrap();

    let source = BinarySource::path(&path);
    let bundle = env.bundles().get(&source).expect("bundle loaded");
    let file = bundle.assets.first().expect("bundle has asset 0");

    let sub = env
        .build_dependency_graph_for_source(
            &source,
            BinarySourceKind::AssetBundle,
            Some(0),
            DependencyGraphBuildOptions::default(),
        )
        .unwrap();
    assert_eq!(sub.nodes().len(), file.objects.len());

    env.invalidate_dependency_scan_cache_for_source(&source, BinarySourceKind::AssetBundle, None);
    let sub2 = env
        .build_dependency_graph_for_source(
            &source,
            BinarySourceKind::AssetBundle,
            Some(0),
            DependencyGraphBuildOptions::default(),
        )
        .unwrap();
    assert_eq!(sub2.nodes().len(), file.objects.len());
}

#[test]
fn environment_can_find_binary_pptr_references_to_target_key() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    env.load_file(&path).unwrap();

    let graph = env.build_dependency_graph(DependencyGraphBuildOptions::default());
    let mut picked: Option<(BinaryObjectKey, BinaryObjectKey)> = None;

    for from in graph.nodes() {
        if let Some(to) = graph.internal_refs_from(from).first() {
            picked = Some((from.clone(), to.clone()));
            break;
        }
    }

    let Some((from, target)) = picked else {
        return;
    };

    let refs = env
        .find_binary_pptr_references_to(&target, PptrReferenceSearchOptions::default())
        .unwrap();

    assert!(
        refs.iter()
            .any(|r| r.from == from && r.resolved.as_ref() == Some(&target)),
        "expected at least one resolved reference from a known dependency edge"
    );
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
    env.load_file(&meta_path).unwrap();

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
    let stats = env.load_project(root, options).unwrap();

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
    env.load_file(&path).unwrap();

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
        file.types
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
        file.enable_type_tree = false;
        for t in file.types.iter_mut() {
            t.type_tree.clear();
        }
        file.set_type_tree_registry(None);
    }

    let obj = env.read_binary_object_key(&key).unwrap();
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

    env.set_type_tree_registry_from_paths(&[reg_path]).unwrap();

    let obj = env.read_binary_object_key(&key).unwrap();
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
    assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
    assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));
}

#[test]
fn environment_can_edit_and_save_stripped_assets_with_typetree_registry() {
    use serde::Serialize;
    use unity_asset_binary::typetree::JsonTypeTreeRegistry;
    use unity_asset_write::{PackerOptions, UnityPyPacker};

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
    env.load_file(&path).unwrap();

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
        file.types
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
        file.enable_type_tree = false;
        for t in file.types.iter_mut() {
            t.type_tree.clear();
        }
        file.set_type_tree_registry(None);
    }

    let obj = env.read_binary_object_key(&key).unwrap();
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

    env.set_type_tree_registry_from_paths(&[reg_path.clone()])
        .unwrap();

    env.edit_binary_object_key(&key, |class| {
        class.set(
            "m_Name".to_string(),
            UnityValue::String("banner_1_edited".to_string()),
        );
        Ok(())
    })
    .unwrap();

    let out_dir = tmp.path().join("out");
    env.save(
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
        &out_dir,
    )
    .unwrap();

    let out_path = out_dir.join("banner_1");
    assert!(out_path.is_file());

    let mut saved_bundle =
        unity_asset_binary::bundle::BundleParser::from_bytes(std::fs::read(&out_path).unwrap())
            .unwrap();
    let reg = std::sync::Arc::new(JsonTypeTreeRegistry::from_path(&reg_path).unwrap());

    let file = saved_bundle.assets.first_mut().expect("bundle has asset 0");
    file.set_type_tree_registry(Some(reg));

    let saved = file
        .find_object_handle(texture_path_id)
        .expect("edited object exists after save")
        .read()
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
    env.load_file(&split0).unwrap();

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

    let entries = env.bundle_container_entries_source(&source).unwrap();
    assert!(!entries.is_empty());
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
    env.load_file(&zip_path).unwrap();

    let source = BinarySource::ArchiveEntry {
        archive_path: zip_path.clone(),
        entry_name: "inner/char_118_yuki.ab".to_string(),
    };

    let entries = env.bundle_container_entries_source(&source).unwrap();
    assert!(!entries.is_empty());
}

#[test]
fn environment_can_edit_zip_assetbundle_entry_and_save() {
    use std::io::Write;
    use unity_asset_write::{PackerOptions, UnityPyPacker};
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
    env.load_file(&zip_path).unwrap();

    let source = BinarySource::ArchiveEntry {
        archive_path: zip_path.clone(),
        entry_name: "inner/char_118_yuki.ab".to_string(),
    };

    let bundle = env.bundles().get(&source).expect("zip bundle loaded");
    let sf = bundle.assets.first().expect("bundle has asset 0");

    let (path_id, old_name) = sf
        .object_handles()
        .filter_map(|h| h.peek_name().ok().flatten().map(|n| (h.path_id(), n)))
        .find(|(_id, name)| !name.is_empty())
        .expect("expected at least one object with peekable name in sample");

    let key = BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::AssetBundle,
        asset_index: Some(0),
        path_id,
    };

    let new_name = format!("RUST_ZIP_ENV_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, |class| {
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

    env.save(
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
        &out_dir,
    )
    .unwrap();

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
    let saved_name = saved_obj.peek_name().unwrap().unwrap();
    assert_eq!(saved_name, new_name);
}

#[test]
fn environment_assetbundle_container_raw_matches_typetree_when_stripped() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/xinzexi_2_n_tex"),
    );
    env.load_file(&path).unwrap();

    let baseline = env.bundle_container_entries(&path).unwrap();
    assert!(
        !baseline.is_empty(),
        "expected at least one m_Container entry in sample bundle"
    );

    let source = BinarySource::path(&path);
    {
        let bundle = env
            .bundles
            .get_mut(&source)
            .expect("sample bundle loaded (mutable)");
        for file in bundle.assets.iter_mut() {
            file.enable_type_tree = false;
            for t in file.types.iter_mut() {
                t.type_tree.clear();
            }
            file.set_type_tree_registry(None);
        }
    }
    env.bundle_container_cache.write().unwrap().remove(&source);

    let stripped = env.bundle_container_entries(&path).unwrap();
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
    env.load_file(&path).unwrap();

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
fn environment_object_graph_scans_yaml_pptrs_and_meta_guid_paths() {
    let temp = tempfile::tempdir().unwrap();
    let script_asset_path = temp.path().join("MyScript.asset");
    let script_meta_path = temp.path().join("MyScript.asset.meta");

    std::fs::write(&script_asset_path, b"not a real asset").unwrap();
    std::fs::write(
        &script_meta_path,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    let prefab_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab");
    let prefab_path = canonicalize_path(prefab_path);

    let mut env = Environment::new();
    env.load_file(&script_meta_path).unwrap();
    env.load_file(&prefab_path).unwrap();

    let graph = env.build_object_graph(ObjectGraphBuildOptions {
        include_yaml: true,
        include_binary: false,
        binary: DependencyGraphBuildOptions::default(),
    });

    let from = EnvironmentObjectKey::Yaml(YamlObjectKey {
        path: prefab_path.clone(),
        anchor: "1003".to_string(),
    });
    let to = EnvironmentObjectKey::Yaml(YamlObjectKey {
        path: prefab_path.clone(),
        anchor: "1001".to_string(),
    });
    assert!(
        graph.internal_refs_from(&from).contains(&to),
        "expected MonoBehaviour (1003) to reference GameObject (1001)"
    );

    let exts = graph.external_refs_from(&from);
    let yaml_ext = exts
        .iter()
        .find_map(|e| match e {
            ExternalObjectEdge::Yaml(y) if y.guid.is_some() => Some(y),
            _ => None,
        })
        .expect("expected at least one YAML external edge with a GUID");

    assert_eq!(yaml_ext.file_id, 11500000);
    assert_eq!(
        yaml_ext.asset_path,
        Some(canonicalize_path(script_asset_path))
    );
    assert_eq!(yaml_ext.resolved, None);
}

#[test]
fn environment_can_find_yaml_pptr_references_to_yaml_anchor_with_paths() {
    let prefab_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab");
    let prefab_path = canonicalize_path(prefab_path);

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let target = EnvironmentObjectKey::Yaml(YamlObjectKey {
        path: prefab_path.clone(),
        anchor: "1001".to_string(),
    });

    let refs = env
        .find_yaml_pptr_references_to(&target, YamlPptrReferenceSearchOptions::default())
        .unwrap();

    assert!(
        refs.iter().any(|r| {
            r.from.path == prefab_path && r.from.anchor == "1002" && r.pptr_path == "m_GameObject"
        }),
        "expected Transform (1002) to reference GameObject (1001) at m_GameObject"
    );
    assert!(
        refs.iter().any(|r| {
            r.from.path == prefab_path && r.from.anchor == "1003" && r.pptr_path == "m_GameObject"
        }),
        "expected MonoBehaviour (1003) to reference GameObject (1001) at m_GameObject"
    );
}

#[test]
fn environment_can_find_yaml_pptr_references_to_binary_object_with_paths() {
    use unity_asset_binary::bundle::load_bundle;
    use unity_asset_binary::file::load_unity_file_from_memory;

    let bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle = load_bundle(&bundle_path).unwrap();

    let mut extracted: Option<(Vec<u8>, i64)> = None;
    for name in bundle.file_names() {
        let Some(info) = bundle.find_file(name) else {
            continue;
        };
        let bytes = bundle.extract_file_data(info).unwrap();
        let Ok(unity_file) = load_unity_file_from_memory(bytes.clone()) else {
            continue;
        };
        let Some(file) = unity_file.as_serialized() else {
            continue;
        };
        let Some(first) = file.objects.first() else {
            continue;
        };
        if first.path_id == 0 {
            continue;
        }
        extracted = Some((bytes, first.path_id));
        break;
    }

    let (bytes, target_path_id) = extracted.expect("bundle contains at least one SerializedFile");

    let temp = tempfile::tempdir().unwrap();
    let target_path = temp.path().join("Target.assets");
    let target_meta = temp.path().join("Target.assets.meta");
    let yaml_path = temp.path().join("Ref.prefab");

    std::fs::write(&target_path, &bytes).unwrap();
    std::fs::write(
        &target_meta,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    let yaml = format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &1\nMonoBehaviour:\n  m_Ref: {{fileID: {}, guid: 0123456789abcdef0123456789abcdef, type: 2}}\n",
        target_path_id
    );
    std::fs::write(&yaml_path, yaml.as_bytes()).unwrap();

    let target_path = canonicalize_path(target_path);
    let target_meta = canonicalize_path(target_meta);
    let yaml_path = canonicalize_path(yaml_path);

    let mut env = Environment::new();
    env.load_file(&target_meta).unwrap();
    env.load_file(&target_path).unwrap();
    env.load_file(&yaml_path).unwrap();

    let target = EnvironmentObjectKey::Binary(BinaryObjectKey {
        source: BinarySource::path(&target_path),
        source_kind: BinarySourceKind::SerializedFile,
        asset_index: None,
        path_id: target_path_id,
    });

    let refs = env
        .find_yaml_pptr_references_to(&target, YamlPptrReferenceSearchOptions::default())
        .unwrap();

    assert!(
        refs.iter().any(|r| {
            r.from.path == yaml_path && r.from.anchor == "1" && r.pptr_path == "m_Ref"
        }),
        "expected Ref.prefab &1 to reference the target at m_Ref"
    );
    assert!(
        refs.iter().any(|r| r.resolved.as_ref() == Some(&target)),
        "expected at least one resolved reference to the binary target"
    );
}

#[test]
fn environment_object_graph_resolves_yaml_guid_to_loaded_serialized_file_object() {
    use unity_asset_binary::bundle::load_bundle;
    use unity_asset_binary::file::load_unity_file_from_memory;

    let bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle = load_bundle(&bundle_path).unwrap();

    let mut extracted: Option<(Vec<u8>, i64)> = None;
    for name in bundle.file_names() {
        let Some(info) = bundle.find_file(name) else {
            continue;
        };
        let bytes = bundle.extract_file_data(info).unwrap();
        let Ok(unity_file) = load_unity_file_from_memory(bytes.clone()) else {
            continue;
        };
        let Some(file) = unity_file.as_serialized() else {
            continue;
        };
        let Some(first) = file.objects.first() else {
            continue;
        };
        if first.path_id == 0 {
            continue;
        }
        extracted = Some((bytes, first.path_id));
        break;
    }

    let (bytes, target_path_id) = extracted.expect("bundle contains at least one SerializedFile");

    let temp = tempfile::tempdir().unwrap();
    let target_path = temp.path().join("Target.assets");
    let target_meta = temp.path().join("Target.assets.meta");

    std::fs::write(&target_path, &bytes).unwrap();
    std::fs::write(
        &target_meta,
        b"fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .unwrap();

    let yaml_path = temp.path().join("Ref.prefab");
    let yaml = format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &1\nMonoBehaviour:\n  m_Ref: {{fileID: {}, guid: 0123456789abcdef0123456789abcdef, type: 2}}\n",
        target_path_id
    );
    std::fs::write(&yaml_path, yaml.as_bytes()).unwrap();

    let target_path = canonicalize_path(target_path);
    let target_meta = canonicalize_path(target_meta);
    let yaml_path = canonicalize_path(yaml_path);

    let mut env = Environment::new();
    env.load_file(&target_meta).unwrap();
    env.load_file(&target_path).unwrap();
    env.load_file(&yaml_path).unwrap();

    let graph = env.build_object_graph(ObjectGraphBuildOptions::default());

    let from = EnvironmentObjectKey::Yaml(YamlObjectKey {
        path: yaml_path.clone(),
        anchor: "1".to_string(),
    });
    let exts = graph.external_refs_from(&from);
    let yaml_ext = exts
        .iter()
        .find_map(|e| match e {
            ExternalObjectEdge::Yaml(y) if y.guid.is_some() => Some(y),
            _ => None,
        })
        .expect("expected at least one YAML external edge with a GUID");

    let expected = BinaryObjectKey {
        source: BinarySource::path(&target_path),
        source_kind: BinarySourceKind::SerializedFile,
        asset_index: None,
        path_id: target_path_id,
    };
    assert_eq!(
        yaml_ext.resolved,
        Some(EnvironmentObjectKey::Binary(expected))
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
    env.load_file(&path).unwrap();

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
fn environment_stream_data_falls_back_to_filesystem_for_bundles() {
    let temp = tempfile::tempdir().unwrap();
    let bundle_src = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_path = temp.path().join("char_118_yuki.ab");
    link_or_copy_file(&bundle_src, &bundle_path).unwrap();

    let cab = "8579bc75d50073df38987733a7cb3193";
    let stream_path = format!("archive:/CAB-{cab}/CAB-{cab}.resource");
    let resource_dir = temp.path().join(format!("CAB-{cab}"));
    fs::create_dir_all(&resource_dir).unwrap();
    let resource_path = resource_dir.join(format!("CAB-{cab}.resource"));

    let mut bytes = vec![0u8; 4096 + 4];
    bytes[4096..4096 + 4].copy_from_slice(b"OggS");
    fs::write(&resource_path, bytes).unwrap();

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

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
fn environment_loads_extless_webfile_entries_and_reads_resource_bytes() {
    let sample_bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_bytes = fs::read(&sample_bundle_path).unwrap();

    let cab = "8579bc75d50073df38987733a7cb3193";
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
    env.load_file(&web_path).unwrap();
    let web_path = canonicalize_path(web_path);
    assert!(env.webfiles().contains_key(&web_path));

    let bundle_source = BinarySource::WebEntry {
        web_path: web_path.clone(),
        entry_name,
    };
    assert!(env.bundles().contains_key(&bundle_source));

    let obj_ref = env
        .binary_object_infos()
        .find(|r| r.source == &bundle_source && r.source_kind == BinarySourceKind::AssetBundle)
        .expect("web bundle yields at least one object handle");

    let key = obj_ref.key();
    assert_eq!(key.source, bundle_source);

    let key_str = key.to_string();
    let parsed: BinaryObjectKey = key_str.parse().expect("BinaryObjectKey parse");
    assert_eq!(parsed, key);

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
    env.load_file(&web_path).unwrap();
    let web_path = canonicalize_path(web_path);

    let bundle_source = BinarySource::WebEntry {
        web_path: web_path.clone(),
        entry_name: entry_name.clone(),
    };

    // Pick a stable object inside the embedded bundle and patch its name.
    let mut chosen: Option<(BinaryObjectKey, String)> = None;
    for r in env.binary_object_infos() {
        if r.source != &bundle_source || r.source_kind != BinarySourceKind::AssetBundle {
            continue;
        }
        if let Ok(Some(name)) = r.object.peek_name() {
            if !name.is_empty() {
                chosen = Some((r.key(), name));
                break;
            }
        }
    }

    let (key, old_name) = chosen.expect("expected at least one object with a peekable name");
    let new_name = format!("RUST_WEBFILE_SAVE_{}", old_name);

    env.edit_binary_object_key(&key, |class| {
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
    env.save(
        unity_asset_write::PackerOptions {
            packer: unity_asset_write::UnityPyPacker::Original,
        },
        &out_dir,
    )
    .unwrap();

    // UnityPy-style save should rebuild the container, not emit extracted entry files.
    let out_web_path = out_dir.join("UnityWebData");
    assert!(out_web_path.exists());
    assert!(!out_dir.join(&entry_name).exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_web_path).unwrap();
    let out_web_path = canonicalize_path(out_web_path);

    let out_bundle_source = BinarySource::WebEntry {
        web_path: out_web_path,
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
        .peek_name()
        .unwrap()
        .expect("edited object should still have a name");
    assert_eq!(observed, new_name);
}

#[test]
fn environment_can_write_streamed_resource_cab_into_bundle_and_reload() {
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );

    let mut env = Environment::new();
    env.load_file(&path).unwrap();

    let bundle_source = BinarySource::path(&path);

    let key = env
        .binary_object_infos()
        .find(|r| r.source == &bundle_source && r.source_kind == BinarySourceKind::AssetBundle)
        .expect("expected at least one binary object in sample bundle")
        .key();

    let mut session = env.edit_session();
    let write = session.write_to_cab(&key, None, b"OggS").unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join("char_118_yuki.ab");
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let bytes = env2
        .read_stream_data(
            &out_bundle_path,
            BinarySourceKind::AssetBundle,
            &write.path,
            write.offset,
            write.size,
        )
        .unwrap();
    assert_eq!(bytes, b"OggS");

    // The cab should be present as a bundle node after saving.
    let out_source = BinarySource::path(canonicalize_path(out_bundle_path));
    let out_bundle = env2
        .bundles()
        .get(&out_source)
        .expect("saved bundle should be loaded");
    assert!(
        out_bundle
            .nodes
            .iter()
            .any(|n| n.is_file() && n.name == "CAB-UnityPy_Mod.resS")
    );

    // Externals should include the cab path on the serialized file that owns this object.
    let asset_index = key
        .asset_index
        .expect("chosen object is from an AssetBundle source");
    let sf = out_bundle
        .assets
        .get(asset_index)
        .expect("asset_index should exist");
    assert!(sf.externals.iter().any(|e| e.path == write.path));
}

#[test]
fn environment_can_write_streamed_resource_cab_for_standalone_serialized_file_and_reload() {
    // Extract a SerializedFile from a sample bundle so we have a realistic standalone `.assets`.
    let bundle_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );
    let bundle_bytes = fs::read(&bundle_path).unwrap();
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bundle_bytes).unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
        .expect("sample bundle should contain a serialized file node");
    let node_bytes = bundle.extract_node_data(node).unwrap();

    let temp = tempfile::tempdir().unwrap();
    let assets_path = temp.path().join("standalone.assets");
    fs::write(&assets_path, node_bytes).unwrap();

    let mut env = Environment::new();
    env.load_file(&assets_path).unwrap();
    let assets_path = canonicalize_path(assets_path);
    let source = BinarySource::path(&assets_path);

    let key = env
        .binary_object_infos()
        .find(|r| r.source == &source && r.source_kind == BinarySourceKind::SerializedFile)
        .expect("standalone serialized file should yield objects")
        .key();

    let mut session = env.edit_session();
    let write = session.write_to_cab(&key, None, b"OggS").unwrap();

    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_assets_path = out_dir.join("standalone.assets");
    assert!(out_assets_path.exists());

    // Sidecar cab should be written under `out/{asset_file_name}_data/{cab_name}`.
    let cab_path = out_dir
        .join("standalone.assets_data")
        .join("CAB-UnityPy_Mod.resS");
    assert!(cab_path.exists());

    // Saved serialized file should include the external reference.
    let saved_bytes = fs::read(&out_assets_path).unwrap();
    let sf = unity_asset_binary::asset::SerializedFileParser::from_bytes(saved_bytes).unwrap();
    assert!(sf.externals.iter().any(|e| e.path == write.path));

    // The environment stream reader should be able to resolve the cab from filesystem candidates.
    let mut env2 = Environment::new();
    env2.load_file(&out_assets_path).unwrap();
    let bytes = env2
        .read_stream_data(
            &out_assets_path,
            BinarySourceKind::SerializedFile,
            &write.path,
            write.offset,
            write.size,
        )
        .unwrap();
    assert_eq!(bytes, b"OggS");
}

#[test]
fn environment_typed_audio_clip_helper_can_repoint_streamed_resource_and_reload() {
    use unity_asset_binary::unity_version::UnityVersion;
    use unity_asset_decode::audio::AudioClipConverter;
    use unity_asset_write::{PackerOptions, UnityPyPacker};

    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab"),
    );

    let mut env = Environment::new();
    env.load_file(&path).unwrap();

    let entry = env
        .bundle_container_entries(&path)
        .unwrap()
        .into_iter()
        .find(|e| e.asset_path.to_ascii_lowercase().ends_with("/cn_001.ogg"))
        .expect("sample bundle contains cn_001.ogg container entry");
    let key = entry
        .key
        .expect("cn_001.ogg container entry resolves to an object key");

    let mut session = env.edit_session();
    let write = session
        .write_streamed_audio_clip_data(&key, None, b"OggS")
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("char_118_yuki.ab"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let entry2 = env2
        .bundle_container_entries(&out_bundle_path)
        .unwrap()
        .into_iter()
        .find(|e| e.asset_path.to_ascii_lowercase().ends_with("/cn_001.ogg"))
        .expect("saved bundle contains cn_001.ogg container entry");
    let key2 = entry2
        .key
        .expect("saved cn_001.ogg container entry resolves to an object key");

    assert_eq!(key2.path_id, key.path_id);

    let obj = env2.read_binary_object_key(&key2).unwrap();
    let unity_version = env2
        .bundles()
        .get(&BinarySource::path(&out_bundle_path))
        .and_then(|b| key2.asset_index.and_then(|i| b.assets.get(i)))
        .and_then(|f| UnityVersion::parse_version(&f.unity_version).ok())
        .unwrap_or_default();

    let converter = AudioClipConverter::new(unity_version);
    let clip = converter.from_unity_object(&obj).unwrap();

    assert!(clip.data.is_empty(), "streamed clip should not embed bytes");
    assert!(clip.is_streamed());
    assert_eq!(clip.stream_info.path, write.path);
    assert_eq!(clip.stream_info.offset, write.offset);
    assert_eq!(clip.stream_info.size, write.size);

    let bytes = env2
        .read_stream_data(
            &out_bundle_path,
            BinarySourceKind::AssetBundle,
            &write.path,
            write.offset,
            write.size,
        )
        .unwrap();
    assert_eq!(bytes, b"OggS");
}

#[test]
fn environment_typed_texture2d_helper_can_repoint_streamed_resource_and_reload() {
    use unity_asset_write::{PackerOptions, UnityPyPacker};

    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/xinzexi_2_n_tex"),
    );

    let mut env = Environment::new();
    env.load_file(&path).unwrap();

    let bundle_source = BinarySource::path(&path);
    let tex_ref = env
        .binary_object_infos()
        .find(|r| {
            r.source == &bundle_source
                && r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == 28
        })
        .expect("expected at least one Texture2D object in sample bundle");
    let key = tex_ref.key();

    let mut session = env.edit_session();
    let write = session
        .write_streamed_texture2d_image_data(&key, None, b"RUST_TEX")
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("xinzexi_2_n_tex"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let key2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source == &out_source
                && r.source_kind == BinarySourceKind::AssetBundle
                && r.asset_index == key.asset_index
                && r.object.path_id() == key.path_id
        })
        .expect("expected edited Texture2D object after save")
        .key();

    let obj = env2.read_binary_object_key(&key2).unwrap();
    let props = obj.class.properties();
    let UnityValue::Object(stream) = props
        .get("m_StreamData")
        .expect("Texture2D has m_StreamData field")
    else {
        panic!("Texture2D m_StreamData should be an object");
    };
    let path = stream
        .get("path")
        .or_else(|| stream.get("m_Source"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let offset = stream
        .get("offset")
        .or_else(|| stream.get("m_Offset"))
        .and_then(|v| v.as_i64())
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or_default();
    let size = stream
        .get("size")
        .or_else(|| stream.get("m_Size"))
        .and_then(|v| v.as_i64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or_default();

    assert_eq!(path, write.path);
    assert_eq!(offset, write.offset);
    assert_eq!(size, write.size);

    let bytes = env2
        .read_stream_data(
            &out_bundle_path,
            BinarySourceKind::AssetBundle,
            &write.path,
            write.offset,
            write.size,
        )
        .unwrap();
    assert_eq!(bytes, b"RUST_TEX");
}

#[test]
fn typed_mesh_helper_updates_stream_data_and_clears_buffers() {
    let mut class = UnityClass::new(0, "Mesh".to_string(), "0".to_string());
    class.set(
        "m_IndexBuffer".to_string(),
        UnityValue::Bytes(vec![1, 2, 3]),
    );

    let mut vertex_data = UnityValue::Object(Default::default());
    let UnityValue::Object(map) = &mut vertex_data else {
        unreachable!();
    };
    map.insert("m_DataSize".to_string(), UnityValue::Integer(3));
    map.insert("m_Data".to_string(), UnityValue::Bytes(vec![9, 9, 9]));
    class.set("m_VertexData".to_string(), vertex_data);

    let write = super::edit::StreamedResourceWrite {
        path: "archive:/foo_data/CAB-UnityPy_Mod.resS".to_string(),
        offset: 123,
        size: 4,
    };

    super::typed::apply_mesh_streaming_write(&mut class, &write).unwrap();

    let UnityValue::Object(stream) = class.get("m_StreamData").unwrap() else {
        panic!("m_StreamData should be an object after write");
    };
    assert_eq!(
        stream.get("path").and_then(|v| v.as_str()),
        Some("archive:/foo_data/CAB-UnityPy_Mod.resS")
    );
    assert_eq!(stream.get("offset").and_then(|v| v.as_i64()), Some(123));
    assert_eq!(stream.get("size").and_then(|v| v.as_i64()), Some(4));

    assert_eq!(
        class.get("m_IndexBuffer"),
        Some(&UnityValue::Bytes(Vec::new()))
    );

    let UnityValue::Object(vd) = class.get("m_VertexData").unwrap() else {
        panic!("m_VertexData should be an object");
    };
    assert_eq!(vd.get("m_DataSize").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(vd.get("m_Data"), Some(&UnityValue::Bytes(Vec::new())));
}

#[test]
fn streamed_write_helper_updates_m_stream_data_shape() {
    let mut class = UnityClass::new(0, "Test".to_string(), "0".to_string());
    let mut stream_data = UnityValue::Object(Default::default());
    let UnityValue::Object(stream) = &mut stream_data else {
        unreachable!();
    };
    stream.insert(
        "m_Source".to_string(),
        UnityValue::String("old".to_string()),
    );
    stream.insert("m_Offset".to_string(), UnityValue::Integer(1));
    stream.insert("m_Size".to_string(), UnityValue::Integer(2));
    class.set("m_StreamData".to_string(), stream_data);

    let write = super::edit::StreamedResourceWrite {
        path: "archive:/foo_data/CAB-UnityPy_Mod.resS".to_string(),
        offset: 123,
        size: 4,
    };

    super::streamed_write::apply_streamed_resource_write(&mut class, "m_StreamData", &write)
        .unwrap();

    let UnityValue::Object(stream) = class.get("m_StreamData").unwrap() else {
        panic!("m_StreamData should be an object after write");
    };
    assert_eq!(
        stream.get("m_Source").and_then(|v| v.as_str()),
        Some("archive:/foo_data/CAB-UnityPy_Mod.resS")
    );
    assert_eq!(stream.get("m_Offset").and_then(|v| v.as_i64()), Some(123));
    assert_eq!(stream.get("m_Size").and_then(|v| v.as_i64()), Some(4));
}

#[test]
fn streamed_write_helper_updates_video_clip_external_resources_shape() {
    let mut class = UnityClass::new(0, "Test".to_string(), "0".to_string());
    let mut res = UnityValue::Object(Default::default());
    let UnityValue::Object(map) = &mut res else {
        unreachable!();
    };
    map.insert(
        "m_Source".to_string(),
        UnityValue::String("old".to_string()),
    );
    map.insert("m_Offset".to_string(), UnityValue::Integer(1));
    map.insert("m_Size".to_string(), UnityValue::Integer(2));
    class.set("m_ExternalResources".to_string(), res);

    let write = super::edit::StreamedResourceWrite {
        path: "archive:/foo_data/CAB-UnityPy_Mod.resS".to_string(),
        offset: 123,
        size: 4,
    };

    super::streamed_write::apply_streamed_resource_write(&mut class, "m_ExternalResources", &write)
        .unwrap();

    let UnityValue::Object(stream) = class.get("m_ExternalResources").unwrap() else {
        panic!("m_ExternalResources should be an object after write");
    };
    assert_eq!(
        stream.get("m_Source").and_then(|v| v.as_str()),
        Some("archive:/foo_data/CAB-UnityPy_Mod.resS")
    );
    assert_eq!(stream.get("m_Offset").and_then(|v| v.as_i64()), Some(123));
    assert_eq!(stream.get("m_Size").and_then(|v| v.as_i64()), Some(4));
}

#[test]
fn typed_text_asset_script_helper_updates_field() {
    let mut class = UnityClass::new(0, "TextAsset".to_string(), "0".to_string());
    class.set(
        "m_Script".to_string(),
        UnityValue::String("old".to_string()),
    );

    super::typed::apply_text_asset_script(&mut class, "new").unwrap();
    assert_eq!(class.get("m_Script").and_then(|v| v.as_str()), Some("new"));
}

#[test]
fn typed_video_player_helpers_update_url_and_video_clip_pptr() {
    let mut class = UnityClass::new(0, "VideoPlayer".to_string(), "0".to_string());
    class.set("m_Url".to_string(), UnityValue::String("old".to_string()));

    let mut clip = UnityValue::Object(Default::default());
    let UnityValue::Object(map) = &mut clip else {
        unreachable!();
    };
    map.insert("m_FileID".to_string(), UnityValue::Integer(1));
    map.insert("m_PathID".to_string(), UnityValue::Integer(2));
    class.set("m_VideoClip".to_string(), clip);

    super::typed::apply_video_player_url(&mut class, "https://example.test/video.mp4").unwrap();
    super::typed::apply_video_player_video_clip_pptr(&mut class, 0, 123).unwrap();

    assert_eq!(
        class.get("m_Url").and_then(|v| v.as_str()),
        Some("https://example.test/video.mp4")
    );

    let UnityValue::Object(clip) = class.get("m_VideoClip").unwrap() else {
        panic!("m_VideoClip should be an object");
    };
    assert_eq!(clip.get("fileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(clip.get("pathID").and_then(|v| v.as_i64()), Some(123));
    assert_eq!(clip.get("m_FileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(clip.get("m_PathID").and_then(|v| v.as_i64()), Some(123));
}

#[test]
fn typed_mesh_renderer_helpers_update_materials_and_additional_vertex_streams() {
    let mut class = UnityClass::new(0, "MeshRenderer".to_string(), "0".to_string());
    class.set("m_Materials".to_string(), UnityValue::Array(Vec::new()));
    class.set(
        "m_AdditionalVertexStreams".to_string(),
        UnityValue::Object(Default::default()),
    );

    super::typed::apply_renderer_materials(&mut class, &[(0, 10), (2, 20)]).unwrap();
    super::typed::apply_mesh_renderer_additional_vertex_streams_pptr(&mut class, 0, 99).unwrap();

    let UnityValue::Array(materials) = class.get("m_Materials").unwrap() else {
        panic!("m_Materials should be an array");
    };
    assert_eq!(materials.len(), 2);

    let UnityValue::Object(m0) = &materials[0] else {
        panic!("m_Materials[0] should be an object");
    };
    assert_eq!(m0.get("m_FileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(m0.get("m_PathID").and_then(|v| v.as_i64()), Some(10));

    let UnityValue::Object(m1) = &materials[1] else {
        panic!("m_Materials[1] should be an object");
    };
    assert_eq!(m1.get("m_FileID").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(m1.get("m_PathID").and_then(|v| v.as_i64()), Some(20));

    let UnityValue::Object(vs) = class.get("m_AdditionalVertexStreams").unwrap() else {
        panic!("m_AdditionalVertexStreams should be an object");
    };
    assert_eq!(vs.get("m_FileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(vs.get("m_PathID").and_then(|v| v.as_i64()), Some(99));
}

#[test]
fn typed_renderer_helper_updates_materials_field() {
    let mut class = UnityClass::new(0, "SkinnedMeshRenderer".to_string(), "0".to_string());

    super::typed::apply_renderer_materials(&mut class, &[(0, 10), (2, 20)]).unwrap();

    let UnityValue::Array(materials) = class.get("m_Materials").unwrap() else {
        panic!("m_Materials should be an array");
    };
    assert_eq!(materials.len(), 2);

    let UnityValue::Object(m0) = &materials[0] else {
        panic!("m_Materials[0] should be an object");
    };
    assert_eq!(m0.get("m_FileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(m0.get("m_PathID").and_then(|v| v.as_i64()), Some(10));

    let UnityValue::Object(m1) = &materials[1] else {
        panic!("m_Materials[1] should be an object");
    };
    assert_eq!(m1.get("m_FileID").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(m1.get("m_PathID").and_then(|v| v.as_i64()), Some(20));
}

#[test]
fn typed_mesh_filter_helper_updates_mesh_pptr() {
    let mut class = UnityClass::new(0, "MeshFilter".to_string(), "0".to_string());
    class.set("m_Mesh".to_string(), UnityValue::Object(Default::default()));

    super::typed::apply_mesh_filter_mesh_pptr(&mut class, 0, 123);

    let UnityValue::Object(mesh) = class.get("m_Mesh").unwrap() else {
        panic!("m_Mesh should be an object");
    };
    assert_eq!(mesh.get("m_FileID").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(mesh.get("m_PathID").and_then(|v| v.as_i64()), Some(123));
}

#[test]
fn environment_resolve_pptr_path_key_resolves_sprite_texture() {
    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    env.load_file(&path).unwrap();

    let sprite_ref = env
        .binary_object_infos()
        .find(|r| r.source_kind == BinarySourceKind::AssetBundle && r.object.class_id() == 213)
        .expect("sample bundle contains at least one Sprite");
    let sprite_key = sprite_ref.key();

    let resolved = env
        .resolve_pptr_path_key(&sprite_key, "m_RD.texture")
        .unwrap()
        .expect("sprite should reference a texture via m_RD.texture");

    let sprite_obj = env.read_binary_object_key(&sprite_key).unwrap();
    let v = super::pptr_path::get_value_at_path(sprite_obj.as_unity_class(), "m_RD.texture")
        .expect("m_RD.texture exists");
    let (_, expected_path_id) = super::pptr_path::read_pptr(v).expect("m_RD.texture is a PPtr");
    assert_eq!(resolved.path_id, expected_path_id);

    let texture = env.read_binary_object_key(&resolved).unwrap();
    assert_eq!(texture.class_id(), 28, "expected Texture2D target");
}

#[test]
fn environment_can_set_pptr_path_to_key_and_reload() {
    use unity_asset_write::{PackerOptions, UnityPyPacker};

    let mut env = Environment::new();
    let path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );
    env.load_file(&path).unwrap();

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

    let mut session = env.edit_session();
    session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("atlas_test"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();
    let sprite_ref = env2
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_obj = env2.read_binary_object_key(&sprite_ref.key()).unwrap();

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
    use unity_asset_write::{PackerOptions, UnityPyPacker};

    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );

    env.load_file(&banner_path).unwrap();
    env.load_file(&atlas_path).unwrap();

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

    let mut session = env.edit_session();
    let (file_id, _) = session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();
    assert!(file_id > 0);

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("banner_1"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();
    let sprite_ref = env2
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_obj = env2.read_binary_object_key(&sprite_ref.key()).unwrap();

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
    assert!(
        sf.externals.iter().any(|e| e.path == "atlas_test"),
        "expected added external entry for cross-source PPtr"
    );
}

#[test]
fn environment_resolve_pptr_path_key_best_effort_loads_external_bundle_from_subdir() {
    use unity_asset_write::{PackerOptions, UnityPyPacker};

    let mut env = Environment::new();
    let banner_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1"),
    );
    let atlas_path = canonicalize_path(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/atlas_test"),
    );

    env.load_file(&banner_path).unwrap();
    env.load_file(&atlas_path).unwrap();

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

    let mut session = env.edit_session();
    let (file_id, _) = session
        .set_pptr_path_to_key(&sprite_key, "m_SpriteAtlas", &atlas_key)
        .unwrap();
    assert!(file_id > 0);

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = canonicalize_path(out_dir.join("banner_1"));
    assert!(out_bundle_path.exists());

    // Place the external dependency in a nested folder to force the `find_file`-style directory scan.
    let deps_dir = out_dir.join("deps");
    std::fs::create_dir_all(&deps_dir).unwrap();
    let atlas_copy_path = deps_dir.join("atlas_test");
    std::fs::copy(&atlas_path, &atlas_copy_path).unwrap();
    let atlas_copy_path = canonicalize_path(atlas_copy_path);

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let sprite_ref = env2
        .find_binary_object_in_bundle_asset(&out_bundle_path, 0, sprite_key.path_id)
        .expect("saved bundle contains sprite path id");
    let sprite_key2 = sprite_ref.key();

    let mut session2 = env2.edit_session();
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
fn typed_material_helper_updates_or_inserts_texenv_texture() {
    let mut class = UnityClass::new(0, "Material".to_string(), "0".to_string());

    let mut saved = UnityValue::Object(Default::default());
    saved
        .as_object_mut()
        .unwrap()
        .insert("m_TexEnvs".to_string(), UnityValue::Array(Vec::new()));

    let UnityValue::Array(envs) = saved.as_object_mut().unwrap().get_mut("m_TexEnvs").unwrap()
    else {
        unreachable!();
    };

    let first = UnityValue::Object(
        [(
            "name".to_string(),
            UnityValue::String("_MainTex".to_string()),
        )]
        .into_iter()
        .collect(),
    );
    let second = UnityValue::Object(
        [(
            "m_Texture".to_string(),
            UnityValue::Object(Default::default()),
        )]
        .into_iter()
        .collect(),
    );
    envs.push(UnityValue::Array(vec![first, second]));

    class.set("m_SavedProperties".to_string(), saved);

    super::typed::apply_material_set_texenv_texture_pptr(&mut class, "_MainTex", 0, 123).unwrap();
    super::typed::apply_material_set_texenv_texture_pptr(&mut class, "_DetailTex", 0, 456).unwrap();

    let saved = class
        .get("m_SavedProperties")
        .and_then(|v| v.as_object())
        .expect("m_SavedProperties object");
    let envs = saved
        .get("m_TexEnvs")
        .and_then(|v| v.as_array())
        .expect("m_TexEnvs array");
    assert_eq!(envs.len(), 2);

    let main = envs
        .iter()
        .find(|v| {
            v.as_array()
                .and_then(|a| a.get(0))
                .and_then(|f| f.as_object())
                .and_then(|o| o.get("name"))
                .and_then(|v| v.as_str())
                == Some("_MainTex")
        })
        .expect("_MainTex entry exists");
    let second = main
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_object())
        .expect("second texenv object");
    let texture = second.get("m_Texture").expect("m_Texture present");
    let (_, path_id) = super::pptr_path::read_pptr(texture).expect("m_Texture is PPtr");
    assert_eq!(path_id, 123);

    let detail = envs
        .iter()
        .find(|v| {
            v.as_array().and_then(|a| a.get(0)).and_then(|f| f.as_str()) == Some("_DetailTex")
        })
        .expect("_DetailTex entry exists");
    let second = detail
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_object())
        .expect("second texenv object");
    let texture = second.get("m_Texture").expect("m_Texture present");
    let (_, path_id) = super::pptr_path::read_pptr(texture).expect("m_Texture is PPtr");
    assert_eq!(path_id, 456);
}

#[test]
fn typed_material_helpers_update_floats_ints_colors_and_texenv_scale_offset() {
    let mut class = UnityClass::new(0, "Material".to_string(), "0".to_string());
    class.set(
        "m_SavedProperties".to_string(),
        UnityValue::Object(Default::default()),
    );

    super::typed::apply_material_set_float(&mut class, "_Glossiness", 0.75).unwrap();
    super::typed::apply_material_set_int(&mut class, "_Mode", 2).unwrap();
    super::typed::apply_material_set_color(&mut class, "_Color", (1.0, 0.5, 0.25, 1.0)).unwrap();
    super::typed::apply_material_set_texenv_scale_offset(
        &mut class,
        "_MainTex",
        (2.0, 3.0),
        (0.1, 0.2),
    )
    .unwrap();

    let saved = class
        .get("m_SavedProperties")
        .and_then(|v| v.as_object())
        .expect("m_SavedProperties object");

    let floats = saved
        .get("m_Floats")
        .and_then(|v| v.as_array())
        .expect("m_Floats array");
    assert!(floats.iter().any(|v| {
        v.as_array().and_then(|a| a.get(0)).and_then(|k| k.as_str()) == Some("_Glossiness")
            && v.as_array().and_then(|a| a.get(1)).and_then(|x| x.as_f64()) == Some(0.75)
    }));

    let ints = saved
        .get("m_Ints")
        .and_then(|v| v.as_array())
        .expect("m_Ints array");
    assert!(ints.iter().any(|v| {
        v.as_array().and_then(|a| a.get(0)).and_then(|k| k.as_str()) == Some("_Mode")
            && v.as_array().and_then(|a| a.get(1)).and_then(|x| x.as_i64()) == Some(2)
    }));

    let colors = saved
        .get("m_Colors")
        .and_then(|v| v.as_array())
        .expect("m_Colors array");
    let color = colors
        .iter()
        .find(|v| v.as_array().and_then(|a| a.get(0)).and_then(|k| k.as_str()) == Some("_Color"))
        .expect("_Color entry exists");
    let rgba = color
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_object())
        .expect("color value object");
    assert_eq!(rgba.get("r").and_then(|v| v.as_f64()), Some(1.0));
    assert_eq!(rgba.get("g").and_then(|v| v.as_f64()), Some(0.5));
    assert_eq!(rgba.get("b").and_then(|v| v.as_f64()), Some(0.25));
    assert_eq!(rgba.get("a").and_then(|v| v.as_f64()), Some(1.0));

    let texenvs = saved
        .get("m_TexEnvs")
        .and_then(|v| v.as_array())
        .expect("m_TexEnvs array");
    let main = texenvs
        .iter()
        .find(|v| v.as_array().and_then(|a| a.get(0)).and_then(|k| k.as_str()) == Some("_MainTex"))
        .expect("_MainTex entry exists");
    let env = main
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|v| v.as_object())
        .expect("texenv object");
    let scale = env.get("m_Scale").and_then(|v| v.as_object()).unwrap();
    let offset = env.get("m_Offset").and_then(|v| v.as_object()).unwrap();
    assert_eq!(scale.get("x").and_then(|v| v.as_f64()), Some(2.0));
    assert_eq!(scale.get("y").and_then(|v| v.as_f64()), Some(3.0));
    assert_eq!(offset.get("x").and_then(|v| v.as_f64()), Some(0.1));
    assert_eq!(offset.get("y").and_then(|v| v.as_f64()), Some(0.2));
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should be two levels above unity-asset crate")
        .to_path_buf()
}

fn unitypy_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UNITYPY_PYTHON") {
        return Some(PathBuf::from(p));
    }

    let venv = repo_root()
        .join(".venv-unitypy")
        .join("Scripts")
        .join("python.exe");
    if venv.exists() {
        return Some(venv);
    }

    None
}

fn unitypy_check(script: &str, args: &[String]) -> Result<()> {
    let python = unitypy_python().ok_or_else(|| {
        UnityAssetError::format(format!(
            "UnityPy E2E is enabled, but no python was found. Set `UNITYPY_PYTHON`, or create a venv at `{}`.",
            repo_root().join(".venv-unitypy").display()
        ))
    })?;

    let out = Command::new(python)
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| UnityAssetError::format(format!("Failed to run UnityPy python: {}", e)))?;

    if !out.status.success() {
        return Err(UnityAssetError::format(format!(
            "UnityPy check failed (exit={:?}).\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    Ok(())
}

fn find_material_texenv_texture_pptr(
    class: &UnityClass,
    property_name: &str,
) -> Option<(i32, i64)> {
    let saved = class.get("m_SavedProperties")?.as_object()?;
    let tex_envs = saved.get("m_TexEnvs")?.as_array()?;

    for entry in tex_envs {
        let (first, second) = match entry {
            UnityValue::Array(a) if a.len() == 2 => (&a[0], &a[1]),
            UnityValue::Object(map) => (map.get("first")?, map.get("second")?),
            _ => continue,
        };

        let name = match first {
            UnityValue::String(s) => s.as_str(),
            UnityValue::Object(map) => map.get("name")?.as_str()?,
            _ => continue,
        };
        if name != property_name {
            continue;
        }

        let tex_env = second.as_object()?;
        let texture_pptr = tex_env.get("m_Texture")?;
        return super::pptr_path::read_pptr(texture_pptr);
    }

    None
}

fn read_renderer_materials_pptrs(class: &UnityClass) -> Vec<(i32, i64)> {
    let Some(materials) = class.get("m_Materials").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    materials
        .iter()
        .filter_map(|v| super::pptr_path::read_pptr(v))
        .collect()
}

#[test]
fn external_bundle_can_edit_material_and_unitypy_observes_change() {
    let Ok(bundle_path) = std::env::var("UNITY_ASSET_EXTERNAL_BUNDLE") else {
        return;
    };
    let bundle_path = PathBuf::from(bundle_path);
    if !bundle_path.exists() {
        return;
    }

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

    let mut chosen: Option<(BinaryObjectKey, String, Option<(i32, i64)>, BinaryObjectKey)> = None;
    for r in env
        .binary_object_infos()
        .filter(|r| r.source_kind == BinarySourceKind::AssetBundle && r.object.class_id() == 21)
    {
        let key = r.key();
        let obj = r.read().unwrap();
        let class = obj.as_unity_class();

        let saved = class.get("m_SavedProperties").and_then(|v| v.as_object());
        let tex_envs = saved
            .and_then(|s| s.get("m_TexEnvs"))
            .and_then(|v| v.as_array());
        let Some(tex_envs) = tex_envs else {
            continue;
        };

        // Prefer `_MainTex` if present, else pick the first property name we can parse.
        let mut prop_name: Option<String> = None;
        for entry in tex_envs {
            let first = match entry {
                UnityValue::Array(a) if a.len() == 2 => Some(&a[0]),
                UnityValue::Object(map) => map.get("first"),
                _ => None,
            };
            let Some(first) = first else {
                continue;
            };
            let name = match first {
                UnityValue::String(s) => Some(s.as_str()),
                UnityValue::Object(map) => map.get("name").and_then(|v| v.as_str()),
                _ => None,
            };
            let Some(name) = name else {
                continue;
            };
            if name == "_MainTex" {
                prop_name = Some("_MainTex".to_string());
                break;
            }
            if prop_name.is_none() {
                prop_name = Some(name.to_string());
            }
        }

        let Some(prop_name) = prop_name else {
            continue;
        };

        let before = find_material_texenv_texture_pptr(class, &prop_name);

        let mut textures: Vec<BinaryObjectKey> = env
            .binary_object_infos()
            .filter(|t| {
                t.source_kind == BinarySourceKind::AssetBundle
                    && t.object.class_id() == 28
                    && t.source == &key.source
            })
            .map(|t| t.key())
            .collect();
        textures.sort_by(|a, b| a.path_id.cmp(&b.path_id));
        if textures.is_empty() {
            continue;
        }

        let texture_key = match before {
            Some((_, before_path_id)) => textures
                .iter()
                .find(|k| k.path_id != before_path_id)
                .cloned()
                .unwrap_or_else(|| textures[0].clone()),
            None => textures[0].clone(),
        };

        chosen = Some((key, prop_name, before, texture_key));
        break;
    }

    let (material_key, property_name, _before, texture_key) =
        chosen.expect("expected at least one Material with m_SavedProperties.m_TexEnvs");

    let mut session = env.edit_session();
    session
        .set_material_texenv_texture_to_key(&material_key, &property_name, &texture_key)
        .unwrap();
    session
        .set_material_float(&material_key, "_Glossiness", 0.123)
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join(bundle_path.file_name().expect("bundle has file name"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let mat_ref = env2
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == 21
                && r.object.path_id() == material_key.path_id
                && r.source == &out_source
        })
        .expect("saved bundle contains edited material path id");
    let mat_obj = mat_ref.read().unwrap();

    let pptr = find_material_texenv_texture_pptr(mat_obj.as_unity_class(), &property_name)
        .expect("expected texenv entry after save");
    assert_eq!(pptr.0, 0, "expected in-file PPtr (fileID=0)");
    assert_eq!(pptr.1, texture_key.path_id);

    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return;
    }

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
mat_path_id = int(sys.argv[3])
prop_name = sys.argv[4]
tex_path_id = int(sys.argv[5])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
mats = [o for o in env.objects if o.type.name == "Material"]
target = None
for o in mats:
    if getattr(o, "path_id", None) == mat_path_id:
        target = o
        break
assert target is not None, ("material path_id not found", mat_path_id, len(mats))
mat = target.read()
sheet = mat.m_SavedProperties
tex_envs = getattr(sheet, "m_TexEnvs", [])
found = False
for (k, envtex) in tex_envs:
    name = getattr(k, "name", k)
    if name != prop_name:
        continue
    tex = envtex.m_Texture
    assert getattr(tex, "path_id", None) == tex_path_id, (name, tex.path_id, tex_path_id)
    found = True
    break
assert found, ("texenv not found", prop_name)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            out_bundle_path.display().to_string(),
            material_key.path_id.to_string(),
            property_name,
            texture_key.path_id.to_string(),
        ],
    )
    .unwrap();
}

#[test]
fn external_bundle_can_edit_mesh_renderer_materials_and_reload() {
    let Ok(bundle_path) = std::env::var("UNITY_ASSET_EXTERNAL_BUNDLE") else {
        return;
    };
    let bundle_path = PathBuf::from(bundle_path);
    if !bundle_path.exists() {
        return;
    }

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

    let Some(renderer_ref) = env.binary_object_infos().find(|r| {
        r.source_kind == BinarySourceKind::AssetBundle
            && r.object.class_id() == 23
            && r.asset_index.is_some()
    }) else {
        return;
    };
    let renderer_key = renderer_ref.key();

    let Some(material_key) = env
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == 21
                && r.source == renderer_ref.source
                && r.asset_index == renderer_ref.asset_index
        })
        .map(|r| r.key())
    else {
        return;
    };

    let mut session = env.edit_session();
    session
        .set_mesh_renderer_materials_to_keys(&renderer_key, &[material_key.clone()])
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join(bundle_path.file_name().expect("bundle has file name"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let renderer_ref2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == 23
                && r.object.path_id() == renderer_key.path_id
                && r.source == &out_source
        })
        .expect("saved bundle contains edited MeshRenderer path id");
    let renderer_obj2 = renderer_ref2.read().unwrap();

    let materials = read_renderer_materials_pptrs(renderer_obj2.as_unity_class());
    assert!(
        !materials.is_empty(),
        "expected MeshRenderer to have m_Materials after save"
    );
    assert_eq!(materials[0].0, 0, "expected in-file PPtr (fileID=0)");
    assert_eq!(materials[0].1, material_key.path_id);

    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return;
    }

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
renderer_path_id = int(sys.argv[3])
mat_path_id = int(sys.argv[4])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
objs = [o for o in env.objects if o.type.name == "MeshRenderer"]
target = None
for o in objs:
    if getattr(o, "path_id", None) == renderer_path_id:
        target = o
        break
assert target is not None, ("meshrenderer path_id not found", renderer_path_id, len(objs))
r = target.read()
mats = getattr(r, "m_Materials", [])
assert len(mats) > 0, "m_Materials is empty"
assert getattr(mats[0], "path_id", None) == mat_path_id, (mats[0].path_id, mat_path_id)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            out_bundle_path.display().to_string(),
            renderer_key.path_id.to_string(),
            material_key.path_id.to_string(),
        ],
    )
    .unwrap();
}

fn read_streamed_resource(v: &UnityValue) -> Option<(String, i64, i64)> {
    let UnityValue::Object(map) = v else {
        return None;
    };
    let path = map
        .get("path")
        .or_else(|| map.get("m_Source"))
        .and_then(|v| v.as_str())?
        .to_string();
    let offset = map
        .get("offset")
        .or_else(|| map.get("m_Offset"))
        .and_then(|v| v.as_i64())?;
    let size = map
        .get("size")
        .or_else(|| map.get("m_Size"))
        .and_then(|v| v.as_i64())?;
    Some((path, offset, size))
}

#[test]
fn external_bundle_can_edit_text_asset_script_and_unitypy_observes_change() {
    let Ok(bundle_path) = std::env::var("UNITY_ASSET_EXTERNAL_BUNDLE") else {
        return;
    };
    let bundle_path = PathBuf::from(bundle_path);
    if !bundle_path.exists() {
        return;
    }

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

    let Some(text_ref) = env.binary_object_infos().find(|r| {
        r.source_kind == BinarySourceKind::AssetBundle
            && r.object.class_id() == unity_asset_core::class_ids::TEXT_ASSET
            && r.asset_index.is_some()
    }) else {
        return;
    };
    let text_key = text_ref.key();

    let new_script = "unity-asset: edited TextAsset m_Script";
    let mut session = env.edit_session();
    // Skip bundles whose TextAsset typetree doesn't expose m_Script.
    if session
        .set_text_asset_script(&text_key, new_script)
        .is_err()
    {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join(bundle_path.file_name().expect("bundle has file name"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let text_ref2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == unity_asset_core::class_ids::TEXT_ASSET
                && r.object.path_id() == text_key.path_id
                && r.source == &out_source
        })
        .expect("saved bundle contains edited TextAsset path id");
    let obj2 = text_ref2.read().unwrap();
    let class2 = obj2.as_unity_class();
    assert_eq!(
        class2.get("m_Script").and_then(|v| v.as_str()),
        Some(new_script)
    );

    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return;
    }

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
path_id = int(sys.argv[3])
expected = sys.argv[4]
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
objs = [o for o in env.objects if o.type.name == "TextAsset"]
target = None
for o in objs:
    if getattr(o, "path_id", None) == path_id:
        target = o
        break
assert target is not None, ("textasset path_id not found", path_id, len(objs))
t = target.read()
v = getattr(t, "m_Script", None)
if v is None:
    v = getattr(t, "script", None)
assert v == expected, (v, expected)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            out_bundle_path.display().to_string(),
            text_key.path_id.to_string(),
            new_script.to_string(),
        ],
    )
    .unwrap();
}

#[test]
fn external_bundle_can_edit_mesh_stream_data_and_unitypy_observes_change() {
    let Ok(bundle_path) = std::env::var("UNITY_ASSET_EXTERNAL_BUNDLE") else {
        return;
    };
    let bundle_path = PathBuf::from(bundle_path);
    if !bundle_path.exists() {
        return;
    }

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

    let Some(mesh_ref) = env.binary_object_infos().find(|r| {
        r.source_kind == BinarySourceKind::AssetBundle
            && r.object.class_id() == unity_asset_core::class_ids::MESH
            && r.asset_index.is_some()
    }) else {
        return;
    };
    let mesh_key = mesh_ref.key();

    let data = b"unity-asset mesh streamed bytes";
    let mut session = env.edit_session();
    let write = session
        .write_streamed_mesh_data(&mesh_key, Some("CAB-UnityAsset_Mesh.resS"), data)
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join(bundle_path.file_name().expect("bundle has file name"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let mesh_ref2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == unity_asset_core::class_ids::MESH
                && r.object.path_id() == mesh_key.path_id
                && r.source == &out_source
        })
        .expect("saved bundle contains edited Mesh path id");
    let obj2 = mesh_ref2.read().unwrap();
    let class2 = obj2.as_unity_class();

    let (path, offset, size) = read_streamed_resource(class2.get("m_StreamData").unwrap())
        .expect("Mesh m_StreamData is a StreamedResource");
    assert_eq!(path, write.path);
    assert_eq!(offset as u64, write.offset);
    assert_eq!(size as u32, write.size);

    assert_eq!(
        class2.get("m_IndexBuffer").and_then(|v| v.as_bytes()),
        Some(&[][..])
    );
    if let Some(UnityValue::Object(vd)) = class2.get("m_VertexData") {
        assert_eq!(vd.get("m_DataSize").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(vd.get("m_Data").and_then(|v| v.as_bytes()), Some(&[][..]));
    }

    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return;
    }

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
path_id = int(sys.argv[3])
expected_path = sys.argv[4]
expected_offset = int(sys.argv[5])
expected_size = int(sys.argv[6])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
objs = [o for o in env.objects if o.type.name == "Mesh"]
target = None
for o in objs:
    if getattr(o, "path_id", None) == path_id:
        target = o
        break
assert target is not None, ("mesh path_id not found", path_id, len(objs))
m = target.read()
sd = getattr(m, "m_StreamData", None)
assert sd is not None
path = getattr(sd, "path", getattr(sd, "m_Source", None))
offset = getattr(sd, "offset", getattr(sd, "m_Offset", None))
size = getattr(sd, "size", getattr(sd, "m_Size", None))
assert path == expected_path, (path, expected_path)
assert int(offset) == expected_offset, (offset, expected_offset)
assert int(size) == expected_size, (size, expected_size)

# Best-effort: ensure buffers were cleared
ib = getattr(m, "m_IndexBuffer", None)
if ib is not None:
    assert len(ib) == 0, len(ib)
vd = getattr(m, "m_VertexData", None)
if vd is not None:
    ds = getattr(vd, "m_DataSize", None)
    if ds is not None:
        assert int(ds) == 0, ds
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            out_bundle_path.display().to_string(),
            mesh_key.path_id.to_string(),
            write.path,
            write.offset.to_string(),
            write.size.to_string(),
        ],
    )
    .unwrap();
}

#[test]
fn external_bundle_can_edit_video_clip_external_resources_and_unitypy_observes_change() {
    let Ok(bundle_path) = std::env::var("UNITY_ASSET_EXTERNAL_BUNDLE") else {
        return;
    };
    let bundle_path = PathBuf::from(bundle_path);
    if !bundle_path.exists() {
        return;
    }

    let mut env = Environment::new();
    env.load_file(&bundle_path).unwrap();

    let Some(clip_ref) = env.binary_object_infos().find(|r| {
        r.source_kind == BinarySourceKind::AssetBundle
            && r.object.class_id() == 329
            && r.asset_index.is_some()
    }) else {
        return;
    };
    let clip_key = clip_ref.key();

    let data = b"unity-asset videoclip streamed bytes";
    let mut session = env.edit_session();
    let write = session
        .write_streamed_video_clip_data(&clip_key, Some("CAB-UnityAsset_Video.resS"), data)
        .unwrap();

    let temp = tempfile::tempdir().unwrap();
    let out_dir = temp.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_bundle_path = out_dir.join(bundle_path.file_name().expect("bundle has file name"));
    assert!(out_bundle_path.exists());

    let mut env2 = Environment::new();
    env2.load_file(&out_bundle_path).unwrap();

    let out_source = BinarySource::path(&out_bundle_path);
    let clip_ref2 = env2
        .binary_object_infos()
        .find(|r| {
            r.source_kind == BinarySourceKind::AssetBundle
                && r.object.class_id() == 329
                && r.object.path_id() == clip_key.path_id
                && r.source == &out_source
        })
        .expect("saved bundle contains edited VideoClip path id");
    let obj2 = clip_ref2.read().unwrap();
    let class2 = obj2.as_unity_class();

    let (path, offset, size) = read_streamed_resource(class2.get("m_ExternalResources").unwrap())
        .expect("VideoClip m_ExternalResources is a StreamedResource");
    assert_eq!(path, write.path);
    assert_eq!(offset as u64, write.offset);
    assert_eq!(size as u32, write.size);

    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return;
    }

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
path_id = int(sys.argv[3])
expected_path = sys.argv[4]
expected_offset = int(sys.argv[5])
expected_size = int(sys.argv[6])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
objs = [o for o in env.objects if o.type.name == "VideoClip"]
target = None
for o in objs:
    if getattr(o, "path_id", None) == path_id:
        target = o
        break
assert target is not None, ("videoclip path_id not found", path_id, len(objs))
c = target.read()
er = getattr(c, "m_ExternalResources", None)
assert er is not None
path = getattr(er, "path", getattr(er, "m_Source", None))
offset = getattr(er, "offset", getattr(er, "m_Offset", None))
size = getattr(er, "size", getattr(er, "m_Size", None))
assert path == expected_path, (path, expected_path)
assert int(offset) == expected_offset, (offset, expected_offset)
assert int(size) == expected_size, (size, expected_size)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            out_bundle_path.display().to_string(),
            clip_key.path_id.to_string(),
            write.path,
            write.offset.to_string(),
            write.size.to_string(),
        ],
    )
    .unwrap();
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
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
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
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui.prefab");
    assert!(out_prefab.exists());

    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();
    let go = doc.get(Some("GameObject"), Some(&["m_Name"])).unwrap();
    assert_eq!(go.get("m_Name").and_then(|v| v.as_str()), Some("New"));
}

#[test]
fn environment_can_edit_yaml_prefab_ui_by_query_and_save() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Old
  m_Component:
  - component: {fileID: 100001}
  - component: {fileID: 100002}
--- !u!224 &100001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
  m_AnchorMin: {x: 0, y: 0}
  m_AnchorMax: {x: 1, y: 1}
  m_AnchoredPosition: {x: 0, y: 0}
  m_SizeDelta: {x: 0, y: 0}
  m_Pivot: {x: 0.5, y: 0.5}
--- !u!114 &100002
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Script: {fileID: 11500000, guid: 0123456789abcdef0123456789abcdef, type: 3}
  m_Text: Hello
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Old")
        .unwrap();
    session
        .set_yaml_string_at_key_path(&go, "m_Name", "New")
        .unwrap();

    let rect = session
        .find_yaml_component_key_by_class_name(&go, "RectTransform")
        .unwrap();
    session
        .yaml_rect_transform_set_anchored_position(&rect, 10.0, 20.0)
        .unwrap();
    session
        .yaml_rect_transform_set_size_delta(&rect, 30.0, 40.0)
        .unwrap();

    let mono = session
        .find_yaml_monobehaviour_key_by_script_guid(&go, "0123456789abcdef0123456789abcdef")
        .unwrap();
    session
        .set_yaml_string_at_key_path(&mono, "m_Text", "World")
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let go = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100000")
        .expect("GameObject anchor");
    assert_eq!(go.get("m_Name").and_then(|v| v.as_str()), Some("New"));

    let rect = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100001")
        .expect("RectTransform anchor");
    let anchored = rect
        .get("m_AnchoredPosition")
        .and_then(|v| v.as_object())
        .expect("m_AnchoredPosition object");
    assert_eq!(anchored.get("x").and_then(|v| v.as_f64()), Some(10.0));
    assert_eq!(anchored.get("y").and_then(|v| v.as_f64()), Some(20.0));
    let size = rect
        .get("m_SizeDelta")
        .and_then(|v| v.as_object())
        .expect("m_SizeDelta object");
    assert_eq!(size.get("x").and_then(|v| v.as_f64()), Some(30.0));
    assert_eq!(size.get("y").and_then(|v| v.as_f64()), Some(40.0));

    let mono = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100002")
        .expect("MonoBehaviour anchor");
    assert_eq!(mono.get("m_Text").and_then(|v| v.as_str()), Some("World"));
}

#[test]
fn environment_can_edit_yaml_prefab_ui_helpers_extended() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Root
  m_IsActive: 0
  m_Component:
  - component: {fileID: 100001}
  - component: {fileID: 100002}
--- !u!224 &100001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
  m_AnchorMin: {x: 0, y: 0}
  m_AnchorMax: {x: 1, y: 1}
  m_AnchoredPosition: {x: 0, y: 0}
  m_SizeDelta: {x: 0, y: 0}
  m_Pivot: {x: 0.5, y: 0.5}
  m_OffsetMin: {x: 0, y: 0}
  m_OffsetMax: {x: 0, y: 0}
--- !u!114 &100002
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Script: {fileID: 11500000, guid: 0123456789abcdef0123456789abcdef, type: 3}
  m_Color: {r: 1, g: 1, b: 1, a: 1}
  m_Sprite: {fileID: 0}
  m_Ref: {fileID: 0}
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Root")
        .unwrap();
    session.yaml_gameobject_set_active(&go, true).unwrap();

    let rect = session
        .find_yaml_component_key_by_class_name(&go, "RectTransform")
        .unwrap();
    session
        .yaml_rect_transform_set_anchor_min(&rect, 0.2, 0.3)
        .unwrap();
    session
        .yaml_rect_transform_set_anchor_max(&rect, 0.8, 0.9)
        .unwrap();
    session
        .yaml_rect_transform_set_pivot(&rect, 0.1, 0.2)
        .unwrap();
    session
        .yaml_rect_transform_set_offset_min(&rect, -1.0, -2.0)
        .unwrap();
    session
        .yaml_rect_transform_set_offset_max(&rect, 3.0, 4.0)
        .unwrap();

    let mono = session
        .find_yaml_monobehaviour_key_by_script_guid(&go, "0123456789abcdef0123456789abcdef")
        .unwrap();
    session
        .set_yaml_color_rgba_at_key_path(&mono, "m_Color", 0.1, 0.2, 0.3, 0.4)
        .unwrap();
    session
        .set_yaml_pptr_at_key_path(
            &mono,
            "m_Sprite",
            21300000,
            Some("fedcba9876543210fedcba9876543210"),
            Some(3),
        )
        .unwrap();
    session
        .set_yaml_pptr_to_yaml_anchor_at_key_path(&mono, "m_Ref", "100001")
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let go = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100000")
        .expect("GameObject anchor");
    assert_eq!(go.get("m_IsActive").and_then(|v| v.as_i64()), Some(1));

    let rect = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100001")
        .expect("RectTransform anchor");
    let anchor_min = rect
        .get("m_AnchorMin")
        .and_then(|v| v.as_object())
        .expect("m_AnchorMin object");
    assert_eq!(anchor_min.get("x").and_then(|v| v.as_f64()), Some(0.2));
    assert_eq!(anchor_min.get("y").and_then(|v| v.as_f64()), Some(0.3));
    let anchor_max = rect
        .get("m_AnchorMax")
        .and_then(|v| v.as_object())
        .expect("m_AnchorMax object");
    assert_eq!(anchor_max.get("x").and_then(|v| v.as_f64()), Some(0.8));
    assert_eq!(anchor_max.get("y").and_then(|v| v.as_f64()), Some(0.9));
    let pivot = rect
        .get("m_Pivot")
        .and_then(|v| v.as_object())
        .expect("m_Pivot object");
    assert_eq!(pivot.get("x").and_then(|v| v.as_f64()), Some(0.1));
    assert_eq!(pivot.get("y").and_then(|v| v.as_f64()), Some(0.2));
    let offset_min = rect
        .get("m_OffsetMin")
        .and_then(|v| v.as_object())
        .expect("m_OffsetMin object");
    assert_eq!(offset_min.get("x").and_then(|v| v.as_f64()), Some(-1.0));
    assert_eq!(offset_min.get("y").and_then(|v| v.as_f64()), Some(-2.0));
    let offset_max = rect
        .get("m_OffsetMax")
        .and_then(|v| v.as_object())
        .expect("m_OffsetMax object");
    assert_eq!(offset_max.get("x").and_then(|v| v.as_f64()), Some(3.0));
    assert_eq!(offset_max.get("y").and_then(|v| v.as_f64()), Some(4.0));

    let mono = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100002")
        .expect("MonoBehaviour anchor");
    let color = mono
        .get("m_Color")
        .and_then(|v| v.as_object())
        .expect("m_Color object");
    assert_eq!(color.get("r").and_then(|v| v.as_f64()), Some(0.1));
    assert_eq!(color.get("g").and_then(|v| v.as_f64()), Some(0.2));
    assert_eq!(color.get("b").and_then(|v| v.as_f64()), Some(0.3));
    assert_eq!(color.get("a").and_then(|v| v.as_f64()), Some(0.4));

    let sprite = mono
        .get("m_Sprite")
        .and_then(|v| v.as_object())
        .expect("m_Sprite object");
    assert_eq!(
        sprite.get("fileID").and_then(|v| v.as_i64()),
        Some(21300000)
    );
    assert_eq!(
        sprite.get("guid").and_then(|v| v.as_str()),
        Some("fedcba9876543210fedcba9876543210")
    );
    assert_eq!(sprite.get("type").and_then(|v| v.as_i64()), Some(3));

    let r = mono
        .get("m_Ref")
        .and_then(|v| v.as_object())
        .expect("m_Ref object");
    assert_eq!(r.get("fileID").and_then(|v| v.as_i64()), Some(100001));
}

#[test]
fn environment_can_edit_yaml_prefab_transform_helpers_extended() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("transform.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Root
  m_Component:
  - component: {fileID: 100001}
--- !u!4 &100001
Transform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Root")
        .unwrap();
    let tr = session
        .find_yaml_component_key_by_class_name(&go, "Transform")
        .unwrap();
    session
        .yaml_transform_set_local_position(&tr, 1.0, 2.0, 3.0)
        .unwrap();
    session
        .yaml_transform_set_local_scale(&tr, 4.0, 5.0, 6.0)
        .unwrap();
    session
        .yaml_transform_set_local_rotation_quat(&tr, 0.1, 0.2, 0.3, 0.4)
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("transform.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let tr = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "100001")
        .expect("Transform anchor");
    let pos = tr
        .get("m_LocalPosition")
        .and_then(|v| v.as_object())
        .expect("m_LocalPosition object");
    assert_eq!(pos.get("x").and_then(|v| v.as_f64()), Some(1.0));
    assert_eq!(pos.get("y").and_then(|v| v.as_f64()), Some(2.0));
    assert_eq!(pos.get("z").and_then(|v| v.as_f64()), Some(3.0));
    let scale = tr
        .get("m_LocalScale")
        .and_then(|v| v.as_object())
        .expect("m_LocalScale object");
    assert_eq!(scale.get("x").and_then(|v| v.as_f64()), Some(4.0));
    assert_eq!(scale.get("y").and_then(|v| v.as_f64()), Some(5.0));
    assert_eq!(scale.get("z").and_then(|v| v.as_f64()), Some(6.0));
    let rot = tr
        .get("m_LocalRotation")
        .and_then(|v| v.as_object())
        .expect("m_LocalRotation object");
    assert_eq!(rot.get("x").and_then(|v| v.as_f64()), Some(0.1));
    assert_eq!(rot.get("y").and_then(|v| v.as_f64()), Some(0.2));
    assert_eq!(rot.get("z").and_then(|v| v.as_f64()), Some(0.3));
    assert_eq!(rot.get("w").and_then(|v| v.as_f64()), Some(0.4));
}

#[test]
fn environment_can_find_yaml_gameobject_by_hierarchy_path_and_reparent() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("hierarchy.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Canvas
  m_Component:
  - component: {fileID: 200001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 200002}
--- !u!1 &100001
GameObject:
  m_Name: Button
  m_Component:
  - component: {fileID: 200002}
--- !u!224 &200002
RectTransform:
  m_GameObject: {fileID: 100001}
  m_Father: {fileID: 200001}
  m_Children:
  - {fileID: 200003}
--- !u!1 &100002
GameObject:
  m_Name: Text
  m_Component:
  - component: {fileID: 200003}
--- !u!224 &200003
RectTransform:
  m_GameObject: {fileID: 100002}
  m_Father: {fileID: 200002}
  m_Children: []
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let canvas = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Canvas")
        .unwrap();

    let text = session
        .find_yaml_child_gameobject_key_by_hierarchy_path(&canvas, "Button/Text")
        .unwrap();
    assert_eq!(text.anchor, "100002");

    session.yaml_reparent_gameobject(&text, &canvas).unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("hierarchy.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let canvas_tr = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "200001")
        .expect("Canvas RectTransform anchor");
    let canvas_children = canvas_tr
        .get("m_Children")
        .and_then(|v| v.as_array())
        .expect("Canvas m_Children array");
    let canvas_child_ids: Vec<i64> = canvas_children
        .iter()
        .filter_map(|v| v.as_object())
        .filter_map(|m| m.get("fileID").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(canvas_child_ids, vec![200002, 200003]);

    let button_tr = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "200002")
        .expect("Button RectTransform anchor");
    let button_children = button_tr
        .get("m_Children")
        .and_then(|v| v.as_array())
        .expect("Button m_Children array");
    let button_child_ids: Vec<i64> = button_children
        .iter()
        .filter_map(|v| v.as_object())
        .filter_map(|m| m.get("fileID").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(button_child_ids, Vec::<i64>::new());

    let text_tr = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "200003")
        .expect("Text RectTransform anchor");
    let father = text_tr
        .get("m_Father")
        .and_then(|v| v.as_object())
        .expect("Text m_Father object");
    assert_eq!(father.get("fileID").and_then(|v| v.as_i64()), Some(200001));
}

#[test]
fn environment_can_edit_yaml_prefab_ui_text_image_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_text_image.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Canvas
  m_Component:
  - component: {fileID: 200001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 200002}
--- !u!1 &100001
GameObject:
  m_Name: Button
  m_Component:
  - component: {fileID: 200002}
  - component: {fileID: 300002}
--- !u!224 &200002
RectTransform:
  m_GameObject: {fileID: 100001}
  m_Father: {fileID: 200001}
  m_Children:
  - {fileID: 200003}
--- !u!114 &300002
MonoBehaviour:
  m_GameObject: {fileID: 100001}
  m_Sprite: {fileID: 0}
  m_Color: {r: 1, g: 1, b: 1, a: 1}
  m_RaycastTarget: 1
--- !u!1 &100002
GameObject:
  m_Name: Text
  m_Component:
  - component: {fileID: 200003}
  - component: {fileID: 300003}
--- !u!224 &200003
RectTransform:
  m_GameObject: {fileID: 100002}
  m_Father: {fileID: 200002}
  m_Children: []
--- !u!114 &300003
MonoBehaviour:
  m_GameObject: {fileID: 100002}
  m_Text: Hello
  m_Color: {r: 1, g: 1, b: 1, a: 1}
  m_FontData:
    m_FontSize: 14
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let canvas = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Canvas")
        .unwrap();
    let button_go = session
        .find_yaml_child_gameobject_key_by_hierarchy_path(&canvas, "Button")
        .unwrap();
    let text_go = session
        .find_yaml_child_gameobject_key_by_hierarchy_path(&canvas, "Button/Text")
        .unwrap();

    let image_mb = session
        .find_yaml_monobehaviour_key_by_required_fields(
            &button_go,
            &["m_Sprite", "m_RaycastTarget", "m_Color"],
        )
        .unwrap();
    session
        .yaml_ui_set_image_sprite(
            &image_mb,
            21300000,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some(3),
        )
        .unwrap();
    session
        .yaml_ui_set_graphic_color_rgba(&image_mb, 0.1, 0.2, 0.3, 0.4)
        .unwrap();
    session
        .yaml_ui_set_graphic_raycast_target(&image_mb, false)
        .unwrap();

    let text_mb = session
        .find_yaml_monobehaviour_key_by_required_fields(&text_go, &["m_Text", "m_FontData"])
        .unwrap();
    session.yaml_ui_set_text_string(&text_mb, "World").unwrap();
    session.yaml_ui_set_text_font_size(&text_mb, 32).unwrap();
    session
        .yaml_ui_set_graphic_color_rgba(&text_mb, 0.9, 0.8, 0.7, 0.6)
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_text_image.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let image = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "300002")
        .expect("Image MonoBehaviour anchor");
    let sprite = image
        .get("m_Sprite")
        .and_then(|v| v.as_object())
        .expect("m_Sprite object");
    assert_eq!(
        sprite.get("fileID").and_then(|v| v.as_i64()),
        Some(21300000)
    );
    assert_eq!(
        sprite.get("guid").and_then(|v| v.as_str()),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(sprite.get("type").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(
        image.get("m_RaycastTarget").and_then(|v| v.as_i64()),
        Some(0)
    );

    let text = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "300003")
        .expect("Text MonoBehaviour anchor");
    assert_eq!(text.get("m_Text").and_then(|v| v.as_str()), Some("World"));
    let font_data = text
        .get("m_FontData")
        .and_then(|v| v.as_object())
        .expect("m_FontData object");
    assert_eq!(
        font_data.get("m_FontSize").and_then(|v| v.as_i64()),
        Some(32)
    );
}

#[test]
fn environment_can_edit_yaml_prefab_ui_tmp_text_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_tmp.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Root
  m_Component:
  - component: {fileID: 200001}
  - component: {fileID: 300001}
--- !u!4 &200001
Transform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!114 &300001
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_text: Hi
  m_fontSize: 12
  m_fontColor: {r: 1, g: 1, b: 1, a: 1}
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let root = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Root")
        .unwrap();
    let tmp = session
        .find_yaml_monobehaviour_key_by_required_fields(&root, &["m_text", "m_fontSize"])
        .unwrap();
    session.yaml_ui_set_text_string(&tmp, "Bye").unwrap();
    session.yaml_ui_set_text_font_size(&tmp, 99).unwrap();
    session
        .yaml_ui_set_graphic_color_rgba(&tmp, 0.0, 0.1, 0.2, 0.3)
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_tmp.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();
    let tmp = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "300001")
        .expect("TMP MonoBehaviour anchor");
    assert_eq!(tmp.get("m_text").and_then(|v| v.as_str()), Some("Bye"));
    assert_eq!(tmp.get("m_fontSize").and_then(|v| v.as_i64()), Some(99));
    let c = tmp
        .get("m_fontColor")
        .and_then(|v| v.as_object())
        .expect("m_fontColor object");
    assert_eq!(c.get("r").and_then(|v| v.as_f64()), Some(0.0));
    assert_eq!(c.get("g").and_then(|v| v.as_f64()), Some(0.1));
    assert_eq!(c.get("b").and_then(|v| v.as_f64()), Some(0.2));
    assert_eq!(c.get("a").and_then(|v| v.as_f64()), Some(0.3));
}

#[test]
fn environment_can_edit_yaml_prefab_ui_button_onclick_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_button.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Canvas
  m_Component:
  - component: {fileID: 200001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 200002}
--- !u!1 &100001
GameObject:
  m_Name: Button
  m_Component:
  - component: {fileID: 200002}
  - component: {fileID: 300002}
--- !u!224 &200002
RectTransform:
  m_GameObject: {fileID: 100001}
  m_Father: {fileID: 200001}
  m_Children: []
--- !u!114 &300002
MonoBehaviour:
  m_GameObject: {fileID: 100001}
  m_Interactable: 1
  m_OnClick:
    m_PersistentCalls:
      m_Calls: []
--- !u!1 &100002
GameObject:
  m_Name: Target
  m_Component:
  - component: {fileID: 200003}
  - component: {fileID: 300003}
--- !u!4 &200003
Transform:
  m_GameObject: {fileID: 100002}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!114 &300003
MonoBehaviour:
  m_GameObject: {fileID: 100002}
  m_Enabled: 1
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let canvas = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Canvas")
        .unwrap();
    let button_go = session
        .find_yaml_child_gameobject_key_by_hierarchy_path(&canvas, "Button")
        .unwrap();
    let button = session.find_yaml_button_key(&button_go).unwrap();

    session
        .yaml_ui_button_set_interactable(&button, false)
        .unwrap();
    session.yaml_ui_button_clear_on_click(&button).unwrap();
    session
        .yaml_ui_button_add_on_click_target_anchor(&button, "300003", "OnClick")
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_button.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let button = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "300002")
        .expect("Button MonoBehaviour anchor");
    assert_eq!(
        button.get("m_Interactable").and_then(|v| v.as_i64()),
        Some(0)
    );

    let calls = button
        .get("m_OnClick")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_PersistentCalls"))
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_Calls"))
        .and_then(|v| v.as_array())
        .expect("m_OnClick.m_PersistentCalls.m_Calls array");
    assert_eq!(calls.len(), 1);
    let call = calls[0].as_object().expect("call is object");
    assert_eq!(
        call.get("m_MethodName").and_then(|v| v.as_str()),
        Some("OnClick")
    );
    let target = call
        .get("m_Target")
        .and_then(|v| v.as_object())
        .expect("m_Target object");
    let target_file_id = target.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("fileID") || k.eq_ignore_ascii_case("m_FileID") {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        } else {
            None
        }
    });
    assert_eq!(target_file_id, Some(300003), "target={:?}", target);
}

#[test]
fn environment_can_edit_yaml_prefab_ui_canvas_scaler_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_canvas.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Canvas
  m_Component:
  - component: {fileID: 200001}
  - component: {fileID: 223001}
  - component: {fileID: 114001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!223 &223001
Canvas:
  m_GameObject: {fileID: 100000}
  m_RenderMode: 0
  m_PixelPerfect: 0
  m_OverrideSorting: 0
  m_SortingOrder: 0
  m_TargetDisplay: 0
--- !u!114 &114001
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Enabled: 1
  m_UiScaleMode: 0
  m_ReferencePixelsPerUnit: 100
  m_ScaleFactor: 1
  m_ReferenceResolution: {x: 800, y: 600}
  m_ScreenMatchMode: 0
  m_MatchWidthOrHeight: 0
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let canvas_go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Canvas")
        .unwrap();
    let canvas = session.find_yaml_canvas_key(&canvas_go).unwrap();
    let scaler = session.find_yaml_canvas_scaler_key(&canvas_go).unwrap();

    session.yaml_ui_canvas_set_render_mode(&canvas, 2).unwrap();
    session
        .yaml_ui_canvas_set_pixel_perfect(&canvas, true)
        .unwrap();
    session
        .yaml_ui_canvas_set_override_sorting(&canvas, true)
        .unwrap();
    session
        .yaml_ui_canvas_set_sorting_order(&canvas, 10)
        .unwrap();

    session
        .yaml_ui_canvas_scaler_set_ui_scale_mode(&scaler, 1)
        .unwrap();
    session
        .yaml_ui_canvas_scaler_set_reference_resolution(&scaler, 1920.0, 1080.0)
        .unwrap();
    session
        .yaml_ui_canvas_scaler_set_screen_match_mode(&scaler, 0)
        .unwrap();
    session
        .yaml_ui_canvas_scaler_set_match_width_or_height(&scaler, 0.5)
        .unwrap();
    session
        .yaml_ui_canvas_scaler_set_scale_factor(&scaler, 2.0)
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_canvas.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let canvas = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "223001")
        .expect("Canvas anchor");
    assert_eq!(canvas.get("m_RenderMode").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        canvas.get("m_PixelPerfect").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        canvas.get("m_OverrideSorting").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        canvas.get("m_SortingOrder").and_then(|v| v.as_i64()),
        Some(10)
    );

    let read_f64 = |v: &unity_asset_core::UnityValue| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|i| i as f64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
    };

    let scaler = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "114001")
        .expect("CanvasScaler MonoBehaviour anchor");
    assert_eq!(
        scaler.get("m_UiScaleMode").and_then(|v| v.as_i64()),
        Some(1)
    );
    let ref_res = scaler
        .get("m_ReferenceResolution")
        .and_then(|v| v.as_object())
        .expect("m_ReferenceResolution object");
    assert_eq!(ref_res.get("x").and_then(read_f64), Some(1920.0));
    assert_eq!(ref_res.get("y").and_then(read_f64), Some(1080.0));
    assert_eq!(
        scaler.get("m_MatchWidthOrHeight").and_then(read_f64),
        Some(0.5)
    );
    assert_eq!(scaler.get("m_ScaleFactor").and_then(read_f64), Some(2.0));
}

#[test]
fn environment_can_edit_yaml_prefab_ui_layout_group_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_layout.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Layout
  m_Component:
  - component: {fileID: 200001}
  - component: {fileID: 114001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!114 &114001
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Padding: {m_Left: 0, m_Right: 0, m_Top: 0, m_Bottom: 0}
  m_ChildAlignment: 0
  m_Spacing: 0
  m_ChildControlWidth: 1
  m_ChildControlHeight: 1
  m_ChildForceExpandWidth: 1
  m_ChildForceExpandHeight: 1
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Layout")
        .unwrap();
    let layout = session.find_yaml_layout_group_key(&go).unwrap();

    session
        .yaml_ui_layout_group_set_padding(&layout, 1, 2, 3, 4)
        .unwrap();
    session
        .yaml_ui_layout_group_set_child_alignment(&layout, 2)
        .unwrap();
    session
        .yaml_ui_layout_group_set_spacing(&layout, 12.5)
        .unwrap();
    session
        .yaml_ui_layout_group_set_child_control(&layout, false, true)
        .unwrap();
    session
        .yaml_ui_layout_group_set_child_force_expand(&layout, false, false)
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_layout.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let layout = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "114001")
        .expect("LayoutGroup MonoBehaviour anchor");
    let padding = layout
        .get("m_Padding")
        .and_then(|v| v.as_object())
        .expect("m_Padding object");
    assert_eq!(padding.get("m_Left").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(padding.get("m_Right").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(padding.get("m_Top").and_then(|v| v.as_i64()), Some(3));
    assert_eq!(padding.get("m_Bottom").and_then(|v| v.as_i64()), Some(4));
    assert_eq!(
        layout.get("m_ChildAlignment").and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(layout.get("m_Spacing").and_then(|v| v.as_f64()), Some(12.5));
    assert_eq!(
        layout.get("m_ChildControlWidth").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert_eq!(
        layout.get("m_ChildControlHeight").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        layout
            .get("m_ChildForceExpandWidth")
            .and_then(|v| v.as_i64()),
        Some(0)
    );
    assert_eq!(
        layout
            .get("m_ChildForceExpandHeight")
            .and_then(|v| v.as_i64()),
        Some(0)
    );
}

#[test]
fn environment_can_edit_yaml_prefab_ui_toggle_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_toggle.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Toggle
  m_Component:
  - component: {fileID: 200001}
  - component: {fileID: 114001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!114 &114001
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Interactable: 1
  m_IsOn: 0
  m_OnValueChanged:
    m_PersistentCalls:
      m_Calls: []
--- !u!1 &100001
GameObject:
  m_Name: Target
  m_Component:
  - component: {fileID: 200002}
  - component: {fileID: 114002}
--- !u!4 &200002
Transform:
  m_GameObject: {fileID: 100001}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!114 &114002
MonoBehaviour:
  m_GameObject: {fileID: 100001}
  m_Enabled: 1
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let toggle_go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Toggle")
        .unwrap();
    let toggle = session.find_yaml_toggle_key(&toggle_go).unwrap();

    session
        .yaml_ui_toggle_set_interactable(&toggle, false)
        .unwrap();
    session.yaml_ui_toggle_set_is_on(&toggle, true).unwrap();
    session
        .yaml_ui_toggle_add_on_value_changed_target_anchor(&toggle, "114002", "OnToggle")
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_toggle.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let toggle = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "114001")
        .expect("Toggle MonoBehaviour anchor");
    assert_eq!(
        toggle.get("m_Interactable").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert_eq!(toggle.get("m_IsOn").and_then(|v| v.as_i64()), Some(1));

    let calls = toggle
        .get("m_OnValueChanged")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_PersistentCalls"))
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_Calls"))
        .and_then(|v| v.as_array())
        .expect("m_OnValueChanged.m_PersistentCalls.m_Calls array");
    assert_eq!(calls.len(), 1);
    let call = calls[0].as_object().expect("call is object");
    assert_eq!(
        call.get("m_MethodName").and_then(|v| v.as_str()),
        Some("OnToggle")
    );
    let target = call
        .get("m_Target")
        .and_then(|v| v.as_object())
        .expect("m_Target object");
    let target_file_id = target.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("fileID") || k.eq_ignore_ascii_case("m_FileID") {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        } else {
            None
        }
    });
    assert_eq!(target_file_id, Some(114002), "target={:?}", target);
}

#[test]
fn environment_can_edit_yaml_prefab_ui_slider_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let prefab_path = dir.path().join("ui_slider.prefab");
    let prefab = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100000
GameObject:
  m_Name: Slider
  m_Component:
  - component: {fileID: 200001}
  - component: {fileID: 114001}
--- !u!224 &200001
RectTransform:
  m_GameObject: {fileID: 100000}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!114 &114001
MonoBehaviour:
  m_GameObject: {fileID: 100000}
  m_Interactable: 1
  m_Value: 0
  m_MinValue: 0
  m_MaxValue: 1
  m_WholeNumbers: 0
  m_OnValueChanged:
    m_PersistentCalls:
      m_Calls: []
--- !u!1 &100001
GameObject:
  m_Name: Target
  m_Component:
  - component: {fileID: 200002}
  - component: {fileID: 114002}
--- !u!4 &200002
Transform:
  m_GameObject: {fileID: 100001}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
--- !u!114 &114002
MonoBehaviour:
  m_GameObject: {fileID: 100001}
  m_Enabled: 1
"#;
    fs::write(&prefab_path, prefab).unwrap();

    let mut env = Environment::new();
    env.load_file(&prefab_path).unwrap();

    let mut session = env.edit_session();
    let slider_go = session
        .find_yaml_gameobject_key_by_name(&prefab_path, "Slider")
        .unwrap();
    let slider = session.find_yaml_slider_key(&slider_go).unwrap();

    session
        .yaml_ui_slider_set_min_max(&slider, -1.0, 3.0)
        .unwrap();
    session.yaml_ui_slider_set_value(&slider, 2.5).unwrap();
    session
        .yaml_ui_slider_set_whole_numbers(&slider, true)
        .unwrap();
    session
        .yaml_ui_slider_set_interactable(&slider, false)
        .unwrap();
    session
        .yaml_ui_slider_add_on_value_changed_target_anchor(&slider, "114002", "OnSlider")
        .unwrap();

    let out_dir = dir.path().join("out");
    session
        .save(
            unity_asset_write::PackerOptions {
                packer: unity_asset_write::UnityPyPacker::Original,
            },
            &out_dir,
        )
        .unwrap();

    let out_prefab = out_dir.join("ui_slider.prefab");
    let doc = YamlDocument::load_yaml(&out_prefab, false).unwrap();

    let slider = doc
        .entries()
        .iter()
        .find(|o| o.anchor == "114001")
        .expect("Slider MonoBehaviour anchor");

    assert_eq!(
        slider.get("m_Interactable").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert_eq!(
        slider.get("m_WholeNumbers").and_then(|v| v.as_i64()),
        Some(1)
    );

    let read_f64 = |v: &unity_asset_core::UnityValue| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|i| i as f64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
    };
    assert_eq!(slider.get("m_MinValue").and_then(read_f64), Some(-1.0));
    assert_eq!(slider.get("m_MaxValue").and_then(read_f64), Some(3.0));
    assert_eq!(slider.get("m_Value").and_then(read_f64), Some(2.5));

    let calls = slider
        .get("m_OnValueChanged")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_PersistentCalls"))
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("m_Calls"))
        .and_then(|v| v.as_array())
        .expect("m_OnValueChanged.m_PersistentCalls.m_Calls array");
    assert_eq!(calls.len(), 1);
    let call = calls[0].as_object().expect("call is object");
    assert_eq!(
        call.get("m_MethodName").and_then(|v| v.as_str()),
        Some("OnSlider")
    );
    let target = call
        .get("m_Target")
        .and_then(|v| v.as_object())
        .expect("m_Target object");
    let target_file_id = target.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("fileID") || k.eq_ignore_ascii_case("m_FileID") {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        } else {
            None
        }
    });
    assert_eq!(target_file_id, Some(114002), "target={:?}", target);
}
