use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, path::Path};

use unity_asset::environment::{BinarySource, BinarySourceKind, Environment};
use unity_asset::workspace::{
    CatalogError, PhysicalOrigin, PhysicalOriginError, SourceCatalog, SourceDescriptor,
    SourceLocationKind,
};
use unity_asset_core::{
    BundleMemberId, ChangeSetError, IdentityRemap, ObjectAddress, ObjectId, SourceAlias,
    SourceFingerprint, SourceKind, SourceLocator, UnityAssetError, UnityValue, WorkspaceId,
};

fn fingerprint(kind: SourceKind, bytes: &[u8]) -> SourceFingerprint {
    SourceFingerprint::from_bytes(kind, bytes)
}

static PHYSICAL_ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| tempfile::tempdir().unwrap());
static PHYSICAL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn physical_file(contents: &str) -> PhysicalOrigin {
    let sequence = PHYSICAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = PHYSICAL_ROOT
        .path()
        .join(format!("origin-{sequence}.asset"));
    fs::write(&path, contents).unwrap();
    PhysicalOrigin::from_existing_path(path).unwrap()
}

fn root(kind: SourceKind, alias: &str, physical: &str) -> SourceDescriptor {
    SourceDescriptor::root(
        kind,
        SourceAlias::new(alias).unwrap(),
        physical_file(physical),
    )
}

#[test]
fn catalog_uses_explicit_portable_aliases_without_collapsing_members() {
    let workspace = WorkspaceId::from_u128(7).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let source_fingerprint = fingerprint(SourceKind::SerializedFile, b"same bytes");
    let temp = tempfile::tempdir().unwrap();
    let assets = temp.path().join("Assets");
    fs::create_dir(&assets).unwrap();
    let source_path = assets.join("Main.assets");
    fs::write(&source_path, b"same bytes").unwrap();
    let dot_dot_alias = assets.join("..").join("Assets").join("Main.assets");

    let source = catalog
        .register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("main.assets").unwrap(),
                PhysicalOrigin::from_existing_path(dot_dot_alias).unwrap(),
            ),
            source_fingerprint,
        )
        .unwrap();
    assert_eq!(
        catalog
            .lookup_physical(&PhysicalOrigin::from_existing_path(source_path).unwrap())
            .unwrap(),
        source
    );

    let archive = catalog
        .register(
            root(SourceKind::Archive, "game.apk", "build/game.apk"),
            fingerprint(SourceKind::Archive, b"archive"),
        )
        .unwrap();
    let first_member = catalog
        .register(
            SourceDescriptor::archive_member(
                archive,
                SourceKind::SerializedFile,
                BundleMemberId::new("a/main.assets").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"first"),
        )
        .unwrap();
    let second_member = catalog
        .register(
            SourceDescriptor::archive_member(
                archive,
                SourceKind::SerializedFile,
                BundleMemberId::new("b/main.assets").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"second"),
        )
        .unwrap();

    assert_ne!(first_member, second_member);
    assert_eq!(catalog.len(), 4);
}

#[test]
fn catalog_models_recursive_container_and_resource_placement() {
    let workspace = WorkspaceId::from_u128(8).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let archive = catalog
        .register(
            root(SourceKind::Archive, "game.apk", "build/game.apk"),
            fingerprint(SourceKind::Archive, b"archive"),
        )
        .unwrap();
    let webfile = catalog
        .register(
            SourceDescriptor::archive_member(
                archive,
                SourceKind::WebFile,
                BundleMemberId::new("assets/data.web").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::WebFile, b"webfile"),
        )
        .unwrap();
    let bundle = catalog
        .register(
            SourceDescriptor::webfile_member(
                webfile,
                SourceKind::AssetBundle,
                BundleMemberId::new("game.bundle").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::AssetBundle, b"bundle"),
        )
        .unwrap();
    let serialized = catalog
        .register(
            SourceDescriptor::bundle_member(
                bundle,
                SourceKind::SerializedFile,
                BundleMemberId::new("CAB-main").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"serialized"),
        )
        .unwrap();
    let resource = catalog
        .register(
            SourceDescriptor::sidecar(bundle, BundleMemberId::new("CAB-main.resS").unwrap())
                .unwrap(),
            fingerprint(SourceKind::StreamedResource, b"resource"),
        )
        .unwrap();

    assert_eq!(catalog.resolve(serialized).unwrap().parent(), Some(bundle));
    assert_eq!(
        catalog.resolve(serialized).unwrap().location_kind(),
        SourceLocationKind::BundleMember
    );
    assert_eq!(catalog.resolve(resource).unwrap().parent(), Some(bundle));
    assert_eq!(
        catalog.source_locator(serialized).unwrap().members().len(),
        3
    );
    assert_eq!(
        catalog
            .source_locator(serialized)
            .unwrap()
            .bundle_member()
            .unwrap()
            .name(),
        "CAB-main"
    );
    assert_eq!(
        catalog.physical_origin(serialized).unwrap(),
        catalog.physical_origin(archive).unwrap()
    );
}

#[test]
fn two_serialized_files_with_the_same_path_id_resolve_to_distinct_objects() {
    let workspace = WorkspaceId::from_u128(9).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let bundle = catalog
        .register(
            root(SourceKind::AssetBundle, "game.bundle", "build/game.bundle"),
            fingerprint(SourceKind::AssetBundle, b"bundle"),
        )
        .unwrap();
    let left_member = BundleMemberId::new("CAB-left").unwrap();
    let right_member = BundleMemberId::new("CAB-right").unwrap();
    let left_source = catalog
        .register(
            SourceDescriptor::bundle_member(
                bundle,
                SourceKind::SerializedFile,
                left_member.clone(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"left"),
        )
        .unwrap();
    let right_source = catalog
        .register(
            SourceDescriptor::bundle_member(
                bundle,
                SourceKind::SerializedFile,
                right_member.clone(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"right"),
        )
        .unwrap();

    let left_address = ObjectAddress::binary_bundle_member(
        SourceLocator::path("game.bundle").unwrap(),
        left_member,
        42,
    )
    .unwrap();
    let right_address = ObjectAddress::binary_bundle_member(
        SourceLocator::path("game.bundle").unwrap(),
        right_member,
        42,
    )
    .unwrap();

    let left = catalog.resolve_object_address(&left_address).unwrap();
    let right = catalog.resolve_object_address(&right_address).unwrap();
    assert_eq!(left.object().source(), left_source);
    assert_eq!(right.object().source(), right_source);
    assert_ne!(left, right);
}

#[test]
fn duplicate_member_names_require_an_explicit_occurrence() {
    let workspace = WorkspaceId::from_u128(10).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let bundle = catalog
        .register(
            root(SourceKind::AssetBundle, "game.bundle", "build/game.bundle"),
            fingerprint(SourceKind::AssetBundle, b"bundle"),
        )
        .unwrap();
    let first = catalog
        .register(
            SourceDescriptor::bundle_member(
                bundle,
                SourceKind::SerializedFile,
                BundleMemberId::new("CAB-main").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"first"),
        )
        .unwrap();
    let duplicate = catalog
        .register(
            SourceDescriptor::bundle_member(
                bundle,
                SourceKind::SerializedFile,
                BundleMemberId::with_occurrence("CAB-main", 1).unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"second"),
        )
        .unwrap();

    assert_ne!(first, duplicate);
}

#[test]
fn duplicate_names_are_representable_in_every_container_family() {
    let workspace = WorkspaceId::from_u128(18).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let archive = catalog
        .register(
            root(SourceKind::Archive, "game.apk", "game.apk"),
            fingerprint(SourceKind::Archive, b"archive"),
        )
        .unwrap();
    let first = catalog
        .register(
            SourceDescriptor::archive_member(
                archive,
                SourceKind::WebFile,
                BundleMemberId::new("data.web").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::WebFile, b"first"),
        )
        .unwrap();
    let second = catalog
        .register(
            SourceDescriptor::archive_member(
                archive,
                SourceKind::WebFile,
                BundleMemberId::with_occurrence("data.web", 1).unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::WebFile, b"second"),
        )
        .unwrap();
    assert_ne!(first, second);

    let first_bundle = catalog
        .register(
            SourceDescriptor::webfile_member(
                first,
                SourceKind::AssetBundle,
                BundleMemberId::new("game.bundle").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::AssetBundle, b"first bundle"),
        )
        .unwrap();
    let second_bundle = catalog
        .register(
            SourceDescriptor::webfile_member(
                first,
                SourceKind::AssetBundle,
                BundleMemberId::with_occurrence("game.bundle", 1).unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::AssetBundle, b"second bundle"),
        )
        .unwrap();
    assert_ne!(first_bundle, second_bundle);
    assert_eq!(
        catalog
            .source_locator(second_bundle)
            .unwrap()
            .members()
            .last()
            .unwrap()
            .member()
            .same_name_occurrence(),
        1
    );

    let first_resource = catalog
        .register(
            SourceDescriptor::sidecar(archive, BundleMemberId::new("shared.resS").unwrap())
                .unwrap(),
            fingerprint(SourceKind::StreamedResource, b"first resource"),
        )
        .unwrap();
    let second_resource = catalog
        .register(
            SourceDescriptor::sidecar(
                archive,
                BundleMemberId::with_occurrence("shared.resS", 1).unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::StreamedResource, b"second resource"),
        )
        .unwrap();
    assert_ne!(first_resource, second_resource);
}

#[test]
fn catalog_rejects_cross_workspace_invalid_parent_and_fingerprint_kinds() {
    let mut first = SourceCatalog::new(WorkspaceId::from_u128(1).unwrap());
    let second = SourceCatalog::new(WorkspaceId::from_u128(2).unwrap());
    let serialized = first
        .register(
            root(SourceKind::SerializedFile, "mainData", "mainData"),
            fingerprint(SourceKind::SerializedFile, b"main"),
        )
        .unwrap();

    assert!(first.resolve(serialized).is_ok());
    assert!(second.resolve(serialized).is_err());
    let wrong_kind_address =
        ObjectAddress::yaml(SourceLocator::path("mainData").unwrap(), "1").unwrap();
    assert!(matches!(
        first.resolve_object_address(&wrong_kind_address),
        Err(CatalogError::ObjectAddressSourceKindMismatch { .. })
    ));
    assert!(matches!(
        SourceDescriptor::bundle_member(
            serialized,
            SourceKind::SerializedFile,
            BundleMemberId::new("CAB-bad").unwrap(),
        ),
        Err(CatalogError::InvalidParentKind { .. })
    ));
    assert!(matches!(
        first.register(
            root(SourceKind::Yaml, "scene.unity", "scene.unity"),
            fingerprint(SourceKind::SerializedFile, b"wrong"),
        ),
        Err(CatalogError::SourceKindMismatch { .. })
    ));

    let archive = first
        .register(
            root(SourceKind::Archive, "archive.zip", "archive.zip"),
            fingerprint(SourceKind::Archive, b"archive"),
        )
        .unwrap();
    let member = BundleMemberId::new("same-slot").unwrap();
    first
        .register(
            SourceDescriptor::archive_member(archive, SourceKind::SerializedFile, member.clone())
                .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"serialized"),
        )
        .unwrap();
    assert!(matches!(
        first.register(
            SourceDescriptor::archive_member(archive, SourceKind::Yaml, member).unwrap(),
            fingerprint(SourceKind::Yaml, b"yaml"),
        ),
        Err(CatalogError::LocatorCollision { .. })
    ));
}

#[test]
fn fingerprint_kind_is_immutable_and_content_changes_revision() {
    let mut catalog = SourceCatalog::new(WorkspaceId::from_u128(11).unwrap());
    let source = catalog
        .register(
            root(SourceKind::SerializedFile, "mainData", "mainData"),
            fingerprint(SourceKind::SerializedFile, b"version one"),
        )
        .unwrap();
    let first = catalog.revision().unwrap();

    let _ = catalog.resolve(source).unwrap();
    let _ = catalog.fingerprint(source).unwrap();
    assert_eq!(catalog.revision().unwrap(), first);

    assert!(matches!(
        catalog.update_fingerprint(source, fingerprint(SourceKind::Yaml, b"wrong kind")),
        Err(CatalogError::SourceKindMismatch { .. })
    ));
    assert_eq!(catalog.revision().unwrap(), first);

    catalog
        .update_fingerprint(
            source,
            fingerprint(SourceKind::SerializedFile, b"version two"),
        )
        .unwrap();
    assert_ne!(catalog.revision().unwrap(), first);
}

#[test]
fn source_ids_and_revision_do_not_depend_on_registration_order() {
    let workspace = WorkspaceId::from_u128(12).unwrap();
    let descriptors = [
        root(SourceKind::SerializedFile, "a.assets", "root/a.assets"),
        root(SourceKind::SerializedFile, "b.assets", "root/b.assets"),
        root(SourceKind::Yaml, "c.prefab", "root/c.prefab"),
    ];
    let fingerprints = [
        fingerprint(SourceKind::SerializedFile, b"a"),
        fingerprint(SourceKind::SerializedFile, b"b"),
        fingerprint(SourceKind::Yaml, b"c"),
    ];
    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut expected_ids = None;
    let mut expected_revision = None;

    for permutation in permutations {
        let mut catalog = SourceCatalog::new(workspace);
        let mut ids = Vec::new();
        for index in permutation {
            ids.push((
                index,
                catalog
                    .register(descriptors[index].clone(), fingerprints[index])
                    .unwrap(),
            ));
        }
        ids.sort_unstable_by_key(|(index, _)| *index);
        let ids = ids.into_iter().map(|(_, id)| id).collect::<Vec<_>>();
        let revision = catalog.revision().unwrap();
        assert_eq!(expected_ids.get_or_insert_with(|| ids.clone()), &ids);
        assert_eq!(
            expected_revision.get_or_insert(revision),
            &revision,
            "registration order changed canonical revision"
        );
    }

    assert_eq!(
        expected_revision.unwrap().to_string(),
        "blake3-v1:564ac1d6c0e1ffe16ebb5e79cda16873368bfd91bcdcbfb7d89e9e504bd9d8db"
    );
}

#[test]
fn physical_alias_population_does_not_change_revision_or_logical_identity() {
    let mut catalog = SourceCatalog::new(WorkspaceId::from_u128(13).unwrap());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("Main.assets");
    fs::write(&path, b"same").unwrap();
    let source = catalog
        .register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("main.assets").unwrap(),
                PhysicalOrigin::from_existing_path(&path).unwrap(),
            ),
            fingerprint(SourceKind::SerializedFile, b"same"),
        )
        .unwrap();
    let revision = catalog.revision().unwrap();

    let spelling_alias = temp.path().join(".").join("Main.assets");
    let rebound = catalog
        .register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("main.assets").unwrap(),
                PhysicalOrigin::from_existing_path(&spelling_alias).unwrap(),
            ),
            fingerprint(SourceKind::SerializedFile, b"same"),
        )
        .unwrap();

    assert_eq!(rebound, source);
    assert_eq!(catalog.revision().unwrap(), revision);
    assert_eq!(
        catalog
            .lookup_physical(&PhysicalOrigin::from_existing_path(spelling_alias).unwrap())
            .unwrap(),
        source
    );
}

#[test]
fn physical_origins_reject_relative_missing_and_directory_paths() {
    assert!(matches!(
        PhysicalOrigin::from_existing_path("relative.assets"),
        Err(PhysicalOriginError::NotAbsolute(_))
    ));

    let temp = tempfile::tempdir().unwrap();
    assert!(matches!(
        PhysicalOrigin::from_existing_path(temp.path().join("missing.assets")),
        Err(PhysicalOriginError::Io { .. })
    ));
    assert!(matches!(
        PhysicalOrigin::from_existing_path(temp.path()),
        Err(PhysicalOriginError::NotRegularFile(_))
    ));
}

#[test]
fn physical_origin_canonicalizes_symlink_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.assets");
    let alias = temp.path().join("alias.assets");
    fs::write(&target, b"asset").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&target, &alias) {
        eprintln!("skipping symlink identity assertion: {error}");
        return;
    }

    assert_eq!(
        PhysicalOrigin::from_existing_path(target).unwrap(),
        PhysicalOrigin::from_existing_path(alias).unwrap()
    );
}

#[test]
fn catalog_rejects_conflicting_logical_and_physical_root_bindings() {
    let workspace = WorkspaceId::from_u128(19).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let first_path = physical_file("first");
    let second_path = physical_file("second");
    catalog
        .register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("first.assets").unwrap(),
                first_path.clone(),
            ),
            fingerprint(SourceKind::SerializedFile, b"same"),
        )
        .unwrap();

    assert!(matches!(
        catalog.register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("second.assets").unwrap(),
                first_path,
            ),
            fingerprint(SourceKind::SerializedFile, b"same"),
        ),
        Err(CatalogError::PhysicalOriginConflict { .. })
    ));
    assert!(matches!(
        catalog.register(
            SourceDescriptor::root(
                SourceKind::SerializedFile,
                SourceAlias::new("first.assets").unwrap(),
                second_path,
            ),
            fingerprint(SourceKind::SerializedFile, b"same"),
        ),
        Err(CatalogError::PhysicalOriginChanged { .. })
    ));
}

#[test]
fn portable_alias_and_member_validation_rejects_platform_ambiguity() {
    for invalid in ["", "/absolute", r"a\b", "a/../b", "C:/drive"] {
        assert!(
            SourceAlias::new(invalid).is_err(),
            "accepted alias {invalid:?}"
        );
    }
    for invalid in ["", "/absolute", r"a\b", "a/../b", "a//b"] {
        assert!(
            BundleMemberId::new(invalid).is_err(),
            "accepted member {invalid:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn unix_physical_backslash_and_separator_paths_remain_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let backslash_path = temp.path().join(r"a\b");
    let separator_path = temp.path().join("a").join("b");
    fs::write(&backslash_path, b"backslash").unwrap();
    fs::create_dir(temp.path().join("a")).unwrap();
    fs::write(&separator_path, b"separator").unwrap();
    let backslash = PhysicalOrigin::from_existing_path(backslash_path).unwrap();
    let separator = PhysicalOrigin::from_existing_path(separator_path).unwrap();
    assert_ne!(backslash, separator);
}

#[cfg(windows)]
#[test]
fn windows_rejects_unstable_namespaces_and_alternate_streams() {
    for invalid in [r"C:main.assets", r"\main.assets"] {
        assert!(matches!(
            PhysicalOrigin::from_existing_path(invalid),
            Err(PhysicalOriginError::NotAbsolute(_))
        ));
    }
    assert!(matches!(
        PhysicalOrigin::from_existing_path(r"\\.\C:\main.assets"),
        Err(PhysicalOriginError::UnsupportedWindowsNamespace(_))
    ));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("main.assets");
    fs::write(&path, b"asset").unwrap();
    let ads = PathBuf::from(format!("{}:stream", path.display()));
    assert!(matches!(
        PhysicalOrigin::from_existing_path(ads),
        Err(PhysicalOriginError::AlternateDataStream(_))
    ));
}

#[cfg(windows)]
#[test]
fn windows_physical_origin_uses_filesystem_case_identity() {
    let temp = tempfile::tempdir().unwrap();
    let mixed = temp.path().join("MiXeD.assets");
    let upper = temp.path().join("MIXED.ASSETS");
    fs::write(&mixed, b"asset").unwrap();
    assert_eq!(
        PhysicalOrigin::from_existing_path(mixed).unwrap(),
        PhysicalOrigin::from_existing_path(upper).unwrap()
    );
}

#[test]
fn logical_addresses_survive_repack_and_workspace_relocation() {
    let workspace = WorkspaceId::from_u128(16).unwrap();
    let member = BundleMemberId::new("CAB-main").unwrap();

    let mut before = SourceCatalog::new(workspace);
    let old_bundle = before
        .register(
            root(SourceKind::AssetBundle, "game.bundle", "input/game.bundle"),
            fingerprint(SourceKind::AssetBundle, b"old bundle"),
        )
        .unwrap();
    let old_source = before
        .register(
            SourceDescriptor::bundle_member(old_bundle, SourceKind::SerializedFile, member.clone())
                .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"old serialized"),
        )
        .unwrap();
    let address = ObjectAddress::binary_bundle_member(
        SourceLocator::path("game.bundle").unwrap(),
        member.clone(),
        -7,
    )
    .unwrap();
    let old_handle = before.resolve_object_address(&address).unwrap();
    let old_revision = before.revision().unwrap();

    let mut after = SourceCatalog::new(workspace);
    let new_bundle = after
        .register(
            root(SourceKind::AssetBundle, "game.bundle", "output/game.bundle"),
            fingerprint(SourceKind::AssetBundle, b"repacked bundle"),
        )
        .unwrap();
    let new_source = after
        .register(
            SourceDescriptor::bundle_member(new_bundle, SourceKind::SerializedFile, member)
                .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"repacked serialized"),
        )
        .unwrap();
    let new_revision = after.revision().unwrap();
    let new_handle = after.resolve_object_address(&address).unwrap();

    assert_eq!(old_source, new_source);
    assert_eq!(
        new_handle.object(),
        &ObjectId::binary(new_source, -7).unwrap()
    );
    assert_eq!(after.address_for_handle(&new_handle).unwrap(), address);
    assert!(matches!(
        after.address_for_handle(&old_handle),
        Err(CatalogError::InvalidHandleContext(_))
    ));
    assert_ne!(old_revision, new_revision);
    assert!(
        old_handle
            .validate_context(workspace, new_revision)
            .is_err()
    );
}

#[test]
fn member_boundaries_remain_distinct_without_string_key_collisions() {
    let workspace = WorkspaceId::from_u128(17).unwrap();
    let mut catalog = SourceCatalog::new(workspace);
    let first_archive = catalog
        .register(
            root(SourceKind::Archive, "a-colon-b", "a:b"),
            fingerprint(SourceKind::Archive, b"first archive"),
        )
        .unwrap();
    let second_archive = catalog
        .register(
            root(SourceKind::Archive, "a", "a"),
            fingerprint(SourceKind::Archive, b"second archive"),
        )
        .unwrap();
    let left = catalog
        .register(
            SourceDescriptor::archive_member(
                first_archive,
                SourceKind::SerializedFile,
                BundleMemberId::new("c").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"same"),
        )
        .unwrap();
    let right = catalog
        .register(
            SourceDescriptor::archive_member(
                second_archive,
                SourceKind::SerializedFile,
                BundleMemberId::new("b/c").unwrap(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, b"same"),
        )
        .unwrap();

    assert_ne!(left, right);
}

#[test]
fn real_bundle_repack_and_relocation_preserve_the_logical_address() {
    let sample =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/samples/char_118_yuki.ab");
    assert!(
        sample.exists(),
        "missing identity decision fixture: {sample:?}"
    );

    let temp = tempfile::tempdir().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir_all(&input_dir).unwrap();
    let input = input_dir.join("game.ab");
    fs::copy(&sample, &input).unwrap();
    let input = fs::canonicalize(input).unwrap();

    let mut environment = Environment::new();
    environment.load_file(&input).unwrap();
    let bundle_source = BinarySource::path(&input);

    let (key, old_name) = environment
        .binary_object_infos()
        .filter(|object| {
            object.source == &bundle_source && object.source_kind == BinarySourceKind::AssetBundle
        })
        .find_map(|object| {
            object
                .object
                .peek_name()
                .ok()
                .flatten()
                .filter(|name| !name.is_empty())
                .map(|name| (object.key(), name))
        })
        .expect("fixture must contain a named serialized object");
    let asset_index = key
        .asset_index
        .expect("bundle object must have an asset index");
    let (member_name, old_member_bytes) = {
        let bundle = environment.bundles().get(&bundle_source).unwrap();
        let member_name = bundle.asset_names[asset_index].clone();
        let node = bundle
            .nodes
            .iter()
            .find(|node| node.name == member_name)
            .expect("asset name must identify its wire node");
        (member_name, bundle.extract_node_data(node).unwrap())
    };
    let old_bundle_bytes = fs::read(&input).unwrap();

    let workspace = WorkspaceId::from_u128(0xdecaf).unwrap();
    let alias = SourceAlias::new("fixtures/game.ab").unwrap();
    let member = BundleMemberId::new(member_name.clone()).unwrap();
    let mut before = SourceCatalog::new(workspace);
    let old_bundle_source = before
        .register(
            SourceDescriptor::root(
                SourceKind::AssetBundle,
                alias.clone(),
                PhysicalOrigin::from_existing_path(input.clone()).unwrap(),
            ),
            fingerprint(SourceKind::AssetBundle, &old_bundle_bytes),
        )
        .unwrap();
    let old_serialized_source = before
        .register(
            SourceDescriptor::bundle_member(
                old_bundle_source,
                SourceKind::SerializedFile,
                member.clone(),
            )
            .unwrap(),
            fingerprint(SourceKind::SerializedFile, &old_member_bytes),
        )
        .unwrap();
    let address = ObjectAddress::binary_bundle_member(
        SourceLocator::path(alias.as_str()).unwrap(),
        member.clone(),
        key.path_id,
    )
    .unwrap();
    let old_handle = before.resolve_object_address(&address).unwrap();
    assert_eq!(old_handle.object().source(), old_serialized_source);
    let old_revision = before.revision().unwrap();

    let new_name = format!("IDENTITY_REPACK_{old_name}");
    environment
        .edit_binary_object_key(&key, |class| {
            if let Some(value) = class.get_mut("m_Name") {
                *value = UnityValue::String(new_name.clone());
            } else if let Some(value) = class.get_mut("name") {
                *value = UnityValue::String(new_name.clone());
            } else {
                return Err(UnityAssetError::format(
                    "named fixture object has no m_Name/name field",
                ));
            }
            Ok(())
        })
        .unwrap();
    let output_dir = temp.path().join("relocated");
    environment
        .save(unity_asset_write::PackerOptions::default(), &output_dir)
        .unwrap();
    let output = fs::canonicalize(output_dir.join(file_name(&input))).unwrap();

    let mut reopened = Environment::new();
    reopened.load_file(&output).unwrap();
    let output_source = BinarySource::path(&output);
    let (new_member_bytes, new_asset_index) = {
        let bundle = reopened.bundles().get(&output_source).unwrap();
        let new_asset_index = bundle
            .asset_names
            .iter()
            .position(|name| name == &member_name)
            .expect("repack must preserve the logical member name");
        assert!(
            bundle.assets[new_asset_index]
                .find_object(key.path_id)
                .is_some()
        );
        let node = bundle
            .nodes
            .iter()
            .find(|node| node.name == member_name)
            .unwrap();
        (bundle.extract_node_data(node).unwrap(), new_asset_index)
    };
    let observed_name = reopened
        .binary_object_infos()
        .find(|object| {
            object.source == &output_source
                && object.source_kind == BinarySourceKind::AssetBundle
                && object.asset_index == Some(new_asset_index)
                && object.object.path_id() == key.path_id
        })
        .expect("repack must preserve the local pathID")
        .object
        .peek_name()
        .unwrap()
        .unwrap();
    assert_eq!(observed_name, new_name);

    let mut after = SourceCatalog::new(workspace);
    let new_bundle_source = after
        .register(
            SourceDescriptor::root(
                SourceKind::AssetBundle,
                alias,
                PhysicalOrigin::from_existing_path(output.clone()).unwrap(),
            ),
            fingerprint(SourceKind::AssetBundle, &fs::read(output).unwrap()),
        )
        .unwrap();
    let new_serialized_source = after
        .register(
            SourceDescriptor::bundle_member(new_bundle_source, SourceKind::SerializedFile, member)
                .unwrap(),
            fingerprint(SourceKind::SerializedFile, &new_member_bytes),
        )
        .unwrap();
    let new_revision = after.revision().unwrap();
    let new_handle = after.resolve_object_address(&address).unwrap();

    assert_eq!(new_serialized_source, old_serialized_source);
    assert_eq!(new_handle.object().source(), new_serialized_source);
    assert_eq!(after.address_for_handle(&new_handle).unwrap(), address);
    assert!(matches!(
        after.address_for_handle(&old_handle),
        Err(CatalogError::InvalidHandleContext(_))
    ));
    assert_ne!(new_revision, old_revision);
    assert!(matches!(
        IdentityRemap::new(address.clone(), address.clone()),
        Err(ChangeSetError::IdentityDidNotChange)
    ));
    assert!(
        old_handle
            .validate_context(workspace, new_revision)
            .is_err()
    );
}

fn file_name(path: &Path) -> &std::ffi::OsStr {
    path.file_name()
        .expect("fixture path must have a file name")
}
