//! Constants for Unity YAML format
//!
//! This module contains Unity-specific constants used in YAML serialization.

// Re-export from unity-asset-core
pub use unity_asset_core::constants::*;

/// Unity YAML tag URI
pub const UNITY_TAG_URI: &str = "tag:unity3d.com,2011:";

/// Unity YAML version
pub const UNITY_YAML_VERSION: (u32, u32) = (1, 1);

/// Line ending types
pub use unity_asset_core::LineEnding;
