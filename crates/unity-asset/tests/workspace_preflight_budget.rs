use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use unity_asset::workspace::{
    AssetWorkspace, FieldGuard, GenericMutation, MutationPlan, MutationValue, PrepareOptions,
    SourceExpectation, SourceOpenRequest, WorkspaceLookup, WorkspaceView,
};
use unity_asset::{
    AssetLoadBudget, ContainmentKind, FieldPath, ObjectAddress, SourceAlias, SourceFingerprint,
    SourceKind, SourceLocator, SourceMemberId, UnityValue, WorkspaceId,
};
use unity_asset_binary::bundle::{AssetBundle, BundleHeader};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_core::{VerifiedSourceImage, semantic_value_digest, yaml_field_schema_digest};
use unity_asset_write::PackingPolicy;
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::bundle::{BundleArtifactEntry, BundleWriter};

const BUNDLE_ALIAS: &str = "large.bundle";
const YAML_MEMBER: &str = "scene.prefab";
const YAML: &str =
    "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Before\n";

fn name_path() -> FieldPath {
    FieldPath::root().push_field("m_Name").unwrap()
}

fn yaml_locator() -> SourceLocator {
    SourceLocator::path(BUNDLE_ALIAS)
        .unwrap()
        .child(
            ContainmentKind::Bundle,
            SourceMemberId::new(YAML_MEMBER).unwrap(),
        )
        .unwrap()
}

fn address() -> ObjectAddress {
    ObjectAddress::yaml(yaml_locator(), "1").unwrap()
}

fn guard_for(value: &str) -> FieldGuard {
    let class = unity_asset::UnityClass::new(1, "GameObject".to_owned(), "1".to_owned());
    let value = UnityValue::String(value.to_owned());
    let mut budget = AssetLoadBudget::default();
    FieldGuard::new(
        yaml_field_schema_digest(&class, &name_path(), &value, &mut budget).unwrap(),
        semantic_value_digest(&value, &mut budget).unwrap(),
    )
}

fn plan(workspace: &AssetWorkspace) -> MutationPlan {
    MutationPlan::new(
        workspace.workspace_id(),
        workspace.revision(),
        vec![SourceExpectation::new(
            yaml_locator(),
            SourceFingerprint::from_bytes(SourceKind::Yaml, YAML.as_bytes()),
        )],
        Vec::new(),
        vec![GenericMutation::FieldReplace {
            target: address(),
            path: name_path(),
            guard: guard_for("Before"),
            replacement: MutationValue::string("After").unwrap(),
        }],
    )
    .unwrap()
}

fn read_name(view: &impl WorkspaceView) -> String {
    let mut budget = AssetLoadBudget::default();
    let WorkspaceLookup::Resolved(handle) = view.resolve_object(&address(), &mut budget).unwrap()
    else {
        panic!("fixture object must resolve");
    };
    view.read_object(&handle, &mut budget)
        .unwrap()
        .class()
        .value_at_path(&name_path())
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

fn deterministic_padding(length: usize) -> Vec<u8> {
    let mut state = 0x8a5c_1f37_u32;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn source_payload(workspace: WorkspaceId, local: u128, bytes: Arc<[u8]>) -> ArtifactPayload {
    let source = unity_asset::SourceId::new(workspace, SourceKind::Yaml, local).unwrap();
    let image = VerifiedSourceImage::verify(SourceKind::Yaml, bytes);
    ArtifactPayload::source_backed(source, image).unwrap()
}

fn compressed_bundle() -> Vec<u8> {
    let header = BundleHeader {
        signature: "UnityFS".to_owned(),
        version: 7,
        unity_version: "2021.3.0f1".to_owned(),
        unity_revision: "2021.3.0f1".to_owned(),
        size: 1,
        compressed_blocks_info_size: 1,
        uncompressed_blocks_info_size: 1,
        flags: 0xc0,
        actual_header_size: 0,
        legacy_web_raw: None,
        file_stream_header_byte: None,
    };
    let mut bundle = AssetBundle::new(header, Vec::new());
    bundle.blocks.push(CompressionBlock::new(1, 1, 2));

    let workspace = WorkspaceId::from_u128(0x13_09).unwrap();
    let yaml = source_payload(workspace, 1, Arc::from(YAML.as_bytes()));
    let padding = source_payload(
        workspace,
        2,
        Arc::from(deterministic_padding(2 * 1024 * 1024)),
    );
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget).unwrap();
    let output = declaration
        .declare_output(LogicalArtifactName::new(BUNDLE_ALIAS).unwrap())
        .unwrap();
    let mut batch = declaration.seal_output_names().unwrap();
    let yaml = batch.prepare_verbatim_source(&yaml).unwrap();
    let padding = batch.prepare_verbatim_source(&padding).unwrap();
    let entries = [
        BundleArtifactEntry::file(&batch, YAML_MEMBER, 0, yaml).unwrap(),
        BundleArtifactEntry::file(&batch, "padding.bin", 0, padding).unwrap(),
    ];
    let root =
        BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Lz4).unwrap();
    batch.bind_output(output, root).unwrap();
    let artifacts = batch.finish().unwrap();
    let mut bytes = Vec::new();
    artifacts
        .artifact(root)
        .unwrap()
        .stream_verified_to(&mut bytes)
        .unwrap();
    bytes
}

fn workspace_fixture() -> (TempDir, std::path::PathBuf, AssetWorkspace) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(BUNDLE_ALIAS);
    fs::write(&path, compressed_bundle()).unwrap();
    let mut workspace = AssetWorkspace::new().unwrap();
    workspace
        .load_source(
            SourceOpenRequest::new(&path, SourceAlias::new(BUNDLE_ALIAS).unwrap())
                .with_kind_hint(SourceKind::AssetBundle),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    (directory, path, workspace)
}

fn exact_limits(usage: unity_asset_write::artifact::ArtifactBudgetUsage) -> ArtifactLimits {
    ArtifactLimits::default()
        .with_max_outputs(usage.outputs())
        .with_max_proof_images(usage.proof_images())
        .with_max_segments(usage.segments())
        .with_max_publication_bytes(usage.publication_bytes())
        .with_max_proof_bytes(usage.proof_bytes())
        .with_max_generated_bytes(usage.generated_bytes())
        .with_max_generated_chunk_bytes(usage.generated_bytes())
        .with_max_metadata_bytes(usage.metadata_bytes())
        .with_max_pinned_source_bytes(usage.pinned_source_bytes())
        .with_max_retained_bytes(usage.retained_bytes())
        .with_max_scratch_bytes(usage.peak_scratch_bytes())
}

#[test]
fn large_compressed_bundle_obeys_exact_artifact_limits_with_an_old_snapshot_retained() {
    let (_directory, path, workspace) = workspace_fixture();
    let baseline_bytes = fs::read(&path).unwrap();
    let old_snapshot = workspace.snapshot();

    let measured = workspace
        .prepare(
            plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    let usage = measured.artifact_usage();
    let limits = exact_limits(usage);
    let exact = workspace
        .prepare(
            plan(&workspace),
            PrepareOptions::new(limits),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    assert_eq!(exact.artifact_usage(), usage);
    assert!(usage.retained_bytes() <= limits.max_retained_bytes());
    assert!(usage.peak_scratch_bytes() <= limits.max_scratch_bytes());
    assert_eq!(read_name(&exact.view()), "After");
    assert_eq!(read_name(&old_snapshot), "Before");
    assert_eq!(fs::read(&path).unwrap(), baseline_bytes);

    let one_short = limits.with_max_retained_bytes(usage.retained_bytes() - 1);
    let rejected = workspace.prepare(
        plan(&workspace),
        PrepareOptions::new(one_short),
        &mut AssetLoadBudget::default(),
    );
    assert!(rejected.is_err());
    assert_eq!(read_name(&old_snapshot), "Before");
    assert_eq!(read_name(&workspace.snapshot()), "Before");
    assert_eq!(fs::read(&path).unwrap(), baseline_bytes);
}

#[test]
fn small_nested_edit_reports_the_complete_compressed_bundle_rewrite_cost() {
    let (_directory, path, workspace) = workspace_fixture();
    let baseline_len = fs::metadata(&path).unwrap().len();
    let prepared = workspace
        .prepare(
            plan(&workspace),
            PrepareOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

    let root = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id().kind() == SourceKind::AssetBundle)
        .unwrap();
    let leaf = prepared
        .report()
        .sources()
        .iter()
        .find(|source| source.source_id().kind() == SourceKind::Yaml)
        .unwrap();

    assert!(root.publication_root());
    assert_eq!(root.logical_changed_bytes(), 0);
    assert_eq!(root.physical_rewrite_bytes(), root.artifact_bytes());
    assert!(root.physical_rewrite_bytes() > leaf.artifact_bytes() * 1_000);
    assert!(!leaf.publication_root());
    assert_eq!(leaf.logical_changed_bytes(), leaf.artifact_bytes());
    assert_eq!(leaf.physical_rewrite_bytes(), 0);
    assert!(root.artifact_bytes() >= baseline_len / 2);
}
