//! Basic usage example for unity_asset
//!
//! This example demonstrates the current capabilities of the unity_asset crate,
//! including creating Unity objects, documents, and using the filtering API.

use unity_asset::{UnityClass, YamlDocument, environment::Environment};
use unity_asset_core::UnityDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Unity Asset Parser - Basic Usage Example");
    println!("=========================================\n");

    // Example 1: Creating Unity objects manually
    println!("1. Creating Unity Objects");
    println!("--------------------------");

    let mut game_object = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());
    game_object.set("m_Name".to_string(), "Player");
    game_object.set("m_IsActive".to_string(), true);
    game_object.set("m_Layer".to_string(), 0i32);
    game_object.set("m_Tag".to_string(), "Player");

    println!("Created GameObject: {}", game_object);
    println!("  Name: {:?}", game_object.name());
    println!("  Active: {:?}", game_object.get("m_IsActive"));
    println!("  Layer: {:?}", game_object.get("m_Layer"));
    println!("  Tag: {:?}", game_object.get("m_Tag"));
    println!();

    // Example 2: Creating a Unity document
    println!("2. Working with Unity Documents");
    println!("--------------------------------");

    let mut document = YamlDocument::new();

    // Add the GameObject
    document.add_entry(game_object);

    // Add a MonoBehaviour
    let mut behaviour = UnityClass::new(114, "MonoBehaviour".to_string(), "987654321".to_string());
    behaviour.set("m_Enabled".to_string(), true);
    behaviour.set("m_Script".to_string(), "PlayerController");
    behaviour.set("m_MaxHealth".to_string(), 100i32);
    behaviour.set("m_Speed".to_string(), 5.0f64);
    document.add_entry(behaviour);

    // Add a Transform
    let mut transform = UnityClass::new(4, "Transform".to_string(), "456789123".to_string());
    transform.set("m_LocalPosition".to_string(), "Vector3(0, 0, 0)");
    transform.set("m_LocalRotation".to_string(), "Quaternion(0, 0, 0, 1)");
    transform.set("m_LocalScale".to_string(), "Vector3(1, 1, 1)");
    document.add_entry(transform);

    println!("Document contains {} objects", document.len());
    println!("Main entry: {:?}", document.entry().map(|e| &e.class_name));
    println!();

    // Example 3: Filtering objects
    println!("3. Filtering Objects");
    println!("--------------------");

    let game_objects = document.filter_by_class("GameObject");
    println!("Found {} GameObject(s)", game_objects.len());

    let behaviours = document.filter_by_class("MonoBehaviour");
    println!("Found {} MonoBehaviour(s)", behaviours.len());
    for behaviour in &behaviours {
        if let Some(script) = behaviour.get("m_Script") {
            println!("  Script: {:?}", script);
        }
        if let Some(health) = behaviour.get("m_MaxHealth") {
            println!("  Max Health: {:?}", health);
        }
    }

    let transforms = document.filter_by_class("Transform");
    println!("Found {} Transform(s)", transforms.len());

    // Filter by multiple classes
    let components = document.filter_by_classes(&["MonoBehaviour", "Transform"]);
    println!("Found {} component(s) total", components.len());
    println!();

    // Example 4: Advanced filtering using the new API
    println!("4. Advanced Filtering");
    println!("---------------------");

    // Find objects with m_Enabled property
    let enabled_objects = document.filter(None, Some(&["m_Enabled"]));
    println!(
        "Found {} objects with m_Enabled property",
        enabled_objects.len()
    );

    // Find MonoBehaviour objects
    let monobehaviours = document.filter(Some(&["MonoBehaviour"]), None);
    println!("Found {} MonoBehaviour objects", monobehaviours.len());

    // Find objects with specific properties
    let objects_with_health = document.filter(None, Some(&["m_MaxHealth"]));
    println!(
        "Found {} objects with health property",
        objects_with_health.len()
    );
    println!();

    // Example 5: Environment usage
    println!("5. Environment Management");
    println!("-------------------------");

    let _env = Environment::new();
    // Note: File loading is not yet implemented, so we'll demonstrate the API
    println!("Environment created (file loading not yet implemented)");
    println!("Future usage:");
    println!("  env.load(\"UnityProject/\")?;");
    println!("  let all_objects: Vec<_> = env.objects().collect();");
    println!("  let textures = env.filter_by_class(\"Texture2D\");");
    println!();

    // Example 6: Property manipulation
    println!("6. Property Manipulation");
    println!("------------------------");

    if let Some(entry) = document.entry_mut() {
        println!("Before: Name = {:?}", entry.name());
        entry.set("m_Name".to_string(), "UpdatedPlayer");
        println!("After: Name = {:?}", entry.name());

        // Add a new property
        entry.set("m_CustomProperty".to_string(), "CustomValue");
        println!("Added custom property: {:?}", entry.get("m_CustomProperty"));
    }
    println!();

    // Example 7: Property types demonstration
    println!("7. Property Types");
    println!("-----------------");

    let mut demo_object = UnityClass::new(999, "DemoObject".to_string(), "demo123".to_string());

    // Different value types
    demo_object.set("bool_prop".to_string(), true);
    demo_object.set("int_prop".to_string(), 42i32);
    demo_object.set("float_prop".to_string(), std::f64::consts::PI);
    demo_object.set("string_prop".to_string(), "Hello Unity");

    println!("Boolean: {:?}", demo_object.get("bool_prop"));
    println!("Integer: {:?}", demo_object.get("int_prop"));
    println!("Float: {:?}", demo_object.get("float_prop"));
    println!("String: {:?}", demo_object.get("string_prop"));

    // Property names
    let prop_names: Vec<_> = demo_object.property_names().collect();
    println!("All properties: {:?}", prop_names);
    println!();

    println!("Example completed successfully!");
    println!("\nNote: YAML file loading/saving is not yet implemented.");
    println!("This example demonstrates the current object model and API.");

    Ok(())
}
