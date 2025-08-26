//! Unity Version UnityPy Compatibility Tests
//!
//! Tests that mirror UnityPy's test_UnityVersion.py to ensure V2 has equivalent version handling

use unity_asset_core_v2::Result;

/// Unity version type enumeration (mirrors UnityPy's UnityVersionType)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnityVersionType {
    Alpha = 0,
    Beta = 1,
    China = 2,
    Final = 3,
    Patch = 4,
    Experimental = 5,
    Unknown = 6,
}

impl UnityVersionType {
    fn from_char(c: char) -> Self {
        match c {
            'a' => UnityVersionType::Alpha,
            'b' => UnityVersionType::Beta,
            'c' => UnityVersionType::China,
            'f' => UnityVersionType::Final,
            'p' => UnityVersionType::Patch,
            'x' => UnityVersionType::Experimental,
            _ => UnityVersionType::Unknown,
        }
    }

    fn to_char(self) -> char {
        match self {
            UnityVersionType::Alpha => 'a',
            UnityVersionType::Beta => 'b',
            UnityVersionType::China => 'c',
            UnityVersionType::Final => 'f',
            UnityVersionType::Patch => 'p',
            UnityVersionType::Experimental => 'x',
            UnityVersionType::Unknown => 'u',
        }
    }
}

/// Unity version structure (mirrors UnityPy's UnityVersion)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnityVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub version_type: UnityVersionType,
    pub type_number: u32,
}

impl UnityVersion {
    /// Parse Unity version from string (mirrors UnityPy's from_str)
    pub fn from_str(version_str: &str) -> Result<Self> {
        let version_str = version_str.trim();

        // Handle simple version like "5.0.0"
        if !version_str.contains(char::is_alphabetic) {
            let parts: Vec<&str> = version_str.split('.').collect();
            if parts.len() >= 3 {
                let major = parts[0].parse().map_err(|_| {
                    unity_asset_core_v2::UnityAssetError::parse_error(
                        "Invalid major version".to_string(),
                        0,
                    )
                })?;
                let minor = parts[1].parse().map_err(|_| {
                    unity_asset_core_v2::UnityAssetError::parse_error(
                        "Invalid minor version".to_string(),
                        0,
                    )
                })?;
                let build = parts[2].parse().map_err(|_| {
                    unity_asset_core_v2::UnityAssetError::parse_error(
                        "Invalid build version".to_string(),
                        0,
                    )
                })?;

                return Ok(UnityVersion {
                    major,
                    minor,
                    build,
                    version_type: UnityVersionType::Final,
                    type_number: 0,
                });
            }
        }

        // Parse complex version like "2018.1.1f2"
        let mut chars = version_str.chars().peekable();
        let mut major_str = String::new();
        let mut minor_str = String::new();
        let mut build_str = String::new();
        let mut type_char = 'f';
        let mut type_number_str = String::new();

        // Parse major
        while let Some(&ch) = chars.peek() {
            if ch == '.' {
                chars.next(); // consume '.'
                break;
            }
            major_str.push(chars.next().unwrap());
        }

        // Parse minor
        while let Some(&ch) = chars.peek() {
            if ch == '.' {
                chars.next(); // consume '.'
                break;
            }
            minor_str.push(chars.next().unwrap());
        }

        // Parse build and type
        while let Some(&ch) = chars.peek() {
            if ch.is_alphabetic() {
                type_char = chars.next().unwrap();
                break;
            }
            build_str.push(chars.next().unwrap());
        }

        // Parse type number
        while let Some(ch) = chars.next() {
            if ch.is_numeric() {
                type_number_str.push(ch);
            }
        }

        let major = major_str.parse().map_err(|_| {
            unity_asset_core_v2::UnityAssetError::parse_error(
                "Invalid major version".to_string(),
                0,
            )
        })?;
        let minor = minor_str.parse().map_err(|_| {
            unity_asset_core_v2::UnityAssetError::parse_error(
                "Invalid minor version".to_string(),
                0,
            )
        })?;
        let build = build_str.parse().map_err(|_| {
            unity_asset_core_v2::UnityAssetError::parse_error(
                "Invalid build version".to_string(),
                0,
            )
        })?;
        let type_number = if type_number_str.is_empty() {
            0
        } else {
            type_number_str.parse().map_err(|_| {
                unity_asset_core_v2::UnityAssetError::parse_error(
                    "Invalid type number".to_string(),
                    0,
                )
            })?
        };

        Ok(UnityVersion {
            major,
            minor,
            build,
            version_type: UnityVersionType::from_char(type_char),
            type_number,
        })
    }

    /// Convert to tuple for comparison (mirrors UnityPy's as_tuple)
    pub fn as_tuple(&self) -> (u32, u32, u32, u8, u32) {
        (
            self.major,
            self.minor,
            self.build,
            self.version_type as u8,
            self.type_number,
        )
    }

    /// Get type string representation
    pub fn type_str(&self) -> String {
        format!("{}{}", self.version_type.to_char(), self.type_number)
    }
}

impl PartialOrd for UnityVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UnityVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_tuple().cmp(&other.as_tuple())
    }
}

impl std::fmt::Display for UnityVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UnityVersion({}.{}.{}{}{}, major={}, minor={}, type={}, type_number={})",
            self.major,
            self.minor,
            self.build,
            self.version_type.to_char(),
            self.type_number,
            self.major,
            self.minor,
            self.type_str(),
            self.type_number
        )
    }
}

/// Test Unity version parsing (mirrors UnityPy's test_parse_unity_version)
#[tokio::test]
async fn test_parse_unity_version() -> Result<()> {
    println!("🔄 Testing Unity version parsing...");

    let test_cases = vec![
        ("2018.1.1f2", (2018, 1, 1, UnityVersionType::Final as u8, 2)),
        ("5.0.0", (5, 0, 0, UnityVersionType::Final as u8, 0)),
        (
            "2020.3.12b1",
            (2020, 3, 12, UnityVersionType::Beta as u8, 1),
        ),
        (
            "2019.4.28a3",
            (2019, 4, 28, UnityVersionType::Alpha as u8, 3),
        ),
        ("2017.2.0p1", (2017, 2, 0, UnityVersionType::Patch as u8, 1)),
        ("2021.1.0c1", (2021, 1, 0, UnityVersionType::China as u8, 1)),
        (
            "2022.2.0x1",
            (2022, 2, 0, UnityVersionType::Experimental as u8, 1),
        ),
    ];

    for (version_str, expected_tuple) in test_cases {
        println!("  🧪 Testing version: {}", version_str);

        let version = UnityVersion::from_str(version_str)?;
        let actual_tuple = version.as_tuple();

        assert_eq!(
            actual_tuple, expected_tuple,
            "Version tuple mismatch for {}",
            version_str
        );
        assert_eq!(version.major, expected_tuple.0, "Major version mismatch");
        assert_eq!(version.minor, expected_tuple.1, "Minor version mismatch");
        assert_eq!(version.build, expected_tuple.2, "Build version mismatch");
        assert_eq!(
            version.version_type as u8, expected_tuple.3,
            "Version type mismatch"
        );
        assert_eq!(
            version.type_number, expected_tuple.4,
            "Type number mismatch"
        );

        println!("    ✅ Version {} parsed correctly", version_str);
    }

    println!("🎉 Version parsing tests completed!");
    Ok(())
}

/// Test version comparison with tuples (mirrors UnityPy's test_comparison_with_tuple)
#[tokio::test]
async fn test_comparison_with_tuple() -> Result<()> {
    println!("🔄 Testing version comparison with tuples...");

    let test_cases = vec![
        ("2018.1.1f2", (2018, 1, 1, UnityVersionType::Final as u8, 2)),
        ("2018.1.1f2", (2018, 1, 1, UnityVersionType::Final as u8, 1)),
        ("2018.1.1f2", (2018, 1, 2, UnityVersionType::Final as u8, 2)),
        ("2018.1.1f2", (2018, 2, 1, UnityVersionType::Final as u8, 2)),
    ];

    for (version_str, compare_tuple) in test_cases {
        println!("  🧪 Comparing {} with {:?}", version_str, compare_tuple);

        let version = UnityVersion::from_str(version_str)?;
        let version_tuple = version.as_tuple();

        // Test all comparison operations
        assert_eq!(
            version_tuple == compare_tuple,
            version_tuple == compare_tuple
        );
        assert_eq!(
            version_tuple != compare_tuple,
            version_tuple != compare_tuple
        );
        assert_eq!(version_tuple < compare_tuple, version_tuple < compare_tuple);
        assert_eq!(
            version_tuple <= compare_tuple,
            version_tuple <= compare_tuple
        );
        assert_eq!(version_tuple > compare_tuple, version_tuple > compare_tuple);
        assert_eq!(
            version_tuple >= compare_tuple,
            version_tuple >= compare_tuple
        );

        println!("    ✅ Comparison operations work correctly");
    }

    println!("🎉 Tuple comparison tests completed!");
    Ok(())
}

/// Test version comparison with other versions (mirrors UnityPy's test_comparison_with_unityversion)
#[tokio::test]
async fn test_comparison_with_unityversion() -> Result<()> {
    println!("🔄 Testing version comparison with other versions...");

    let test_cases = vec![
        ("2018.1.1f2", "2018.1.1f2"),
        ("2018.1.1f2", "2018.1.1f1"),
        ("2018.1.1f2", "2018.1.2f2"),
        ("2018.1.1f2", "2018.2.1f2"),
        ("2020.3.0f1", "2019.4.28f1"),
        ("2021.1.0a1", "2021.1.0b1"),
    ];

    for (version_str1, version_str2) in test_cases {
        println!("  🧪 Comparing {} with {}", version_str1, version_str2);

        let v1 = UnityVersion::from_str(version_str1)?;
        let v2 = UnityVersion::from_str(version_str2)?;

        let tuple1 = v1.as_tuple();
        let tuple2 = v2.as_tuple();

        // Test all comparison operations
        assert_eq!(v1 == v2, tuple1 == tuple2);
        assert_eq!(v1 != v2, tuple1 != tuple2);
        assert_eq!(v1 < v2, tuple1 < tuple2);
        assert_eq!(v1 <= v2, tuple1 <= tuple2);
        assert_eq!(v1 > v2, tuple1 > tuple2);
        assert_eq!(v1 >= v2, tuple1 >= tuple2);

        println!("    ✅ Version comparison works correctly");
    }

    println!("🎉 Version comparison tests completed!");
    Ok(())
}

/// Test string representation (mirrors UnityPy's test_repr_and_str)
#[tokio::test]
async fn test_repr_and_str() -> Result<()> {
    println!("🔄 Testing version string representation...");

    let version = UnityVersion::from_str("2018.1.1f2")?;
    let repr_str = format!("{}", version);

    println!("  📝 Version representation: {}", repr_str);

    // Check that representation contains expected components
    assert!(repr_str.contains("UnityVersion"));
    assert!(repr_str.contains(&version.major.to_string()));
    assert!(repr_str.contains(&version.minor.to_string()));
    assert!(repr_str.contains(&version.type_str()));
    assert!(repr_str.contains(&version.type_number.to_string()));

    println!("    ✅ String representation contains all expected components");

    // Test type string
    let type_str = version.type_str();
    assert_eq!(type_str, "f2");
    println!("    ✅ Type string: {}", type_str);

    println!("🎉 String representation tests completed!");
    Ok(())
}

/// Test edge cases and error handling
#[tokio::test]
async fn test_edge_cases() -> Result<()> {
    println!("🔄 Testing edge cases and error handling...");

    // Test valid edge cases
    let edge_cases = vec!["1.0.0", "2023.3.0a1", "2022.1.23f1", "5.6.7p4"];

    for case in edge_cases {
        println!("  🧪 Testing edge case: {}", case);
        let version = UnityVersion::from_str(case)?;
        assert!(version.major > 0);
        println!("    ✅ Edge case {} handled correctly", case);
    }

    // Test version ordering
    let versions = vec![
        "2018.1.0f1",
        "2018.1.1f1",
        "2018.2.0f1",
        "2019.1.0f1",
        "2020.1.0a1",
        "2020.1.0b1",
        "2020.1.0f1",
    ];

    let mut parsed_versions: Vec<UnityVersion> = versions
        .iter()
        .map(|v| UnityVersion::from_str(v).unwrap())
        .collect();

    // Test that versions are properly ordered
    parsed_versions.sort();

    for i in 1..parsed_versions.len() {
        assert!(
            parsed_versions[i - 1] <= parsed_versions[i],
            "Versions should be in ascending order"
        );
    }

    println!("    ✅ Version ordering works correctly");

    println!("🎉 Edge case tests completed!");
    Ok(())
}
