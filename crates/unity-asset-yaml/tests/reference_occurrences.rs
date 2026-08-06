use indexmap::IndexMap;
use std::sync::Arc;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, BudgetError, FieldPath, UnityClass, UnityDocument,
    UnityValue, YamlDocumentSelector, YamlFileId,
};
use unity_asset_yaml::{
    YamlDocument, YamlReferenceClassification, YamlReferenceDiagnostic, YamlReferenceField,
    YamlReferenceOccurrence, YamlReferenceScanError, YamlReferenceShape, YamlValueKind,
    classify_reference_value, parse_budgeted_yaml_source, scan_reference_class_occurrences,
    scan_reference_occurrences,
};

fn yaml_file_id(value: i64) -> YamlFileId {
    YamlFileId::new(value).unwrap()
}

fn parse_document(input: &str) -> Arc<YamlDocument> {
    let mut budget = AssetLoadBudget::default();
    let source = parse_budgeted_yaml_source(Arc::from(input.as_bytes()), &mut budget).unwrap();
    Arc::clone(source.document())
}

fn with_property(class: UnityClass, key: impl Into<String>, value: UnityValue) -> UnityClass {
    let (header, mut properties) = class.into_parts();
    properties.insert(key.into(), value);
    UnityClass::from_parts(header, properties)
}

fn path(fields: &[PathPart<'_>]) -> FieldPath {
    fields
        .iter()
        .fold(FieldPath::root(), |path, field| match field {
            PathPart::Field(field) => path.push_field(*field).unwrap(),
            PathPart::Index(index) => path.push_index(*index).unwrap(),
        })
}

#[derive(Clone, Copy)]
enum PathPart<'a> {
    Field(&'a str),
    Index(u32),
}

fn occurrence_at<'a>(
    occurrences: &'a [YamlReferenceOccurrence],
    field_path: &FieldPath,
) -> &'a YamlReferenceOccurrence {
    occurrences
        .iter()
        .find(|occurrence| &occurrence.field_path == field_path)
        .unwrap_or_else(|| panic!("missing occurrence at {field_path}"))
}

#[test]
fn scans_structural_yaml_with_stable_paths_signed_ids_and_no_text_false_positives() {
    let document = parse_document(
        r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &-42
MonoBehaviour:
  # Comments and scalar styles must not affect structural discovery.
  m_Null: { fileID: 0 }
  m_Script: {
    fileID: 11500000,
    guid: ABCDEF0123456789abcdef0123456789,
    type: 3
  }
  m_Refs:
    - { fileID: -17 }
    - fileID: 23
  m_Text: |
    This is not a reference: {fileID: 999}
"#,
    );

    let scan = scan_reference_occurrences(&document, &mut AssetLoadBudget::default()).unwrap();

    assert!(scan.complete);
    assert_eq!(scan.occurrences.len(), 4);
    assert_eq!(scan.stats.null_occurrences, 1);
    assert_eq!(scan.stats.valid_occurrences, 3);
    assert_eq!(scan.stats.invalid_occurrences, 0);
    assert_eq!(scan.stats.occurrences_emitted, 4);

    for occurrence in &scan.occurrences {
        assert_eq!(
            occurrence.object,
            YamlDocumentSelector::file_id(yaml_file_id(-42))
        );
    }

    let null = occurrence_at(&scan.occurrences, &path(&[PathPart::Field("m_Null")]));
    assert!(matches!(
        &null.shape,
        YamlReferenceShape::Null(target) if target.file_id == 0
    ));

    let external = occurrence_at(&scan.occurrences, &path(&[PathPart::Field("m_Script")]));
    assert!(matches!(
        &external.shape,
        YamlReferenceShape::Valid(target)
            if target.file_id == 11_500_000
                && target.guid.as_deref() == Some("ABCDEF0123456789abcdef0123456789")
                && target.type_id == Some(3)
    ));

    let signed = occurrence_at(
        &scan.occurrences,
        &path(&[PathPart::Field("m_Refs"), PathPart::Index(0)]),
    );
    assert!(matches!(
        &signed.shape,
        YamlReferenceShape::Valid(target) if target.file_id == -17
    ));
    assert_eq!(
        scan.occurrences[3].field_path,
        path(&[PathPart::Field("m_Refs"), PathPart::Index(1)])
    );
}

#[test]
fn emits_structured_invalid_shapes_and_preserves_decodable_raw_fields() {
    let document = parse_document(
        r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &9
MonoBehaviour:
  m_NullScalar: {fileID: null}
  m_BadGuidLength: {fileID: 1, guid: abc, type: 3}
  m_BadGuidHex: {fileID: 2, guid: zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz, type: 3}
  m_MissingType: {fileID: 3, guid: 0123456789abcdef0123456789abcdef}
  m_MissingGuid: {fileID: 4, type: 3}
  m_BadType: {fileID: 5, guid: 0123456789abcdef0123456789abcdef, type: text}
  m_FileAliases: {fileID: 6, m_FileID: -6}
  m_GuidAliases: {fileID: 7, guid: 0123456789abcdef0123456789abcdef, m_GUID: 0123456789abcdef0123456789abcdef, type: 3}
  m_TypeAliases: {fileID: 8, guid: 0123456789abcdef0123456789abcdef, type: 3, m_Type: 3}
  m_Extra:
    fileID: 10
    guid: 0123456789abcdef0123456789abcdef
    type: 2
    nested: {fileID: -11}
  m_NotCandidate: {guid: 0123456789abcdef0123456789abcdef, type: 3}
"#,
    );

    let scan = scan_reference_occurrences(&document, &mut AssetLoadBudget::default()).unwrap();

    assert_eq!(scan.occurrences.len(), 11);
    assert_eq!(scan.stats.invalid_occurrences, 10);
    assert_eq!(scan.stats.valid_occurrences, 1);
    assert_eq!(scan.stats.diagnostics_emitted, 10);

    let invalid = |field: &str| {
        let occurrence = occurrence_at(&scan.occurrences, &path(&[PathPart::Field(field)]));
        let YamlReferenceShape::Invalid { raw, diagnostic } = &occurrence.shape else {
            panic!("expected invalid occurrence at {field}");
        };
        (raw, diagnostic)
    };

    assert!(matches!(
        invalid("m_NullScalar").1,
        YamlReferenceDiagnostic::InvalidValueType {
            field: YamlReferenceField::FileId,
            actual: YamlValueKind::Null,
        }
    ));
    assert!(matches!(
        invalid("m_BadGuidLength").1,
        YamlReferenceDiagnostic::InvalidGuidLength { actual: 3 }
    ));
    assert!(matches!(
        invalid("m_BadGuidHex").1,
        YamlReferenceDiagnostic::InvalidGuidHex
    ));
    assert!(matches!(
        invalid("m_MissingType").1,
        YamlReferenceDiagnostic::IncompleteExternalReference {
            missing: YamlReferenceField::Type,
        }
    ));
    assert!(matches!(
        invalid("m_MissingGuid").1,
        YamlReferenceDiagnostic::IncompleteExternalReference {
            missing: YamlReferenceField::Guid,
        }
    ));
    assert!(matches!(
        invalid("m_BadType").1,
        YamlReferenceDiagnostic::InvalidValueType {
            field: YamlReferenceField::Type,
            actual: YamlValueKind::String,
        }
    ));
    assert!(matches!(
        invalid("m_FileAliases").1,
        YamlReferenceDiagnostic::ConflictingAliases {
            field: YamlReferenceField::FileId,
        }
    ));
    assert_eq!(invalid("m_FileAliases").0.file_id, None);
    assert!(matches!(
        invalid("m_GuidAliases").1,
        YamlReferenceDiagnostic::ConflictingAliases {
            field: YamlReferenceField::Guid,
        }
    ));
    assert_eq!(invalid("m_GuidAliases").0.guid, None);
    assert!(matches!(
        invalid("m_TypeAliases").1,
        YamlReferenceDiagnostic::ConflictingAliases {
            field: YamlReferenceField::Type,
        }
    ));
    assert_eq!(invalid("m_TypeAliases").0.type_id, None);

    let (extra_raw, extra_diagnostic) = invalid("m_Extra");
    assert_eq!(extra_raw.file_id, Some(10));
    assert_eq!(
        extra_raw.guid.as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(extra_raw.type_id, Some(2));
    assert!(matches!(
        extra_diagnostic,
        YamlReferenceDiagnostic::UnexpectedField { field } if field == "nested"
    ));

    let nested = occurrence_at(
        &scan.occurrences,
        &path(&[PathPart::Field("m_Extra"), PathPart::Field("nested")]),
    );
    assert!(matches!(
        &nested.shape,
        YamlReferenceShape::Valid(target) if target.file_id == -11
    ));
}

#[test]
fn accepts_unity_alias_spellings_and_preserves_unanchored_document_identity() {
    let mut pointer = IndexMap::new();
    pointer.insert("m_FileID".to_string(), UnityValue::Integer(-7));
    pointer.insert(
        "m_GUID".to_string(),
        UnityValue::String("ABCDEF0123456789ABCDEF0123456789".to_string()),
    );
    pointer.insert("m_Type".to_string(), UnityValue::Integer(-3));

    let class = with_property(
        UnityClass::new(0, "PlainDocument".to_string(), "doc_0".to_string()),
        "target",
        UnityValue::Object(pointer),
    );
    let document = YamlDocument::from_entries(vec![class]);

    let scan = scan_reference_occurrences(&document, &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(scan.occurrences.len(), 1);
    assert_eq!(scan.occurrences[0].object, YamlDocumentSelector::ordinal(0));
    assert!(matches!(
        &scan.occurrences[0].shape,
        YamlReferenceShape::Valid(target)
            if target.file_id == -7
                && target.guid.as_deref() == Some("ABCDEF0123456789ABCDEF0123456789")
                && target.type_id == Some(-3)
    ));
}

#[test]
fn exact_budget_succeeds_and_one_short_budget_fails_as_a_typed_resource_error() {
    let document = parse_document(
        r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Targets:
    - {fileID: 0}
    - {fileID: -123}
"#,
    );
    let mut probe = AssetLoadBudget::default();
    let expected = scan_reference_occurrences(&document, &mut probe).unwrap();
    let usage = probe.usage();

    let exact_limits = AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        max_depth: usage.max_observed_depth,
        max_members: usage.members,
        ..AssetLoadLimits::default()
    };
    let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
    assert_eq!(
        scan_reference_occurrences(&document, &mut exact).unwrap(),
        expected
    );
    assert_eq!(exact.usage(), usage);

    let mut byte_short = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: usage.bytes - 1,
        ..exact_limits
    })
    .unwrap();
    assert!(matches!(
        scan_reference_occurrences(&document, &mut byte_short),
        Err(YamlReferenceScanError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            ..
        }))
    ));

    let mut depth_short = AssetLoadBudget::new(AssetLoadLimits {
        max_depth: usage.max_observed_depth - 1,
        ..exact_limits
    })
    .unwrap();
    assert!(matches!(
        scan_reference_occurrences(&document, &mut depth_short),
        Err(YamlReferenceScanError::Budget(BudgetError::Exceeded {
            resource: "depth",
            ..
        }))
    ));
}

#[test]
fn rejects_duplicate_file_ids_before_emitting_occurrences() {
    let first = with_property(
        UnityClass::new(1, "GameObject".to_string(), "100".to_string()),
        "target",
        UnityValue::Object(pointer(1)),
    );
    let second = with_property(
        UnityClass::new(4, "Transform".to_string(), "100".to_string()),
        "target",
        UnityValue::Object(pointer(2)),
    );
    let document = YamlDocument::from_entries(vec![first, second]);

    assert!(matches!(
        scan_reference_occurrences(&document, &mut AssetLoadBudget::default()),
        Err(YamlReferenceScanError::DuplicateDocumentFileId {
            first_document_index: 0,
            second_document_index: 1,
        })
    ));
}

#[test]
fn emits_an_occurrence_at_the_maximum_representable_field_path_depth() {
    let mut value = UnityValue::Object(pointer(77));
    for _ in 0..511 {
        value = UnityValue::Array(vec![value]);
    }
    let class = with_property(
        UnityClass::new(1, "GameObject".to_string(), "1".to_string()),
        "root",
        value,
    );
    let document = YamlDocument::from_entries(vec![class]);

    let scan = scan_reference_occurrences(&document, &mut AssetLoadBudget::default()).unwrap();
    assert_eq!(scan.occurrences.len(), 1);
    assert_eq!(scan.occurrences[0].field_path.segments().len(), 512);
    assert!(matches!(
        scan.occurrences[0].shape,
        YamlReferenceShape::Valid(ref target) if target.file_id == 77
    ));
}

#[test]
fn indexed_class_projection_scans_sparse_replacements_without_cloning_the_document() {
    let first = with_property(
        UnityClass::new(1, "GameObject".to_string(), "100".to_string()),
        "target",
        UnityValue::Object(pointer(1)),
    );
    let second = with_property(
        UnityClass::new(4, "Transform".to_string(), "200".to_string()),
        "target",
        UnityValue::Object(pointer(2)),
    );
    let document = YamlDocument::from_entries(vec![first, second]);

    let replacement = with_property(
        UnityClass::new(1, "GameObject".to_string(), "100".to_string()),
        "target",
        UnityValue::Object(pointer(91)),
    );
    let scan = scan_reference_class_occurrences(
        document.entries().len(),
        |index| match index {
            0 => Some(&replacement),
            _ => document.entries().get(index),
        },
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    assert_eq!(scan.occurrences.len(), 2);
    assert!(matches!(
        scan.occurrences[0].shape,
        YamlReferenceShape::Valid(ref target) if target.file_id == 91
    ));
    assert!(matches!(
        scan.occurrences[1].shape,
        YamlReferenceShape::Valid(ref target) if target.file_id == 2
    ));
    assert_eq!(
        scan.occurrences[0].object,
        YamlDocumentSelector::file_id(yaml_file_id(100))
    );
    assert_eq!(
        scan.occurrences[1].object,
        YamlDocumentSelector::file_id(yaml_file_id(200))
    );
}

#[test]
fn indexed_class_projection_rejects_a_missing_declared_document() {
    let class = UnityClass::new(1, "GameObject".to_string(), "100".to_string());
    assert!(matches!(
        scan_reference_class_occurrences(
            2,
            |index| (index == 0).then_some(&class),
            &mut AssetLoadBudget::default(),
        ),
        Err(YamlReferenceScanError::MissingDocument { document_index: 1 })
    ));
}

#[test]
fn classifies_malformed_reference_markers_without_fail_open() {
    assert_eq!(
        classify_reference_value(&UnityValue::Integer(1)),
        YamlReferenceClassification::NotReference
    );
    assert_eq!(
        classify_reference_value(&UnityValue::Object(IndexMap::from([(
            "value".to_string(),
            UnityValue::Integer(1),
        )]))),
        YamlReferenceClassification::NotReference
    );
    assert_eq!(
        classify_reference_value(&UnityValue::Object(pointer(0))),
        YamlReferenceClassification::ValidReference
    );
    assert_eq!(
        classify_reference_value(&UnityValue::Object(IndexMap::from([
            ("fileID".to_string(), UnityValue::Integer(1)),
            ("unexpected".to_string(), UnityValue::Integer(2)),
        ]))),
        YamlReferenceClassification::MalformedReference
    );
}

fn pointer(file_id: i64) -> IndexMap<String, UnityValue> {
    IndexMap::from([("fileID".to_string(), UnityValue::Integer(file_id))])
}
