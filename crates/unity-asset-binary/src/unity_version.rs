//! Unity Version Management System
//!
//! This module provides Unity version parsing and comparison based on UnityPy's implementation.

use crate::error::{BinaryError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Unity version type (release channel)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UnityVersionType {
    /// Alpha release
    A = 0,
    /// Beta release
    B = 1,
    /// China release
    C = 2,
    /// Final release
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
        // Mirrors UnityPy `UnityVersion.from_str` behavior:
        // - parse `<major>.<minor>.<build><type_str><type_number>` where `<type_str>` can be more than 1 char
        // - unknown type strings are preserved (e.g. Tuanjie `t`, UnityCN `f1c`)
        // - ignore any revision hash suffix in parentheses (ProjectVersion.txt style)
        let raw = version.trim();
        if raw.is_empty() {
            return Err(BinaryError::invalid_data("Unity version is empty"));
        }
        let raw = raw.split_whitespace().next().unwrap_or(raw);

        let mut parts = raw.splitn(3, '.');
        let major = parts
            .next()
            .ok_or_else(|| BinaryError::invalid_data(format!("Invalid version format: {}", raw)))?
            .parse::<u16>()
            .map_err(|e| BinaryError::invalid_data(format!("Invalid major version: {}", e)))?;
        let minor = parts
            .next()
            .ok_or_else(|| BinaryError::invalid_data(format!("Invalid version format: {}", raw)))?
            .parse::<u16>()
            .map_err(|e| BinaryError::invalid_data(format!("Invalid minor version: {}", e)))?;
        let tail = parts
            .next()
            .ok_or_else(|| BinaryError::invalid_data(format!("Invalid version format: {}", raw)))?;

        let (build_digits, suffix) = split_leading_digits(tail);
        if build_digits.is_empty() {
            return Err(BinaryError::invalid_data(format!(
                "Invalid build version: {}",
                raw
            )));
        }
        let build = build_digits
            .parse::<u16>()
            .map_err(|e| BinaryError::invalid_data(format!("Invalid build version: {}", e)))?;

        if suffix.is_empty() {
            return Ok(Self::new(major, minor, build, UnityVersionType::F, 0));
        }

        let (type_str, type_number) = split_trailing_number(suffix);
        let type_number_u8 = type_number.ok_or_else(|| {
            BinaryError::invalid_data(format!("Invalid version channel number: {raw}"))
        })?;

        let parsed_type = UnityVersionType::from_str(type_str).unwrap_or(UnityVersionType::U);
        let mut out = Self::new(major, minor, build, parsed_type, type_number_u8);

        // Preserve unknown/custom type strings exactly, UnityPy-style.
        if out.version_type == UnityVersionType::U {
            out.type_str = Some(type_str.to_string());
        }

        Ok(out)
    }

    /// Convert to tuple for comparison
    fn as_tuple(&self) -> (u16, u16, u16, u8, u8) {
        (
            self.major,
            self.minor,
            self.build,
            self.version_type as u8,
            self.type_number,
        )
    }
}

impl fmt::Display for UnityVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.version_type == UnityVersionType::U {
            let channel = self.type_str.as_deref().unwrap_or("u");
            write!(
                f,
                "{}.{}.{}{}{}",
                self.major, self.minor, self.build, channel, self.type_number
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

fn split_leading_digits(s: &str) -> (&str, &str) {
    let idx = s
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or_else(|| s.len());
    s.split_at(idx)
}

fn split_trailing_number(s: &str) -> (&str, Option<u8>) {
    let idx = s
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(i, ch)| i + ch.len_utf8())
        .unwrap_or(0);
    let (head, tail) = s.split_at(idx);
    if tail.is_empty() {
        return (head, Some(0));
    }
    let n = tail.parse::<u8>().ok();
    (head, n)
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
    fn empty_version_is_not_guessed() {
        for input in ["", "   "] {
            assert!(matches!(
                UnityVersion::parse_version(input),
                Err(BinaryError::InvalidData(message)) if message == "Unity version is empty"
            ));
        }
    }

    #[test]
    fn test_version_comparison() {
        let v1 = UnityVersion::parse_version("2020.3.12f1").unwrap();
        let v2 = UnityVersion::parse_version("2021.1.0f1").unwrap();

        assert!(v1 < v2);
    }

    #[test]
    fn test_version_display() {
        let version = UnityVersion::parse_version("2020.3.12f1").unwrap();
        assert_eq!(version.to_string(), "2020.3.12f1");
    }

    #[test]
    fn test_unitycn_suffix_parsing() {
        let version = UnityVersion::parse_version("2022.3.48f1c1").unwrap();
        assert_eq!(version.major, 2022);
        assert_eq!(version.minor, 3);
        assert_eq!(version.build, 48);
        assert_eq!(version.version_type, UnityVersionType::U);
        assert_eq!(version.type_number, 1);
        assert_eq!(version.type_str.as_deref(), Some("f1c"));
        assert_eq!(version.to_string(), "2022.3.48f1c1");
    }

    #[test]
    fn test_tuanjie_channel_parsing() {
        let version = UnityVersion::parse_version("2022.3.48t6").unwrap();
        assert_eq!(version.major, 2022);
        assert_eq!(version.minor, 3);
        assert_eq!(version.build, 48);
        assert_eq!(version.version_type, UnityVersionType::U);
        assert_eq!(version.type_number, 6);
        assert_eq!(version.type_str.as_deref(), Some("t"));
        assert_eq!(version.to_string(), "2022.3.48t6");
    }

    #[test]
    fn test_version_parsing_ignores_revision_suffix() {
        let version = UnityVersion::parse_version("2022.3.48t6 (b281c1694403)").unwrap();
        assert_eq!(version.to_string(), "2022.3.48t6");
    }

    #[test]
    fn version_channel_number_overflow_is_not_coerced_to_zero() {
        assert!(matches!(
            UnityVersion::parse_version("2020.3.0f999"),
            Err(BinaryError::InvalidData(message))
                if message == "Invalid version channel number: 2020.3.0f999"
        ));
    }
}
