//! Unity Version Management System
//!
//! This module provides comprehensive Unity version parsing, comparison, and compatibility
//! handling based on UnityPy's implementation.

use crate::error::{BinaryError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Unity version type (release channel)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum UnityVersionType {
    /// Alpha release
    A = 0,
    /// Beta release
    B = 1,
    /// China release
    C = 2,
    /// Final release
    #[default]
    F = 3,
    /// Patch release
    P = 4,
    /// Experimental release
    X = 5,
    /// Unknown/Custom release
    U = 6,
}

impl fmt::Display for UnityVersionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnityVersionType::A => write!(f, "a"),
            UnityVersionType::B => write!(f, "b"),
            UnityVersionType::C => write!(f, "c"),
            UnityVersionType::F => write!(f, "f"),
            UnityVersionType::P => write!(f, "p"),
            UnityVersionType::X => write!(f, "x"),
            UnityVersionType::U => write!(f, "u"),
        }
    }
}

impl FromStr for UnityVersionType {
    type Err = BinaryError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "a" => Ok(UnityVersionType::A),
            "b" => Ok(UnityVersionType::B),
            "c" => Ok(UnityVersionType::C),
            "f" => Ok(UnityVersionType::F),
            "p" => Ok(UnityVersionType::P),
            "x" => Ok(UnityVersionType::X),
            _ => Ok(UnityVersionType::U),
        }
    }
}

/// Unity version representation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnityVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub version_type: UnityVersionType,
    pub type_number: u8,
    pub type_str: Option<String>, // For custom/unknown types
}

impl Default for UnityVersion {
    fn default() -> Self {
        Self {
            major: 2020,
            minor: 3,
            build: 0,
            version_type: UnityVersionType::F,
            type_number: 1,
            type_str: None,
        }
    }
}

impl UnityVersion {
    /// Create a new Unity version
    pub fn new(
        major: u16,
        minor: u16,
        build: u16,
        version_type: UnityVersionType,
        type_number: u8,
    ) -> Self {
        Self {
            major,
            minor,
            build,
            version_type,
            type_number,
            type_str: None,
        }
    }

    /// Parse Unity version from string
    /// Supports formats like: "2020.3.12f1", "5.6.0", "2018.1.1b2"
    pub fn parse_version(version: &str) -> Result<Self> {
        if version.is_empty() {
            return Ok(Self::default());
        }

        // Use regex to parse version string
        let version_regex = regex::Regex::new(r"^(\d+)\.(\d+)\.(\d+)([a-zA-Z]?)(\d*)$")
            .map_err(|e| BinaryError::invalid_data(format!("Regex error: {}", e)))?;

        if let Some(captures) = version_regex.captures(version) {
            let major = captures
                .get(1)
                .unwrap()
                .as_str()
                .parse::<u16>()
                .map_err(|e| BinaryError::invalid_data(format!("Invalid major version: {}", e)))?;
            let minor = captures
                .get(2)
                .unwrap()
                .as_str()
                .parse::<u16>()
                .map_err(|e| BinaryError::invalid_data(format!("Invalid minor version: {}", e)))?;
            let build = captures
                .get(3)
                .unwrap()
                .as_str()
                .parse::<u16>()
                .map_err(|e| BinaryError::invalid_data(format!("Invalid build version: {}", e)))?;

            let type_str = captures.get(4).map(|m| m.as_str()).unwrap_or("");
            let type_number_str = captures.get(5).map(|m| m.as_str()).unwrap_or("0");

            // If no type letter is provided, default to "f" (final release)
            let version_type = if type_str.is_empty() {
                UnityVersionType::F
            } else {
                UnityVersionType::from_str(type_str)?
            };
            let type_number = if type_number_str.is_empty() {
                0
            } else {
                type_number_str
                    .parse::<u8>()
                    .map_err(|e| BinaryError::invalid_data(format!("Invalid type number: {}", e)))?
            };

            let mut version = Self::new(major, minor, build, version_type, type_number);

            // Store custom type string for unknown types
            if version_type == UnityVersionType::U {
                version.type_str = Some(type_str.to_string());
            }

            Ok(version)
        } else {
            Err(BinaryError::invalid_data(format!(
                "Invalid version format: {}",
                version
            )))
        }
    }

    /// Convert to tuple for comparison
    pub fn as_tuple(&self) -> (u16, u16, u16, u8, u8) {
        (
            self.major,
            self.minor,
            self.build,
            self.version_type as u8,
            self.type_number,
        )
    }

    /// Check if this version is greater than or equal to another
    pub fn is_gte(&self, other: &UnityVersion) -> bool {
        self.as_tuple() >= other.as_tuple()
    }

    /// Check if this version is less than another
    pub fn is_lt(&self, other: &UnityVersion) -> bool {
        self.as_tuple() < other.as_tuple()
    }

    /// Check if this version supports a specific feature
    pub fn supports_feature(&self, feature: UnityFeature) -> bool {
        match feature {
            UnityFeature::BigIds => self.major >= 2019 || (self.major == 2018 && self.minor >= 2),
            UnityFeature::TypeTreeEnabled => {
                self.major >= 5 || (self.major == 4 && self.minor >= 5)
            }
            UnityFeature::ScriptTypeTree => self.major >= 2018,
            UnityFeature::RefTypes => self.major >= 2019,
            UnityFeature::UnityFS => self.major >= 5 && self.minor >= 3,
            UnityFeature::LZ4Compression => self.major >= 5 && self.minor >= 3,
            UnityFeature::LZMACompression => self.major >= 3,
            UnityFeature::BrotliCompression => self.major >= 2020,
            UnityFeature::ModernSerialization => self.major >= 2018,
        }
    }

    /// Get the appropriate byte alignment for this version
    pub fn get_alignment(&self) -> usize {
        if self.major >= 2022 {
            8 // Unity 2022+ uses 8-byte alignment
        } else {
            4 // Unity 2019+ and older versions use 4-byte alignment
        }
    }

    /// Check if this version uses big endian by default
    pub fn uses_big_endian(&self) -> bool {
        // Most Unity versions use little endian, but some platforms/versions may differ
        false
    }

    /// Get the serialized file format version for this Unity version
    pub fn get_serialized_file_format_version(&self) -> u32 {
        if self.major >= 2022 {
            22
        } else if self.major >= 2020 {
            21
        } else if self.major >= 2019 {
            20
        } else if self.major >= 2018 {
            19
        } else if self.major >= 2017 {
            17
        } else if self.major >= 5 {
            15
        } else {
            10
        }
    }
}

impl fmt::Display for UnityVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref custom_type) = self.type_str {
            write!(
                f,
                "{}.{}.{}{}{}",
                self.major, self.minor, self.build, custom_type, self.type_number
            )
        } else {
            write!(
                f,
                "{}.{}.{}{}{}",
                self.major, self.minor, self.build, self.version_type, self.type_number
            )
        }
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

/// Unity features that depend on version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityFeature {
    /// Support for 64-bit object IDs
    BigIds,
    /// TypeTree is enabled by default
    TypeTreeEnabled,
    /// Script type tree support
    ScriptTypeTree,
    /// Reference types support
    RefTypes,
    /// UnityFS format support
    UnityFS,
    /// LZ4 compression support
    LZ4Compression,
    /// LZMA compression support
    LZMACompression,
    /// Brotli compression support
    BrotliCompression,
    /// Modern serialization format
    ModernSerialization,
}

/// Unity version compatibility checker
pub struct VersionCompatibility;

impl VersionCompatibility {
    /// Check if a version is supported by this parser
    pub fn is_supported(version: &UnityVersion) -> bool {
        // We support Unity 3.4 to 2023.x
        version.major >= 3 && version.major <= 2023
    }

    /// Get recommended settings for a Unity version
    pub fn get_recommended_settings(version: &UnityVersion) -> VersionSettings {
        VersionSettings {
            use_type_tree: version.supports_feature(UnityFeature::TypeTreeEnabled),
            alignment: version.get_alignment(),
            big_endian: version.uses_big_endian(),
            supports_big_ids: version.supports_feature(UnityFeature::BigIds),
            supports_ref_types: version.supports_feature(UnityFeature::RefTypes),
            serialized_file_format: version.get_serialized_file_format_version(),
        }
    }

    /// Get a list of known Unity versions for testing
    pub fn get_known_versions() -> Vec<UnityVersion> {
        vec![
            UnityVersion::parse_version("3.4.0f5").unwrap(),
            UnityVersion::parse_version("4.7.2f1").unwrap(),
            UnityVersion::parse_version("5.0.0f4").unwrap(),
            UnityVersion::parse_version("5.6.7f1").unwrap(),
            UnityVersion::parse_version("2017.4.40f1").unwrap(),
            UnityVersion::parse_version("2018.4.36f1").unwrap(),
            UnityVersion::parse_version("2019.4.40f1").unwrap(),
            UnityVersion::parse_version("2020.3.48f1").unwrap(),
            UnityVersion::parse_version("2021.3.21f1").unwrap(),
            UnityVersion::parse_version("2022.3.21f1").unwrap(),
            UnityVersion::parse_version("2023.2.20f1").unwrap(),
        ]
    }
}

/// Version-specific settings
#[derive(Debug, Clone)]
pub struct VersionSettings {
    pub use_type_tree: bool,
    pub alignment: usize,
    pub big_endian: bool,
    pub supports_big_ids: bool,
    pub supports_ref_types: bool,
    pub serialized_file_format: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parsing() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        assert_eq!(version.major, 2020);
        assert_eq!(version.minor, 3);
        assert_eq!(version.build, 12);
        assert_eq!(version.version_type, UnityVersionType::F);
        assert_eq!(version.type_number, 1);
    }

    #[test]
    fn test_version_comparison() {
        let v1 = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let v2 = UnityVersion::parse_version("2021.1.0f1").unwrap();

        assert!(v1 < v2);
        assert!(v2.is_gte(&v1));
        assert!(v1.is_lt(&v2));
    }

    #[test]
    fn test_feature_support() {
        let old_version = UnityVersion::parse_version("5.0.0f1").unwrap();
        let unity_fs_version = UnityVersion::parse_version("5.3.0f1").unwrap();
        let new_version = UnityVersion::parse_version("2020.3.12f1").unwrap();

        assert!(!old_version.supports_feature(UnityFeature::BigIds));
        assert!(new_version.supports_feature(UnityFeature::BigIds));

        // Unity 5.0 doesn't support UnityFS (introduced in 5.3)
        assert!(!old_version.supports_feature(UnityFeature::UnityFS));
        assert!(unity_fs_version.supports_feature(UnityFeature::UnityFS));
        assert!(new_version.supports_feature(UnityFeature::UnityFS));
    }

    #[test]
    fn test_version_display() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        assert_eq!(version.to_string(), "2020.3.12f1");
    }

    #[test]
    fn test_compatibility_check() {
        let supported = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let unsupported = UnityVersion::parse_version("2.0.0f1").unwrap();

        assert!(VersionCompatibility::is_supported(&supported));
        assert!(!VersionCompatibility::is_supported(&unsupported));
    }
}
