//! Tests for advanced filtering API
//!
//! These tests verify the new filter() and get() methods work correctly
//! and provide the same functionality as the Python reference library.

use std::path::Path;
use unity_asset_core::{UnityClass, UnityDocument, UnityValue};
use unity_asset_yaml::YamlDocument;

/// Test the filter() method with various combinations
#[test]
fn test_filter_method() {
    let fixture_path = Path::new("tests/fixtures/MultiDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let doc = YamlDocument::load_yaml(fixture_path, false).unwrap();
    println!("Loaded document with {} entries", doc.entries().len());

    // Test 1: Filter by class name only
    let gameobjects = doc.filter(Some(&["GameObject"]), None);
    println!("Found {} GameObjects", gameobjects.len());
    assert_eq!(gameobjects.len(), 1);
    assert_eq!(gameobjects[0].class_name, "GameObject");

    // Test 2: Filter by multiple class names
    let transforms_and_mono = doc.filter(Some(&["Transform", "MonoBehaviour"]), None);
    println!(
        "Found {} Transform or MonoBehaviour entries",
        transforms_and_mono.len()
    );
    assert!(transforms_and_mono.len() >= 2);

    // Test 3: Filter by attributes only
    let enabled_objects = doc.filter(None, Some(&["m_Enabled"]));
    println!("Found {} objects with m_Enabled", enabled_objects.len());
    for obj in &enabled_objects {
        assert!(obj.has_property("m_Enabled"));
        println!("  - {} has m_Enabled", obj.class_name);
    }

    // Test 4: Filter by class name AND attributes
    let enabled_mono = doc.filter(Some(&["MonoBehaviour"]), Some(&["m_Enabled"]));
    println!("Found {} MonoBehaviour with m_Enabled", enabled_mono.len());
    for obj in &enabled_mono {
        assert_eq!(obj.class_name, "MonoBehaviour");
        assert!(obj.has_property("m_Enabled"));
    }

    // Test 5: Filter with no matches
    let no_matches = doc.filter(Some(&["NonExistentClass"]), None);
    assert_eq!(no_matches.len(), 0);

    // Test 6: Filter all (no filters)
    let all_entries = doc.filter(None, None);
    assert_eq!(all_entries.len(), doc.entries().len());
}

/// Test the get() method for single entry retrieval
#[test]
fn test_get_method() {
    let fixture_path = Path::new("tests/fixtures/MultiDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let doc = YamlDocument::load_yaml(fixture_path, false).unwrap();

    // Test 1: Get by class name
    let gameobject = doc.get(Some("GameObject"), None).unwrap();
    assert_eq!(gameobject.class_name, "GameObject");
    println!("✓ Found GameObject: {}", gameobject.class_name);

    // Test 2: Get by class name and attributes
    let transform = doc
        .get(Some("Transform"), Some(&["m_LocalPosition"]))
        .unwrap();
    assert_eq!(transform.class_name, "Transform");
    assert!(transform.has_property("m_LocalPosition"));
    println!("✓ Found Transform with m_LocalPosition");

    // Test 3: Get with no matches should fail
    let result = doc.get(Some("NonExistentClass"), None);
    assert!(result.is_err());
    println!("✓ Correctly failed to find non-existent class");

    // Test 4: Get with attribute that doesn't exist should fail
    let result = doc.get(Some("GameObject"), Some(&["m_NonExistentProperty"]));
    assert!(result.is_err());
    println!("✓ Correctly failed to find GameObject with non-existent property");
}

/// Test filtering with complex attribute combinations
#[test]
fn test_complex_attribute_filtering() {
    let fixture_path = Path::new("tests/fixtures/MultiDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let doc = YamlDocument::load_yaml(fixture_path, false).unwrap();

    // Find objects that have multiple specific attributes
    let complex_filter = doc.filter(None, Some(&["m_ObjectHideFlags", "m_GameObject"]));
    println!(
        "Found {} objects with both m_ObjectHideFlags and m_GameObject",
        complex_filter.len()
    );

    for obj in &complex_filter {
        assert!(obj.has_property("m_ObjectHideFlags"));
        assert!(obj.has_property("m_GameObject"));
        println!("  - {} has both properties", obj.class_name);
    }

    // Test with attributes that should exist in specific classes
    let mono_with_script = doc.filter(Some(&["MonoBehaviour"]), Some(&["m_Script"]));
    println!(
        "Found {} MonoBehaviour with m_Script",
        mono_with_script.len()
    );

    for obj in &mono_with_script {
        assert_eq!(obj.class_name, "MonoBehaviour");
        assert!(obj.has_property("m_Script"));
    }
}

/// Test filtering edge cases
#[test]
fn test_filtering_edge_cases() {
    let fixture_path = Path::new("tests/fixtures/SingleDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let doc = YamlDocument::load_yaml(fixture_path, false).unwrap();

    // Test with empty class names array
    let empty_class_filter = doc.filter(Some(&[]), None);
    assert_eq!(empty_class_filter.len(), doc.entries().len());
    println!("✓ Empty class names array returns all entries");

    // Test with empty attributes array
    let empty_attr_filter = doc.filter(None, Some(&[]));
    assert_eq!(empty_attr_filter.len(), doc.entries().len());
    println!("✓ Empty attributes array returns all entries");

    // Test case sensitivity
    let case_sensitive = doc.filter(Some(&["playerSettings"]), None); // lowercase
    assert_eq!(case_sensitive.len(), 0);
    println!("✓ Class name filtering is case sensitive");

    let correct_case = doc.filter(Some(&["PlayerSettings"]), None); // correct case
    assert_eq!(correct_case.len(), 1);
    println!("✓ Found PlayerSettings with correct case");
}

/// Test that filtering preserves object references
#[test]
fn test_filtering_preserves_references() {
    let fixture_path = Path::new("tests/fixtures/MultiDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let doc = YamlDocument::load_yaml(fixture_path, false).unwrap();

    // Get a reference through filtering
    let gameobjects = doc.filter(Some(&["GameObject"]), None);
    assert_eq!(gameobjects.len(), 1);

    let filtered_gameobject = gameobjects[0];

    // Get the same object directly from entries
    let direct_gameobject = doc
        .entries()
        .iter()
        .find(|entry| entry.class_name == "GameObject")
        .unwrap();

    // They should be the same object (same memory address)
    assert!(std::ptr::eq(filtered_gameobject, direct_gameobject));
    println!("✓ Filtering returns references to original objects");

    // Verify we can access properties through the filtered reference
    if let Some(name) = filtered_gameobject.get("m_Name") {
        println!(
            "✓ Can access properties through filtered reference: {:?}",
            name
        );
    }
}

/// Test performance with larger documents
#[test]
fn test_filtering_performance() {
    // Create a synthetic document with many entries for performance testing
    let mut doc = YamlDocument::new();

    // Add many different types of objects
    for i in 0..100 {
        let mut gameobject = UnityClass::new(1, "GameObject".to_string(), format!("go_{}", i));
        gameobject.set(
            "m_Name".to_string(),
            UnityValue::String(format!("GameObject_{}", i)),
        );
        gameobject.set("m_IsActive".to_string(), UnityValue::Bool(i % 2 == 0));
        doc.add_entry(gameobject);

        let mut transform = UnityClass::new(4, "Transform".to_string(), format!("tr_{}", i));
        transform.set(
            "m_LocalPosition".to_string(),
            UnityValue::Object(indexmap::IndexMap::new()),
        );
        doc.add_entry(transform);

        if i % 3 == 0 {
            let mut mono = UnityClass::new(114, "MonoBehaviour".to_string(), format!("mb_{}", i));
            mono.set("m_Enabled".to_string(), UnityValue::Bool(true));
            mono.set(
                "m_Script".to_string(),
                UnityValue::Object(indexmap::IndexMap::new()),
            );
            doc.add_entry(mono);
        }
    }

    println!(
        "Created synthetic document with {} entries",
        doc.entries().len()
    );

    // Time the filtering operations
    let start = std::time::Instant::now();

    let gameobjects = doc.filter(Some(&["GameObject"]), None);
    let transforms = doc.filter(Some(&["Transform"]), None);
    let monobehaviours = doc.filter(Some(&["MonoBehaviour"]), Some(&["m_Enabled"]));
    let all_with_position = doc.filter(None, Some(&["m_LocalPosition"]));

    let duration = start.elapsed();

    println!("Filtering operations completed in {:?}", duration);
    println!("  - Found {} GameObjects", gameobjects.len());
    println!("  - Found {} Transforms", transforms.len());
    println!(
        "  - Found {} MonoBehaviours with m_Enabled",
        monobehaviours.len()
    );
    println!(
        "  - Found {} objects with m_LocalPosition",
        all_with_position.len()
    );

    // Verify counts
    assert_eq!(gameobjects.len(), 100);
    assert_eq!(transforms.len(), 100);
    assert_eq!(monobehaviours.len(), 34); // Every 3rd entry from 0-99
    assert_eq!(all_with_position.len(), 100); // All transforms have m_LocalPosition

    // Performance should be reasonable (less than 10ms for this size)
    assert!(
        duration.as_millis() < 100,
        "Filtering took too long: {:?}",
        duration
    );
}
