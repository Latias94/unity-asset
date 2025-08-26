//! Demonstration of the serde-based Unity YAML loader
//!
//! This example shows how the improved serde-based loader handles complex Unity YAML files.

use unity_asset_core::UnityValue;
use unity_asset_yaml::serde_unity_loader::SerdeUnityLoader;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Serde-based Unity YAML Loader Demo");
    println!("===================================\n");

    let loader = SerdeUnityLoader::new();

    // Test 1: Simple Unity GameObject
    println!("Test 1: Simple Unity GameObject");
    println!("--------------------------------");

    let yaml1 = r#"
GameObject:
  m_ObjectHideFlags: 0
  m_Name: Player
  m_TagString: Player
  m_Layer: 0
  m_IsActive: 1
"#;

    println!("YAML:");
    println!("{}", yaml1);

    match loader.load_from_str(yaml1) {
        Ok(classes) => {
            println!("✓ Successfully loaded {} Unity classes", classes.len());
            for class in &classes {
                println!(
                    "  Class: {} (ID: {}, Anchor: {})",
                    class.class_name, class.class_id, class.anchor
                );

                if let Some(UnityValue::String(name)) = class.get("m_Name") {
                    println!("    Name: {}", name);
                }
                if let Some(UnityValue::String(tag)) = class.get("m_TagString") {
                    println!("    Tag: {}", tag);
                }
            }
        }
        Err(e) => {
            println!("✗ Error: {}", e);
        }
    }

    println!();

    // Test 2: Unity Transform with nested objects
    println!("Test 2: Unity Transform with nested objects");
    println!("--------------------------------------------");

    let yaml2 = r#"
Transform:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 123456789}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalPosition: {x: 1.5, y: 2.0, z: 0}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
"#;

    println!("YAML:");
    println!("{}", yaml2);

    match loader.load_from_str(yaml2) {
        Ok(classes) => {
            println!("✓ Successfully loaded {} Unity classes", classes.len());
            for class in &classes {
                println!(
                    "  Class: {} (ID: {}, Anchor: {})",
                    class.class_name, class.class_id, class.anchor
                );

                // Check nested objects
                if let Some(UnityValue::Object(pos)) = class.get("m_LocalPosition") {
                    println!("    Position:");
                    if let Some(UnityValue::Float(x)) = pos.get("x") {
                        println!("      x: {}", x);
                    }
                    if let Some(UnityValue::Float(y)) = pos.get("y") {
                        println!("      y: {}", y);
                    }
                    if let Some(UnityValue::Float(z)) = pos.get("z") {
                        println!("      z: {}", z);
                    }
                }
            }
        }
        Err(e) => {
            println!("✗ Error: {}", e);
        }
    }

    println!();

    // Test 3: Unity MonoBehaviour with arrays
    println!("Test 3: Unity MonoBehaviour with arrays");
    println!("---------------------------------------");

    let yaml3 = r#"
MonoBehaviour:
  m_ObjectHideFlags: 0
  m_GameObject: {fileID: 123456789}
  m_Enabled: 1
  m_Script: {fileID: 11500000, guid: abc123def456, type: 3}
  m_Components:
  - {fileID: 111}
  - {fileID: 222}
  - {fileID: 333}
  m_Tags:
  - Player
  - Enemy
  - Collectible
  customValues:
  - 1.0
  - 2.5
  - 3.14
"#;

    println!("YAML:");
    println!("{}", yaml3);

    match loader.load_from_str(yaml3) {
        Ok(classes) => {
            println!("✓ Successfully loaded {} Unity classes", classes.len());
            for class in &classes {
                println!(
                    "  Class: {} (ID: {}, Anchor: {})",
                    class.class_name, class.class_id, class.anchor
                );

                // Check arrays
                if let Some(UnityValue::Array(components)) = class.get("m_Components") {
                    println!("    Components: {} items", components.len());
                }

                if let Some(UnityValue::Array(tags)) = class.get("m_Tags") {
                    println!("    Tags: {} items", tags.len());
                    for (i, tag) in tags.iter().enumerate() {
                        if let UnityValue::String(tag_str) = tag {
                            println!("      [{}]: {}", i, tag_str);
                        }
                    }
                }

                if let Some(UnityValue::Array(values)) = class.get("customValues") {
                    println!("    Custom Values: {} items", values.len());
                    for (i, value) in values.iter().enumerate() {
                        match value {
                            UnityValue::Float(n) => println!("      [{}]: {}", i, n),
                            UnityValue::Integer(n) => println!("      [{}]: {}", i, n),
                            _ => println!("      [{}]: {:?}", i, value),
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("✗ Error: {}", e);
        }
    }

    println!();

    // Test 4: Complex Unity YAML with multiple documents
    println!("Test 4: Multiple documents");
    println!("-------------------------");

    let yaml4 = r#"
---
GameObject:
  m_Name: Object1
  m_IsActive: 1
---
Transform:
  m_LocalPosition: {x: 0, y: 0, z: 0}
---
MonoBehaviour:
  m_Enabled: 1
  customProperty: 42
"#;

    println!("YAML:");
    println!("{}", yaml4);

    match loader.load_from_str(yaml4) {
        Ok(classes) => {
            println!("✓ Successfully loaded {} Unity classes", classes.len());
            for (i, class) in classes.iter().enumerate() {
                println!(
                    "  Document {}: {} (ID: {}, Anchor: {})",
                    i + 1,
                    class.class_name,
                    class.class_id,
                    class.anchor
                );
            }
        }
        Err(e) => {
            println!("✗ Error: {}", e);
        }
    }

    println!();

    // Test 5: Error handling
    println!("Test 5: Error handling");
    println!("----------------------");

    let invalid_yaml = "invalid: yaml: content: [unclosed";
    println!("Invalid YAML: {}", invalid_yaml);

    match loader.load_from_str(invalid_yaml) {
        Ok(classes) => {
            println!("  → Unexpectedly succeeded with {} classes", classes.len());
        }
        Err(e) => {
            println!("  → Error (expected): {}", e);
        }
    }

    println!();
    println!("Serde-based Unity YAML loader demo completed!");
    println!("\nKey advantages of the serde-based approach:");
    println!("  ✓ Uses mature, battle-tested serde_yaml library");
    println!("  ✓ Handles complex YAML structures reliably");
    println!("  ✓ Better error handling and recovery");
    println!("  ✓ Supports all YAML features out of the box");
    println!("  ✓ Easier to maintain and extend");

    Ok(())
}
