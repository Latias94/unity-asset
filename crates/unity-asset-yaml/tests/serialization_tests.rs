//! Tests for Unity YAML serialization functionality
//!
//! These tests verify that our serialization produces valid Unity YAML
//! that can be round-tripped successfully.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use unity_asset_core::{AssetLoadBudget, LineEnding, UnityAssetError, UnityClass, UnityValue};
use unity_asset_yaml::{
    BudgetedYamlSource, UnityYamlSerializer, YamlDocument, load_budgeted_yaml_path,
    parse_budgeted_yaml_source,
};

struct FailingWriter {
    remaining: usize,
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected YAML writer failure",
            ));
        }
        let written = self.remaining.min(bytes.len());
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn with_property(
    class: UnityClass,
    key: impl Into<String>,
    value: impl Into<UnityValue>,
) -> UnityClass {
    let (header, mut properties) = class.into_parts();
    properties.insert(key.into(), value.into());
    UnityClass::from_parts(header, properties)
}

fn parse_yaml(input: &str) -> BudgetedYamlSource {
    let mut budget = AssetLoadBudget::default();
    parse_budgeted_yaml_source(Arc::from(input.as_bytes()), &mut budget).unwrap()
}

fn serialize_yaml<'class, I>(serializer: &mut UnityYamlSerializer, classes: I) -> String
where
    I: IntoIterator<Item = &'class UnityClass>,
{
    let mut budget = AssetLoadBudget::default();
    serializer
        .serialize_to_string_with_budget(classes, &mut budget)
        .unwrap()
}

/// Test basic serialization of a simple GameObject
#[test]
fn test_serialize_simple_gameobject() {
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());

    gameobject = with_property(gameobject, "m_ObjectHideFlags", UnityValue::Integer(0));
    gameobject = with_property(
        gameobject,
        "m_Name".to_string(),
        UnityValue::String("TestObject".to_string()),
    );
    gameobject = with_property(
        gameobject,
        "m_TagString".to_string(),
        UnityValue::String("Untagged".to_string()),
    );
    gameobject = with_property(gameobject, "m_Layer", UnityValue::Integer(0));
    gameobject = with_property(gameobject, "m_IsActive", UnityValue::Bool(true));

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serialize_yaml(&mut serializer, &[gameobject]);

    // Verify YAML structure
    assert!(yaml_output.contains("%YAML 1.1"));
    assert!(yaml_output.contains("%TAG !u! tag:unity3d.com,2011:"));
    assert!(yaml_output.contains("--- !u!1 &123456789"));
    assert!(yaml_output.contains("GameObject:"));
    assert!(yaml_output.contains("m_Name: TestObject"));
    assert!(yaml_output.contains("m_IsActive: 1"));

    println!("Generated YAML:\n{}", yaml_output);
}

/// Test serialization of complex nested objects
#[test]
fn test_serialize_complex_transform() {
    let mut transform = UnityClass::new(4, "Transform".to_string(), "987654321".to_string());

    // Add nested position object
    let mut position = HashMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.5));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(-0.5));
    transform = with_property(
        transform,
        "m_LocalPosition".to_string(),
        UnityValue::Object(position.into_iter().collect()),
    );

    // Add nested rotation object
    let mut rotation = HashMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.0));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(1.0));
    transform = with_property(
        transform,
        "m_LocalRotation".to_string(),
        UnityValue::Object(rotation.into_iter().collect()),
    );

    // Add array of children
    let children = vec![
        UnityValue::Integer(111111),
        UnityValue::Integer(222222),
        UnityValue::Integer(333333),
    ];
    transform = with_property(transform, "m_Children", UnityValue::Array(children));

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serialize_yaml(&mut serializer, &[transform]);

    // Verify complex structure
    assert!(yaml_output.contains("--- !u!4 &987654321"));
    assert!(yaml_output.contains("Transform:"));
    assert!(yaml_output.contains("m_LocalPosition:"));
    assert!(yaml_output.contains("m_LocalRotation:"));
    assert!(yaml_output.contains("m_Children:"));

    println!("Generated complex YAML:\n{}", yaml_output);
}

/// Test round-trip serialization (serialize -> parse -> serialize)
#[test]
fn test_round_trip_serialization() {
    // Create original data
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());
    gameobject = with_property(
        gameobject,
        "m_Name".to_string(),
        UnityValue::String("RoundTripTest".to_string()),
    );
    gameobject = with_property(gameobject, "m_IsActive", UnityValue::Bool(true));

    let mut position = HashMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.0));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(3.0));
    gameobject = with_property(
        gameobject,
        "m_Position".to_string(),
        UnityValue::Object(position.into_iter().collect()),
    );

    let original_classes = vec![gameobject];

    // First serialization
    let mut serializer = UnityYamlSerializer::new();
    let yaml1 = serialize_yaml(&mut serializer, &original_classes);

    // Parse back
    let parsed_source = parse_yaml(&yaml1);
    let parsed_classes = parsed_source.document().entries();

    // Second serialization
    let yaml2 = serialize_yaml(&mut serializer, parsed_classes.iter());

    // Verify data integrity
    assert_eq!(original_classes.len(), parsed_classes.len());

    let original = &original_classes[0];
    let parsed = &parsed_classes[0];

    assert_eq!(original.class_name(), parsed.class_name());
    assert_eq!(original.class_id(), parsed.class_id());
    assert_eq!(original.anchor(), parsed.anchor());

    // Check specific properties
    assert_eq!(original.get("m_Name"), parsed.get("m_Name"));

    // Note: Unity YAML represents booleans as integers (1/0)
    // So Bool(true) becomes Integer(1) after round-trip
    match (original.get("m_IsActive"), parsed.get("m_IsActive")) {
        (Some(UnityValue::Bool(true)), Some(UnityValue::Integer(1))) => {
            // This is expected - Unity represents true as 1
        }
        (Some(UnityValue::Bool(false)), Some(UnityValue::Integer(0))) => {
            // This is expected - Unity represents false as 0
        }
        (orig, parsed) => {
            panic!("Unexpected boolean conversion: {:?} -> {:?}", orig, parsed);
        }
    }

    println!("First YAML:\n{}", yaml1);
    println!("Second YAML:\n{}", yaml2);
}

/// Test serialization of multiple documents
#[test]
fn test_serialize_multiple_documents() {
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
    gameobject = with_property(
        gameobject,
        "m_Name".to_string(),
        UnityValue::String("Object1".to_string()),
    );

    let mut transform = UnityClass::new(4, "Transform".to_string(), "456".to_string());
    let mut pos = HashMap::new();
    pos.insert("x".to_string(), UnityValue::Float(0.0));
    pos.insert("y".to_string(), UnityValue::Float(0.0));
    pos.insert("z".to_string(), UnityValue::Float(0.0));
    transform = with_property(
        transform,
        "m_LocalPosition".to_string(),
        UnityValue::Object(pos.into_iter().collect()),
    );

    let mut monobehaviour = UnityClass::new(114, "MonoBehaviour".to_string(), "789".to_string());
    monobehaviour = with_property(monobehaviour, "m_Enabled", UnityValue::Bool(true));

    let classes = vec![gameobject, transform, monobehaviour];

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serialize_yaml(&mut serializer, &classes);

    // Should have YAML header only once
    let yaml_header_count = yaml_output.matches("%YAML 1.1").count();
    assert_eq!(yaml_header_count, 1);

    // Should have three document separators
    let doc_separator_count = yaml_output.matches("--- !u!").count();
    assert_eq!(doc_separator_count, 3);

    // Should contain all three class types
    assert!(yaml_output.contains("GameObject:"));
    assert!(yaml_output.contains("Transform:"));
    assert!(yaml_output.contains("MonoBehaviour:"));

    println!("Multi-document YAML:\n{}", yaml_output);
}

#[test]
fn io_writer_matches_string_output_for_borrowed_classes() {
    let mut gameobject = UnityClass::new(1, "GameObject".into(), "123".into());
    gameobject = with_property(gameobject, "m_Name", UnityValue::String("Streamed".into()));
    let mut transform = UnityClass::new(4, "Transform".into(), "456".into());
    transform = with_property(
        transform,
        "m_LocalPosition",
        UnityValue::Object(indexmap::indexmap! {
            "x".into() => UnityValue::Float(1.0),
            "y".into() => UnityValue::Float(2.0),
            "z".into() => UnityValue::Float(3.0),
        }),
    );
    let classes = [gameobject, transform];

    let mut string_serializer = UnityYamlSerializer::new().with_line_ending(LineEnding::Windows);
    let expected = serialize_yaml(&mut string_serializer, classes.iter());
    let mut actual = Vec::new();
    let mut writer_serializer = UnityYamlSerializer::new().with_line_ending(LineEnding::Windows);
    let mut writer_budget = AssetLoadBudget::default();
    writer_serializer
        .serialize_to_writer_with_budget(&mut actual, classes.iter(), &mut writer_budget)
        .unwrap();

    assert_eq!(actual, expected.as_bytes());
    assert_eq!(classes[0].class_name(), "GameObject");
}

#[test]
fn io_writer_failure_preserves_the_original_io_error() {
    let class = UnityClass::new(1, "GameObject".into(), "123".into());
    let mut writer = FailingWriter { remaining: 9 };
    let mut budget = AssetLoadBudget::default();

    let error = UnityYamlSerializer::new()
        .serialize_to_writer_with_budget(&mut writer, std::iter::once(&class), &mut budget)
        .unwrap_err();

    let UnityAssetError::Io(error) = error else {
        panic!("expected an I/O error, got {error:?}");
    };
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "injected YAML writer failure");
}

/// Test immutable YamlDocument serialization through the dedicated serializer.
#[test]
fn test_yaml_document_serialization() {
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
    gameobject = with_property(
        gameobject,
        "m_Name".to_string(),
        UnityValue::String("DocumentTest".to_string()),
    );
    gameobject = with_property(gameobject, "m_IsActive", UnityValue::Bool(true));

    let mut transform = UnityClass::new(4, "Transform".to_string(), "456".to_string());
    let mut pos = HashMap::new();
    pos.insert("x".to_string(), UnityValue::Float(1.0));
    pos.insert("y".to_string(), UnityValue::Float(2.0));
    pos.insert("z".to_string(), UnityValue::Float(3.0));
    transform = with_property(
        transform,
        "m_LocalPosition".to_string(),
        UnityValue::Object(pos.into_iter().collect()),
    );
    let doc = YamlDocument::from_entries(vec![gameobject, transform]);

    let mut serializer = UnityYamlSerializer::new().with_line_ending(doc.line_ending());
    let yaml_content = serialize_yaml(&mut serializer, doc.entries());

    // Verify structure
    assert!(yaml_content.contains("%YAML 1.1"));
    assert!(yaml_content.contains("GameObject:"));
    assert!(yaml_content.contains("Transform:"));
    assert!(yaml_content.contains("m_Name: DocumentTest"));
    assert!(yaml_content.contains("m_LocalPosition:"));

    // Test round-trip through string
    let parsed_source = parse_yaml(&yaml_content);
    let parsed_classes = parsed_source.document().entries();

    assert_eq!(parsed_classes.len(), 2);
    assert_eq!(parsed_classes[0].class_name(), "GameObject");
    assert_eq!(parsed_classes[1].class_name(), "Transform");

    println!("YamlDocument YAML:\n{}", yaml_content);
}

#[test]
fn yaml_document_preserves_u64_max_exactly() {
    let mut class = UnityClass::new(114, "MonoBehaviour".to_string(), "123".to_string());
    class = with_property(class, "m_StreamOffset", UnityValue::from(u64::MAX));
    let doc = YamlDocument::from_entries(vec![class]);

    let mut serializer = UnityYamlSerializer::new().with_line_ending(doc.line_ending());
    let yaml = serialize_yaml(&mut serializer, doc.entries());
    assert!(yaml.contains(&format!("m_StreamOffset: {}", u64::MAX)));

    let parsed_source = parse_yaml(&yaml);
    let classes = parsed_source.document().entries();
    assert_eq!(
        classes[0].get("m_StreamOffset"),
        Some(&UnityValue::Unsigned(u64::MAX))
    );
}

#[test]
fn zero_prefixed_decimal_strings_survive_fixture_rewrites() {
    let cases: [(&str, &[(&str, &str)]); 2] = [
        ("MultipleTypesDoc.asset", &[("scalar_str_002", "00000000")]),
        (
            "SingleDoc.asset",
            &[
                ("m_ColorGamuts", "00000000"),
                ("metroCertificateNotAfter", "0000000000000000"),
            ],
        ),
    ];

    for (fixture, fields) in cases {
        let path = Path::new("tests/fixtures").join(fixture);
        let mut load_budget = AssetLoadBudget::default();
        let source = load_budgeted_yaml_path(&path, &mut load_budget).unwrap();
        let class = &source.document().entries()[0];
        for &(field, expected) in fields {
            assert_eq!(
                class.get(field).and_then(UnityValue::as_str),
                Some(expected)
            );
        }

        let mut serializer = UnityYamlSerializer::new();
        let rewritten = serialize_yaml(&mut serializer, source.document().entries());
        for &(field, expected) in fields {
            assert!(
                rewritten.contains(&format!("{field}: \"{expected}\"")),
                "rewritten {fixture} must quote the zero-prefixed string {field}"
            );
        }

        let reparsed = parse_yaml(&rewritten);
        let class = &reparsed.document().entries()[0];
        for &(field, expected) in fields {
            assert_eq!(
                class.get(field).and_then(UnityValue::as_str),
                Some(expected)
            );
        }
    }
}

#[test]
fn unsigned_values_remain_simple_inline_scalars() {
    let mut class = UnityClass::new(114, "MonoBehaviour".to_string(), "123".to_string());
    class = with_property(
        class,
        "m_Offsets".to_string(),
        UnityValue::Array(vec![UnityValue::Unsigned(u64::MAX)]),
    );
    class = with_property(
        class,
        "m_StreamData".to_string(),
        UnityValue::Object(indexmap::indexmap! {
            "offset".to_string() => UnityValue::Unsigned(u64::MAX),
        }),
    );

    let mut serializer = UnityYamlSerializer::new();
    let yaml = serialize_yaml(&mut serializer, &[class]);

    assert!(yaml.contains(&format!("m_Offsets: [{}]", u64::MAX)));
    assert!(yaml.contains(&format!("m_StreamData: {{offset: {}}}", u64::MAX)));
}

/// Test serialization with special characters and edge cases
#[test]
fn test_serialize_special_cases() {
    let mut test_class = UnityClass::new(114, "MonoBehaviour".to_string(), "123".to_string());

    // Test various string types
    test_class = with_property(
        test_class,
        "empty_string".to_string(),
        UnityValue::String("".to_string()),
    );
    test_class = with_property(
        test_class,
        "quoted_string".to_string(),
        UnityValue::String("Hello \"World\"".to_string()),
    );
    test_class = with_property(
        test_class,
        "multiline_string".to_string(),
        UnityValue::String("Line 1\nLine 2".to_string()),
    );
    test_class = with_property(
        test_class,
        "special_chars".to_string(),
        UnityValue::String("Special: []{},".to_string()),
    );

    // Test edge case numbers
    test_class = with_property(test_class, "zero_int", UnityValue::Integer(0));
    test_class = with_property(test_class, "negative_int", UnityValue::Integer(-42));
    test_class = with_property(test_class, "zero_float", UnityValue::Float(0.0));
    test_class = with_property(
        test_class,
        "negative_float".to_string(),
        UnityValue::Float(-std::f64::consts::PI),
    );

    // Test empty collections
    test_class = with_property(test_class, "empty_array", UnityValue::Array(vec![]));
    test_class = with_property(
        test_class,
        "empty_object".to_string(),
        UnityValue::Object(indexmap::IndexMap::new()),
    );

    // Test null value
    test_class = with_property(test_class, "null_value", UnityValue::Null);

    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serialize_yaml(&mut serializer, &[test_class]);

    // Verify special cases are handled
    assert!(yaml_output.contains("empty_string:"));
    assert!(yaml_output.contains("quoted_string:"));
    assert!(yaml_output.contains("empty_array: []"));
    assert!(yaml_output.contains("empty_object: {}"));
    assert!(yaml_output.contains("null_value: {fileID: 0}"));

    // Test that it can be parsed back
    let parsed_source = parse_yaml(&yaml_output);
    let parsed_classes = parsed_source.document().entries();
    assert_eq!(parsed_classes.len(), 1);

    println!("Special cases YAML:\n{}", yaml_output);
}
