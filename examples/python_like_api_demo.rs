//! Python-like API Demo
//!
//! This example demonstrates the Python-like API that provides similar
//! functionality to the Python unity-yaml-parser reference library.

use std::collections::HashMap;
use unity_asset_core::{DynamicAccess, DynamicValue, UnityClass, UnityValue};
use unity_asset_yaml::python_like_api::PythonLikeUnityDocument;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🐍 Python-like API Demo");
    println!("========================");

    // Demo 1: Basic Python-like usage
    demo_basic_python_like_usage()?;

    // Demo 2: Dynamic property access
    demo_dynamic_property_access()?;

    // Demo 3: Advanced operations
    demo_advanced_operations()?;

    println!("\n✅ All Python-like API demos completed successfully!");
    Ok(())
}

/// Demonstrate basic Python-like usage
fn demo_basic_python_like_usage() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n📝 Demo 1: Basic Python-like Usage");
    println!("-----------------------------------");

    // Create a mock Unity class for demonstration
    let mut gameobject = UnityClass::new(1, "GameObject".to_string(), "123456789".to_string());
    gameobject.set(
        "m_Name".to_string(),
        UnityValue::String("Player".to_string()),
    );
    gameobject.set("m_IsActive".to_string(), UnityValue::Bool(true));
    gameobject.set("m_Layer".to_string(), UnityValue::Integer(0));
    gameobject.set("m_MaxHealth".to_string(), UnityValue::Integer(100));

    println!("Created GameObject with properties:");
    println!(
        "  Class: {} (ID: {})",
        gameobject.class_name, gameobject.class_id
    );

    // Demonstrate dynamic access (similar to Python's entry.m_Name)
    if let Some(name) = gameobject.get_dynamic("m_Name") {
        println!("  Name: {}", name);
    }

    if let Some(health) = gameobject.get_dynamic("m_MaxHealth") {
        println!("  Health: {}", health);
    }

    if let Some(active) = gameobject.get_dynamic("m_IsActive") {
        println!("  Active: {}", active);
    }

    // Demonstrate setting values (similar to Python's entry.m_Name = "NewName")
    gameobject.set_dynamic("m_Name", DynamicValue::String("Hero".to_string()))?;
    gameobject.set_dynamic("m_MaxHealth", DynamicValue::Integer(150))?;

    println!("\nAfter modifications:");
    if let Some(name) = gameobject.get_dynamic("m_Name") {
        println!("  Name: {}", name);
    }
    if let Some(health) = gameobject.get_dynamic("m_MaxHealth") {
        println!("  Health: {}", health);
    }

    Ok(())
}

/// Demonstrate dynamic property access
fn demo_dynamic_property_access() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n🔧 Demo 2: Dynamic Property Access");
    println!("-----------------------------------");

    let mut transform = UnityClass::new(4, "Transform".to_string(), "987654321".to_string());

    // Create nested position object (similar to Python's complex objects)
    let mut position = HashMap::new();
    position.insert("x".to_string(), DynamicValue::Float(1.5));
    position.insert("y".to_string(), DynamicValue::Float(2.0));
    position.insert("z".to_string(), DynamicValue::Float(-0.5));
    transform.set_dynamic("m_LocalPosition", DynamicValue::Object(position))?;

    // Create array of children (similar to Python's arrays)
    let children = vec![
        DynamicValue::Integer(111111),
        DynamicValue::Integer(222222),
        DynamicValue::Integer(333333),
    ];
    transform.set_dynamic("m_Children", DynamicValue::Array(children))?;

    // Create a string property
    transform.set_dynamic("m_Tag", DynamicValue::String("Player".to_string()))?;

    println!("Transform properties:");

    // Access nested object (similar to Python's entry.m_LocalPosition.x)
    if let Some(pos) = transform.get_dynamic("m_LocalPosition") {
        if let Some(pos_obj) = pos.as_object() {
            if let Some(x) = pos_obj.get("x") {
                println!("  Position X: {}", x);
            }
            if let Some(y) = pos_obj.get("y") {
                println!("  Position Y: {}", y);
            }
            if let Some(z) = pos_obj.get("z") {
                println!("  Position Z: {}", z);
            }
        }
    }

    // Access array (similar to Python's entry.m_Children[0])
    if let Some(children) = transform.get_dynamic("m_Children") {
        if let Some(children_arr) = children.as_array() {
            println!("  Children count: {}", children_arr.len());
            for (i, child) in children_arr.iter().enumerate() {
                println!("    Child {}: {}", i, child);
            }
        }
    }

    // Demonstrate Python-like operations
    println!("\nDemonstrating Python-like operations:");

    // String concatenation (similar to Python's entry.m_Tag += "_Suffix")
    if let Some(mut tag) = transform.get_dynamic("m_Tag") {
        tag.concat_string("_Modified")?;
        transform.set_dynamic("m_Tag", tag)?;

        if let Some(new_tag) = transform.get_dynamic("m_Tag") {
            println!("  Modified tag: {}", new_tag);
        }
    }

    Ok(())
}

/// Demonstrate advanced operations
fn demo_advanced_operations() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n⚡ Demo 3: Advanced Operations");
    println!("------------------------------");

    let mut monobehaviour =
        UnityClass::new(114, "MonoBehaviour".to_string(), "555666777".to_string());

    // Set up various property types
    monobehaviour.set_dynamic("m_Enabled", DynamicValue::Bool(true))?;
    monobehaviour.set_dynamic("m_Health", DynamicValue::Integer(100))?;
    monobehaviour.set_dynamic("m_Speed", DynamicValue::Float(5.5))?;
    monobehaviour.set_dynamic("m_PlayerName", DynamicValue::String("Alice".to_string()))?;

    // Create a complex nested structure
    let mut weapon = HashMap::new();
    weapon.insert(
        "name".to_string(),
        DynamicValue::String("Sword".to_string()),
    );
    weapon.insert("damage".to_string(), DynamicValue::Integer(25));
    weapon.insert("durability".to_string(), DynamicValue::Float(0.8));
    monobehaviour.set_dynamic("m_Weapon", DynamicValue::Object(weapon))?;

    // Create an inventory array
    let inventory = vec![
        DynamicValue::String("Health Potion".to_string()),
        DynamicValue::String("Mana Potion".to_string()),
        DynamicValue::String("Key".to_string()),
    ];
    monobehaviour.set_dynamic("m_Inventory", DynamicValue::Array(inventory))?;

    println!("MonoBehaviour initial state:");
    for key in monobehaviour.keys_dynamic() {
        if let Some(value) = monobehaviour.get_dynamic(&key) {
            println!("  {}: {}", key, value);
        }
    }

    // Demonstrate Python-like numeric operations (entry.m_Health += 50)
    println!("\nPerforming operations:");

    // Add to health
    if let Some(mut health) = monobehaviour.get_dynamic("m_Health") {
        health.add_numeric(50.0)?;
        monobehaviour.set_dynamic("m_Health", health)?;
        println!("  ✓ Added 50 to health");
    }

    // Modify speed
    if let Some(mut speed) = monobehaviour.get_dynamic("m_Speed") {
        speed.add_numeric(1.5)?;
        monobehaviour.set_dynamic("m_Speed", speed)?;
        println!("  ✓ Increased speed by 1.5");
    }

    // Concatenate to name
    if let Some(mut name) = monobehaviour.get_dynamic("m_PlayerName") {
        name.concat_string(" the Brave")?;
        monobehaviour.set_dynamic("m_PlayerName", name)?;
        println!("  ✓ Added title to player name");
    }

    // Add item to inventory
    if let Some(mut inventory) = monobehaviour.get_dynamic("m_Inventory") {
        inventory.push(DynamicValue::String("Magic Ring".to_string()))?;
        monobehaviour.set_dynamic("m_Inventory", inventory)?;
        println!("  ✓ Added item to inventory");
    }

    // Modify weapon damage
    if let Some(mut weapon) = monobehaviour.get_dynamic("m_Weapon") {
        if let Some(weapon_obj) = weapon.as_object_mut() {
            if let Some(damage) = weapon_obj.get_mut("damage") {
                damage.add_numeric(10.0)?;
                println!("  ✓ Increased weapon damage by 10");
            }
        }
        monobehaviour.set_dynamic("m_Weapon", weapon)?;
    }

    println!("\nMonoBehaviour final state:");
    for key in monobehaviour.keys_dynamic() {
        if let Some(value) = monobehaviour.get_dynamic(&key) {
            println!("  {}: {}", key, value);
        }
    }

    // Demonstrate type checking and conversion
    println!("\nType checking and conversion:");

    if let Some(health) = monobehaviour.get_dynamic("m_Health") {
        if let Some(health_int) = health.as_integer() {
            println!("  Health as integer: {}", health_int);
        }
        if let Some(health_float) = health.as_float() {
            println!("  Health as float: {}", health_float);
        }
    }

    if let Some(enabled) = monobehaviour.get_dynamic("m_Enabled") {
        if let Some(enabled_bool) = enabled.as_bool() {
            println!("  Enabled as bool: {}", enabled_bool);
        }
        if let Some(enabled_int) = enabled.as_integer() {
            println!("  Enabled as integer: {}", enabled_int);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_value_operations() {
        let mut value = DynamicValue::Integer(10);
        value.add_numeric(5.0).unwrap();
        assert_eq!(value.as_integer(), Some(15));

        let mut text = DynamicValue::String("Hello".to_string());
        text.concat_string(" World").unwrap();
        assert_eq!(text.as_string(), Some("Hello World"));
    }

    #[test]
    fn test_unity_class_dynamic_access() {
        let mut class = UnityClass::new(1, "Test".to_string(), "123".to_string());

        let value = DynamicValue::String("TestValue".to_string());
        class.set_dynamic("test_prop", value).unwrap();

        let retrieved = class.get_dynamic("test_prop").unwrap();
        assert_eq!(retrieved.as_string(), Some("TestValue"));
    }
}
