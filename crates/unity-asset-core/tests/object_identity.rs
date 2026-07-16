use std::str::FromStr;

use unity_asset_core::{
    BundleMemberId, ContainmentKind, ContractError, DigestV1, ObjectAddress, ObjectId, ObjectKind,
    RevisionedObjectHandle, SourceId, SourceKind, SourceLocator, WorkspaceId, WorkspaceRevision,
    YamlDocumentSelector,
};

fn source(workspace: WorkspaceId, kind: SourceKind, local: u64) -> SourceId {
    SourceId::new(workspace, kind, u128::from(local)).unwrap()
}

#[test]
fn binary_object_ids_include_kind_and_owning_source() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let left = ObjectId::binary(source(workspace, SourceKind::SerializedFile, 1), -42).unwrap();
    let right = ObjectId::binary(source(workspace, SourceKind::SerializedFile, 2), -42).unwrap();

    assert_ne!(left, right);
    assert_eq!(left.kind(), ObjectKind::Binary);
    assert_eq!(left.binary_path_id(), Some(-42));
    assert_eq!(right.binary_path_id(), Some(-42));
}

#[test]
fn workspace_id_rejects_oversized_text_before_error_payload_allocation() {
    assert!(matches!(
        WorkspaceId::from_str(&"x".repeat(1_000_000)),
        Err(ContractError::InvalidWorkspaceIdLength { .. })
    ));
}

#[test]
fn object_ids_reject_null_binary_ids_and_wrong_source_kinds() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let serialized = source(workspace, SourceKind::SerializedFile, 1);
    let yaml = source(workspace, SourceKind::Yaml, 2);

    assert!(matches!(
        ObjectId::binary(serialized, 0),
        Err(ContractError::NullBinaryObjectId)
    ));
    assert!(matches!(
        ObjectId::binary(yaml, 1),
        Err(ContractError::ObjectSourceKindMismatch { .. })
    ));
    assert!(matches!(
        ObjectId::yaml(serialized, "100001"),
        Err(ContractError::ObjectSourceKindMismatch { .. })
    ));
}

#[test]
fn yaml_object_ids_preserve_string_anchors_and_explicit_ordinals() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let yaml = source(workspace, SourceKind::Yaml, 1);
    let anchored = ObjectId::yaml(yaml, "doc_0").unwrap();
    let ordinal = ObjectId::yaml_document(yaml, 3).unwrap();

    assert_eq!(anchored.kind(), ObjectKind::Yaml);
    assert_eq!(anchored.yaml_anchor(), Some("doc_0"));
    assert_eq!(anchored.yaml_document_ordinal(), None);
    assert_eq!(ordinal.yaml_anchor(), None);
    assert_eq!(ordinal.yaml_document_ordinal(), Some(3));

    let real_doc_anchor = ObjectId::yaml(yaml, "doc_0").unwrap();
    let unanchored_document = ObjectId::yaml_document(yaml, 0).unwrap();
    assert_ne!(real_doc_anchor, unanchored_document);
    assert_eq!(ObjectId::yaml(yaml, "0").unwrap().yaml_anchor(), Some("0"));

    for invalid in ["", "has space", "bad,anchor", "bad\0anchor"] {
        assert!(
            ObjectId::yaml(yaml, invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn object_id_deserialization_revalidates_source_kind() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let yaml = source(workspace, SourceKind::Yaml, 1);
    let invalid = format!(
        r#"{{"version":1,"kind":"binary","source":{},"path_id":7}}"#,
        serde_json::to_string(&yaml).unwrap()
    );

    assert!(serde_json::from_str::<ObjectId>(&invalid).is_err());
}

#[test]
fn source_locators_preserve_nested_container_ownership() {
    let archive = SourceLocator::archive_member("build/game.apk", "assets/data.web").unwrap();
    let nested = archive
        .child(
            ContainmentKind::WebFile,
            BundleMemberId::new("data.unity3d").unwrap(),
        )
        .unwrap();

    assert_eq!(nested.root_alias().as_str(), "build/game.apk");
    assert_eq!(nested.members().len(), 2);
    assert_eq!(nested.members()[0].container(), ContainmentKind::Archive);
    assert_eq!(nested.members()[0].name(), "assets/data.web");
    assert_eq!(nested.members()[1].container(), ContainmentKind::WebFile);
    assert_eq!(nested.members()[1].name(), "data.unity3d");

    let json = serde_json::to_string(&nested).unwrap();
    assert_eq!(
        serde_json::from_str::<SourceLocator>(&json).unwrap(),
        nested
    );
}

#[test]
fn object_addresses_make_source_ownership_explicit() {
    let origin = SourceLocator::path("Library/mainData").unwrap();
    let direct = ObjectAddress::binary_direct(origin.clone(), -7).unwrap();
    let member = ObjectAddress::binary_bundle_member(
        origin,
        BundleMemberId::with_occurrence("sharedassets0.assets", 3).unwrap(),
        -7,
    )
    .unwrap();

    assert_ne!(direct, member);
    assert_eq!(direct.binary_path_id(), Some(-7));
    assert_eq!(member.binary_path_id(), Some(-7));
    assert_eq!(
        member.bundle_member().unwrap().name(),
        "sharedassets0.assets"
    );
    assert_eq!(member.bundle_member().unwrap().same_name_occurrence(), 3);
}

#[test]
fn every_object_address_variant_round_trips_canonically() {
    let archive =
        SourceLocator::archive_member("build/game.apk", "assets/bin/Data/data.unity3d").unwrap();
    let webfile = SourceLocator::webfile_member("build/game.data", "game.bundle").unwrap();
    let addresses = vec![
        ObjectAddress::binary_direct(SourceLocator::path("mainData").unwrap(), 1).unwrap(),
        ObjectAddress::binary_direct(archive.clone(), i64::MIN + 1).unwrap(),
        ObjectAddress::binary_direct(webfile, 42).unwrap(),
        ObjectAddress::binary_bundle_member(
            archive.clone(),
            BundleMemberId::with_occurrence("CAB-main", 1).unwrap(),
            42,
        )
        .unwrap(),
        ObjectAddress::yaml(archive.clone(), "100001").unwrap(),
        ObjectAddress::yaml_document(archive, 4).unwrap(),
    ];

    for address in addresses {
        let json = serde_json::to_string(&address).unwrap();
        let from_json: ObjectAddress = serde_json::from_str(&json).unwrap();
        assert_eq!(from_json, address);
        assert_eq!(serde_json::to_string(&from_json).unwrap(), json);

        let compact = address.to_compact_string().unwrap();
        assert_eq!(ObjectAddress::from_str(&compact).unwrap(), address);
        assert_eq!(
            ObjectAddress::from_str(&compact)
                .unwrap()
                .to_compact_string()
                .unwrap(),
            compact
        );
    }
}

#[test]
fn object_address_deserialization_rejects_missing_bundle_identity() {
    let invalid = r#"{
        "version":1,
        "kind":"binary_bundle_member",
        "source":{"version":1,"outer_path":"mainData","members":[]},
        "path_id":12
    }"#;

    assert!(serde_json::from_str::<ObjectAddress>(invalid).is_err());
}

#[test]
fn object_address_wire_rejects_illegal_variants_versions_and_path_id_coercions() {
    let cases = [
        (
            "direct address with bundle membership",
            r#"{
                "version":1,
                "kind":"binary_direct",
                "source":{
                    "version":1,
                    "outer_path":"mainData",
                    "members":[{
                        "container":"bundle",
                        "member":{"name":"CAB-main","same_name_occurrence":0}
                    }]
                },
                "path_id":12
            }"#,
        ),
        (
            "unknown kind tag",
            r#"{
                "version":1,
                "kind":"binary_future",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":12
            }"#,
        ),
        (
            "unknown contract version",
            r#"{
                "version":2,
                "kind":"binary_direct",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":12
            }"#,
        ),
        (
            "null path id",
            r#"{
                "version":1,
                "kind":"binary_direct",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":0
            }"#,
        ),
        (
            "unsigned path id overflow",
            r#"{
                "version":1,
                "kind":"binary_direct",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":18446744073709551615
            }"#,
        ),
        (
            "string path id coercion",
            r#"{
                "version":1,
                "kind":"binary_direct",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":"12"
            }"#,
        ),
        (
            "floating path id coercion",
            r#"{
                "version":1,
                "kind":"binary_direct",
                "source":{"version":1,"outer_path":"mainData","members":[]},
                "path_id":12.0
            }"#,
        ),
    ];

    for (case, wire) in cases {
        assert!(
            serde_json::from_str::<ObjectAddress>(wire).is_err(),
            "accepted {case}"
        );
    }
}

#[test]
fn compact_addresses_reject_unbounded_input_before_decoding() {
    let oversized = format!("oa1:{}", "00".repeat(512 * 1024));
    assert!(matches!(
        ObjectAddress::from_str(&oversized),
        Err(ContractError::CompactAddressTooLong { .. })
    ));
    assert!(ObjectAddress::from_str("oa1:0").is_err());
    assert!(ObjectAddress::from_str("oa1:zz").is_err());
    assert!(ObjectAddress::from_str("oa1:ff").is_err());
    assert!(ObjectAddress::from_str("bok3:legacy").is_err());
}

#[test]
fn source_locators_are_structured_and_inspectable() {
    let origin = SourceLocator::webfile_member("build.data", "game.bundle").unwrap();
    let member = BundleMemberId::new("CAB-main").unwrap();
    let source = origin
        .clone()
        .child(ContainmentKind::Bundle, member.clone())
        .unwrap();

    assert_eq!(source.root_alias(), origin.root_alias());
    assert_eq!(source.bundle_member(), Some(&member));
    assert_eq!(origin.bundle_member(), None);
}

#[test]
fn source_locator_size_limit_guarantees_compact_address_round_trip() {
    let mut locator = SourceLocator::path("a".repeat(64 * 1024)).unwrap();
    for index in 0..16 {
        let member = format!("{index}-{}", "x".repeat(16 * 1024 - 3));
        let next = locator.clone().child(
            ContainmentKind::Archive,
            BundleMemberId::new(member).unwrap(),
        );
        if let Ok(next) = next {
            locator = next;
        } else {
            let address = ObjectAddress::binary_direct(locator, 1).unwrap();
            assert!(address.to_compact_string().is_ok());
            return;
        }
    }
    panic!("locator accepted an unbounded cumulative member payload");
}

#[test]
fn source_locator_wire_rejects_members_beyond_the_static_depth_limit() {
    let step = r#"{"container":"archive","member":{"name":"entry","same_name_occurrence":0}}"#;
    let json = format!(
        r#"{{"version":1,"outer_path":"root.zip","members":[{}]}}"#,
        vec![step; 65].join(",")
    );
    assert!(serde_json::from_str::<SourceLocator>(&json).is_err());
}

#[test]
fn revisioned_handles_reject_foreign_contexts() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let object = ObjectId::binary(source(workspace, SourceKind::SerializedFile, 1), 99).unwrap();
    let other_workspace = WorkspaceId::from_u128(2).unwrap();
    let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"revision-one"));
    let other_revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"revision-two"));
    let handle = RevisionedObjectHandle::new(workspace, revision, object.clone()).unwrap();

    assert!(handle.validate_context(workspace, revision).is_ok());
    assert_eq!(handle.object(), &object);
    assert!(matches!(
        handle.validate_context(other_workspace, revision),
        Err(ContractError::WorkspaceMismatch { .. })
    ));
    assert!(matches!(
        handle.validate_context(workspace, other_revision),
        Err(ContractError::RevisionMismatch { .. })
    ));
}

#[test]
fn revisioned_handle_cannot_bind_an_object_from_another_workspace() {
    let first = WorkspaceId::from_u128(1).unwrap();
    let second = WorkspaceId::from_u128(2).unwrap();
    let object = ObjectId::binary(source(first, SourceKind::SerializedFile, 1), 8).unwrap();
    let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"revision"));

    assert!(matches!(
        RevisionedObjectHandle::new(second, revision, object.clone()),
        Err(ContractError::ObjectWorkspaceMismatch { .. })
    ));
}

#[test]
fn yaml_selectors_have_stable_structured_serialization() {
    let anchor = YamlDocumentSelector::anchor("1158508787625206").unwrap();
    let ordinal = YamlDocumentSelector::ordinal(0);

    assert_eq!(anchor.anchor_str(), Some("1158508787625206"));
    assert_eq!(ordinal.ordinal_index(), Some(0));
    assert_eq!(
        serde_json::from_str::<YamlDocumentSelector>(&serde_json::to_string(&anchor).unwrap())
            .unwrap(),
        anchor
    );
}

#[test]
fn yaml_document_hints_are_not_part_of_persisted_identity() {
    let locator = SourceLocator::path("scene.unity").unwrap();
    let canonical = ObjectAddress::yaml(locator, "100001").unwrap();
    let json = serde_json::to_string(&canonical).unwrap();

    assert!(!json.contains("document_hint"));
    assert!(
        serde_json::from_str::<ObjectAddress>(&json.replace(
            r#""anchor":"100001""#,
            r#""anchor":"100001","document_hint":7"#
        ))
        .is_err()
    );
}
