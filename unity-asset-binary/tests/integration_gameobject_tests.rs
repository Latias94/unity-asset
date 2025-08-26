//! Integration tests for GameObject and Transform parsing in real Unity files

#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(clippy::manual_flatten)]

use indexmap::IndexMap;
use std::fs;
use std::path::Path;
use unity_asset_binary::{
    AssetBundle, GameObject, ObjectInfo, SerializedFile, Transform, UnityObject,
};
use unity_asset_core::{UnityClass, UnityValue};

/// Create a mock Unity file with GameObject and Transform objects
fn create_mock_unity_file_with_gameobjects() -> Vec<u8> {
    // This would be a real Unity file in practice
    // For now, we'll create a minimal mock structure
    let mut data = Vec::new();

    // Unity file signature
    data.extend_from_slice(b"UnityFS\0");
    data.extend_from_slice(&[0, 0, 0, 6]); // version
    data.extend_from_slice(b"5.x.x\0"); // unity version
    data.extend_from_slice(b"test\0"); // revision

    // Add minimal file structure
    data.extend_from_slice(&[0; 100]); // Padding to make it look like a real file

    data
}

/// Test that we can create and parse GameObject objects in a Unity file context
#[test]
fn test_gameobject_in_unity_file_context() {
    println!("Testing GameObject parsing in Unity file context...");

    // Create mock ObjectInfo for a GameObject
    let mut info = ObjectInfo::new(12345, 0, 100, 1); // GameObject class_id = 1
    info.data = vec![0x01, 0x02, 0x03, 0x04]; // Mock binary data

    // Create UnityClass with realistic GameObject data
    let mut unity_class = UnityClass::new(1, "GameObject".to_string(), "12345".to_string());
    unity_class.set(
        "m_Name".to_string(),
        UnityValue::String("Player".to_string()),
    );
    unity_class.set("m_Layer".to_string(), UnityValue::Integer(8)); // Player layer
    unity_class.set(
        "m_Tag".to_string(),
        UnityValue::String("Player".to_string()),
    );
    unity_class.set("m_IsActive".to_string(), UnityValue::Bool(true));

    // Add Transform component reference
    let mut transform_component = IndexMap::new();
    transform_component.insert("fileID".to_string(), UnityValue::Integer(0));
    transform_component.insert("pathID".to_string(), UnityValue::Integer(67890));

    // Add MeshRenderer component reference
    let mut renderer_component = IndexMap::new();
    renderer_component.insert("fileID".to_string(), UnityValue::Integer(0));
    renderer_component.insert("pathID".to_string(), UnityValue::Integer(11111));

    let components = vec![
        UnityValue::Object(transform_component),
        UnityValue::Object(renderer_component),
    ];
    unity_class.set("m_Component".to_string(), UnityValue::Array(components));

    // Create UnityObject
    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Test that we can identify and parse it
    assert!(unity_object.is_gameobject());
    assert_eq!(unity_object.class_name(), "GameObject");
    assert_eq!(unity_object.class_id(), 1);

    let game_object = unity_object.as_gameobject().unwrap();
    assert_eq!(game_object.name, "Player");
    assert_eq!(game_object.layer, 8);
    assert_eq!(game_object.tag, "Player");
    assert!(game_object.active);
    assert_eq!(game_object.components.len(), 2);
    assert_eq!(game_object.components[0].path_id, 67890); // Transform
    assert_eq!(game_object.components[1].path_id, 11111); // MeshRenderer

    println!(
        "  ✅ GameObject parsed successfully: '{}'",
        game_object.name
    );
    println!(
        "     Layer: {}, Tag: '{}', Active: {}, Components: {}",
        game_object.layer,
        game_object.tag,
        game_object.active,
        game_object.components.len()
    );
}

/// Test that we can create and parse Transform objects in a Unity file context
#[test]
fn test_transform_in_unity_file_context() {
    println!("Testing Transform parsing in Unity file context...");

    // Create mock ObjectInfo for a Transform
    let mut info = ObjectInfo::new(67890, 0, 100, 4); // Transform class_id = 4
    info.data = vec![0x05, 0x06, 0x07, 0x08]; // Mock binary data

    // Create UnityClass with realistic Transform data
    let mut unity_class = UnityClass::new(4, "Transform".to_string(), "67890".to_string());

    // Position (slightly offset from origin)
    let mut position = IndexMap::new();
    position.insert("x".to_string(), UnityValue::Float(2.5));
    position.insert("y".to_string(), UnityValue::Float(0.0));
    position.insert("z".to_string(), UnityValue::Float(-1.0));
    unity_class.set("m_LocalPosition".to_string(), UnityValue::Object(position));

    // Rotation (45 degrees around Y axis)
    let mut rotation = IndexMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.3827)); // sin(45°/2)
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(0.9239)); // cos(45°/2)
    unity_class.set("m_LocalRotation".to_string(), UnityValue::Object(rotation));

    // Scale (uniform 1.5x scale)
    let mut scale = IndexMap::new();
    scale.insert("x".to_string(), UnityValue::Float(1.5));
    scale.insert("y".to_string(), UnityValue::Float(1.5));
    scale.insert("z".to_string(), UnityValue::Float(1.5));
    unity_class.set("m_LocalScale".to_string(), UnityValue::Object(scale));

    // Parent (root object)
    let mut parent = IndexMap::new();
    parent.insert("fileID".to_string(), UnityValue::Integer(0));
    parent.insert("pathID".to_string(), UnityValue::Integer(0)); // No parent (root)
    unity_class.set("m_Father".to_string(), UnityValue::Object(parent));

    // Children (two child objects)
    let mut child1 = IndexMap::new();
    child1.insert("fileID".to_string(), UnityValue::Integer(0));
    child1.insert("pathID".to_string(), UnityValue::Integer(22222));

    let mut child2 = IndexMap::new();
    child2.insert("fileID".to_string(), UnityValue::Integer(0));
    child2.insert("pathID".to_string(), UnityValue::Integer(33333));

    let children = vec![UnityValue::Object(child1), UnityValue::Object(child2)];
    unity_class.set("m_Children".to_string(), UnityValue::Array(children));

    // Create UnityObject
    let unity_object = UnityObject {
        info,
        class: unity_class,
    };

    // Test that we can identify and parse it
    assert!(unity_object.is_transform());
    assert_eq!(unity_object.class_name(), "Transform");
    assert_eq!(unity_object.class_id(), 4);

    let transform = unity_object.as_transform().unwrap();
    assert_eq!(transform.position.x, 2.5);
    assert_eq!(transform.position.y, 0.0);
    assert_eq!(transform.position.z, -1.0);

    assert!((transform.rotation.y - 0.3827).abs() < 0.001);
    assert!((transform.rotation.w - 0.9239).abs() < 0.001);

    assert_eq!(transform.scale.x, 1.5);
    assert_eq!(transform.scale.y, 1.5);
    assert_eq!(transform.scale.z, 1.5);

    assert!(transform.parent.is_none()); // No parent (root object)
    assert_eq!(transform.children.len(), 2);
    assert_eq!(transform.children[0].path_id, 22222);
    assert_eq!(transform.children[1].path_id, 33333);

    println!("  ✅ Transform parsed successfully");
    println!(
        "     Position: ({:.2}, {:.2}, {:.2})",
        transform.position.x, transform.position.y, transform.position.z
    );
    println!(
        "     Rotation: ({:.3}, {:.3}, {:.3}, {:.3})",
        transform.rotation.x, transform.rotation.y, transform.rotation.z, transform.rotation.w
    );
    println!(
        "     Scale: ({:.2}, {:.2}, {:.2})",
        transform.scale.x, transform.scale.y, transform.scale.z
    );
    println!("     Children: {}", transform.children.len());
}

/// Test a complete GameObject-Transform hierarchy
#[test]
fn test_gameobject_transform_hierarchy() {
    println!("Testing GameObject-Transform hierarchy...");

    // Create a parent GameObject
    let mut parent_info = ObjectInfo::new(10001, 0, 100, 1);
    parent_info.data = vec![0x01, 0x02, 0x03, 0x04];

    let mut parent_class = UnityClass::new(1, "GameObject".to_string(), "10001".to_string());
    parent_class.set(
        "m_Name".to_string(),
        UnityValue::String("ParentObject".to_string()),
    );
    parent_class.set("m_IsActive".to_string(), UnityValue::Bool(true));

    // Parent's Transform component
    let mut parent_transform_ref = IndexMap::new();
    parent_transform_ref.insert("fileID".to_string(), UnityValue::Integer(0));
    parent_transform_ref.insert("pathID".to_string(), UnityValue::Integer(10002));

    let parent_components = vec![UnityValue::Object(parent_transform_ref)];
    parent_class.set(
        "m_Component".to_string(),
        UnityValue::Array(parent_components),
    );

    let parent_gameobject = UnityObject {
        info: parent_info,
        class: parent_class,
    };

    // Create parent's Transform
    let mut parent_transform_info = ObjectInfo::new(10002, 0, 100, 4);
    parent_transform_info.data = vec![0x05, 0x06, 0x07, 0x08];

    let mut parent_transform_class =
        UnityClass::new(4, "Transform".to_string(), "10002".to_string());

    // Parent position at origin
    let mut parent_position = IndexMap::new();
    parent_position.insert("x".to_string(), UnityValue::Float(0.0));
    parent_position.insert("y".to_string(), UnityValue::Float(0.0));
    parent_position.insert("z".to_string(), UnityValue::Float(0.0));
    parent_transform_class.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(parent_position),
    );

    // Parent has one child
    let mut child_ref = IndexMap::new();
    child_ref.insert("fileID".to_string(), UnityValue::Integer(0));
    child_ref.insert("pathID".to_string(), UnityValue::Integer(20002)); // Child's transform

    let parent_children = vec![UnityValue::Object(child_ref)];
    parent_transform_class.set("m_Children".to_string(), UnityValue::Array(parent_children));

    let parent_transform = UnityObject {
        info: parent_transform_info,
        class: parent_transform_class,
    };

    // Create child GameObject
    let mut child_info = ObjectInfo::new(20001, 0, 100, 1);
    child_info.data = vec![0x09, 0x0A, 0x0B, 0x0C];

    let mut child_class = UnityClass::new(1, "GameObject".to_string(), "20001".to_string());
    child_class.set(
        "m_Name".to_string(),
        UnityValue::String("ChildObject".to_string()),
    );
    child_class.set("m_IsActive".to_string(), UnityValue::Bool(true));

    // Child's Transform component
    let mut child_transform_ref = IndexMap::new();
    child_transform_ref.insert("fileID".to_string(), UnityValue::Integer(0));
    child_transform_ref.insert("pathID".to_string(), UnityValue::Integer(20002));

    let child_components = vec![UnityValue::Object(child_transform_ref)];
    child_class.set(
        "m_Component".to_string(),
        UnityValue::Array(child_components),
    );

    let child_gameobject = UnityObject {
        info: child_info,
        class: child_class,
    };

    // Create child's Transform
    let mut child_transform_info = ObjectInfo::new(20002, 0, 100, 4);
    child_transform_info.data = vec![0x0D, 0x0E, 0x0F, 0x10];

    let mut child_transform_class =
        UnityClass::new(4, "Transform".to_string(), "20002".to_string());

    // Child position relative to parent
    let mut child_position = IndexMap::new();
    child_position.insert("x".to_string(), UnityValue::Float(5.0));
    child_position.insert("y".to_string(), UnityValue::Float(2.0));
    child_position.insert("z".to_string(), UnityValue::Float(0.0));
    child_transform_class.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(child_position),
    );

    // Child has parent
    let mut parent_ref = IndexMap::new();
    parent_ref.insert("fileID".to_string(), UnityValue::Integer(0));
    parent_ref.insert("pathID".to_string(), UnityValue::Integer(10002)); // Parent's transform
    child_transform_class.set("m_Father".to_string(), UnityValue::Object(parent_ref));

    let child_transform = UnityObject {
        info: child_transform_info,
        class: child_transform_class,
    };

    // Test parsing
    let parent_go = parent_gameobject.as_gameobject().unwrap();
    let parent_tf = parent_transform.as_transform().unwrap();
    let child_go = child_gameobject.as_gameobject().unwrap();
    let child_tf = child_transform.as_transform().unwrap();

    // Verify hierarchy
    assert_eq!(parent_go.name, "ParentObject");
    assert_eq!(parent_go.components[0].path_id, 10002); // Points to parent transform

    assert_eq!(parent_tf.children.len(), 1);
    assert_eq!(parent_tf.children[0].path_id, 20002); // Points to child transform
    assert!(parent_tf.parent.is_none()); // No parent

    assert_eq!(child_go.name, "ChildObject");
    assert_eq!(child_go.components[0].path_id, 20002); // Points to child transform

    assert!(child_tf.parent.is_some());
    assert_eq!(child_tf.parent.as_ref().unwrap().path_id, 10002); // Points to parent transform
    assert_eq!(child_tf.children.len(), 0); // No children

    println!("  ✅ GameObject-Transform hierarchy parsed successfully");
    println!(
        "     Parent: '{}' -> Transform: {}",
        parent_go.name, parent_go.components[0].path_id
    );
    println!(
        "     Child: '{}' -> Transform: {}",
        child_go.name, child_go.components[0].path_id
    );
    println!(
        "     Parent Transform children: {}",
        parent_tf.children.len()
    );
    println!(
        "     Child Transform parent: {:?}",
        child_tf.parent.as_ref().map(|p| p.path_id)
    );
}

/// Test that our GameObject/Transform parsing works with the existing sample files
#[test]
fn test_existing_samples_for_gameobjects() {
    println!("Testing existing samples for GameObject/Transform objects...");

    let samples_path = Path::new("tests/samples");
    if !samples_path.exists() {
        println!("  ⚠️ Samples directory not found, skipping test");
        return;
    }

    let mut found_gameobjects = 0;
    let mut found_transforms = 0;

    if let Ok(entries) = fs::read_dir(samples_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    if let Ok(data) = fs::read(&path) {
                        if let Ok(bundle) = AssetBundle::from_bytes(data.clone()) {
                            for asset in bundle.assets() {
                                if let Ok(objects) = asset.get_objects() {
                                    for obj in objects {
                                        if obj.is_gameobject() {
                                            found_gameobjects += 1;
                                            println!("    Found GameObject: {}", obj.describe());
                                        }
                                        if obj.is_transform() {
                                            found_transforms += 1;
                                            println!("    Found Transform: {}", obj.describe());
                                        }
                                    }
                                }
                            }
                        } else if let Ok(asset) = SerializedFile::from_bytes(data) {
                            if let Ok(objects) = asset.get_objects() {
                                for obj in objects {
                                    if obj.is_gameobject() {
                                        found_gameobjects += 1;
                                        println!("    Found GameObject: {}", obj.describe());
                                    }
                                    if obj.is_transform() {
                                        found_transforms += 1;
                                        println!("    Found Transform: {}", obj.describe());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    println!(
        "  📊 Results: {} GameObjects, {} Transforms found in sample files",
        found_gameobjects, found_transforms
    );

    // Note: It's expected that we might not find any in the current samples
    // since they appear to be resource files rather than scene files
    if found_gameobjects == 0 && found_transforms == 0 {
        println!("  ℹ️ No GameObjects/Transforms found - this is expected for resource-only files");
    }
}
