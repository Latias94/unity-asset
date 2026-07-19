use unity_asset_binary::asset::{SerializedFile, SerializedFileParser};
use unity_asset_binary::bundle::BundleParser;
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, DigestV1, FieldPath, UnityValue};
use unity_asset_write::object::{
    SerializedFieldGuard, SerializedObjectEncodeError, SerializedObjectEncoder,
    SerializedObjectEncodingMode, SerializedObjectGuard, SerializedObjectMutation,
    SerializedSequenceEdit, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
};
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};

struct ObservedName {
    path_id: i64,
    field: &'static str,
    value: UnityValue,
    schema_digest: DigestV1,
}

struct ObservedSequence {
    path_id: i64,
    field: String,
    value: UnityValue,
    first: UnityValue,
    length: usize,
    schema_digest: DigestV1,
}

fn sample_serialized_file() -> SerializedFile {
    let bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let bundle = BundleParser::from_bytes(bytes).expect("parse sample bundle");
    let node = bundle
        .nodes
        .iter()
        .find(|node| {
            node.is_file() && !node.name.ends_with(".resS") && !node.name.ends_with(".resource")
        })
        .expect("sample bundle contains a SerializedFile");
    let bytes = bundle
        .extract_node_data(node)
        .expect("extract sample SerializedFile");
    SerializedFileParser::from_bytes(bytes).expect("parse sample SerializedFile")
}

fn observe_name(file: &SerializedFile) -> ObservedName {
    let mut budget = AssetLoadBudget::default();
    for handle in file.object_handles() {
        let Ok(materialized) = handle.materialize(&mut budget) else {
            continue;
        };
        let Some(schema) = materialized.schema() else {
            continue;
        };
        let Some((field, value)) = ["m_Name", "name"].into_iter().find_map(|field| {
            materialized
                .object()
                .class
                .get(field)
                .filter(|value| matches!(value, UnityValue::String(name) if !name.is_empty()))
                .cloned()
                .map(|value| (field, value))
        }) else {
            continue;
        };
        let schema_digest = schema
            .semantic_digest_with_budget(&mut budget)
            .expect("digest sample TypeTree");
        return ObservedName {
            path_id: handle.path_id(),
            field,
            value,
            schema_digest,
        };
    }
    panic!("sample contains an object with a non-empty name")
}

fn observe_short_sequence(file: &SerializedFile) -> ObservedSequence {
    let mut budget = AssetLoadBudget::default();
    for handle in file.object_handles() {
        let Ok(materialized) = handle.materialize(&mut budget) else {
            continue;
        };
        let Some(schema) = materialized.schema() else {
            continue;
        };
        let Some((field, values)) = materialized.object().class.properties().iter().find_map(
            |(field, value)| match value {
                UnityValue::Array(values) if !values.is_empty() && values.len() <= 64 => {
                    Some((field.clone(), values))
                }
                _ => None,
            },
        ) else {
            continue;
        };
        let schema_digest = schema
            .semantic_digest_with_budget(&mut budget)
            .expect("digest sample sequence TypeTree");
        return ObservedSequence {
            path_id: handle.path_id(),
            field,
            value: UnityValue::Array(values.clone()),
            first: values[0].clone(),
            length: values.len(),
            schema_digest,
        };
    }
    panic!("sample contains an object with a short non-empty root sequence")
}

fn name_path(field: &str) -> FieldPath {
    FieldPath::root()
        .push_field(field)
        .expect("valid test field path")
}

fn field_guard(observed: &ObservedName, value: &UnityValue) -> (FieldPath, SerializedFieldGuard) {
    let path = name_path(observed.field);
    let guard = SerializedFieldGuard::from_observed(
        observed.schema_digest,
        &path,
        value,
        &mut AssetLoadBudget::default(),
    )
    .expect("build field guard");
    (path, guard)
}

fn rename_operations(
    observed: &ObservedName,
    first: &str,
    second: &str,
) -> Vec<SerializedObjectMutation> {
    let first_value = UnityValue::String(first.to_owned());
    let second_value = UnityValue::String(second.to_owned());
    let (first_path, first_guard) = field_guard(observed, &observed.value);
    let (second_path, second_guard) = field_guard(observed, &first_value);
    vec![
        SerializedObjectMutation::replace_field(3, first_path, first_guard, first_value),
        SerializedObjectMutation::replace_field(9, second_path, second_guard, second_value),
    ]
}

#[test]
fn ordered_mutations_observe_prior_results_and_rewrite_once() {
    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let final_name = "ENCODER_SECOND_RESULT";
    let encoded = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind encoder")
        .encode_semantic(
            rename_operations(&observed, "ENCODER_FIRST_RESULT", final_name),
            &mut AssetLoadBudget::default(),
        )
        .expect("encode both ordered mutations");

    assert_eq!(encoded.mode(), SerializedObjectEncodingMode::Semantic);
    assert_eq!(encoded.path_id(), observed.path_id);
    assert_eq!(encoded.schema_digest(), Some(observed.schema_digest));
    assert_ne!(encoded.original_digest(), encoded.output_digest());
    assert_eq!(encoded.stats().parse_passes, 1);
    assert_eq!(encoded.stats().validation_passes, 2);
    assert_eq!(encoded.stats().rewrite_passes, 1);
    assert_eq!(encoded.stats().operations_applied, 2);
    assert_eq!(encoded.stats().validation.owned_bytes, 0);
    assert!(encoded.stats().validation.node_visits > 0);
    assert!(encoded.stats().preserved_bytes > 0);

    let mut edits = SerializedFileEdits::default();
    edits.set_object_bytes(observed.path_id, encoded.into_bytes());
    let saved = SerializedFileWriter::save(&file, &edits).expect("rebuild SerializedFile");
    let reparsed = SerializedFileParser::from_bytes(saved).expect("reparse rebuilt file");
    let object = reparsed
        .find_object_handle(observed.path_id)
        .expect("edited object remains addressable")
        .read(&mut AssetLoadBudget::default())
        .expect("read edited object");
    assert_eq!(
        object.class.get(observed.field),
        Some(&UnityValue::String(final_name.to_owned()))
    );
}

#[test]
fn later_guard_failure_returns_no_encoded_output() {
    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let original = file
        .find_object_handle(observed.path_id)
        .expect("observed object exists")
        .raw_data()
        .expect("read original bytes");
    let original_digest = DigestV1::hash_bytes(original);
    let first_value = UnityValue::String("VISIBLE_ONLY_INSIDE_FAILED_RUN".to_owned());
    let (first_path, first_guard) = field_guard(&observed, &observed.value);
    let (second_path, stale_guard) = field_guard(&observed, &observed.value);
    let operations = vec![
        SerializedObjectMutation::replace_field(0, first_path, first_guard, first_value),
        SerializedObjectMutation::replace_field(
            1,
            second_path,
            stale_guard,
            UnityValue::String("MUST_NOT_ESCAPE".to_owned()),
        ),
    ];

    let error = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind encoder")
        .encode_semantic(operations, &mut AssetLoadBudget::default())
        .expect_err("second guard must observe the first replacement");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::FieldValueGuardMismatch { ordinal: 1, .. }
    ));
    let still_original = file
        .find_object_handle(observed.path_id)
        .expect("observed object remains present")
        .raw_data()
        .expect("reread original bytes");
    assert_eq!(DigestV1::hash_bytes(still_original), original_digest);
}

#[test]
fn duplicate_operation_ordinal_returns_no_encoded_output() {
    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let first_value = UnityValue::String("FIRST_LOCAL_RESULT".to_owned());
    let (first_path, first_guard) = field_guard(&observed, &observed.value);
    let (second_path, second_guard) = field_guard(&observed, &first_value);
    let operations = [
        SerializedObjectMutation::replace_field(0, first_path, first_guard, first_value),
        SerializedObjectMutation::replace_field(
            0,
            second_path,
            second_guard,
            UnityValue::String("MUST_NOT_ESCAPE".to_owned()),
        ),
    ];

    let error = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind encoder")
        .encode_semantic(operations, &mut AssetLoadBudget::default())
        .expect_err("duplicate ordinals must reject the complete encoding");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::OperationOrder {
            previous: 0,
            current: 0,
        }
    ));
}

#[test]
fn invalid_later_replacement_reports_its_operation_and_path() {
    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let first_value = UnityValue::String("VALID_FIRST_RESULT".to_owned());
    let (first_path, first_guard) = field_guard(&observed, &observed.value);
    let (second_path, second_guard) = field_guard(&observed, &first_value);
    let expected_path = second_path.clone();
    let operations = [
        SerializedObjectMutation::replace_field(3, first_path, first_guard, first_value),
        SerializedObjectMutation::replace_field(
            9,
            second_path,
            second_guard,
            UnityValue::Integer(7),
        ),
    ];

    let error = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind encoder")
        .encode_semantic(operations, &mut AssetLoadBudget::default())
        .expect_err("integer replacement cannot satisfy a string TypeTree node");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::ReplacementValue {
            path_id,
            ordinal: 9,
            path,
            source,
        } if path_id == observed.path_id
            && path == expected_path
            && source.to_string().contains("requires a String")
    ));
}

#[test]
fn sequence_edit_uses_the_same_guarded_single_rewrite_pipeline() {
    let file = sample_serialized_file();
    let observed = observe_short_sequence(&file);
    let path = name_path(&observed.field);
    let guard = SerializedFieldGuard::from_observed(
        observed.schema_digest,
        &path,
        &observed.value,
        &mut AssetLoadBudget::default(),
    )
    .expect("build sequence guard");
    let index = u32::try_from(observed.length).expect("short sequence length fits u32");
    let operation = SerializedObjectMutation::edit_sequence(
        4,
        path,
        guard,
        SerializedSequenceEdit::Insert {
            index,
            value: observed.first.clone(),
        },
    );
    let encoded = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind sequence encoder")
        .encode_semantic([operation], &mut AssetLoadBudget::default())
        .expect("insert cloned sequence element");
    assert_eq!(encoded.stats().parse_passes, 1);
    assert_eq!(encoded.stats().validation_passes, 1);
    assert_eq!(encoded.stats().rewrite_passes, 1);
    assert_eq!(encoded.stats().operations_applied, 1);

    let mut edits = SerializedFileEdits::default();
    edits.set_object_bytes(observed.path_id, encoded.into_bytes());
    let saved = SerializedFileWriter::save(&file, &edits).expect("rebuild sequence file");
    let reparsed = SerializedFileParser::from_bytes(saved).expect("reparse sequence file");
    let object = reparsed
        .find_object_handle(observed.path_id)
        .expect("sequence object remains addressable")
        .read(&mut AssetLoadBudget::default())
        .expect("read sequence object");
    let values = object
        .class
        .get(&observed.field)
        .and_then(UnityValue::as_array)
        .expect("edited field remains a sequence");
    assert_eq!(values.len(), observed.length + 1);
    assert_eq!(values.last(), Some(&observed.first));
}

#[test]
fn object_replacement_rejects_fields_absent_from_the_typetree() {
    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let materialized = file
        .find_object_handle(observed.path_id)
        .expect("observed object exists")
        .materialize(&mut AssetLoadBudget::default())
        .expect("materialize observed object");
    let original_root = UnityValue::Object(materialized.object().class.properties().clone());
    let guard = SerializedObjectGuard::from_observed(
        observed.schema_digest,
        &original_root,
        &mut AssetLoadBudget::default(),
    )
    .expect("build object guard");
    let UnityValue::Object(mut replacement) = original_root else {
        unreachable!("test constructs an object root")
    };
    let expected_fields = replacement.len();
    replacement.insert("__encoder_extra_field".to_owned(), UnityValue::Null);
    let operation = SerializedObjectMutation::replace_object(0, guard, replacement);

    let error = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind object encoder")
        .encode_semantic([operation], &mut AssetLoadBudget::default())
        .expect_err("field absent from TypeTree must be rejected");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::ReplacementShape {
            path_id,
            ordinal: 0,
            path,
            expected_fields: actual_expected,
            actual_fields,
        } if path_id == observed.path_id
            && path.segments().is_empty()
            && actual_expected == expected_fields
            && actual_fields == expected_fields + 1
    ));
}

#[test]
fn missing_typetree_is_a_structured_rejection() {
    let mut file = sample_serialized_file();
    let path_id = file.objects()[0].path_id();
    let class_id = file.objects()[0].class_id();
    file.set_type_tree_enabled(false);
    let guard = SerializedFieldGuard::new(
        DigestV1::from_bytes([0; DigestV1::BYTE_LEN]),
        DigestV1::from_bytes([0; DigestV1::BYTE_LEN]),
    );
    let operation = SerializedObjectMutation::replace_field(
        0,
        name_path("m_Name"),
        guard,
        UnityValue::String("unused".to_owned()),
    );

    let error = SerializedObjectEncoder::new(&file, path_id)
        .expect("bind encoder")
        .encode_semantic([operation], &mut AssetLoadBudget::default())
        .expect_err("disabled TypeTree must prevent semantic encoding");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::TypeTreeUnavailable {
            path_id: actual_path_id,
            class_id: actual_class_id,
        } if actual_path_id == path_id && actual_class_id == class_id
    ));
}

#[test]
fn unsafe_raw_digest_mismatch_returns_no_output_or_budget_charge() {
    let file = sample_serialized_file();
    let path_id = file.objects()[0].path_id();
    let original = file
        .find_object_handle(path_id)
        .expect("first object exists")
        .raw_data()
        .expect("read original object");
    let actual = DigestV1::hash_bytes(original);
    let expected = DigestV1::hash_bytes(b"not the original object");
    assert_ne!(expected, actual);
    let replacement = UnsafeRawObjectReplacement::new(
        expected,
        vec![1, 2, 3, 4],
        UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
    );
    let mut budget = AssetLoadBudget::default();
    let before = budget.usage();

    let error = SerializedObjectEncoder::new(&file, path_id)
        .expect("bind encoder")
        .encode_unsafe_raw(replacement, &mut budget)
        .expect_err("wrong raw digest must reject replacement");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::RawDigestMismatch {
            path_id: actual_path_id,
            expected: actual_expected,
            actual: actual_digest,
        } if actual_path_id == path_id && actual_expected == expected && actual_digest == actual
    ));
    assert_eq!(budget.usage(), before);
    assert_eq!(DigestV1::hash_bytes(original), actual);
}

#[test]
fn unsafe_raw_success_is_explicit_and_budgeted() {
    let file = sample_serialized_file();
    let path_id = file.objects()[0].path_id();
    let original = file
        .find_object_handle(path_id)
        .expect("first object exists")
        .raw_data()
        .expect("read original object");
    let expected = DigestV1::hash_bytes(original);
    let bytes = vec![1, 2, 3, 4];
    let replacement = UnsafeRawObjectReplacement::new(
        expected,
        bytes.clone(),
        UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
    );
    let mut budget = AssetLoadBudget::default();

    let encoded = SerializedObjectEncoder::new(&file, path_id)
        .expect("bind encoder")
        .encode_unsafe_raw(replacement, &mut budget)
        .expect("matching digest permits acknowledged raw replacement");
    assert_eq!(encoded.mode(), SerializedObjectEncodingMode::UnsafeRaw);
    assert_eq!(encoded.original_digest(), expected);
    assert_eq!(encoded.output_digest(), DigestV1::hash_bytes(&bytes));
    assert_eq!(encoded.bytes(), bytes);
    assert_eq!(encoded.stats().parse_passes, 0);
    assert_eq!(encoded.stats().validation_passes, 0);
    assert_eq!(encoded.stats().rewrite_passes, 0);
    assert_eq!(encoded.stats().operations_applied, 1);
    assert_eq!(budget.usage().bytes, bytes.len() as u64);
    assert_eq!(budget.usage().entries, 1);
}

#[test]
fn late_budget_failure_drops_all_local_mutations() {
    let measured_file = sample_serialized_file();
    let measured_observation = observe_name(&measured_file);
    let mut measured_budget = AssetLoadBudget::default();
    let _measured = SerializedObjectEncoder::new(&measured_file, measured_observation.path_id)
        .expect("bind measured encoder")
        .encode_semantic(
            rename_operations(
                &measured_observation,
                "BUDGET_FIRST_RESULT",
                "BUDGET_SECOND_RESULT",
            ),
            &mut measured_budget,
        )
        .expect("measure successful encoding");
    let successful_bytes = measured_budget.usage().bytes;
    assert!(successful_bytes > 1);

    let file = sample_serialized_file();
    let observed = observe_name(&file);
    let original = file
        .find_object_handle(observed.path_id)
        .expect("observed object exists")
        .raw_data()
        .expect("read original object");
    let original_digest = DigestV1::hash_bytes(original);
    let mut limited = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: successful_bytes - 1,
        ..AssetLoadLimits::default()
    })
    .expect("valid one-short budget");

    let error = SerializedObjectEncoder::new(&file, observed.path_id)
        .expect("bind limited encoder")
        .encode_semantic(
            rename_operations(&observed, "BUDGET_FIRST_RESULT", "BUDGET_SECOND_RESULT"),
            &mut limited,
        )
        .expect_err("one-short byte budget must reject encoding");
    assert!(matches!(
        error,
        SerializedObjectEncodeError::Budget(unity_asset_core::BudgetError::Exceeded {
            resource: "bytes",
            ..
        })
    ));
    let still_original = file
        .find_object_handle(observed.path_id)
        .expect("observed object remains present")
        .raw_data()
        .expect("reread original bytes");
    assert_eq!(DigestV1::hash_bytes(still_original), original_digest);
}
