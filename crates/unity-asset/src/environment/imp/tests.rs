use super::*;
use std::fs;

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
