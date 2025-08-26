//! Unity YAML serialization demonstration
//!
//! This example shows how to:
//! 1. Load Unity YAML files
//! 2. Modify the data
//! 3. Save back to YAML format
//! 4. Demonstrate round-trip compatibility

use std::collections::HashMap;
use unity_asset_core::{UnityClass, UnityDocument, UnityValue};
use unity_asset_yaml::{SerdeUnityLoader, UnityYamlSerializer, YamlDocument};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Unity YAML Serialization Demo");
    println!("=================================\n");

    // Demo 1: Create and serialize a simple GameObject
    demo_create_and_serialize()?;

    // Demo 2: Load, modify, and save a YAML document
    demo_load_modify_save()?;

    // Demo 3: Round-trip test with complex data
    demo_round_trip_test()?;

    println!("✅ All serialization demos completed successfully!");
    Ok(())
}

/// Demo 1: Create a Unity GameObject from scratch and serialize it
fn demo_create_and_serialize() -> Result<(), Box<dyn std::error::Error>> {
    println!("📝 Demo 1: Creating and serializing a GameObject");
    println!("------------------------------------------------");

    // Create a new GameObject
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());

    // Add properties
    gameobject.set("m_ObjectHideFlags".to_string(), UnityValue::Integer(0));
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("MyGameObject".to_string()),
    );
    gameobject.set(
        "m_TagString".to_string(),
        UnityValue::String("Untagged".to_string()),
    );
    gameobject.set("m_Layer".to_string(), UnityValue::Integer(0));
    gameobject.set("m_IsActive".to_string(), UnityValue::Bool(true));

    // Create component array
    let mut components = Vec::new();
    let mut transform_ref = HashMap::new();
    transform_ref.insert("fileID".to_string(), UnityValue::Integer(987654321));
    components.push(UnityValue::Object(transform_ref.into_iter().collect()));
    gameobject.set("m_Component".to_string(), UnityValue::Array(components));

    // Serialize to YAML
    let mut serializer = UnityYamlSerializer::new();
    let yaml_output = serializer.serialize_to_string(&[gameobject])?;

    println!("Generated YAML:");
    println!("{}", yaml_output);
    println!();

    Ok(())
}

/// Demo 2: Load a YAML document, modify it, and save it back
fn demo_load_modify_save() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 Demo 2: Load, modify, and save workflow");
    println!("------------------------------------------");

    // Create sample YAML content
    let sample_yaml = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &123456789
GameObject:
  m_ObjectHideFlags: 0
  m_Name: OriginalName
  m_TagString: Untagged
  m_Layer: 0
  m_IsActive: 1
  m_Component:
  - {fileID: 987654321}
"#;

    println!("Original YAML:");
    println!("{}", sample_yaml);

    // Load the YAML
    let loader = SerdeUnityLoader::new();
    let mut classes = loader.load_from_str(sample_yaml)?;

    println!("Loaded {} Unity classes", classes.len());

    // Modify the first class
    if let Some(gameobject) = classes.get_mut(0) {
        println!("Original name: {:?}", gameobject.get("m_Name"));

        // Change the name
        gameobject.set(
            "m_Name".to_string(),
            UnityValue::String("ModifiedName".to_string()),
        );

        // Add a new property
        gameobject.set(
            "m_CustomProperty".to_string(),
            UnityValue::String("Added by Rust!".to_string()),
        );

        println!("Modified name: {:?}", gameobject.get("m_Name"));
        println!(
            "Added custom property: {:?}",
            gameobject.get("m_CustomProperty")
        );
    }

    // Serialize back to YAML
    let mut serializer = UnityYamlSerializer::new();
    let modified_yaml = serializer.serialize_to_string(&classes)?;

    println!("\nModified YAML:");
    println!("{}", modified_yaml);

    Ok(())
}

/// Demo 3: Round-trip test to ensure data integrity
fn demo_round_trip_test() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 Demo 3: Round-trip integrity test");
    println!("------------------------------------");

    // Create complex test data
    let mut transform = UnityClass::new(4, "Transform".to_string(), "987654321".to_string());

    // Add complex nested data
    let mut position = HashMap::new();
    position.insert("x".to_string(), UnityValue::Float(1.5));
    position.insert("y".to_string(), UnityValue::Float(2.0));
    position.insert("z".to_string(), UnityValue::Float(-0.5));
    transform.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(position.into_iter().collect()),
    );

    let mut rotation = HashMap::new();
    rotation.insert("x".to_string(), UnityValue::Float(0.0));
    rotation.insert("y".to_string(), UnityValue::Float(0.0));
    rotation.insert("z".to_string(), UnityValue::Float(0.0));
    rotation.insert("w".to_string(), UnityValue::Float(1.0));
    transform.set(
        "m_LocalRotation".to_string(),
        UnityValue::Object(rotation.into_iter().collect()),
    );

    let mut scale = HashMap::new();
    scale.insert("x".to_string(), UnityValue::Float(1.0));
    scale.insert("y".to_string(), UnityValue::Float(1.0));
    scale.insert("z".to_string(), UnityValue::Float(1.0));
    transform.set(
        "m_LocalScale".to_string(),
        UnityValue::Object(scale.into_iter().collect()),
    );

    // Add array data
    let children = vec![
        UnityValue::Integer(111111),
        UnityValue::Integer(222222),
        UnityValue::Integer(333333),
    ];
    transform.set("m_Children".to_string(), UnityValue::Array(children));

    let original_classes = vec![transform];

    println!("Original data:");
    for (i, class) in original_classes.iter().enumerate() {
        println!(
            "  [{}]: {} (ID: {}, Anchor: {})",
            i, class.class_name, class.class_id, class.anchor
        );
        println!("       Properties: {}", class.properties().len());
    }

    // Serialize to YAML
    let mut serializer = UnityYamlSerializer::new();
    let yaml_content = serializer.serialize_to_string(&original_classes)?;

    println!("\nSerialized YAML ({} bytes):", yaml_content.len());
    println!("{}", yaml_content);

    // Parse back from YAML
    let loader = SerdeUnityLoader::new();
    let parsed_classes = loader.load_from_str(&yaml_content)?;

    println!("Parsed back data:");
    for (i, class) in parsed_classes.iter().enumerate() {
        println!(
            "  [{}]: {} (ID: {}, Anchor: {})",
            i, class.class_name, class.class_id, class.anchor
        );
        println!("       Properties: {}", class.properties().len());
    }

    // Verify data integrity
    println!("\n🔍 Data integrity check:");

    if original_classes.len() == parsed_classes.len() {
        println!("✅ Class count matches: {}", original_classes.len());
    } else {
        println!(
            "❌ Class count mismatch: {} vs {}",
            original_classes.len(),
            parsed_classes.len()
        );
    }

    for (i, (original, parsed)) in original_classes
        .iter()
        .zip(parsed_classes.iter())
        .enumerate()
    {
        println!("  Class [{}]:", i);

        if original.class_name == parsed.class_name {
            println!("    ✅ Class name matches: {}", original.class_name);
        } else {
            println!(
                "    ❌ Class name mismatch: {} vs {}",
                original.class_name, parsed.class_name
            );
        }

        if original.class_id == parsed.class_id {
            println!("    ✅ Class ID matches: {}", original.class_id);
        } else {
            println!(
                "    ❌ Class ID mismatch: {} vs {}",
                original.class_id, parsed.class_id
            );
        }

        if original.anchor == parsed.anchor {
            println!("    ✅ Anchor matches: {}", original.anchor);
        } else {
            println!(
                "    ❌ Anchor mismatch: {} vs {}",
                original.anchor, parsed.anchor
            );
        }

        let original_props = original.properties().len();
        let parsed_props = parsed.properties().len();
        if original_props == parsed_props {
            println!("    ✅ Property count matches: {}", original_props);
        } else {
            println!(
                "    ❌ Property count mismatch: {} vs {}",
                original_props, parsed_props
            );
        }
    }

    println!("\n🎯 Round-trip test completed!");

    Ok(())
}

/// Demo 4: YamlDocument save/load workflow
#[allow(dead_code)]
fn demo_yaml_document_workflow() -> Result<(), Box<dyn std::error::Error>> {
    println!("📄 Demo 4: YamlDocument save/load workflow");
    println!("--------------------------------------------");

    // Create a YamlDocument
    let mut doc = YamlDocument::new();

    // Add some Unity classes
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("TestObject".to_string()),
    );
    doc.add_entry(gameobject);

    let mut transform = UnityClass::new(4, "Transform".to_string(), "456".to_string());
    let mut pos = HashMap::new();
    pos.insert("x".to_string(), UnityValue::Float(0.0));
    pos.insert("y".to_string(), UnityValue::Float(0.0));
    pos.insert("z".to_string(), UnityValue::Float(0.0));
    transform.set(
        "m_LocalPosition".to_string(),
        UnityValue::Object(pos.into_iter().collect()),
    );
    doc.add_entry(transform);

    // Get YAML content
    let yaml_content = doc.dump_yaml()?;
    println!("Generated YAML content:");
    println!("{}", yaml_content);

    // Save to file (commented out to avoid creating files in demo)
    // doc.save_to("test_output.yaml")?;
    // println!("✅ Saved to test_output.yaml");

    Ok(())
}
