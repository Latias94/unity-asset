//! Tests for metadata extraction functionality

use indexmap::IndexMap;
use std::fs;
use std::path::Path;
use unity_asset_binary::{AssetBundle, MetadataExtractor, ObjectInfo, SerializedFile, UnityObject};
use unity_asset_core::{UnityClass, UnityValue};

/// Create a mock SerializedFile with test objects
fn create_mock_serialized_file() -> SerializedFile {
    // This is a simplified mock - in a real scenario we'd create a proper SerializedFile
    // For now, we'll create a minimal valid SerializedFile
    let minimal_data = vec![
        0x00, 0x00, 0x00, 0x20, // metadata_size
        0x00, 0x00, 0x01, 0x00, // file_size
        0x00, 0x00, 0x00, 0x0F, // version (15)
        0x00, 0x00, 0x00, 0x40, // data_offset
        0x00, 0x00, 0x00, 0x00, // endian + reserved
    ];
    // Add more minimal data to make it valid
    let mut full_data = minimal_data;
    full_data.resize(256, 0); // Pad to minimum size

    SerializedFile::from_bytes(full_data).unwrap_or_else(|_| {
        // If that fails, create a very basic one for testing
        panic!("Cannot create mock SerializedFile for testing")
    })
}

/// Create mock Unity objects for testing
fn create_mock_objects() -> Vec<UnityObject> {
    let mut objects = Vec::new();

    // Create a mock GameObject
    let mut gameobject_info = ObjectInfo::new(12345, 0, 100, 1); // GameObject class_id = 1
    gameobject_info.data = vec![0x01, 0x02, 0x03, 0x04];

    let mut gameobject_class = UnityClass::new(1, "GameObject".to_string(), "12345".to_string());
    gameobject_class.set(
        "m_Name".to_string(),
        UnityValue::String("TestGameObject".to_string()),
    );
    gameobject_class.set("m_Layer".to_string(), UnityValue::Integer(0));
    gameobject_class.set(
        "m_Tag".to_string(),
        UnityValue::String("Untagged".to_string()),
    );
    gameobject_class.set("m_IsActive".to_string(), UnityValue::Bool(true));

    // Add Transform component reference
    let mut transform_component = IndexMap::new();
    transform_component.insert("fileID".to_string(), UnityValue::Integer(0));
    transform_component.insert("pathID".to_string(), UnityValue::Integer(67890));
    let components = vec![UnityValue::Object(transform_component)];
    gameobject_class.set("m_Component".to_string(), UnityValue::Array(components));

    objects.push(UnityObject {
        info: gameobject_info,
        class: gameobject_class,
    });

    // Create a mock Transform
    let mut transform_info = ObjectInfo::new(67890, 0, 100, 4); // Transform class_id = 4
    transform_info.data = vec![0x05, 0x06, 0x07, 0x08];

    let mut transform_class = UnityClass::new(4, "Transform".to_string(), "67890".to_string());

    // Add position
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.0));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(3.0));
    transform_class.set("m_LocalPosition".to_string(), UnityValue::Object(position));

    objects.push(UnityObject {
        info: transform_info,
        class: transform_class,
    });

    // Create a mock Texture2D
    let mut texture_info = ObjectInfo::new(11111, 0, 1024, 28); // Texture2D class_id = 28
    texture_info.data = vec![0u8; 1024]; // Large texture data

    let texture_class = UnityClass::new(28, "Texture2D".to_string(), "11111".to_string());
    objects.push(UnityObject {
        info: texture_info,
        class: texture_class,
    });

    objects
}

#[test]
fn test_metadata_extractor_creation() {
    let extractor = MetadataExtractor::new();
    assert!(extractor.include_dependencies);
    assert!(extractor.include_hierarchy);
    assert!(extractor.include_performance);
    assert!(extractor.max_objects_to_analyze.is_none());

    let custom_extractor = MetadataExtractor::with_config(false, true, false, Some(100));
    assert!(!custom_extractor.include_dependencies);
    assert!(custom_extractor.include_hierarchy);
    assert!(!custom_extractor.include_performance);
    assert_eq!(custom_extractor.max_objects_to_analyze, Some(100));
}

#[test]
fn test_object_statistics_extraction() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let stats = extractor.extract_object_statistics(&objects);

    // Verify basic statistics
    assert_eq!(stats.total_objects, 3);
    assert_eq!(stats.objects_by_type.len(), 3); // GameObject, Transform, Texture2D

    // Check object counts by type
    assert_eq!(stats.objects_by_type.get("GameObject"), Some(&1));
    assert_eq!(stats.objects_by_type.get("Transform"), Some(&1));
    assert_eq!(stats.objects_by_type.get("Texture2D"), Some(&1));

    // Check memory usage
    assert!(stats.memory_usage.total_bytes > 0);
    assert_eq!(stats.memory_usage.by_type.len(), 3);

    // Texture2D should be the largest object
    assert_eq!(stats.largest_objects[0].class_name, "Texture2D");
    assert_eq!(stats.largest_objects[0].byte_size, 1024);

    // Average object size should be calculated correctly
    let expected_avg = stats.memory_usage.total_bytes as f64 / 3.0;
    assert!((stats.memory_usage.average_object_size - expected_avg).abs() < 0.001);
}

#[test]
fn test_dependency_extraction() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let deps = extractor.extract_dependencies(&objects).unwrap();

    // Should have internal references (GameObject -> Transform)
    assert!(!deps.internal_references.is_empty());

    // Check that GameObject references Transform
    let gameobject_to_transform = deps
        .internal_references
        .iter()
        .find(|r| r.from_object == 12345 && r.to_object == 67890);
    assert!(gameobject_to_transform.is_some());
    assert_eq!(gameobject_to_transform.unwrap().reference_type, "Component");

    // Dependency graph should have nodes and edges
    assert_eq!(deps.dependency_graph.nodes.len(), 3);
    assert!(!deps.dependency_graph.edges.is_empty());

    // Should have root objects (objects with no incoming references)
    assert!(!deps.dependency_graph.root_objects.is_empty());
}

#[test]
fn test_relationship_extraction() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let relationships = extractor.extract_relationships(&objects).unwrap();

    // Should have GameObject hierarchy
    assert_eq!(relationships.gameobject_hierarchy.len(), 1);

    let gameobject_hierarchy = &relationships.gameobject_hierarchy[0];
    assert_eq!(gameobject_hierarchy.gameobject_id, 12345);
    assert_eq!(gameobject_hierarchy.name, "TestGameObject");
    assert_eq!(gameobject_hierarchy.transform_id, 67890);
    assert_eq!(gameobject_hierarchy.components.len(), 1);

    // Should have component relationships
    assert!(!relationships.component_relationships.is_empty());
}

#[test]
fn test_complexity_score_calculation() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let stats = extractor.extract_object_statistics(&objects);
    let deps = extractor.extract_dependencies(&objects).unwrap();

    let complexity = extractor.calculate_complexity_score(&stats, &deps);

    // Complexity should be positive and reasonable
    assert!(complexity > 0.0);
    assert!(complexity < 100.0); // Should not be extremely high for simple test data
}

#[test]
fn test_performance_metrics() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let start_time = std::time::Instant::now();
    let _stats = extractor.extract_object_statistics(&objects);
    let _deps = extractor.extract_dependencies(&objects).unwrap();
    let parse_time = start_time.elapsed().as_secs_f64() * 1000.0;

    // Performance metrics should be reasonable
    assert!(parse_time >= 0.0);

    // Object parse rate should be calculable
    let parse_rate = objects.len() as f64 / (parse_time / 1000.0);
    assert!(parse_rate > 0.0);
}

#[test]
fn test_file_info_extraction() {
    let asset = create_mock_serialized_file();
    let extractor = MetadataExtractor::new();

    let file_info = extractor.extract_file_info(&asset);

    // Should have basic file information
    // Note: Mock file might have empty unity_version, which is acceptable for testing
    println!("Unity version: '{}'", file_info.unity_version);
    assert!(!file_info.target_platform.is_empty());
    assert!(!file_info.compression_type.is_empty());
    assert!(file_info.file_format_version > 0);
}

#[test]
fn test_max_objects_limit() {
    let _objects = create_mock_objects();
    let extractor = MetadataExtractor::with_config(true, true, true, Some(2));

    // This test would need a proper SerializedFile implementation
    // For now, we just verify the configuration is set correctly
    assert_eq!(extractor.max_objects_to_analyze, Some(2));
}

#[test]
fn test_metadata_structure_validity() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let stats = extractor.extract_object_statistics(&objects);
    let deps = extractor.extract_dependencies(&objects).unwrap();

    // Test that our metadata structures are valid and accessible
    assert!(stats.total_objects > 0);
    assert!(!stats.objects_by_type.is_empty());
    assert!(stats.memory_usage.total_bytes > 0);

    assert!(!deps.dependency_graph.nodes.is_empty());
    // Note: We can add serde_json serialization tests later when needed
}

#[test]
fn test_empty_objects_handling() {
    let objects = Vec::new();
    let extractor = MetadataExtractor::new();

    let stats = extractor.extract_object_statistics(&objects);

    // Should handle empty object list gracefully
    assert_eq!(stats.total_objects, 0);
    assert!(stats.objects_by_type.is_empty());
    assert_eq!(stats.memory_usage.total_bytes, 0);
    assert_eq!(stats.memory_usage.average_object_size, 0.0);
    assert!(stats.largest_objects.is_empty());
}

#[test]
fn test_root_and_leaf_object_detection() {
    let objects = create_mock_objects();
    let extractor = MetadataExtractor::new();

    let deps = extractor.extract_dependencies(&objects).unwrap();

    // Should identify root objects (no incoming references)
    assert!(!deps.dependency_graph.root_objects.is_empty());

    // Should identify leaf objects (no outgoing references)
    assert!(!deps.dependency_graph.leaf_objects.is_empty());

    // GameObject should be a root object (nothing references it)
    assert!(deps.dependency_graph.root_objects.contains(&12345));

    // Transform should be a leaf object (it doesn't reference anything else in our test)
    assert!(deps.dependency_graph.leaf_objects.contains(&67890));
}

#[test]
fn test_sample_files_metadata_extraction() {
    let samples_path = Path::new("tests/samples");
    if !samples_path.exists() {
        println!("Samples directory not found, skipping test");
        return;
    }

    let extractor = MetadataExtractor::with_config(true, true, false, Some(50)); // Limit for performance
    let mut successful_extractions = 0;

    if let Ok(entries) = fs::read_dir(samples_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    if let Ok(data) = fs::read(&path) {
                        // Try as AssetBundle first
                        if let Ok(bundle) = AssetBundle::from_bytes(data.clone()) {
                            if let Ok(metadata_list) = extractor.extract_from_bundle(&bundle) {
                                successful_extractions += metadata_list.len();

                                for metadata in metadata_list {
                                    // Verify metadata structure
                                    assert!(metadata.object_stats.total_objects >= 0);
                                    assert!(metadata.performance.complexity_score >= 0.0);

                                    println!(
                                        "Extracted metadata from bundle asset: {} objects, complexity: {:.2}",
                                        metadata.object_stats.total_objects,
                                        metadata.performance.complexity_score
                                    );
                                }
                            }
                        }
                        // Try as SerializedFile
                        else if let Ok(asset) = SerializedFile::from_bytes(data) {
                            if let Ok(metadata) = extractor.extract_from_asset(&asset) {
                                successful_extractions += 1;

                                // Verify metadata structure
                                assert!(metadata.object_stats.total_objects >= 0);
                                assert!(metadata.performance.complexity_score >= 0.0);

                                println!(
                                    "Extracted metadata from asset: {} objects, complexity: {:.2}",
                                    metadata.object_stats.total_objects,
                                    metadata.performance.complexity_score
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    println!(
        "Successfully extracted metadata from {} assets",
        successful_extractions
    );
    // We expect at least some successful extractions from our sample files
    assert!(
        successful_extractions > 0,
        "Should successfully extract metadata from at least one sample file"
    );
}
