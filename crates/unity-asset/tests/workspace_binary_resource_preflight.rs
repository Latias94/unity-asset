use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, PlanPayload, PrepareOptions,
    SourceExpectation, SourceOpenRequest, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, ContainmentKind, FieldPath, ObjectAddress,
    SourceAlias, SourceId, SourceKind, SourceLocator, SourceMemberId, UnityValue,
};
use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_binary::bundle::BundleParser;
use unity_asset_core::{field_schema_digest, semantic_value_digest};

const BUNDLE_ALIAS: &str = "banner_1";
const SERIALIZED_MEMBER: &str = "CAB-fa4c27fa39f48e1346f48009626ba08d";
const TEXTURE_PATH_ID: i64 = -3_875_358_842_991_402_074;
const PAYLOAD: &[u8] = b"prepared binary resource payload";
const ORIGINAL_RESOURCE_PATH: &str =
    "archive:/CAB-fa4c27fa39f48e1346f48009626ba08d/CAB-fa4c27fa39f48e1346f48009626ba08d.resS";

fn exact_load_limits(usage: AssetLoadUsage) -> AssetLoadLimits {
    AssetLoadLimits {
        max_entries: usage.entries.max(1),
        max_bytes: usage.bytes.max(1),
        max_depth: usage.max_observed_depth.max(1),
        max_members: usage.members.max(1),
        max_compressed_bytes: usage.compressed_bytes.max(1),
        max_decompressed_bytes: usage.decompressed_bytes.max(1),
        max_expansion_ratio: AssetLoadLimits::default().max_expansion_ratio,
    }
}

struct Fixture {
    directory: TempDir,
    bundle_path: PathBuf,
    original_bundle: Vec<u8>,
    workspace: AssetWorkspace,
    bundle: SourceId,
}

impl Fixture {
    fn open() -> Self {
        let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../unity-asset-binary/tests/samples/banner_1");
        let original_bundle = fs::read(sample).expect("read banner_1 fixture");
        let directory = tempfile::tempdir().unwrap();
        let bundle_path = directory.path().join(BUNDLE_ALIAS);
        fs::write(&bundle_path, &original_bundle).unwrap();

        let mut workspace = AssetWorkspace::new().unwrap();
        let bundle = workspace
            .load_source(
                SourceOpenRequest::new(&bundle_path, SourceAlias::new(BUNDLE_ALIAS).unwrap())
                    .with_kind_hint(SourceKind::AssetBundle),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        Self {
            directory,
            bundle_path,
            original_bundle,
            workspace,
            bundle,
        }
    }
}

fn serialized_locator() -> SourceLocator {
    SourceLocator::path(BUNDLE_ALIAS)
        .unwrap()
        .child(
            ContainmentKind::Bundle,
            SourceMemberId::new(SERIALIZED_MEMBER).unwrap(),
        )
        .unwrap()
}

fn texture_address() -> ObjectAddress {
    ObjectAddress::binary_at(serialized_locator(), TEXTURE_PATH_ID).unwrap()
}

fn resource_path() -> FieldPath {
    FieldPath::root().push_field("m_StreamData").unwrap()
}

fn resource_value(view: &impl WorkspaceView) -> UnityValue {
    let WorkspaceLookup::Resolved(handle) = view
        .resolve_object(&texture_address(), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("banner_1 Texture2D must resolve");
    };
    let object = view
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    assert_eq!(object.class().class_id, 28);
    object
        .class()
        .value_at_path(&resource_path())
        .unwrap()
        .clone()
}

fn resource_plan(fixture: &Fixture) -> MutationPlan {
    let snapshot = fixture.workspace.snapshot();
    let locator = serialized_locator();
    let WorkspaceLookup::Resolved(source) = snapshot
        .resolve_source(&locator, &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("banner_1 SerializedFile member must resolve");
    };
    let WorkspaceLookup::Resolved(handle) = snapshot
        .resolve_object(&texture_address(), &mut AssetLoadBudget::default())
        .unwrap()
    else {
        panic!("banner_1 Texture2D must resolve");
    };
    let object = snapshot
        .read_object(&handle, &mut AssetLoadBudget::default())
        .unwrap();
    let path = resource_path();
    let current = object.class().value_at_path(&path).unwrap();
    let schema = field_schema_digest(
        object
            .schema_provenance()
            .schema_digest()
            .expect("binary object must retain its TypeTree digest"),
        &path,
    )
    .unwrap();
    let value = semantic_value_digest(current, &mut AssetLoadBudget::default()).unwrap();
    let payload = PlanPayload::new(PAYLOAD.to_vec());

    MutationPlan::new(
        snapshot.revision(),
        vec![SourceExpectation::new(locator, source.fingerprint())],
        vec![payload.clone()],
        vec![GenericMutation::ResourceReplace {
            target: texture_address(),
            path,
            guard: FieldGuard::new(schema, value),
            payload: payload.digest(),
        }],
    )
    .unwrap()
}

fn directory_entries(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn source_bytes(view: &impl WorkspaceView, source: SourceId, length: u64) -> Vec<u8> {
    let range = view
        .read_source_range(source, 0, length, &mut AssetLoadBudget::default())
        .unwrap();
    let mut bytes = Vec::new();
    range.copy_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn binary_resource_prepare_rewrites_archive_path_and_external_table_without_writing_disk() {
    let fixture = Fixture::open();
    let before_entries = directory_entries(fixture.directory.path());
    let before_value = resource_value(&fixture.workspace.snapshot());
    assert_eq!(
        before_value
            .as_object()
            .unwrap()
            .get("path")
            .and_then(UnityValue::as_str),
        Some(ORIGINAL_RESOURCE_PATH)
    );

    let original = BundleParser::from_bytes(fixture.original_bundle.clone()).unwrap();
    let original_serialized_node = original
        .nodes
        .iter()
        .find(|node| node.name == SERIALIZED_MEMBER)
        .expect("banner_1 must contain its SerializedFile member");
    assert_eq!(original_serialized_node.flags, 4);
    let original_serialized = SerializedFileParser::from_bytes(
        original
            .extract_node_data(original_serialized_node)
            .unwrap(),
    )
    .unwrap();
    assert!(original_serialized.externals.is_empty());

    let prepared = fixture
        .workspace
        .prepare(
            resource_plan(&fixture),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(directory_entries(fixture.directory.path()), before_entries);
    assert_eq!(
        fs::read(&fixture.bundle_path).unwrap(),
        fixture.original_bundle
    );

    let sidecar = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id().kind() == SourceKind::StreamedResource)
        .expect("resource prepare must report its generated sidecar");
    assert_eq!(sidecar.physical_domain_owner(), fixture.bundle);
    assert!(!sidecar.publication_root());
    let sidecar_source = match prepared
        .view()
        .source(sidecar.source_id(), &mut AssetLoadBudget::default())
        .unwrap()
    {
        WorkspaceLookup::Resolved(source) => source,
        _ => panic!("prepared sidecar source must resolve"),
    };
    assert_eq!(sidecar_source.parent(), Some(fixture.bundle));
    let member = sidecar_source
        .locator()
        .members()
        .last()
        .expect("prepared sidecar must be a Bundle member");
    assert_eq!(member.container(), ContainmentKind::Bundle);
    let sidecar_member = member.name();
    let expected_path = format!("archive:/{SERIALIZED_MEMBER}/{sidecar_member}");

    let fields = resource_value(&prepared.view());
    let fields = fields.as_object().unwrap();
    assert_eq!(
        fields.get("path").and_then(UnityValue::as_str),
        Some(expected_path.as_str())
    );
    assert_eq!(fields.get("offset").and_then(UnityValue::as_u64), Some(0));
    assert_eq!(
        fields.get("size").and_then(UnityValue::as_u64),
        Some(PAYLOAD.len() as u64)
    );

    let serialized_report = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.locator() == &serialized_locator())
        .expect("prepare must report the rebuilt SerializedFile");
    let serialized_bytes = source_bytes(
        &prepared.view(),
        serialized_report.source_id(),
        serialized_report.artifact_bytes(),
    );
    let serialized = SerializedFileParser::from_bytes(serialized_bytes.clone()).unwrap();
    assert_eq!(serialized.externals.len(), 1);
    let external = &serialized.externals[0];
    assert!(external.temp_empty.is_empty());
    assert_eq!(external.guid, [0; 16]);
    assert_eq!(external.type_, 0);
    assert_eq!(external.path, expected_path);

    let sidecar_bytes = source_bytes(
        &prepared.view(),
        sidecar.source_id(),
        sidecar.artifact_bytes(),
    );
    assert_eq!(sidecar_bytes, PAYLOAD);

    let bundle_report = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id() == fixture.bundle)
        .expect("prepare must report the rebuilt Bundle root");
    assert!(bundle_report.publication_root());
    let rebuilt_bundle = BundleParser::from_bytes(source_bytes(
        &prepared.view(),
        fixture.bundle,
        bundle_report.artifact_bytes(),
    ))
    .unwrap();
    let rebuilt_serialized_node = rebuilt_bundle
        .nodes
        .iter()
        .find(|node| node.name == SERIALIZED_MEMBER)
        .expect("rebuilt Bundle must retain its SerializedFile member");
    assert_eq!(rebuilt_serialized_node.flags, 4);
    assert_eq!(
        rebuilt_bundle
            .extract_node_data(rebuilt_serialized_node)
            .unwrap(),
        serialized_bytes
    );
    let rebuilt_sidecar = rebuilt_bundle
        .nodes
        .iter()
        .find(|node| node.name == sidecar_member)
        .expect("rebuilt Bundle must contain the generated sidecar");
    assert_eq!(rebuilt_sidecar.flags, 0);
    assert_eq!(
        rebuilt_bundle.extract_node_data(rebuilt_sidecar).unwrap(),
        PAYLOAD
    );
}

#[test]
fn binary_resource_prepare_obeys_the_measured_exact_caller_budget() {
    let measured_fixture = Fixture::open();
    let mut measured_budget = AssetLoadBudget::default();
    let measured = measured_fixture
        .workspace
        .prepare(
            resource_plan(&measured_fixture),
            PrepareOptions::default(),
            &mut measured_budget,
        )
        .unwrap();
    let usage = measured_budget.usage();
    assert!(usage.bytes > 1);
    drop(measured);

    let limits = exact_load_limits(usage);
    let exact_fixture = Fixture::open();
    let mut exact_budget = AssetLoadBudget::new(limits).unwrap();
    exact_fixture
        .workspace
        .prepare(
            resource_plan(&exact_fixture),
            PrepareOptions::default(),
            &mut exact_budget,
        )
        .unwrap();
    assert_eq!(exact_budget.usage(), usage);

    let mut one_short_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..limits
    })
    .unwrap();
    let one_short_fixture = Fixture::open();
    assert!(
        one_short_fixture
            .workspace
            .prepare(
                resource_plan(&one_short_fixture),
                PrepareOptions::default(),
                &mut one_short_budget,
            )
            .is_err()
    );
}
