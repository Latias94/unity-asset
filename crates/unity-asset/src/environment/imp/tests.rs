use super::*;
use std::fs;
use std::path::Path;
use unity_asset_binary::asset::{FileIdentifier, SerializedFileParser};
use unity_asset_core::{AssetLoadLimits, BudgetError, FieldPath};
use unity_asset_write::object::{
    SerializedFieldGuard, SerializedObjectEncoder, SerializedObjectMutation,
};
use unity_asset_write::serialized_file::{
    ExternalTableAllocator, SerializedFileEdits, SerializedFileWriter,
};

const TRANSFORM_HIERARCHY_FIXTURE: &[u8] = include_bytes!(
    "../../../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
);

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

fn external_transform_fixture(external_path: &str) -> Vec<u8> {
    let file = SerializedFileParser::from_bytes(TRANSFORM_HIERARCHY_FIXTURE.to_vec()).unwrap();
    let mut budget = AssetLoadBudget::default();
    let mut candidate = SerializedObjectEncoder::new(&file, 2)
        .unwrap()
        .begin_semantic(&mut budget)
        .unwrap();
    let father_path = FieldPath::root().push_field("m_Father").unwrap();
    let mut father = candidate.value_at_path(&father_path).unwrap().clone();
    let guard = SerializedFieldGuard::from_observed(
        candidate.schema_digest(),
        &father_path,
        &father,
        &mut budget,
    )
    .unwrap();
    let father_fields = father
        .as_object_mut()
        .expect("Transform fixture must expose m_Father as a PPtr object");
    father_fields.insert("m_FileID".to_owned(), UnityValue::Integer(1));
    father_fields.insert("m_PathID".to_owned(), UnityValue::Integer(1));
    candidate
        .apply(
            SerializedObjectMutation::replace_field(0, father_path, guard, father),
            &mut budget,
        )
        .unwrap();
    let encoded = candidate.finish(&mut budget).unwrap();
    let mut edits = SerializedFileEdits::default();
    edits
        .try_insert_encoded_object(encoded, &mut budget)
        .unwrap();

    let mut allocator = ExternalTableAllocator::new(&file).unwrap();
    allocator
        .intern(
            FileIdentifier {
                temp_empty: String::new(),
                guid: [0x11; 16],
                type_: 3,
                path: external_path.to_owned(),
            },
            &mut budget,
        )
        .unwrap();
    let edits = allocator.into_edits(edits).unwrap();
    SerializedFileWriter::save(&file, &edits).unwrap()
}

fn external_transform_context(env: &Environment, owner_path: &Path) -> BinaryObjectKey {
    env.find_binary_object_in_source(owner_path, 2)
        .expect("owner Transform object must be loaded")
        .key()
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
fn environment_best_effort_pptr_budget_failure_is_atomic_and_typed() {
    let temp = tempfile::tempdir().unwrap();
    let owner_path = temp.path().join("owner.assets");
    let dependency_path = temp.path().join("dependency.assets");
    fs::write(&owner_path, external_transform_fixture("dependency.assets")).unwrap();
    fs::write(&dependency_path, TRANSFORM_HIERARCHY_FIXTURE).unwrap();
    let owner_path = canonicalize_path(owner_path);
    let dependency_path = canonicalize_path(dependency_path);

    let mut env = Environment::new();
    env.load_file(&owner_path, &mut AssetLoadBudget::default())
        .unwrap();
    let context_key = external_transform_context(&env, &owner_path);
    let base_path = env.base_path.clone();
    env.read_binary_object_key(&context_key, &mut AssetLoadBudget::default())
        .unwrap();

    let mut baseline = AssetLoadBudget::default();
    assert_eq!(
        env.resolve_pptr_path_key(&context_key, "m_Father", &mut baseline)
            .unwrap(),
        None,
        "the owner must remain unresolved before the dependency is loaded"
    );
    let baseline_entries = baseline.usage().entries;

    let mut constrained = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: baseline_entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = env
        .resolve_pptr_path_key_best_effort(&context_key, "m_Father", &mut constrained)
        .expect_err("candidate parsing must share and exhaust the caller budget");

    assert!(
        matches!(
            budget_error_in_chain(&error),
            Some(BudgetError::Exceeded {
                resource: "entries",
                limit,
                requested,
            }) if *limit == baseline_entries && *requested > baseline_entries
        ),
        "error={error:?}, usage={:?}",
        constrained.usage()
    );
    assert!(
        !env.binary_assets()
            .contains_key(&BinarySource::path(&dependency_path)),
        "a budget-rejected candidate must not become a loaded dependency"
    );
    assert_eq!(
        env.base_path, base_path,
        "implicit dependency loading must restore the caller's base path"
    );
    assert!(
        env.warnings().is_empty(),
        "resource exhaustion is surfaced as the typed error, not downgraded to a warning"
    );
}

#[test]
fn environment_best_effort_pptr_skips_corrupt_candidate_and_can_retry() {
    let temp = tempfile::tempdir().unwrap();
    let owner_path = temp.path().join("owner.assets");
    let dependency_path = temp.path().join("dependency.assets");
    fs::write(&owner_path, external_transform_fixture("dependency.assets")).unwrap();
    fs::write(&dependency_path, b"not a Unity binary source").unwrap();
    let owner_path = canonicalize_path(owner_path);
    let dependency_path = canonicalize_path(dependency_path);

    let mut env = Environment::new();
    env.load_file(&owner_path, &mut AssetLoadBudget::default())
        .unwrap();
    let context_key = external_transform_context(&env, &owner_path);
    let mut budget = AssetLoadBudget::default();

    assert_eq!(
        env.resolve_pptr_path_key_best_effort(&context_key, "m_Father", &mut budget)
            .unwrap(),
        None,
        "a corrupt dependency candidate must be skipped"
    );
    assert!(
        !env.binary_assets()
            .contains_key(&BinarySource::path(&dependency_path)),
        "a corrupt candidate must not pollute the loaded-source cache"
    );
    assert!(
        env.read_binary_object_key(&context_key, &mut budget)
            .is_ok(),
        "the valid owner must remain readable after a candidate failure"
    );
    assert!(
        env.warnings().iter().any(|warning| {
            matches!(
                warning,
                EnvironmentWarning::LoadFailed { path, error }
                    if path == &dependency_path && !error.is_empty()
            )
        }),
        "a skipped dependency candidate must produce an observable structured warning"
    );

    fs::write(&dependency_path, TRANSFORM_HIERARCHY_FIXTURE).unwrap();
    let resolved = env
        .resolve_pptr_path_key_best_effort(&context_key, "m_Father", &mut budget)
        .unwrap()
        .expect("a later valid candidate must be loadable after the corrupt attempt");
    assert_eq!(resolved.source, BinarySource::path(&dependency_path));
    assert_eq!(resolved.source_kind, BinarySourceKind::SerializedFile);
    assert_eq!(resolved.path_id, 1);
    assert!(
        env.read_binary_object_key(&resolved, &mut budget).is_ok(),
        "the retry must resolve a readable target rather than only update a cache entry"
    );
}

#[test]
fn environment_pptr_paths_read_nested_sequence_elements() {
    use indexmap::IndexMap;

    let pptr = |file_id, path_id| {
        UnityValue::Object(IndexMap::from([
            ("m_FileID".to_owned(), UnityValue::Integer(file_id)),
            ("m_PathID".to_owned(), UnityValue::Integer(path_id)),
        ]))
    };
    let class = UnityClass::with_properties(
        1,
        "GameObject".to_owned(),
        "1".to_owned(),
        IndexMap::from([
            (
                "m_Component".to_owned(),
                UnityValue::Array(vec![
                    UnityValue::Object(IndexMap::from([("component".to_owned(), pptr(0, 11))])),
                    UnityValue::Object(IndexMap::from([("component".to_owned(), pptr(2, 22))])),
                ]),
            ),
            (
                "m_Nested".to_owned(),
                UnityValue::Object(IndexMap::from([(
                    "m_References".to_owned(),
                    UnityValue::Array(vec![UnityValue::Object(IndexMap::from([(
                        "target".to_owned(),
                        pptr(3, 33),
                    )]))]),
                )])),
            ),
        ]),
    );

    let component = super::pptr_path::get_value_at_path(&class, "m_Component[1].component")
        .expect("root sequence index must select the second component PPtr");
    assert_eq!(super::pptr_path::read_pptr(component), Some((2, 22)));

    let nested = super::pptr_path::get_value_at_path(&class, "m_Nested.m_References[0].target")
        .expect("nested sequence index must select the target PPtr");
    assert_eq!(super::pptr_path::read_pptr(nested), Some((3, 33)));
    assert!(
        super::pptr_path::get_value_at_path(&class, "m_Component[2].component").is_none(),
        "an out-of-range sequence index must not resolve a neighboring PPtr"
    );
}
