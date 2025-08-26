//! Async YAML writer
//!
//! High-performance concurrent YAML writer for Unity assets.

use std::path::Path;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::YamlDocument;
use unity_asset_core_v2::{Result, UnityAssetError};

/// Configuration for YAML writing
#[derive(Debug, Clone)]
pub struct WriteConfig {
    /// Pretty print YAML
    pub pretty_print: bool,
    /// Include YAML header
    pub include_header: bool,
    /// Buffer size for writing
    pub buffer_size: usize,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self {
            pretty_print: true,
            include_header: true,
            buffer_size: 8192,
        }
    }
}

/// Async YAML writer
pub struct AsyncYamlWriter {
    config: WriteConfig,
}

impl AsyncYamlWriter {
    /// Create new writer
    pub fn new() -> Self {
        Self {
            config: WriteConfig::default(),
        }
    }

    /// Create with configuration
    pub fn with_config(config: WriteConfig) -> Self {
        Self { config }
    }

    /// Write document to file
    pub async fn write_to_file<P: AsRef<Path> + Send>(
        &self,
        document: &YamlDocument,
        path: P,
    ) -> Result<()> {
        let content = document.serialize_to_yaml().await?;
        tokio::fs::write(path, content)
            .await
            .map_err(|e| UnityAssetError::Io(e.to_string()))?;
        Ok(())
    }

    /// Write document to writer
    pub async fn write_to_writer<W: AsyncWrite + Unpin + Send>(
        &self,
        document: &YamlDocument,
        mut writer: W,
    ) -> Result<()> {
        let content = document.serialize_to_yaml().await?;
        writer
            .write_all(content.as_bytes())
            .await
            .map_err(|e| UnityAssetError::Io(e.to_string()))?;
        Ok(())
    }
}

impl Default for AsyncYamlWriter {
    fn default() -> Self {
        Self::new()
    }
}
