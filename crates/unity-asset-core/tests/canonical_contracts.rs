use unity_asset_core::{
    CHANGE_SET_VERSION, ChangeSet, ChangeSetError, DIAGNOSTIC_VERSION, Diagnostic,
    DiagnosticSeverity, DigestV1, FieldPath, IdentityRemap, ObjectAddress, ObjectId, SourceId,
    SourceKind, SourceLocator, TransactionId, WorkspaceId, WorkspaceRevision, YamlFileId,
};

fn source(workspace: WorkspaceId, kind: SourceKind, local: u64) -> SourceId {
    SourceId::new(workspace, kind, u128::from(local)).unwrap()
}

fn transaction(bytes: &[u8]) -> TransactionId {
    TransactionId::new(DigestV1::hash_bytes(bytes))
}

fn yaml_file_id(value: i64) -> YamlFileId {
    YamlFileId::new(value).unwrap()
}

#[test]
fn change_sets_have_versioned_canonical_bytes_independent_of_insertion_order() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let first_source = source(workspace, SourceKind::SerializedFile, 1);
    let second_source = source(workspace, SourceKind::Yaml, 2);
    let first_object = ObjectId::binary(first_source, -9).unwrap();
    let second_object = ObjectId::yaml(second_source, yaml_file_id(17)).unwrap();
    let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
    let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));
    let first_address =
        ObjectAddress::binary_direct(SourceLocator::path("a.assets").unwrap(), -9).unwrap();
    let second_address =
        ObjectAddress::yaml(SourceLocator::path("b.prefab").unwrap(), yaml_file_id(17)).unwrap();
    let remap = IdentityRemap::new(first_address, second_address).unwrap();

    let left = ChangeSet::new(
        transaction(b"transaction"),
        workspace,
        from,
        to,
        vec![second_source, first_source, second_source],
        vec![second_object.clone(), first_object.clone(), second_object],
        vec![remap.clone(), remap.clone()],
    )
    .unwrap();
    let right = ChangeSet::new(
        transaction(b"transaction"),
        workspace,
        from,
        to,
        vec![first_source, second_source],
        vec![
            first_object,
            ObjectId::yaml(second_source, yaml_file_id(17)).unwrap(),
        ],
        vec![remap],
    )
    .unwrap();

    let left_bytes = serde_json::to_vec(&left).unwrap();
    assert_eq!(left_bytes, serde_json::to_vec(&right).unwrap());
    assert_eq!(
        DigestV1::hash_bytes(&left_bytes).to_string(),
        "blake3-v1:8c5ee9d8062416cc51cd11e759ca0eb9a060ed425523d3e45740d9a59d01b53f"
    );
    assert!(
        std::str::from_utf8(&left_bytes)
            .unwrap()
            .contains("\"version\":2")
    );
    assert_eq!(
        serde_json::from_slice::<ChangeSet>(&left_bytes).unwrap(),
        left
    );
}

#[test]
fn change_sets_reject_cross_workspace_and_incomplete_membership() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let other = WorkspaceId::from_u128(2).unwrap();
    let changed_source = source(workspace, SourceKind::SerializedFile, 1);
    let foreign_source = source(other, SourceKind::SerializedFile, 1);
    let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
    let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));

    assert!(matches!(
        ChangeSet::new(
            transaction(b"foreign"),
            workspace,
            from,
            to,
            vec![foreign_source],
            Vec::new(),
            Vec::new(),
        ),
        Err(ChangeSetError::SourceWorkspaceMismatch { .. })
    ));

    let missing_source_object =
        ObjectId::binary(source(workspace, SourceKind::SerializedFile, 2), 7).unwrap();
    assert!(matches!(
        ChangeSet::new(
            transaction(b"missing"),
            workspace,
            from,
            to,
            vec![changed_source],
            vec![missing_source_object],
            Vec::new(),
        ),
        Err(ChangeSetError::ObjectSourceNotChanged { .. })
    ));
}

#[test]
fn change_sets_reject_same_revision_and_conflicting_remaps() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
    let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));
    assert!(matches!(
        ChangeSet::new(
            transaction(b"same"),
            workspace,
            from,
            from,
            Vec::new(),
            Vec::new(),
            Vec::new()
        ),
        Err(ChangeSetError::RevisionDidNotAdvance)
    ));

    let address =
        ObjectAddress::binary_direct(SourceLocator::path("a.assets").unwrap(), 1).unwrap();
    let first_target =
        ObjectAddress::binary_direct(SourceLocator::path("b.assets").unwrap(), 1).unwrap();
    let second_target =
        ObjectAddress::binary_direct(SourceLocator::path("c.assets").unwrap(), 1).unwrap();
    let first = IdentityRemap::new(address.clone(), first_target).unwrap();
    let second = IdentityRemap::new(address, second_target).unwrap();

    assert!(matches!(
        ChangeSet::new(
            transaction(b"conflict"),
            workspace,
            from,
            to,
            Vec::new(),
            Vec::new(),
            vec![first, second]
        ),
        Err(ChangeSetError::ConflictingIdentityRemap { .. })
    ));
}

#[test]
fn change_sets_are_transaction_keyed_and_reject_empty_transitions() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
    let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));
    let transaction = transaction(b"idempotency-key");

    assert!(matches!(
        ChangeSet::new(
            transaction,
            workspace,
            from,
            to,
            Vec::new(),
            Vec::new(),
            Vec::new()
        ),
        Err(ChangeSetError::NoChanges)
    ));

    let source = source(workspace, SourceKind::SerializedFile, 1);
    let change_set = ChangeSet::new(
        transaction,
        workspace,
        from,
        to,
        vec![source],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(change_set.transaction(), transaction);
    assert!(
        serde_json::to_string(&change_set)
            .unwrap()
            .contains("transaction")
    );
}

#[test]
fn change_set_deserialization_revalidates_version_and_invariants() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"same"));
    let invalid = format!(
        r#"{{"version":2,"transaction":"{}","workspace":"{workspace}","from_revision":"{revision}","to_revision":"{revision}","changed_sources":[],"changed_objects":[],"identity_remaps":[]}}"#,
        transaction(b"deserialize")
    );
    assert!(serde_json::from_str::<ChangeSet>(&invalid).is_err());

    let valid = ChangeSet::new(
        transaction(b"versioned"),
        workspace,
        WorkspaceRevision::new(DigestV1::hash_bytes(b"from")),
        WorkspaceRevision::new(DigestV1::hash_bytes(b"to")),
        vec![source(workspace, SourceKind::Yaml, 1)],
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    let current = serde_json::to_value(valid).unwrap();
    for unsupported_version in [CHANGE_SET_VERSION - 1, CHANGE_SET_VERSION + 1] {
        let mut unsupported = current.clone();
        unsupported["version"] = serde_json::json!(unsupported_version);
        let error = serde_json::from_value::<ChangeSet>(unsupported).unwrap_err();
        assert!(error.to_string().contains(&format!(
            "change set version {unsupported_version} is unsupported"
        )));
    }
}

#[test]
fn diagnostics_sort_by_structured_fields_and_expose_typed_context() {
    let field = FieldPath::root()
        .push_field("m_Component")
        .unwrap()
        .push_index(2)
        .unwrap();
    let address = ObjectAddress::yaml(
        SourceLocator::path("scene.unity").unwrap(),
        yaml_file_id(100001),
    )
    .unwrap();
    let error = Diagnostic::new(
        DiagnosticSeverity::Error,
        "INVALID_REFERENCE",
        "target is missing",
    )
    .unwrap()
    .at_address(address.clone())
    .at_field(field.clone());
    let warning = Diagnostic::new(
        DiagnosticSeverity::Warning,
        "TRUNCATED_SCAN",
        "scan reached its object budget",
    )
    .unwrap();

    assert_eq!(error.address(), Some(&address));
    assert_eq!(error.field_path(), Some(&field));

    let mut left = vec![warning.clone(), error.clone()];
    let mut right = vec![error, warning];
    left.sort();
    right.sort();

    assert_eq!(
        serde_json::to_vec(&left).unwrap(),
        serde_json::to_vec(&right).unwrap()
    );
    assert!(
        serde_json::to_string(&left[0])
            .unwrap()
            .contains(&format!("\"version\":{}", DIAGNOSTIC_VERSION))
    );
    let encoded = serde_json::to_value(&left[0]).unwrap();
    for unsupported_version in [DIAGNOSTIC_VERSION - 1, DIAGNOSTIC_VERSION + 1] {
        let mut unsupported = encoded.clone();
        unsupported["version"] = serde_json::json!(unsupported_version);
        let error = serde_json::from_value::<Diagnostic>(unsupported).unwrap_err();
        assert!(error.to_string().contains(&format!(
            "diagnostic version {unsupported_version} is unsupported"
        )));
    }
}

#[test]
fn field_paths_reject_unbounded_segment_sequences() {
    let segment = r#"{"kind":"index","index":0}"#;
    let json = format!("[{}]", vec![segment; 513].join(","));
    assert!(serde_json::from_str::<FieldPath>(&json).is_err());
}
