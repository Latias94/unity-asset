use super::*;
use std::fs;

fn link_or_copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                if symlink(src, dst).is_ok() {
                    return Ok(());
                }
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::symlink_file;
                if symlink_file(src, dst).is_ok() {
                    return Ok(());
                }
            }

            fs::copy(src, dst).map(|_| ())
        }
    }
}

#[test]
fn environment_loads_yaml_fixture() {
    let mut env = Environment::new();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../unity-asset-yaml/tests/fixtures/SingleDoc.asset");
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
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
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
fn environment_dependency_graph_builds_and_closure_from_container_is_non_empty() {
    let mut env = Environment::new();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
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
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
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
    assert_eq!(cached, Some(asset_path));
}

#[test]
fn environment_typetree_registry_json_restores_parsing_for_stripped_assets() {
    use serde::Serialize;
    use std::sync::Arc;
    use unity_asset_binary::typetree::JsonTypeTreeRegistry;

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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/banner_1");
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

    let registry = JsonTypeTreeRegistry::from_path(&reg_path).unwrap();
    env.set_type_tree_registry(Some(Arc::new(registry)));

    let obj = env.read_binary_object_key(&key).unwrap();
    assert_eq!(obj.name().as_deref(), Some("banner_1"));
    assert_eq!(obj.get("m_Width").and_then(|v| v.as_i64()), Some(492));
    assert_eq!(obj.get("m_Height").and_then(|v| v.as_i64()), Some(180));
}

#[test]
fn environment_assetbundle_container_raw_matches_typetree_when_stripped() {
    let mut env = Environment::new();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/xinzexi_2_n_tex");
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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab");
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
    assert_eq!(yaml_ext.asset_path, Some(script_asset_path));
    assert_eq!(yaml_ext.resolved, None);
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
    let bundle_src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
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
    let sample_bundle_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
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
