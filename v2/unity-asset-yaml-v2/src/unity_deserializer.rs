//! Unity YAML deserializer
//!
//! Specialized deserializer for Unity YAML format with async support.

use serde::de::DeserializeOwned;
use unity_asset_core_v2::{Result, UnityAssetError, UnityValue};

/// Configuration for deserialization
#[derive(Debug, Clone)]
pub struct DeserializeConfig {
    /// Strict mode for type checking
    pub strict: bool,
    /// Allow missing fields
    pub allow_missing_fields: bool,
}

impl Default for DeserializeConfig {
    fn default() -> Self {
        Self {
            strict: false,
            allow_missing_fields: true,
        }
    }
}

/// Unity YAML deserializer
pub struct UnityDeserializer {
    config: DeserializeConfig,
}

impl UnityDeserializer {
    /// Create new deserializer
    pub fn new() -> Self {
        Self {
            config: DeserializeConfig::default(),
        }
    }

    /// Create with configuration
    pub fn with_config(config: DeserializeConfig) -> Self {
        Self { config }
    }

    /// Deserialize Unity value to type
    pub async fn deserialize<T>(&self, value: &UnityValue) -> Result<T>
    where
        T: DeserializeOwned,
    {
        value
            .deserialize()
            .map_err(|e| UnityAssetError::Serialization(e.to_string()))
    }
}

impl Default for UnityDeserializer {
    fn default() -> Self {
        Self::new()
    }
}
