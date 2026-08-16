use std::mem::size_of;

use indexmap::IndexMap;
use unity_asset_binary::typetree::TypeTreeSemanticDigestError;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError, DigestV1, FieldPath,
    SemanticDigestError, UnityClass, UnityValue, semantic_value_digest, yaml_field_schema_digest,
};

use crate::workspace::WorkspaceError;

use super::contract::RecipeError;
use super::output::RecipeOutputBuilder;

fn object(fields: impl IntoIterator<Item = (&'static str, UnityValue)>) -> UnityValue {
    UnityValue::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn class(
    class_id: i32,
    class_name: &str,
    fields: impl IntoIterator<Item = (&'static str, UnityValue)>,
) -> UnityClass {
    UnityClass::with_properties(
        class_id,
        class_name.to_owned(),
        "1".to_owned(),
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect::<IndexMap<_, _>>(),
    )
}

fn target_path() -> FieldPath {
    FieldPath::root().push_field("target").unwrap()
}

fn field_digests(class: &UnityClass) -> (DigestV1, DigestV1, AssetLoadUsage) {
    let path = target_path();
    let value = class.get("target").unwrap();
    let mut budget = AssetLoadBudget::default();
    let schema = yaml_field_schema_digest(class, &path, value, &mut budget).unwrap();
    let value = semantic_value_digest(value, &mut budget).unwrap();
    (schema, value, budget.usage())
}

#[test]
fn yaml_field_schema_is_order_and_value_invariant_but_shape_sensitive() {
    let ordered = class(
        21,
        "Material",
        [(
            "target",
            object([
                ("alpha", UnityValue::Integer(1)),
                ("beta", UnityValue::String("first".to_owned())),
            ]),
        )],
    );
    let reordered = class(
        21,
        "Material",
        [(
            "target",
            object([
                ("beta", UnityValue::String("first".to_owned())),
                ("alpha", UnityValue::Integer(1)),
            ]),
        )],
    );
    assert_eq!(field_digests(&ordered).0, field_digests(&reordered).0);
    assert_eq!(field_digests(&ordered).1, field_digests(&reordered).1);

    let changed_value = class(
        21,
        "Material",
        [(
            "target",
            object([
                ("alpha", UnityValue::Integer(99)),
                ("beta", UnityValue::String("second".to_owned())),
            ]),
        )],
    );
    assert_eq!(field_digests(&ordered).0, field_digests(&changed_value).0);
    assert_ne!(field_digests(&ordered).1, field_digests(&changed_value).1);

    let changed_shape = class(
        21,
        "Material",
        [(
            "target",
            object([
                ("alpha", UnityValue::String("1".to_owned())),
                ("beta", UnityValue::String("first".to_owned())),
            ]),
        )],
    );
    assert_ne!(field_digests(&ordered).0, field_digests(&changed_shape).0);
}

#[test]
fn yaml_array_schema_uses_emptiness_and_first_element_shape_only() {
    let one = class(
        1,
        "Shape",
        [("target", UnityValue::Array(vec![UnityValue::Integer(1)]))],
    );
    let many = class(
        1,
        "Shape",
        [(
            "target",
            UnityValue::Array(vec![UnityValue::Integer(2), UnityValue::Integer(3)]),
        )],
    );
    let changed_later = class(
        1,
        "Shape",
        [(
            "target",
            UnityValue::Array(vec![
                UnityValue::Integer(2),
                UnityValue::String("later".to_owned()),
            ]),
        )],
    );
    let empty = class(1, "Shape", [("target", UnityValue::Array(Vec::new()))]);
    let changed_first = class(
        1,
        "Shape",
        [(
            "target",
            UnityValue::Array(vec![UnityValue::String("first".to_owned())]),
        )],
    );

    assert_eq!(field_digests(&one).0, field_digests(&many).0);
    assert_eq!(field_digests(&one).0, field_digests(&changed_later).0);
    assert_ne!(field_digests(&one).1, field_digests(&many).1);
    assert_ne!(field_digests(&many).1, field_digests(&changed_later).1);
    assert_ne!(field_digests(&one).0, field_digests(&empty).0);
    assert_ne!(field_digests(&one).0, field_digests(&changed_first).0);
}

#[test]
fn yaml_field_schema_is_local_but_binds_class_and_path() {
    let base = class(
        21,
        "Material",
        [
            ("target", UnityValue::Integer(1)),
            ("unrelated", UnityValue::Array(vec![UnityValue::Integer(1)])),
        ],
    );
    let unrelated_changed = class(
        21,
        "Material",
        [
            (
                "unrelated",
                UnityValue::Array(vec![
                    UnityValue::Integer(1),
                    UnityValue::String("different".to_owned()),
                ]),
            ),
            ("target", UnityValue::Integer(9)),
            ("extra", UnityValue::Bool(true)),
        ],
    );
    assert_eq!(field_digests(&base).0, field_digests(&unrelated_changed).0);

    let value = base.get("target").unwrap();
    let path = target_path();
    let digest =
        yaml_field_schema_digest(&base, &path, value, &mut AssetLoadBudget::default()).unwrap();
    for changed in [
        class(22, "Material", [("target", UnityValue::Integer(1))]),
        class(21, "Other", [("target", UnityValue::Integer(1))]),
    ] {
        assert_ne!(digest, field_digests(&changed).0);
    }
    let other_path = FieldPath::root().push_field("other").unwrap();
    assert_ne!(
        digest,
        yaml_field_schema_digest(&base, &other_path, value, &mut AssetLoadBudget::default(),)
            .unwrap()
    );
}

#[test]
fn yaml_digest_budget_has_exact_entry_and_byte_boundaries() {
    let class = class(
        21,
        "Material",
        [(
            "target",
            object([
                ("alpha", UnityValue::Integer(1)),
                ("beta", UnityValue::String("value".to_owned())),
            ]),
        )],
    );
    let path = target_path();
    let value = class.get("target").unwrap();
    let mut measured = AssetLoadBudget::default();
    yaml_field_schema_digest(&class, &path, value, &mut measured).unwrap();
    let usage = measured.usage();
    assert!(usage.entries > 1 && usage.bytes > 1);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    yaml_field_schema_digest(&class, &path, value, &mut exact).unwrap();
    assert_eq!(exact.usage(), usage);

    for limits in [
        AssetLoadLimits {
            max_entries: usage.entries - 1,
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert!(matches!(
            yaml_field_schema_digest(&class, &path, value, &mut one_short),
            Err(SemanticDigestError::Budget(_))
        ));
    }
}

#[test]
fn recipe_output_builder_checks_before_allocating_or_charging() {
    let vector_bytes = u64::try_from(3 * size_of::<u64>()).unwrap();
    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 3,
        max_bytes: vector_bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let values = RecipeOutputBuilder::new(&mut exact)
        .vec::<u64>(3, "test vector")
        .unwrap();
    assert_eq!(values.capacity(), 3);
    assert_eq!(exact.usage().entries, 3);
    assert_eq!(exact.usage().bytes, vector_bytes);

    for limits in [
        AssetLoadLimits {
            max_entries: 2,
            max_bytes: vector_bytes,
            ..AssetLoadLimits::default()
        },
        AssetLoadLimits {
            max_entries: 3,
            max_bytes: vector_bytes - 1,
            ..AssetLoadLimits::default()
        },
    ] {
        let mut one_short = AssetLoadBudget::new(limits).unwrap();
        assert!(matches!(
            RecipeOutputBuilder::new(&mut one_short).vec::<u64>(3, "test vector"),
            Err(RecipeError::Budget(_))
        ));
        assert_eq!(one_short.usage(), AssetLoadUsage::default());
    }
}

#[test]
fn recipe_output_builder_budgeted_clone_handles_deep_paths() {
    let mut path = FieldPath::root();
    for _ in 0..512 {
        path = path.push_field("x").unwrap();
    }
    let mut measured = AssetLoadBudget::default();
    let clone = RecipeOutputBuilder::new(&mut measured).path(&path).unwrap();
    assert_eq!(clone, path);
    let usage = measured.usage();
    assert_eq!(usage.entries, 512);
    assert!(usage.bytes > 512);

    let mut exact = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    RecipeOutputBuilder::new(&mut exact).path(&path).unwrap();
    assert_eq!(exact.usage(), usage);

    let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: usage.entries,
        max_bytes: usage.bytes - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    assert!(matches!(
        RecipeOutputBuilder::new(&mut one_short).path(&path),
        Err(RecipeError::Budget(_))
    ));
    assert_eq!(one_short.usage(), AssetLoadUsage::default());
}

#[test]
fn workspace_schema_digest_budgets_remain_typed_recipe_errors() {
    let expected = BudgetError::Exceeded {
        resource: "schema digest test",
        limit: 4,
        requested: 5,
    };
    let errors = [
        WorkspaceError::operation(
            "YAML semantic schema digest",
            SemanticDigestError::Budget(expected.clone()),
        ),
        WorkspaceError::operation(
            "TypeTree semantic digest",
            TypeTreeSemanticDigestError::Budget(expected.clone()),
        ),
    ];

    for error in errors {
        assert!(matches!(
            RecipeError::from(error),
            RecipeError::Budget(actual) if actual == expected
        ));
    }
}
