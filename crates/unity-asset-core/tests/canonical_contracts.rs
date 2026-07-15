use unity_asset_core::{
    ChangeSet, ChangeSetError, Diagnostic, DiagnosticSeverity, DigestV1, FieldPath, IdentityRemap,
    ObjectAddress, ObjectId, SourceId, SourceKind, SourceLocator, TransactionId, WorkspaceId,
    WorkspaceRevision,
};

fn source(workspace: WorkspaceId, kind: SourceKind, local: u64) -> SourceId {
    SourceId::new(workspace, kind, u128::from(local)).unwrap()
}

fn transaction(bytes: &[u8]) -> TransactionId {
    TransactionId::new(DigestV1::hash_bytes(bytes))
}

#[test]
fn change_sets_have_versioned_canonical_bytes_independent_of_insertion_order() {
    let workspace = WorkspaceId::from_u128(1).unwrap();
    let first_source = source(workspace, SourceKind::SerializedFile, 1);
    let second_source = source(workspace, SourceKind::Yaml, 2);
    let first_object = ObjectId::binary(first_source, -9).unwrap();
    let second_object = ObjectId::yaml(second_source, "17").unwrap();
    let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
    let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));
    let first_address =
        ObjectAddress::binary_direct(SourceLocator::path("a.assets").unwrap(), -9).unwrap();
    let second_address =
        ObjectAddress::yaml(SourceLocator::path("b.prefab").unwrap(), "17").unwrap();
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
        vec![first_object, ObjectId::yaml(second_source, "17").unwrap()],
        vec![remap],
    )
    .unwrap();

    let left_bytes = serde_json::to_vec(&left).unwrap();
    assert_eq!(left_bytes, serde_json::to_vec(&right).unwrap());
    assert_eq!(
        DigestV1::hash_bytes(&left_bytes).to_string(),
        "blake3-v1:9e59c57899fd51c5ac055256beca5738de29b555d718c00ecf49fd9046b45217"
    );
    assert!(
        std::str::from_utf8(&left_bytes)
            .unwrap()
            .contains("\"version\":1")
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
        r#"{{"version":1,"transaction":"{}","workspace":"{workspace}","from_revision":"{revision}","to_revision":"{revision}","changed_sources":[],"changed_objects":[],"identity_remaps":[]}}"#,
        transaction(b"deserialize")
    );
    assert!(serde_json::from_str::<ChangeSet>(&invalid).is_err());

    let unknown_version = invalid.replace("\"version\":1", "\"version\":2");
    assert!(serde_json::from_str::<ChangeSet>(&unknown_version).is_err());
}

#[test]
fn diagnostics_sort_by_structured_fields_and_expose_typed_context() {
    let field = FieldPath::root()
        .push_field("m_Component")
        .unwrap()
        .push_index(2)
        .unwrap();
    let address =
        ObjectAddress::yaml(SourceLocator::path("scene.unity").unwrap(), "100001").unwrap();
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
            .contains("\"version\":1")
    );
    let unsupported = serde_json::to_string(&left[0])
        .unwrap()
        .replace("\"version\":1", "\"version\":2");
    assert!(serde_json::from_str::<Diagnostic>(&unsupported).is_err());
}

#[test]
fn field_paths_reject_unbounded_segment_sequences() {
    let segment = r#"{"kind":"index","index":0}"#;
    let json = format!("[{}]", vec![segment; 513].join(","));
    assert!(serde_json::from_str::<FieldPath>(&json).is_err());
}
