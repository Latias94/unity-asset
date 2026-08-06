use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use unity_asset_core::{AssetLoadBudget, ObjectAddress, SourceLocator, YamlFileId};
use unity_asset_search_protocol::{ApiErrorCode, ReferenceRequest, ReindexDisposition};

use crate::generation::{GenerationStorageContract, StoredGenerationManifest};
use crate::generation_store::{GenerationSourceState, GenerationStore, GenerationStoreOptions};
use crate::projection::reference_object_key_for;
use crate::{FilesystemReindexIntent, IndexPaths, SearchIndex, SearchRequest};

const LEGACY_GENERATION_ID: &str =
    "blake3-v1:33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96";
const LEGACY_GENERATION_DIRECTORY: &str =
    "generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96";
const LEGACY_WORKSPACE_ID: &str = "workspace-v1:4b4d4fe3d97e429492182186bc3c73c3";
const LEGACY_REVISION: &str =
    "blake3-v1:c83e10d6ab96079a3f0c40695cdb41bae3809aa093f97820a83c824922da5c5d";
const LEGACY_MANIFEST_DIGEST: &str =
    "blake3-v1:0fb898559722f2056e9c2a2318362656745a0bba18d064871a7752771a99b332";
const LEGACY_SOURCE_STATE_DIGEST: &str =
    "blake3-v1:3e9dad28f15c415e33ddea771b9d08a87a26aab6a9f0cd0b5a2b81a157e12cee";
const LEGACY_SEARCH_PROJECTION_DIGEST: &str =
    "blake3-v1:46290b873bda60316bb5eb3d6eed50b28ee5a0312db0cd0421c25f8cb4bebb79";
const LEGACY_REFERENCE_PROJECTION_DIGEST: &str =
    "blake3-v1:a4a2a61d8636907d1f10b74ba2eedf49f5661a10f97f5617fba82705a0e50df8";
const LEGACY_SEARCH_STABLE_ID: &str = "path:Assets/LegacyCanonical.prefab";
const LEGACY_CANONICAL_REFERENCE_ID: &str =
    "reference-v1:d623639499912d549bce34eee82cd18a0b9de2e00fc40827f6244906c0077b13";
const LEGACY_ODD_ANCHOR_REFERENCE_ID: &str =
    "reference-v1:b3c6c978b187da18e9752589560b3fc8980a3af709384d21acbfac593dc3f1f7";
const LEGACY_CANONICAL_OBJECT_KEY: &str =
    "object-v1:a4f2beafc3eca3faaa321bf3ac679bffe7fdc16a4eba66e24eedd3b0fc551a10";
const LEGACY_ODD_ANCHOR_OBJECT_KEY: &str =
    "object-v1:f89cf2ee40ad67cc35bee807af54771cab4e16e3f956d08ce8612c679aa474d5";
const TARGET_GUID: &str = "aabbccddeeff00112233445566778899";

#[derive(Debug, Clone, Copy)]
struct FixtureFile {
    path: &'static str,
    length: u64,
    sha256: &'static str,
}

const FIXTURE_FILES: &[FixtureFile] = &[
    FixtureFile {
        path: "project/Assets/LegacyCanonical.prefab",
        length: 167,
        sha256: "392005d4ef4887be22613416bdbb21afded392ce20efaa8878d9b410c092c749",
    },
    FixtureFile {
        path: "project/Assets/LegacyOddAnchor.prefab",
        length: 168,
        sha256: "4acf5a31ff07ecb698b0f0a3fac9aaa23e257e5a2ddcaa59cb6cfdc04e5b65cb",
    },
    FixtureFile {
        path: "project/Assets/LegacyTarget.prefab",
        length: 91,
        sha256: "9edb604e0d0706fa98a5b43aeb0d21dd837fe44723eae1b993f134914e9c359d",
    },
    FixtureFile {
        path: "project/Assets/LegacyTarget.prefab.meta",
        length: 60,
        sha256: "581a461a10c90f86b9b56c5b717430c42d1ed2fe33cf5c535f8344d2855dccf6",
    },
    FixtureFile {
        path: "store/activations/00000000000000000001.json",
        length: 463,
        sha256: "b3efa8bf1808702780dfd0152534ac0526a77a2ca4321d5b4b2548ef5cac86d5",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/manifest.json",
        length: 1219,
        sha256: "2b534e7327788c28debbec7aa64d64ee7a83df99a247bbdde804d5598306e845",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/.managed.json",
        length: 258,
        sha256: "9174459453fd4f939aa977e78704a47ea90b83422b5938e19eb73ce9ebe1ff4c",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/.tantivy-meta.lock",
        length: 0,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/.tantivy-writer.lock",
        length: 0,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.fast",
        length: 617,
        sha256: "c1424612fd107ee9a48741e3c351c30e351e9786e932dd694734515a4d233e8d",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.fieldnorm",
        length: 131,
        sha256: "1a934e03af8ade79185a5043901d2c064f522324d411cc810f1acfb0640f8972",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.idx",
        length: 167,
        sha256: "eea01984a091a32e94fdce9172769156429af9e03ca9314aa5fd0470b4857953",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.pos",
        length: 123,
        sha256: "060f5fc209397ef7c0c108e8a0ac91da1cd9778e224fcdaf40986a1c842d0d6a",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.store",
        length: 148,
        sha256: "afbd68e3ad58727380648f0e031609505325577f848d7abaf857c7c61bf9b056",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/d5293ca3282c457f8f8de41c8fe5a2a3.term",
        length: 1108,
        sha256: "8af3e72252da232f8a7e17b647cd8533c18d01dd15718c932ae7efe9612406ac",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/meta.json",
        length: 1691,
        sha256: "08a574cfec98d01a6132f1d87f8dd8eceb83c9433bac7d7251159ed1ca94a9a0",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/reference-payload-v2.jsonl",
        length: 1888,
        sha256: "0f5e27d4bf95e007897880a617eb69ccbb9a3cb2b163952d1615d0b60c3c4578",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/references/schema-contract.json",
        length: 105,
        sha256: "e22763245867f22797183767c14bcc438e72542611bbb7992746b458e25155f1",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/.managed.json",
        length: 258,
        sha256: "1ed31e90394ea881e70e32bab77018f1df94a2baa7db21deb5b126cc0a5a5de7",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/.tantivy-meta.lock",
        length: 0,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/.tantivy-writer.lock",
        length: 0,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.fast",
        length: 308,
        sha256: "2c0c5b8386358d37aa24301ba8765928dfc33254a155e187fd33bc791ce96098",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.fieldnorm",
        length: 198,
        sha256: "dac5daa55e64d3dd5fe8dd0ca8bb0af620469dcf07341ab06ee21c61e11587f1",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.idx",
        length: 341,
        sha256: "e4909b5c9bf6852cc3e51986a65b76fc14f5ada75aae315903d6710e9f7371e4",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.pos",
        length: 239,
        sha256: "1439c2994e511783e727b8db844173e3c72d61703283de2c0c932ef3314860b4",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.store",
        length: 342,
        sha256: "6b3024d680df1a5f84b07f74723bfb419c2ff3b279eb8efb2e4577e1bfbc03c8",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/9426ff4d8ee747359ccadaefe949d85d.term",
        length: 1726,
        sha256: "0a97324dd7db1294242d54c156ce043856ee48e64ba33670bc663590fc112ea3",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/meta.json",
        length: 3598,
        sha256: "4a24fd3bbf59446af5ef79345a0014d4c19615c4496897f6b286de3e5547f58e",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/schema-contract.json",
        length: 102,
        sha256: "a9eefda1600badf754b15f44c61f5f06151c5291ffbb9b23f2000257c9c64ffe",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/search/unity-asset-path-catalog-v1.bin",
        length: 132,
        sha256: "aa78c9a389c576d04b2b682824598c8ac2a81a4c212c61ad79f84abf64090625",
    },
    FixtureFile {
        path: "store/generations/generation-v1-33a66e65ad03b2980af8bece6936677f334970e3d1e959bc76a9272dd8de8d96/state/source-state-v1.json",
        length: 4952,
        sha256: "bbe89b9b9991e2b7a8023f8ceddf79dbd61a42e48bc2c8ec97f295f16f51c0c8",
    },
];

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ObservedFixtureFile {
    path: String,
    length: u64,
    sha256: String,
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("legacy-storage-v1")
}

fn collect_fixture_files(root: &Path, directory: &Path, files: &mut Vec<ObservedFixtureFile>) {
    let mut entries = fs::read_dir(directory)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "frozen fixture contains a symbolic link or junction: {}",
            path.display()
        );
        if metadata.is_dir() {
            collect_fixture_files(root, &path, files);
            continue;
        }
        assert!(metadata.is_file(), "fixture entry is not a file");
        let bytes = fs::read(&path).unwrap();
        files.push(ObservedFixtureFile {
            path: path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            length: u64::try_from(bytes.len()).unwrap(),
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }
}

fn verify_fixture_inventory() {
    let root = fixture_root();
    let mut observed = Vec::new();
    collect_fixture_files(&root, &root.join("project"), &mut observed);
    collect_fixture_files(&root, &root.join("store"), &mut observed);
    observed.sort_unstable();

    let mut expected = FIXTURE_FILES
        .iter()
        .map(|file| ObservedFixtureFile {
            path: file.path.to_owned(),
            length: file.length,
            sha256: file.sha256.to_owned(),
        })
        .collect::<Vec<_>>();
    expected.sort_unstable();
    assert_eq!(observed, expected);
}

fn copy_fixture_tree(source: &Path, destination: &Path) {
    let metadata = fs::symlink_metadata(source).unwrap();
    assert!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "fixture tree root must be a real directory"
    );
    fs::create_dir_all(destination).unwrap();
    let mut entries = fs::read_dir(source)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            copy_fixture_tree(&source_path, &destination_path);
        } else {
            assert!(metadata.is_file());
            fs::copy(source_path, destination_path).unwrap();
        }
    }
}

pub(crate) fn install_frozen_storage_v1_store(destination: &Path) {
    verify_fixture_inventory();
    copy_fixture_tree(&fixture_root().join("store"), destination);
}

#[test]
fn frozen_storage_v1_bytes_and_identities_are_exact() {
    verify_fixture_inventory();
    let root = fixture_root();
    let generation_root = root
        .join("store")
        .join("generations")
        .join(LEGACY_GENERATION_DIRECTORY);
    let activation: serde_json::Value = serde_json::from_slice(
        &fs::read(
            root.join("store")
                .join("activations")
                .join("00000000000000000001.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(activation["contract_version"], 2);
    assert_eq!(activation["ordinal"], 1);
    assert_eq!(activation["generation"], LEGACY_GENERATION_ID);
    assert_eq!(activation["manifest_digest"], LEGACY_MANIFEST_DIGEST);
    assert_eq!(activation["workspace"], LEGACY_WORKSPACE_ID);
    assert_eq!(activation["revision"], LEGACY_REVISION);
    assert_eq!(activation["desired_revision"], LEGACY_REVISION);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(generation_root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["contract_version"], 1);
    assert_eq!(manifest["workspace"], LEGACY_WORKSPACE_ID);
    assert_eq!(manifest["revision"], LEGACY_REVISION);
    assert_eq!(
        manifest["search_projection_digest"],
        LEGACY_SEARCH_PROJECTION_DIGEST
    );
    assert_eq!(
        manifest["reference_projection_digest"],
        LEGACY_REFERENCE_PROJECTION_DIGEST
    );
    assert_eq!(manifest["source_state_digest"], LEGACY_SOURCE_STATE_DIGEST);
    assert_eq!(manifest["projection_summary"]["assets"], 3);
    assert_eq!(manifest["projection_summary"]["search_documents"], 3);
    assert_eq!(manifest["projection_summary"]["reference_documents"], 2);
    assert_eq!(manifest["projection_summary"]["projection_truncations"], 0);
    assert_eq!(manifest["projection_summary"]["incomplete_assets"], 0);

    let temporary = crate::secure_test_tempdir();
    let store_root = temporary.path().join("store");
    copy_fixture_tree(&root.join("store"), &store_root);
    let store = GenerationStore::open(
        &store_root,
        GenerationStoreOptions::default(),
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let active = store.active().unwrap();
    assert_eq!(active.generation().to_string(), LEGACY_GENERATION_ID);
    assert_eq!(
        active.storage_contract(),
        GenerationStorageContract::LegacyV1
    );
    assert_eq!(
        active.directory().file_name().unwrap().to_string_lossy(),
        LEGACY_GENERATION_DIRECTORY
    );
    let StoredGenerationManifest::LegacyV1(manifest) = active.manifest() else {
        panic!("frozen storage-v1 fixture opened as a current manifest");
    };
    assert_eq!(manifest.generation_id(), active.generation());
    assert_eq!(manifest.workspace().to_string(), LEGACY_WORKSPACE_ID);
    assert_eq!(manifest.revision().to_string(), LEGACY_REVISION);
    assert_eq!(
        manifest.source_state_digest().to_string(),
        LEGACY_SOURCE_STATE_DIGEST
    );

    let state = active
        .load_source_state(&mut AssetLoadBudget::default())
        .unwrap();
    let GenerationSourceState::LegacyV1(state) = state else {
        panic!("frozen storage-v1 fixture opened as current source state");
    };
    assert_eq!(state.workspace().to_string(), LEGACY_WORKSPACE_ID);
    assert_eq!(state.revision().to_string(), LEGACY_REVISION);
    assert_eq!(
        state.logical_digest().to_string(),
        LEGACY_SOURCE_STATE_DIGEST
    );
    assert_eq!(state.scan_hints().len(), 3);
    assert_eq!(
        state
            .assets()
            .iter()
            .map(|asset| asset.relative_path())
            .collect::<Vec<_>>(),
        vec![
            "Assets/LegacyCanonical.prefab",
            "Assets/LegacyOddAnchor.prefab",
            "Assets/LegacyTarget.prefab",
        ]
    );
}

#[test]
fn nonempty_storage_v1_generation_is_queryable_and_forces_full_rebuild() {
    let root = fixture_root();
    let temporary = crate::secure_test_tempdir();
    let project = temporary.path().join("project");
    copy_fixture_tree(&root.join("project"), &project);
    let odd_anchor_path = project.join("Assets/LegacyOddAnchor.prefab");
    let paths =
        IndexPaths::for_project(project, Some(temporary.path().join("index-base")), None).unwrap();
    copy_fixture_tree(&root.join("store"), paths.index_root());

    let index =
        SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
    let status = index.status().unwrap();
    let active = status.generation.active.as_ref().unwrap();
    assert_eq!(active.generation.to_string(), LEGACY_GENERATION_ID);
    assert_eq!(active.workspace.to_string(), LEGACY_WORKSPACE_ID);
    assert_eq!(active.actual_revision.to_string(), LEGACY_REVISION);
    assert_eq!(active.desired_revision.to_string(), LEGACY_REVISION);
    assert!(!active.semantics_current);
    assert!(!active.configuration_current);
    assert!(active.stale);
    assert_eq!(status.indexed_assets, 3);
    assert_eq!(status.indexed_search_documents, 3);
    assert_eq!(status.indexed_reference_facts, 2);

    let search = index
        .search(SearchRequest::new("LegacyBeacon", 10))
        .unwrap();
    assert_eq!(search.generation, *active);
    assert_eq!(search.hits.len(), 1);
    assert_eq!(
        search.hits[0].path.as_str(),
        "Assets/LegacyCanonical.prefab"
    );
    assert_eq!(search.hits[0].stable_id, LEGACY_SEARCH_STABLE_ID);

    let incoming = index
        .references(
            ReferenceRequest::incoming_guid(TARGET_GUID, Some(2001), 10),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(
        incoming
            .hits
            .iter()
            .map(|hit| hit.stable_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            LEGACY_ODD_ANCHOR_REFERENCE_ID,
            LEGACY_CANONICAL_REFERENCE_ID,
        ]
    );
    assert_eq!(incoming.diagnostics.len(), 1);
    assert_eq!(
        incoming.diagnostics[0].code(),
        "LEGACY_YAML_ADDRESS_UNREPRESENTABLE"
    );

    let canonical_address = ObjectAddress::yaml(
        SourceLocator::path("Assets/LegacyCanonical.prefab").unwrap(),
        YamlFileId::new(1001).unwrap(),
    )
    .unwrap();
    assert_eq!(
        reference_object_key_for(GenerationStorageContract::LegacyV1, &canonical_address),
        LEGACY_CANONICAL_OBJECT_KEY
    );
    let outgoing = index
        .references(
            ReferenceRequest::outgoing_object(canonical_address, 10),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(outgoing.hits.len(), 1);
    assert_eq!(outgoing.hits[0].stable_id, LEGACY_CANONICAL_REFERENCE_ID);

    let numeric_alias = ObjectAddress::yaml(
        SourceLocator::path("Assets/LegacyOddAnchor.prefab").unwrap(),
        YamlFileId::new(1).unwrap(),
    )
    .unwrap();
    assert_ne!(
        reference_object_key_for(GenerationStorageContract::LegacyV1, &numeric_alias),
        LEGACY_ODD_ANCHOR_OBJECT_KEY
    );
    let alias_query = index
        .references(
            ReferenceRequest::outgoing_object(numeric_alias, 10),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert!(alias_query.hits.is_empty());

    let rejected = index
        .reindex(
            FilesystemReindexIntent::reconcile(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
    assert_eq!(rejected.api_error().code, ApiErrorCode::IndexBuildFailed);
    assert!(
        rejected
            .api_error()
            .message
            .contains("invalid canonical YAML fileID")
    );
    assert_eq!(
        index
            .status()
            .unwrap()
            .generation
            .active
            .as_ref()
            .unwrap()
            .generation
            .to_string(),
        LEGACY_GENERATION_ID
    );

    let odd_anchor = fs::read_to_string(&odd_anchor_path).unwrap();
    let normalized = odd_anchor.replacen(" &01\n", " &1\n", 1);
    assert_ne!(normalized, odd_anchor);
    fs::write(odd_anchor_path, normalized).unwrap();

    let rebuilt = index
        .reindex(
            FilesystemReindexIntent::reconcile(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
    assert_eq!(rebuilt.disposition, ReindexDisposition::Applied);
    assert!(rebuilt.evidence.forced_full_scan);
    assert!(rebuilt.evidence.forced_full_analysis);
    assert!(rebuilt.evidence.analysis.assets_analyzed > 0);
    let rebuilt_generation = rebuilt.generation.unwrap();
    assert_ne!(
        rebuilt_generation.generation.to_string(),
        LEGACY_GENERATION_ID
    );
    assert!(rebuilt_generation.semantics_current);
    assert!(rebuilt_generation.configuration_current);
    assert!(!rebuilt_generation.stale);
    drop(index);

    let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(
        reopened.status().unwrap().generation.active.as_ref(),
        Some(&rebuilt_generation)
    );
}
