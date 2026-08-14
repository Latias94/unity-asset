//! Comprehensive tests using real Unity YAML files
//!
//! These tests exercise the production budgeted parser against representative files.

use std::path::Path;
use unity_asset_core::{AssetLoadBudget, UnityValue};
use unity_asset_yaml::{BudgetedYamlError, BudgetedYamlSource, load_budgeted_yaml_path};

fn load_fixture(path: &Path) -> Result<BudgetedYamlSource, BudgetedYamlError> {
    let mut budget = AssetLoadBudget::default();
    load_budgeted_yaml_path(path, &mut budget)
}

/// Test loading a complex single document Unity file (PlayerSettings)
#[test]
fn test_single_doc_player_settings() {
    let fixture_path = Path::new("tests/fixtures/SingleDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let result = load_fixture(fixture_path);
    assert!(
        result.is_ok(),
        "Failed to load SingleDoc.asset: {:?}",
        result.err()
    );

    let source = result.unwrap();
    let doc = source.document();

    // Should have exactly one entry (PlayerSettings)
    assert_eq!(doc.entries().len(), 1);

    let player_settings = &doc.entries()[0];

    // Verify it's a PlayerSettings class (Unity class ID 129)
    assert_eq!(player_settings.class_id(), 129);
    assert_eq!(player_settings.class_name(), "PlayerSettings");
    assert_eq!(player_settings.anchor(), "1");

    // Check some key properties exist
    assert!(player_settings.get("m_ObjectHideFlags").is_some());
    assert!(player_settings.get("serializedVersion").is_some());
    assert!(player_settings.get("productGUID").is_some());
    assert!(player_settings.get("companyName").is_some());
    assert!(player_settings.get("productName").is_some());

    // Check specific values
    if let Some(UnityValue::String(company)) = player_settings.get("companyName") {
        assert_eq!(company, "NoArtistNeeded");
    }

    if let Some(UnityValue::String(product)) = player_settings.get("productName") {
        assert_eq!(product, "TowerLoot");
    }

    // Check nested objects
    if let Some(UnityValue::Object(splash_color)) =
        player_settings.get("m_SplashScreenBackgroundColor")
    {
        assert!(splash_color.get("r").is_some());
        assert!(splash_color.get("g").is_some());
        assert!(splash_color.get("b").is_some());
        assert!(splash_color.get("a").is_some());
    }

    // Check arrays
    if let Some(UnityValue::Array(logos)) = player_settings.get("m_SplashScreenLogos") {
        assert_eq!(logos.len(), 0); // Empty array in this file
    }

    // Check complex nested structures
    if let Some(UnityValue::Object(vr_settings)) = player_settings.get("vrSettings") {
        assert!(vr_settings.get("cardboard").is_some());
        assert!(vr_settings.get("daydream").is_some());
        assert!(vr_settings.get("hololens").is_some());
        assert!(vr_settings.get("oculus").is_some());
    }

    println!(
        "✓ SingleDoc.asset test passed - {} properties loaded",
        player_settings.properties().len()
    );
}

/// Test loading a multi-document Unity file (Prefab with multiple components)
#[test]
fn test_multi_doc_prefab() {
    let fixture_path = Path::new("tests/fixtures/MultiDoc.asset");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let result = load_fixture(fixture_path);
    assert!(
        result.is_ok(),
        "Failed to load MultiDoc.asset: {:?}",
        result.err()
    );

    let source = result.unwrap();
    let doc = source.document();

    // Should have multiple entries (Prefab, GameObject, Transform, MonoBehaviour, SpriteRenderer)
    assert_eq!(doc.entries().len(), 5);

    // Check each component type
    let mut found_prefab = false;
    let mut found_gameobject = false;
    let mut found_transform = false;
    let mut found_monobehaviour = false;
    let mut found_spriterenderer = false;

    for entry in doc.entries() {
        match entry.class_name() {
            "Prefab" => {
                found_prefab = true;
                assert_eq!(entry.class_id(), 129);
                assert_eq!(entry.anchor(), "100100000");

                // Check prefab-specific properties
                assert!(entry.get("m_ObjectHideFlags").is_some());
                assert!(entry.get("m_Modification").is_some());
                assert!(entry.get("m_RootGameObject").is_some());
            }
            "GameObject" => {
                found_gameobject = true;
                assert_eq!(entry.class_id(), 1);
                assert_eq!(entry.anchor(), "1158508787625206");

                if let Some(UnityValue::String(name)) = entry.get("m_Name") {
                    assert_eq!(name, "HealthPiece");
                }

                // Check component array
                if let Some(UnityValue::Array(components)) = entry.get("m_Component") {
                    assert_eq!(components.len(), 3);
                }
            }
            "Transform" => {
                found_transform = true;
                assert_eq!(entry.class_id(), 4);
                assert_eq!(entry.anchor(), "4694383200289498");

                // Check transform properties
                if let Some(UnityValue::Object(pos)) = entry.get("m_LocalPosition") {
                    if let Some(UnityValue::Float(x)) = pos.get("x") {
                        assert_eq!(*x, -16.09);
                    }
                    if let Some(UnityValue::Float(y)) = pos.get("y") {
                        assert_eq!(*y, -10.47);
                    }
                }
            }
            "MonoBehaviour" => {
                found_monobehaviour = true;
                assert_eq!(entry.class_id(), 114);
                assert_eq!(entry.anchor(), "114056957583938824");

                // Check MonoBehaviour properties
                if let Some(UnityValue::Integer(x_index)) = entry.get("xIndex") {
                    assert_eq!(*x_index, 0);
                }
                if let Some(UnityValue::Integer(piece_type)) = entry.get("pieceType") {
                    assert_eq!(*piece_type, 2);
                }
            }
            "SpriteRenderer" => {
                found_spriterenderer = true;
                assert_eq!(entry.class_id(), 212);
                assert_eq!(entry.anchor(), "212685313502090504");

                // Check SpriteRenderer properties
                if let Some(UnityValue::Object(color)) = entry.get("m_Color") {
                    assert!(color.get("r").is_some());
                    assert!(color.get("g").is_some());
                    assert!(color.get("b").is_some());
                    assert!(color.get("a").is_some());
                }
            }
            _ => {
                panic!("Unexpected class type: {}", entry.class_name());
            }
        }
    }

    assert!(found_prefab, "Prefab component not found");
    assert!(found_gameobject, "GameObject component not found");
    assert!(found_transform, "Transform component not found");
    assert!(found_monobehaviour, "MonoBehaviour component not found");
    assert!(found_spriterenderer, "SpriteRenderer component not found");

    println!("✓ MultiDoc.asset test passed - all 5 components found and validated");
}

/// Test Unity extra anchor data (stripped components)
#[test]
fn test_unity_extra_anchor_data() {
    let fixture_path = Path::new("tests/fixtures/UnityExtraAnchorData.prefab");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let source = load_fixture(fixture_path).unwrap();
    let doc = source.document();
    assert!(!doc.entries().is_empty());

    let monobehaviour_count = doc
        .entries()
        .iter()
        .filter(|entry| entry.class_name() == "MonoBehaviour")
        .count();
    assert!(monobehaviour_count > 0);
}

/// Test meta file without YAML tags
#[test]
fn test_meta_file_without_tags() {
    let fixture_path = Path::new("tests/fixtures/MetaFileWithoutTags.meta");

    if !fixture_path.exists() {
        println!("Skipping test - fixture file not found: {:?}", fixture_path);
        return;
    }

    let source = load_fixture(fixture_path).unwrap();
    let doc = source.document();
    assert!(!doc.entries().is_empty());

    let entry = &doc.entries()[0];
    println!(
        "First entry: {} (ID: {}, Anchor: {})",
        entry.class_name(),
        entry.class_id(),
        entry.anchor()
    );
}

/// Test that the production parser handles all supported fixtures.
#[test]
fn test_budgeted_parser_with_all_fixtures() {
    let fixtures = [
        ("SingleDoc.asset", "PlayerSettings"),
        ("MultiDoc.asset", "Multi-component prefab"),
        (
            "UnityExtraAnchorData.prefab",
            "Prefab with stripped components",
        ),
        ("MetaFileWithoutTags.meta", "Meta file without tags"),
    ];

    for (filename, description) in &fixtures {
        let fixture_path = Path::new("tests/fixtures").join(filename);

        if !fixture_path.exists() {
            println!("Skipping {} - file not found", filename);
            continue;
        }

        println!("Testing {} ({})", filename, description);

        let source = load_fixture(&fixture_path).unwrap();
        let classes = source.document().entries();
        assert!(!classes.is_empty(), "{filename} should contain an entry");
        for (i, class) in classes.iter().enumerate() {
            println!(
                "    [{}]: {} (ID: {}, Anchor: {}, {} properties)",
                i,
                class.class_name(),
                class.class_id(),
                class.anchor(),
                class.properties().len()
            );
        }

        println!();
    }
}
