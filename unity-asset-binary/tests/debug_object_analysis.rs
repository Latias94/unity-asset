//! Debug test to analyze what objects we can actually find in sample files

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use unity_asset_binary::{AssetBundle, SerializedFile};

#[test]
fn debug_analyze_sample_files() {
    let samples_path = Path::new("tests/samples");
    if !samples_path.exists() {
        println!("Samples directory not found, skipping analysis");
        return;
    }

    println!("=== Detailed Sample File Analysis ===");

    if let Ok(entries) = fs::read_dir(samples_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    analyze_single_file(&path);
                }
            }
        }
    }
}

fn analyze_single_file(file_path: &Path) {
    let file_name = file_path.file_name().unwrap().to_string_lossy();
    println!("\n📁 Analyzing: {}", file_name);

    match fs::read(file_path) {
        Ok(data) => {
            println!("  File size: {} bytes", data.len());

            // Check file signature
            if data.len() >= 16 {
                let signature = String::from_utf8_lossy(&data[0..16]);
                println!("  Signature: {:?}", signature.trim_end_matches('\0'));
            }

            // Try to parse as AssetBundle
            match AssetBundle::from_bytes(data.clone()) {
                Ok(bundle) => {
                    println!("  ✅ Parsed as AssetBundle");
                    analyze_bundle(&bundle);
                }
                Err(bundle_err) => {
                    println!("  ❌ AssetBundle parse failed: {}", bundle_err);

                    // Try as SerializedFile
                    match SerializedFile::from_bytes(data.clone()) {
                        Ok(asset) => {
                            println!("  ✅ Parsed as SerializedFile");
                            analyze_serialized_file(&asset);
                        }
                        Err(asset_err) => {
                            println!("  ❌ SerializedFile parse failed: {}", asset_err);

                            // Try to identify file type by header
                            analyze_raw_header(&data);
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("  ❌ Failed to read file: {}", e);
        }
    }
}

fn analyze_bundle(bundle: &AssetBundle) {
    println!("    Bundle format: {}", bundle.header.signature);
    println!("    Unity version: {}", bundle.header.unity_version);
    println!("    Assets count: {}", bundle.assets.len());

    let mut total_objects = 0;
    let mut object_types = HashMap::new();

    for (i, asset) in bundle.assets().iter().enumerate() {
        println!("    Asset {}: {}", i, asset.name());

        match asset.get_objects() {
            Ok(objects) => {
                println!("      Objects: {}", objects.len());
                total_objects += objects.len();

                for obj in objects {
                    let class_name = obj.class_name().to_string();
                    let class_id = obj.class_id();
                    *object_types
                        .entry(format!("{} (ID:{})", class_name, class_id))
                        .or_insert(0) += 1;

                    // Print detailed info for interesting objects
                    if class_name == "GameObject"
                        || class_name == "Transform"
                        || class_name.starts_with("Class_")
                    {
                        println!(
                            "        🔍 {} (ID:{}, PathID:{})",
                            class_name,
                            class_id,
                            obj.path_id()
                        );
                        if let Some(name) = obj.name() {
                            println!("          Name: {}", name);
                        }

                        // Try to parse as specific Unity objects
                        if obj.is_gameobject() {
                            match obj.as_gameobject() {
                                Ok(game_object) => {
                                    println!(
                                        "          ✅ Parsed as GameObject: '{}', Layer: {}, Tag: '{}', Active: {}, Components: {}",
                                        game_object.name,
                                        game_object.layer,
                                        game_object.tag,
                                        game_object.active,
                                        game_object.components.len()
                                    );
                                }
                                Err(e) => {
                                    println!("          ❌ Failed to parse as GameObject: {}", e);
                                }
                            }
                        }

                        if obj.is_transform() {
                            match obj.as_transform() {
                                Ok(transform) => {
                                    println!(
                                        "          ✅ Parsed as Transform: Pos({:.2}, {:.2}, {:.2}), Children: {}",
                                        transform.position.x,
                                        transform.position.y,
                                        transform.position.z,
                                        transform.children.len()
                                    );
                                }
                                Err(e) => {
                                    println!("          ❌ Failed to parse as Transform: {}", e);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("      ❌ Failed to get objects: {}", e);
            }
        }
    }

    println!("    Total objects: {}", total_objects);
    println!("    Object types found:");
    for (object_type, count) in object_types {
        println!("      {}: {}", object_type, count);
    }
}

fn analyze_serialized_file(asset: &SerializedFile) {
    println!("    SerializedFile version: {}", asset.header.version);
    println!("    Unity version: {}", asset.unity_version());

    match asset.get_objects() {
        Ok(objects) => {
            println!("    Objects: {}", objects.len());

            let mut object_types = HashMap::new();
            for obj in objects {
                let class_name = obj.class_name().to_string();
                let class_id = obj.class_id();
                *object_types
                    .entry(format!("{} (ID:{})", class_name, class_id))
                    .or_insert(0) += 1;

                // Print detailed info for interesting objects
                if class_name == "GameObject"
                    || class_name == "Transform"
                    || class_name.starts_with("Class_")
                {
                    println!(
                        "      🔍 {} (ID:{}, PathID:{})",
                        class_name,
                        class_id,
                        obj.path_id()
                    );
                    if let Some(name) = obj.name() {
                        println!("        Name: {}", name);
                    }

                    // Try to parse as specific Unity objects
                    if obj.is_gameobject() {
                        match obj.as_gameobject() {
                            Ok(game_object) => {
                                println!(
                                    "        ✅ Parsed as GameObject: '{}', Layer: {}, Tag: '{}', Active: {}, Components: {}",
                                    game_object.name,
                                    game_object.layer,
                                    game_object.tag,
                                    game_object.active,
                                    game_object.components.len()
                                );
                            }
                            Err(e) => {
                                println!("        ❌ Failed to parse as GameObject: {}", e);
                            }
                        }
                    }

                    if obj.is_transform() {
                        match obj.as_transform() {
                            Ok(transform) => {
                                println!(
                                    "        ✅ Parsed as Transform: Pos({:.2}, {:.2}, {:.2}), Children: {}",
                                    transform.position.x,
                                    transform.position.y,
                                    transform.position.z,
                                    transform.children.len()
                                );
                            }
                            Err(e) => {
                                println!("        ❌ Failed to parse as Transform: {}", e);
                            }
                        }
                    }
                }
            }

            println!("    Object types found:");
            for (object_type, count) in object_types {
                println!("      {}: {}", object_type, count);
            }
        }
        Err(e) => {
            println!("    ❌ Failed to get objects: {}", e);
        }
    }
}

fn analyze_raw_header(data: &[u8]) {
    println!("    Raw file analysis:");

    if data.len() >= 32 {
        // Check for common Unity signatures
        let signatures_to_check = [
            ("UnityFS", 0),
            ("UnityWeb", 0),
            ("UnityRaw", 0),
            ("UnityArchive", 0),
        ];

        for (sig, offset) in signatures_to_check {
            if data.len() > offset + sig.len() {
                let file_sig = String::from_utf8_lossy(&data[offset..offset + sig.len()]);
                if file_sig == sig {
                    println!("      Found {} signature at offset {}", sig, offset);
                }
            }
        }

        // Print hex dump of first 32 bytes
        println!("      First 32 bytes (hex):");
        for chunk in data[..32.min(data.len())].chunks(16) {
            let hex: String = chunk
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii: String = chunk
                .iter()
                .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
                .collect();
            println!("        {} | {}", hex, ascii);
        }
    }
}

#[test]
fn debug_class_id_mapping() {
    println!("=== Class ID Mapping Test ===");

    // Test our class ID mapping
    let test_class_ids = vec![1, 4, 21, 28, 43, 83, 114, 115, 213];

    for class_id in test_class_ids {
        let class_name = unity_asset_core::get_class_name(class_id);
        println!("Class ID {}: {:?}", class_id, class_name);
    }
}

#[test]
fn debug_typetree_parsing() {
    let samples_path = Path::new("tests/samples");
    if !samples_path.exists() {
        println!("Samples directory not found, skipping TypeTree analysis");
        return;
    }

    println!("=== TypeTree Parsing Analysis ===");

    if let Ok(entries) = fs::read_dir(samples_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() {
                    analyze_typetree_in_file(&path);
                }
            }
        }
    }
}

fn analyze_typetree_in_file(file_path: &Path) {
    let file_name = file_path.file_name().unwrap().to_string_lossy();

    if let Ok(data) = fs::read(file_path) {
        if let Ok(bundle) = AssetBundle::from_bytes(data.clone()) {
            println!("\n📁 TypeTree analysis for: {}", file_name);

            for asset in bundle.assets() {
                if let Ok(objects) = asset.get_objects() {
                    for obj in objects.iter().take(3) {
                        // Only analyze first 3 objects
                        println!("  Object: {} (ID:{})", obj.class_name(), obj.class_id());

                        // Try to access TypeTree information
                        if let Some(type_tree) = &obj.info.type_tree {
                            println!("    TypeTree nodes: {}", type_tree.nodes.len());
                            if let Some(root) = type_tree.root() {
                                println!("    Root type: {} ({})", root.type_name, root.name);
                                print_typetree_structure(root, 2);
                            }
                        } else {
                            println!("    No TypeTree information");
                        }
                    }
                }
            }
        } else if let Ok(asset) = SerializedFile::from_bytes(data) {
            println!("\n📁 TypeTree analysis for: {} (SerializedFile)", file_name);

            if let Ok(objects) = asset.get_objects() {
                for obj in objects.iter().take(3) {
                    // Only analyze first 3 objects
                    println!("  Object: {} (ID:{})", obj.class_name(), obj.class_id());

                    if let Some(type_tree) = &obj.info.type_tree {
                        println!("    TypeTree nodes: {}", type_tree.nodes.len());
                        if let Some(root) = type_tree.root() {
                            println!("    Root type: {} ({})", root.type_name, root.name);
                            print_typetree_structure(root, 2);
                        }
                    } else {
                        println!("    No TypeTree information");
                    }
                }
            }
        }
    }
}

fn print_typetree_structure(node: &unity_asset_binary::TypeTreeNode, indent: usize) {
    let indent_str = "  ".repeat(indent);
    println!(
        "{}├─ {} {} (size: {})",
        indent_str, node.type_name, node.name, node.byte_size
    );

    for child in &node.children {
        print_typetree_structure(child, indent + 1);
    }
}
