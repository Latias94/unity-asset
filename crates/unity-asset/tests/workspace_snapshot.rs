use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::PathBuf;

use flate2::Compression;
use flate2::write::GzEncoder;
use unity_asset::schema::{
    DeclaredUnityVersion, RecipeApplicabilityStatus, RecipeError, RecipeRejectionCode,
    SchemaOrigin, SchemaRecipePlanner,
};
use unity_asset::workspace::{
    AssetWorkspace, MutationPlanBuilder, MutationPlanError, MutationValue, PrepareOptions,
    ReferenceTarget, SourceOpenRequest, WorkspaceError, WorkspaceLookup, WorkspaceObjectValue,
    WorkspaceOptions, WorkspaceSourceContainer, WorkspaceSourceMemberIdentityError, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BinaryError, BudgetError, ContainmentKind,
    ContractError, FieldPath, ObjectAddress, SourceAlias, SourceKind, SourceLocator,
    SourceMemberId,
};
use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_binary::bundle::BundleParser;
use unity_asset_core::arc_slice_allocation_bytes;
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const V22_SERIALIZED_FIXTURE: &[u8] =
    include_bytes!("../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin");
const MULTI_V22_SERIALIZED_FIXTURE: &[u8] = include_bytes!(
    "../../unity-asset-write/tests/fixtures/serialized_file_wire/multi_v22.assets.bin"
);
const TRANSFORM_HIERARCHY_V22_SERIALIZED_FIXTURE: &[u8] = include_bytes!(
    "../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
);

const EXTERNAL_TYPE_TREE_REGISTRY: &str = r#"{
  "schema": 1,
  "entries": [{
    "class_id": 28,
    "type_tree": {
      "nodes": [{
        "type_name": "FrozenExternal",
        "name": "FrozenExternal",
        "byte_size": -1,
        "variable_count": 0,
        "index": 0,
        "type_flags": 0,
        "version": 1,
        "meta_flags": 0,
        "level": 0,
        "type_str_offset": 0,
        "name_str_offset": 0,
        "ref_type_hash": 0,
        "children": [{
          "type_name": "int",
          "name": "m_Value",
          "byte_size": 4,
          "variable_count": 0,
          "index": 1,
          "type_flags": 0,
          "version": 1,
          "meta_flags": 0,
          "level": 1,
          "type_str_offset": 0,
          "name_str_offset": 0,
          "ref_type_hash": 0,
          "children": []
        }]
      }],
      "string_buffer": [],
      "version": 1,
      "platform": 1,
      "has_type_dependencies": false
    }
  }]
}"#;

const FIRST_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: Before
"#;

const SECOND_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: After
"#;

const DUPLICATE_ANCHOR_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: First
--- !u!4 &100
Transform:
  m_GameObject: {fileID: 100}
"#;

const MULTI_OBJECT_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: First
--- !u!1 &200
GameObject:
  m_Name: Second
--- !u!1 &300
GameObject:
  m_Name: Third
"#;

fn workspace(_test_case: u128) -> AssetWorkspace {
    AssetWorkspace::with_options(WorkspaceOptions::strict()).unwrap()
}

fn webfile_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let head_length = entries.iter().fold(20_usize, |length, (name, _)| {
        length.checked_add(12 + name.len()).unwrap()
    });
    let mut payload_offset = head_length;
    let mut directory = Vec::new();
    for (name, payload) in entries {
        directory.extend_from_slice(&i32::try_from(payload_offset).unwrap().to_le_bytes());
        directory.extend_from_slice(&i32::try_from(payload.len()).unwrap().to_le_bytes());
        directory.extend_from_slice(&i32::try_from(name.len()).unwrap().to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
        payload_offset = payload_offset.checked_add(payload.len()).unwrap();
    }
    let mut bytes = b"UnityWebData1.0\0".to_vec();
    bytes.extend_from_slice(&i32::try_from(head_length).unwrap().to_le_bytes());
    bytes.extend_from_slice(&directory);
    for (_, payload) in entries {
        bytes.extend_from_slice(payload);
    }
    bytes
}

fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, payload) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(payload).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn gzip_compress(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn sample_bundle_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab")
}

fn sample_serialized_bytes() -> Vec<u8> {
    let bundle = BundleParser::from_bytes(fs::read(sample_bundle_path()).unwrap()).unwrap();
    let node = bundle
        .nodes
        .iter()
        .find(|node| {
            node.is_file() && !node.name.ends_with(".resS") && !node.name.ends_with(".resource")
        })
        .unwrap();
    bundle.extract_node_data(node).unwrap()
}

fn serialized_fixture_bytes() -> Vec<u8> {
    V22_SERIALIZED_FIXTURE.to_vec()
}

fn duplicate_path_id_serialized_fixture_bytes() -> Vec<u8> {
    const SECOND_PATH_ID_OFFSET: usize = 184;

    let mut bytes = MULTI_V22_SERIALIZED_FIXTURE.to_vec();
    let range = SECOND_PATH_ID_OFFSET..SECOND_PATH_ID_OFFSET + size_of::<i64>();
    assert_eq!(&bytes[range.clone()], 84_i64.to_be_bytes());
    bytes[range].copy_from_slice(&42_i64.to_be_bytes());
    bytes
}

fn stripped_serialized_fixture_bytes() -> Vec<u8> {
    let mut file = SerializedFileParser::from_bytes(serialized_fixture_bytes()).unwrap();
    file.set_type_tree_enabled(false);
    for serialized_type in file.types_mut() {
        serialized_type.type_tree = Default::default();
        serialized_type.type_dependencies.clear();
        serialized_type.class_name.clear();
        serialized_type.namespace.clear();
        serialized_type.assembly_name.clear();
    }
    for serialized_type in file.ref_types_mut() {
        serialized_type.type_tree = Default::default();
        serialized_type.type_dependencies.clear();
        serialized_type.class_name.clear();
        serialized_type.namespace.clear();
        serialized_type.assembly_name.clear();
    }
    SerializedFileWriter::save(&file, &SerializedFileEdits::default()).unwrap()
}

fn transform_fixture_with_unity_version(unity_version: &str) -> Vec<u8> {
    let mut file =
        SerializedFileParser::from_bytes(TRANSFORM_HIERARCHY_V22_SERIALIZED_FIXTURE.to_vec())
            .unwrap();
    assert_eq!(file.format().version(), 22);
    file.unity_version = unity_version.to_owned();

    let rewritten = SerializedFileWriter::save(&file, &SerializedFileEdits::default()).unwrap();
    let reparsed = SerializedFileParser::from_bytes(rewritten.clone()).unwrap();
    assert_eq!(reparsed.format().version(), 22);
    assert_eq!(reparsed.unity_version, unity_version);
    assert_eq!(reparsed.object_count(), 2);
    rewritten
}

fn yaml_with_anchor(anchor: &str) -> String {
    format!(
        "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &{anchor}\nGameObject:\n  m_Name: Invalid\n"
    )
}

fn root_image_budget_bytes(length: usize) -> u64 {
    u64::try_from(length).unwrap() + arc_slice_allocation_bytes::<u8>(length).unwrap()
}

fn assert_load_budget_error(path: &std::path::Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    let mut workspace = workspace(0);
    let revision = workspace.revision();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: root_image_budget_bytes(bytes.len()),
        ..AssetLoadLimits::default()
    })
    .unwrap();

    assert!(matches!(
        workspace.load_path(path, &mut budget),
        Err(WorkspaceError::Budget(_))
    ));
    assert_eq!(workspace.revision(), revision);
}

fn load_usage(path: &std::path::Path, bytes: &[u8], max_bytes: u64) -> AssetLoadUsage {
    fs::write(path, bytes).unwrap();
    let mut workspace = workspace(0);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    workspace.load_path(path, &mut budget).unwrap();
    budget.usage()
}

fn assert_single_result_is_budgeted<T>(
    mut query: impl FnMut(&mut AssetLoadBudget) -> Result<T, WorkspaceError>,
    expected_entries: u64,
    expected_entries_before_byte_limit: u64,
) {
    let mut successful = AssetLoadBudget::default();
    query(&mut successful).unwrap();
    let successful_usage = successful.usage();
    assert_eq!(successful_usage.entries, expected_entries);
    assert!(successful_usage.bytes > 0);

    let mut entry_limited = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: successful_usage.entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    entry_limited
        .consume_entries(successful_usage.entries)
        .unwrap();
    assert!(matches!(
        query(&mut entry_limited),
        Err(WorkspaceError::Budget(_))
    ));
    assert_eq!(entry_limited.usage().entries, successful_usage.entries);
    assert_eq!(entry_limited.usage().bytes, 0);

    let mut byte_limited = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: successful_usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    byte_limited.consume_bytes(successful_usage.bytes).unwrap();
    assert!(matches!(
        query(&mut byte_limited),
        Err(WorkspaceError::Budget(_))
    ));
    assert_eq!(
        byte_limited.usage().entries,
        expected_entries_before_byte_limit
    );
    assert_eq!(byte_limited.usage().bytes, successful_usage.bytes);
}

fn resolved_source_id(view: &impl WorkspaceView, locator: &SourceLocator) -> unity_asset::SourceId {
    match view
        .resolve_source(locator, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(source) => source.id(),
        other => panic!("expected resolved source, got {other:?}"),
    }
}

#[test]
fn old_snapshots_survive_reload_unload_and_physical_file_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(1);

    let root = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let first_revision = workspace.revision();
    let first = workspace.snapshot();
    let first_bytes = first
        .read_source_range(
            root,
            0,
            u64::try_from(FIRST_YAML.len()).unwrap(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(first_bytes.contiguous(), Some(FIRST_YAML.as_bytes()));

    fs::write(&path, SECOND_YAML).unwrap();
    let reloaded = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(reloaded, root);
    assert_ne!(workspace.revision(), first_revision);
    let second = workspace.snapshot();
    assert_eq!(first_bytes.contiguous(), Some(FIRST_YAML.as_bytes()));
    assert_eq!(
        second
            .read_source_range(
                root,
                0,
                u64::try_from(SECOND_YAML.len()).unwrap(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .contiguous(),
        Some(SECOND_YAML.as_bytes())
    );

    fs::remove_file(&path).unwrap();
    assert_eq!(first_bytes.contiguous(), Some(FIRST_YAML.as_bytes()));
    assert_eq!(
        second
            .read_source_range(root, 0, 5, &mut AssetLoadBudget::default())
            .unwrap()
            .contiguous(),
        Some(&SECOND_YAML.as_bytes()[..5])
    );

    workspace
        .unload_source(root, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        workspace
            .snapshot()
            .source(root, &mut AssetLoadBudget::default())
            .unwrap(),
        WorkspaceLookup::Missing
    ));
    assert!(matches!(
        first.source(root, &mut AssetLoadBudget::default()).unwrap(),
        WorkspaceLookup::Resolved(_)
    ));
    assert!(matches!(
        second
            .source(root, &mut AssetLoadBudget::default())
            .unwrap(),
        WorkspaceLookup::Resolved(_)
    ));
}

#[test]
fn source_ranges_support_bounded_read_seek_and_streaming_copy() {
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed sink"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut asset_workspace = workspace(41);
    let source = asset_workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = asset_workspace.snapshot();
    let bytes = snapshot
        .read_source_range(source, 2, 7, &mut AssetLoadBudget::default())
        .unwrap();
    let expected = &FIRST_YAML.as_bytes()[2..9];
    let source_fingerprint = match snapshot
        .source(source, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(source) => source.fingerprint(),
        other => panic!("expected resolved source, got {other:?}"),
    };

    assert_eq!(bytes.source(), source);
    assert_eq!(bytes.fingerprint(), source_fingerprint);
    assert_eq!(bytes.len(), 7);
    assert!(!bytes.is_empty());
    assert_eq!(bytes.contiguous(), Some(expected));

    let cloned = bytes.clone();
    assert!(std::ptr::eq(
        bytes.contiguous().unwrap().as_ptr(),
        cloned.contiguous().unwrap().as_ptr(),
    ));

    let mut reader = bytes.reader();
    assert_eq!(reader.seek(SeekFrom::Start(2)).unwrap(), 2);
    let mut selected = [0; 3];
    reader.read_exact(&mut selected).unwrap();
    assert_eq!(selected.as_slice(), &expected[2..5]);
    assert_eq!(reader.seek(SeekFrom::End(-1)).unwrap(), 6);
    let mut tail = [0; 1];
    reader.read_exact(&mut tail).unwrap();
    assert_eq!(tail.as_slice(), &expected[6..]);
    let error = reader.seek(SeekFrom::Start(bytes.len() + 1)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(reader.stream_position().unwrap(), bytes.len());

    let mut copied = Vec::new();
    assert_eq!(bytes.copy_to(&mut copied).unwrap(), bytes.len());
    assert_eq!(copied, expected);

    let error = bytes.copy_to(&mut FailingWriter).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

    let mut invalid_budget = AssetLoadBudget::default();
    assert!(matches!(
        snapshot.read_source_range(source, u64::MAX, 1, &mut invalid_budget),
        Err(WorkspaceError::RangeOverflow { .. })
    ));
    assert_eq!(invalid_budget.usage().bytes, 0);
    assert_eq!(invalid_budget.usage().entries, 0);
    assert!(matches!(
        snapshot.read_source_range(source, 0, u64::MAX, &mut invalid_budget),
        Err(WorkspaceError::RangeOutOfBounds { .. })
    ));
    assert_eq!(invalid_budget.usage().bytes, 0);
    assert_eq!(invalid_budget.usage().entries, 0);

    assert!(matches!(
        workspace(42)
            .snapshot()
            .read_source_range(source, 0, 1, &mut AssetLoadBudget::default(),),
        Err(WorkspaceError::Contract(
            ContractError::WorkspaceMismatch { .. }
        ))
    ));
}

#[test]
fn failed_load_is_atomic_and_query_caches_do_not_change_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(2);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let revision = workspace.revision();

    let snapshot = workspace.snapshot();
    let handles = snapshot.objects(&mut AssetLoadBudget::default()).unwrap();
    assert_eq!(handles.len(), 1);
    let object = snapshot
        .read_object(&handles[0], &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(object.value(), WorkspaceObjectValue::Yaml(_)));
    assert_eq!(object.class().class_name, "GameObject");
    assert_eq!(workspace.revision(), revision);

    fs::write(&path, SECOND_YAML.repeat(8)).unwrap();
    let limits = AssetLoadLimits {
        max_bytes: 8,
        ..AssetLoadLimits::default()
    };
    let mut insufficient = AssetLoadBudget::new(limits).unwrap();
    assert!(workspace.load_path(&path, &mut insufficient).is_err());
    assert_eq!(workspace.revision(), revision);
    assert_eq!(
        snapshot
            .read_object(&handles[0], &mut AssetLoadBudget::default())
            .unwrap()
            .class()
            .class_name,
        "GameObject"
    );
}

#[test]
fn handles_reject_foreign_and_stale_workspace_contexts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut first_workspace = workspace(3);
    first_workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let old_snapshot = first_workspace.snapshot();
    let handle = old_snapshot
        .objects(&mut AssetLoadBudget::default())
        .unwrap()
        .remove(0);

    let second_workspace = workspace(4);
    assert!(matches!(
        second_workspace
            .snapshot()
            .read_object(&handle, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::Contract(
            ContractError::WorkspaceMismatch { .. }
        ))
    ));

    fs::write(&path, SECOND_YAML).unwrap();
    first_workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(matches!(
        first_workspace
            .snapshot()
            .read_object(&handle, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::Contract(
            ContractError::RevisionMismatch { .. }
        ))
    ));
    assert!(
        old_snapshot
            .read_object(&handle, &mut AssetLoadBudget::default())
            .is_ok()
    );
}

#[test]
fn one_view_resolves_archive_webfile_yaml_and_streamed_members_without_io() {
    let nested_web = webfile_with_entries(&[
        ("scene.prefab", FIRST_YAML.as_bytes()),
        ("data.resource", b"0123456789"),
    ]);
    let archive = zip_with_entries(&[
        ("nested.web", nested_web.as_slice()),
        ("top.prefab", SECOND_YAML.as_bytes()),
        ("top.resource", b"abcdefghij"),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("content.zip");
    fs::write(&path, archive).unwrap();
    let mut workspace = workspace(5);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();

    let archive_locator = SourceLocator::path("content.zip").unwrap();
    let web_locator = archive_locator
        .clone()
        .child(
            ContainmentKind::Archive,
            SourceMemberId::new("nested.web").unwrap(),
        )
        .unwrap();
    let nested_yaml_locator = web_locator
        .clone()
        .child(
            ContainmentKind::WebFile,
            SourceMemberId::new("scene.prefab").unwrap(),
        )
        .unwrap();
    let nested_resource_locator = web_locator
        .clone()
        .child(
            ContainmentKind::WebFile,
            SourceMemberId::new("data.resource").unwrap(),
        )
        .unwrap();

    assert_eq!(
        resolved_source_id(&snapshot, &archive_locator).kind(),
        SourceKind::Archive
    );
    assert_eq!(
        resolved_source_id(&snapshot, &web_locator).kind(),
        SourceKind::WebFile
    );
    assert_eq!(
        resolved_source_id(&snapshot, &nested_yaml_locator).kind(),
        SourceKind::Yaml
    );
    let resource = resolved_source_id(&snapshot, &nested_resource_locator);
    assert_eq!(resource.kind(), SourceKind::StreamedResource);

    fs::remove_file(path).unwrap();
    assert_eq!(
        snapshot
            .read_source_range(resource, 2, 4, &mut AssetLoadBudget::default())
            .unwrap()
            .contiguous(),
        Some(b"2345".as_slice())
    );
    assert!(matches!(
        snapshot
            .resolve_source(
                &web_locator
                    .clone()
                    .child(
                        ContainmentKind::WebFile,
                        SourceMemberId::new("missing.resource").unwrap(),
                    )
                    .unwrap(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        WorkspaceLookup::Missing
    ));
    assert!(matches!(
        snapshot
            .resolve_source(
                &SourceLocator::path("not-loaded.zip").unwrap(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap(),
        WorkspaceLookup::Unloaded
    ));

    let first_order: Vec<_> = snapshot
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .map(|source| source.id())
        .collect();
    let second_order: Vec<_> = snapshot
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .map(|source| source.id())
        .collect();
    assert_eq!(first_order, second_order);
}

#[test]
fn direct_serialized_and_bundle_sources_share_the_workspace_view() {
    let sample_bundle = sample_bundle_path();
    assert!(sample_bundle.exists());
    let directory = tempfile::tempdir().unwrap();
    let serialized_path = directory.path().join("main.assets");
    fs::write(&serialized_path, sample_serialized_bytes()).unwrap();

    let mut workspace = workspace(6);
    let direct = workspace
        .load_path(&serialized_path, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(direct.kind(), SourceKind::SerializedFile);
    let bundle = workspace
        .load_source(
            SourceOpenRequest::new(
                sample_bundle.clone(),
                SourceAlias::new("fixtures/sample.ab").unwrap(),
            ),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(bundle.kind(), SourceKind::AssetBundle);

    let snapshot = workspace.snapshot();
    let kinds: Vec<_> = snapshot
        .sources(&mut AssetLoadBudget::default())
        .unwrap()
        .into_iter()
        .map(|source| source.kind())
        .collect();
    assert!(kinds.contains(&SourceKind::SerializedFile));
    assert!(kinds.contains(&SourceKind::AssetBundle));
    assert!(
        snapshot
            .objects(&mut AssetLoadBudget::default())
            .unwrap()
            .len()
            > 1
    );
}

#[test]
fn concurrent_snapshot_queries_are_stable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(7);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let expected_revision = snapshot.revision();

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let snapshot = snapshot.clone();
            std::thread::spawn(move || {
                let sources = snapshot.sources(&mut AssetLoadBudget::default()).unwrap();
                let objects = snapshot.objects(&mut AssetLoadBudget::default()).unwrap();
                (snapshot.revision(), sources[0].id(), objects[0].clone())
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(
        results
            .iter()
            .all(|(revision, source, handle)| *revision == expected_revision
                && *source == results[0].1
                && handle == &results[0].2)
    );
}

#[test]
fn object_addresses_resolve_without_implicit_loading() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(8);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let address = ObjectAddress::yaml(SourceLocator::path("scene.prefab").unwrap(), "100").unwrap();
    assert!(matches!(
        snapshot
            .resolve_object(&address, &mut AssetLoadBudget::default())
            .unwrap(),
        WorkspaceLookup::Resolved(_)
    ));

    let missing =
        ObjectAddress::yaml(SourceLocator::path("missing.prefab").unwrap(), "100").unwrap();
    assert!(matches!(
        snapshot
            .resolve_object(&missing, &mut AssetLoadBudget::default())
            .unwrap(),
        WorkspaceLookup::Unloaded
    ));
    assert_eq!(snapshot.revision(), workspace.revision());
}

#[test]
fn locators_with_the_wrong_containment_kind_are_invalid() {
    let archive = zip_with_entries(&[("scene.prefab", FIRST_YAML.as_bytes())]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("content.zip");
    fs::write(&path, archive).unwrap();
    let mut workspace = workspace(9);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let invalid = SourceLocator::path("content.zip")
        .unwrap()
        .child(
            ContainmentKind::WebFile,
            SourceMemberId::new("scene.prefab").unwrap(),
        )
        .unwrap();
    match workspace
        .snapshot()
        .resolve_source(&invalid, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Invalid { diagnostic } => {
            assert_eq!(diagnostic.code(), "WORKSPACE_INVALID_SOURCE_LOCATOR");
        }
        other => panic!("expected invalid locator, got {other:?}"),
    }
}

#[test]
fn object_addresses_with_the_wrong_source_kind_are_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(10);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();

    let address =
        ObjectAddress::binary_direct(SourceLocator::path("scene.prefab").unwrap(), 100).unwrap();
    match workspace
        .snapshot()
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Invalid { diagnostic } => {
            assert_eq!(diagnostic.code(), "WORKSPACE_OBJECT_KIND_MISMATCH");
        }
        other => panic!("expected invalid object address, got {other:?}"),
    }
}

#[test]
fn single_query_results_respect_entry_and_byte_budgets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(11);
    let root = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let address = ObjectAddress::yaml(SourceLocator::path("scene.prefab").unwrap(), "100").unwrap();
    let handle = match snapshot
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(handle) => handle,
        other => panic!("expected resolved object, got {other:?}"),
    };

    assert_single_result_is_budgeted(|budget| snapshot.source(root, budget), 1, 0);
    assert_single_result_is_budgeted(|budget| snapshot.resolve_object(&address, budget), 2, 1);
    assert_single_result_is_budgeted(|budget| snapshot.read_object(&handle, budget), 4, 1);
}

#[test]
fn binary_object_table_scans_charge_each_cached_candidate_visit() {
    const CANDIDATE_COUNT: u64 = 2;

    let directory = tempfile::tempdir().unwrap();
    let file_name = "transform-hierarchy.assets";
    let path = directory.path().join(file_name);
    fs::write(&path, TRANSFORM_HIERARCHY_V22_SERIALIZED_FIXTURE).unwrap();
    let mut workspace = workspace(18);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let locator = SourceLocator::path(file_name).unwrap();
    let missing = ObjectAddress::binary_direct(locator.clone(), 999).unwrap();

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        snapshot.resolve_object(&missing, &mut exact).unwrap(),
        WorkspaceLookup::Missing
    ));
    assert_eq!(exact.usage().entries, CANDIDATE_COUNT);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        snapshot.resolve_object(&missing, &mut one_short),
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "entries",
            limit: 1,
            requested: CANDIDATE_COUNT,
        }))
    ));
    assert_eq!(one_short.usage(), AssetLoadUsage::default());

    let address = ObjectAddress::binary_direct(locator, 1).unwrap();
    let mut resolve_then_read = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT * 2,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let handle = match snapshot
        .resolve_object(&address, &mut resolve_then_read)
        .unwrap()
    {
        WorkspaceLookup::Resolved(handle) => handle,
        other => panic!("expected resolved object, got {other:?}"),
    };
    assert_eq!(resolve_then_read.usage().entries, CANDIDATE_COUNT + 1);
    assert!(matches!(
        snapshot.read_object(&handle, &mut resolve_then_read),
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "entries",
            limit: 4,
            requested: 5,
        }))
    ));
    assert_eq!(resolve_then_read.usage().entries, CANDIDATE_COUNT + 1);
}

#[test]
fn yaml_object_table_scans_charge_each_cached_candidate_visit() {
    const CANDIDATE_COUNT: u64 = 3;

    let directory = tempfile::tempdir().unwrap();
    let file_name = "multi.prefab";
    let path = directory.path().join(file_name);
    fs::write(&path, MULTI_OBJECT_YAML).unwrap();
    let mut workspace = workspace(19);
    workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let locator = SourceLocator::path(file_name).unwrap();
    let missing = ObjectAddress::yaml(locator.clone(), "999").unwrap();

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        snapshot.resolve_object(&missing, &mut exact).unwrap(),
        WorkspaceLookup::Missing
    ));
    assert_eq!(exact.usage().entries, CANDIDATE_COUNT);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        snapshot.resolve_object(&missing, &mut one_short),
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "entries",
            limit: 2,
            requested: CANDIDATE_COUNT,
        }))
    ));
    assert_eq!(one_short.usage(), AssetLoadUsage::default());

    let address = ObjectAddress::yaml(locator, "200").unwrap();
    let mut resolve_then_read = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: CANDIDATE_COUNT * 2,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let handle = match snapshot
        .resolve_object(&address, &mut resolve_then_read)
        .unwrap()
    {
        WorkspaceLookup::Resolved(handle) => handle,
        other => panic!("expected resolved object, got {other:?}"),
    };
    assert_eq!(resolve_then_read.usage().entries, CANDIDATE_COUNT + 1);
    assert!(matches!(
        snapshot.read_object(&handle, &mut resolve_then_read),
        Err(WorkspaceError::Budget(BudgetError::Exceeded {
            resource: "entries",
            limit: 6,
            requested: 7,
        }))
    ));
    assert_eq!(resolve_then_read.usage().entries, CANDIDATE_COUNT + 1);
}

#[test]
fn invalid_yaml_identities_are_rejected_without_publishing_a_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scene.prefab");
    fs::write(&path, FIRST_YAML).unwrap();
    let mut workspace = workspace(12);
    let root = workspace
        .load_path(&path, &mut AssetLoadBudget::default())
        .unwrap();
    let revision = workspace.revision();

    for invalid_anchor in ["12x".to_owned(), "9".repeat(1_025)] {
        fs::write(&path, yaml_with_anchor(&invalid_anchor)).unwrap();
        assert!(
            workspace
                .load_path(&path, &mut AssetLoadBudget::default())
                .is_err()
        );
        assert_eq!(workspace.revision(), revision);
        assert!(matches!(
            workspace
                .snapshot()
                .source(root, &mut AssetLoadBudget::default())
                .unwrap(),
            WorkspaceLookup::Resolved(_)
        ));
    }

    fs::write(&path, DUPLICATE_ANCHOR_YAML).unwrap();
    assert!(matches!(
        workspace.load_path(&path, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::InvalidSourceIdentity {
            reason: unity_asset::workspace::WorkspaceSourceIdentityError::DuplicateYamlAnchor,
            ..
        })
    ));
    assert_eq!(workspace.revision(), revision);
}

#[test]
fn duplicate_binary_path_ids_are_rejected_without_publishing_a_revision() {
    let directory = tempfile::tempdir().unwrap();
    let duplicate = duplicate_path_id_serialized_fixture_bytes();
    let path = directory.path().join("duplicate.asset");
    fs::write(&path, &duplicate).unwrap();
    let mut workspace = workspace(16);
    let revision = workspace.revision();

    assert!(matches!(
        workspace.load_path(&path, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::InvalidSourceIdentity {
            reason: unity_asset::workspace::WorkspaceSourceIdentityError::DuplicateBinaryPathId,
            ..
        })
    ));
    assert_eq!(workspace.revision(), revision);

    let archive = zip_with_entries(&[("nested.prefab", duplicate.as_slice())]);
    let archive_path = directory.path().join("duplicate-container.zip");
    fs::write(&archive_path, archive).unwrap();
    assert!(matches!(
        workspace.load_path(&archive_path, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::InvalidSourceIdentity {
            reason: unity_asset::workspace::WorkspaceSourceIdentityError::DuplicateBinaryPathId,
            ..
        })
    ));
    assert_eq!(workspace.revision(), revision);
    assert!(
        workspace
            .snapshot()
            .sources(&mut AssetLoadBudget::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn yaml_extensions_fall_back_to_serialized_files_at_roots_and_in_archives() {
    let serialized = serialized_fixture_bytes();
    let directory = tempfile::tempdir().unwrap();
    let mut workspace = workspace(13);

    for extension in ["asset", "prefab", "unity"] {
        let path = directory.path().join(format!("root.{extension}"));
        fs::write(&path, &serialized).unwrap();
        let source = workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .unwrap();
        assert_eq!(source.kind(), SourceKind::SerializedFile);
    }

    let archive = zip_with_entries(&[
        ("nested.asset", serialized.as_slice()),
        ("nested.prefab", serialized.as_slice()),
        ("nested.unity", serialized.as_slice()),
    ]);
    let archive_path = directory.path().join("binary-extensions.zip");
    fs::write(&archive_path, archive).unwrap();
    workspace
        .load_path(&archive_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    for extension in ["asset", "prefab", "unity"] {
        let locator =
            SourceLocator::archive_member("binary-extensions.zip", format!("nested.{extension}"))
                .unwrap();
        assert_eq!(
            resolved_source_id(&snapshot, &locator).kind(),
            SourceKind::SerializedFile
        );
    }
}

#[test]
fn yaml_archive_and_binary_budget_errors_keep_one_public_error_shape() {
    let directory = tempfile::tempdir().unwrap();
    let serialized = serialized_fixture_bytes();

    // A resource failure during YAML probing must not fall through to binary parsing.
    assert_load_budget_error(&directory.path().join("probe.asset"), FIRST_YAML.as_bytes());

    let empty_archive = zip_with_entries(&[]);
    assert_load_budget_error(&directory.path().join("empty.zip"), &empty_archive);
    assert_load_budget_error(&directory.path().join("direct.assets"), &serialized);
}

#[test]
fn root_descriptor_backing_is_budgeted_before_the_source_image() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("descriptor.resource");
    fs::write(&path, b"payload").unwrap();
    let canonical = fs::canonicalize(&path).unwrap();
    let alias = SourceAlias::new("budgeted/root/descriptor.resource").unwrap();
    let retained_bytes =
        u64::try_from(alias.retained_clone_bytes() + canonical.as_os_str().len()).unwrap();
    let request = SourceOpenRequest::new(path, alias);
    let mut workspace = workspace(0);
    let revision = workspace.revision();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: retained_bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    assert!(matches!(
        workspace.load_source(request, &mut budget),
        Err(WorkspaceError::Budget(_))
    ));
    assert_eq!(budget.usage(), AssetLoadUsage::default());
    assert_eq!(workspace.revision(), revision);
}

#[test]
fn recognized_compressed_binary_corruption_never_publishes_root_or_member_sources() {
    let mut decoded = b"UnityWebData1.0\0".to_vec();
    decoded.extend_from_slice(&(-1_i32).to_le_bytes());
    let corrupt_webfile = gzip_compress(&decoded);
    let directory = tempfile::tempdir().unwrap();

    let root_path = directory.path().join("corrupt.web");
    fs::write(&root_path, &corrupt_webfile).unwrap();
    let mut root_workspace = workspace(0);
    let root_revision = root_workspace.revision();
    assert!(matches!(
        root_workspace.load_path(&root_path, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::Binary(BinaryError::InvalidData(_)))
    ));
    assert_eq!(root_workspace.revision(), root_revision);
    assert!(
        root_workspace
            .snapshot()
            .sources(&mut AssetLoadBudget::default())
            .unwrap()
            .is_empty()
    );

    let container = webfile_with_entries(&[("corrupt.web", corrupt_webfile.as_slice())]);
    let container_path = directory.path().join("container.web");
    fs::write(&container_path, container).unwrap();
    let mut member_workspace = workspace(0);
    let member_revision = member_workspace.revision();
    assert!(matches!(
        member_workspace.load_path(&container_path, &mut AssetLoadBudget::default()),
        Err(WorkspaceError::BinaryMember {
            container: unity_asset::workspace::WorkspaceSourceContainer::WebFile,
            wire_ordinal: 0,
            source: BinaryError::InvalidData(_),
        })
    ));
    assert_eq!(member_workspace.revision(), member_revision);
    assert!(
        member_workspace
            .snapshot()
            .sources(&mut AssetLoadBudget::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_container_member_identities_are_typed_and_never_published() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            directory.path().join("invalid.zip"),
            zip_with_entries(&[("../escape.resource", b"payload")]),
            WorkspaceSourceContainer::Archive,
            false,
        ),
        (
            directory.path().join("invalid.web"),
            webfile_with_entries(&[("../escape.resource", b"payload")]),
            WorkspaceSourceContainer::WebFile,
            true,
        ),
    ];

    for (path, bytes, expected_container, contract_reason) in cases {
        fs::write(&path, bytes).unwrap();
        let mut workspace = workspace(0);
        let revision = workspace.revision();
        let error = workspace
            .load_path(&path, &mut AssetLoadBudget::default())
            .expect_err("unsafe member identity must be rejected");
        let (container, reason) = match error {
            WorkspaceError::InvalidSourceMemberIdentity {
                container, reason, ..
            } => (container, reason),
            error => panic!("unexpected member identity error: {error}"),
        };
        assert_eq!(container, expected_container);
        if contract_reason {
            assert!(matches!(
                reason,
                WorkspaceSourceMemberIdentityError::Contract(_)
            ));
        } else {
            assert_eq!(
                reason,
                WorkspaceSourceMemberIdentityError::TraversalComponent
            );
        }
        assert_eq!(workspace.revision(), revision);
        assert!(
            workspace
                .snapshot()
                .sources(&mut AssetLoadBudget::default())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn serialized_yaml_extension_has_the_same_budget_boundary_as_binary_extension() {
    let directory = tempfile::tempdir().unwrap();
    let serialized = serialized_fixture_bytes();
    let yaml_extension = directory.path().join("route.prefab");
    let binary_extension = directory.path().join("route.assets");

    let yaml_extension_usage = load_usage(&yaml_extension, &serialized, u64::MAX);
    let binary_extension_usage = load_usage(&binary_extension, &serialized, u64::MAX);
    assert_eq!(yaml_extension_usage, binary_extension_usage);

    let exact = yaml_extension_usage.bytes;
    assert_eq!(
        load_usage(&yaml_extension, &serialized, exact),
        yaml_extension_usage
    );
    assert_eq!(
        load_usage(&binary_extension, &serialized, exact),
        binary_extension_usage
    );

    for path in [&yaml_extension, &binary_extension] {
        let mut workspace = workspace(0);
        let revision = workspace.revision();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            workspace.load_path(path, &mut budget),
            Err(WorkspaceError::Budget(_))
        ));
        assert_eq!(workspace.revision(), revision);
    }
}

#[test]
fn nested_container_depth_is_rejected_before_inner_decompression() {
    let inner = zip_with_entries(&[("payload.resource", b"inner payload")]);
    let outer = zip_with_entries(&[("nested.zip", inner.as_slice())]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("outer.zip");
    fs::write(&path, outer).unwrap();
    let mut workspace = workspace(14);
    let revision = workspace.revision();
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    assert!(matches!(
        workspace.load_path(&path, &mut budget),
        Err(WorkspaceError::Budget(_))
    ));
    assert_eq!(workspace.revision(), revision);
    assert_eq!(
        budget.usage().decompressed_bytes,
        u64::try_from(inner.len()).unwrap()
    );
    assert_eq!(budget.usage().max_observed_depth, 1);
}

#[test]
fn empty_nested_container_is_valid_at_the_exact_depth_limit() {
    let empty_inner = zip_with_entries(&[]);
    let outer = zip_with_entries(&[("empty.zip", empty_inner.as_slice())]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("outer-empty.zip");
    fs::write(&path, outer).unwrap();
    let mut workspace = workspace(15);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    workspace.load_path(&path, &mut budget).unwrap();

    let locator = SourceLocator::archive_member("outer-empty.zip", "empty.zip").unwrap();
    let snapshot = workspace.snapshot();
    assert_eq!(
        resolved_source_id(&snapshot, &locator).kind(),
        SourceKind::Archive
    );
    assert_eq!(budget.usage().max_observed_depth, 1);
}

#[test]
fn embedded_binary_schema_provenance_records_declared_version_and_wire_format() {
    let directory = tempfile::tempdir().unwrap();
    let file_name = "transform-hierarchy.assets";
    let asset_path = directory.path().join(file_name);
    fs::write(&asset_path, TRANSFORM_HIERARCHY_V22_SERIALIZED_FIXTURE).unwrap();
    let mut workspace = workspace(16);
    workspace
        .load_path(&asset_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let address = ObjectAddress::binary_direct(SourceLocator::path(file_name).unwrap(), 1).unwrap();
    let object = planner
        .inspect(&address, &mut AssetLoadBudget::default())
        .unwrap();

    assert_eq!(object.class().class_name, "Transform");
    assert_eq!(object.provenance().origin(), SchemaOrigin::EmbeddedTypeTree);
    assert!(object.provenance().schema_digest().is_some());
    let binary_version = object.provenance().binary_version().unwrap();
    assert_eq!(binary_version.serialized_file_format(), 22);
    let DeclaredUnityVersion::Parsed { version } = binary_version.declared_unity() else {
        panic!("expected a parsed Unity version");
    };
    assert_eq!(version.to_string(), "2020.1.0f1");
}

#[test]
fn rewritten_unknown_unity_versions_remain_readable_but_reject_all_recipes() {
    let directory = tempfile::tempdir().unwrap();
    for (file_name, raw_version, expected_version) in [
        (
            "absent-unity-version.assets",
            "",
            DeclaredUnityVersion::Absent,
        ),
        (
            "unparseable-unity-version.assets",
            "not-a-unity-version",
            DeclaredUnityVersion::Unparseable,
        ),
    ] {
        let asset_path = directory.path().join(file_name);
        fs::write(
            &asset_path,
            transform_fixture_with_unity_version(raw_version),
        )
        .unwrap();
        let mut workspace = workspace(17);
        workspace
            .load_path(&asset_path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let locator = SourceLocator::path(file_name).unwrap();
        let address = ObjectAddress::binary_direct(locator.clone(), 1).unwrap();
        let object = planner
            .inspect(&address, &mut AssetLoadBudget::default())
            .unwrap();

        assert_eq!(object.class().class_name, "Transform");
        assert!(object.class().has_property("m_Father"));
        assert_eq!(object.provenance().origin(), SchemaOrigin::EmbeddedTypeTree);
        assert!(object.provenance().schema_digest().is_some());
        let binary_version = object.provenance().binary_version().unwrap();
        assert_eq!(binary_version.serialized_file_format(), 22);
        assert_eq!(binary_version.declared_unity(), &expected_version);

        let capabilities = planner
            .capabilities_for(&object, &mut AssetLoadBudget::default())
            .unwrap();
        assert!(capabilities.iter().all(|capability| {
            capability.status() == RecipeApplicabilityStatus::Rejected
                && capability.rejection() == Some(RecipeRejectionCode::UnsupportedVersion)
        }));

        let error = planner
            .lower_reference(
                &object,
                FieldPath::root().push_field("m_Father").unwrap(),
                ReferenceTarget::null(),
                ReferenceTarget::object(ObjectAddress::binary_direct(locator, 2).unwrap()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(
            matches!(&error, RecipeError::UnsupportedVersion),
            "expected UnsupportedVersion, got {error:?}"
        );
    }
}

#[test]
fn registry_paths_are_loaded_once_and_frozen_type_trees_drive_prepare() {
    let directory = tempfile::tempdir().unwrap();
    let registry_path = directory.path().join("registry.json");
    fs::write(&registry_path, EXTERNAL_TYPE_TREE_REGISTRY).unwrap();
    let mut registry_budget = AssetLoadBudget::default();
    let options = WorkspaceOptions::strict()
        .with_type_tree_registry_paths(std::slice::from_ref(&registry_path), &mut registry_budget)
        .unwrap();
    assert!(registry_budget.usage().entries > 0);
    assert!(registry_budget.usage().bytes > 0);
    fs::remove_file(&registry_path).unwrap();

    let asset_path = directory.path().join("stripped.assets");
    fs::write(&asset_path, stripped_serialized_fixture_bytes()).unwrap();
    let mut workspace = AssetWorkspace::with_options(options).unwrap();
    workspace
        .load_path(&asset_path, &mut AssetLoadBudget::default())
        .unwrap();
    let snapshot = workspace.snapshot();
    let handle = snapshot
        .objects(&mut AssetLoadBudget::default())
        .unwrap()
        .remove(0);

    let first = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let second = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    assert!(first.class().has_property("m_Value"));
    assert_eq!(second.class().properties(), first.class().properties());
    assert_eq!(
        first.schema_provenance().origin(),
        SchemaOrigin::FrozenRegistry
    );
    assert!(first.schema_provenance().schema_digest().is_some());
    assert_eq!(second.schema_provenance(), first.schema_provenance());

    let address = ObjectAddress::binary_direct(
        SourceLocator::path("stripped.assets").unwrap(),
        handle.object().binary_path_id().unwrap(),
    )
    .unwrap();
    let planner = SchemaRecipePlanner::new(&snapshot);
    let observed = planner
        .inspect(&address, &mut AssetLoadBudget::default())
        .unwrap();
    let root_error = planner
        .lower_field_replace(
            &observed,
            FieldPath::root(),
            MutationValue::signed(123),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert!(matches!(
        root_error,
        RecipeError::Plan(MutationPlanError::RootFieldPath {
            operation: "field_replace"
        })
    ));
    let fragment = planner
        .lower_field_replace(
            &observed,
            FieldPath::root().push_field("m_Value").unwrap(),
            MutationValue::signed(123),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let mut builder = MutationPlanBuilder::new(snapshot.revision());
    builder.append(fragment).unwrap();
    let prepared = workspace
        .prepare(
            builder.build().unwrap(),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    fs::remove_file(&asset_path).unwrap();
    let retained = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(retained.class().properties(), first.class().properties());
    let prepared_view = prepared.view();
    let prepared_handle = match prepared_view
        .resolve_object(&address, &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(handle) => handle,
        other => panic!("prepared object must resolve, got {other:?}"),
    };
    let rewritten = prepared_view
        .read_object(&prepared_handle, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(
        rewritten
            .class()
            .get("m_Value")
            .and_then(|value| value.as_i64()),
        Some(123)
    );
    assert_eq!(
        rewritten.schema_provenance().origin(),
        SchemaOrigin::FrozenRegistry
    );
}
