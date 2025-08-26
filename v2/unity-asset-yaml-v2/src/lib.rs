//! Unity Asset YAML V2
//!
//! Fully async YAML processing for Unity assets with streaming support, concurrent operations,
//! and Python-compatible API.
//!
//! # Key Features
//!
//! - **Fully Async**: All I/O and processing operations are async
//! - **Stream Processing**: Memory-efficient processing of large YAML files
//! - **Concurrent Operations**: Parallel loading and processing
//! - **Python API Compatible**: Drop-in replacement for UnityPy YAML functionality
//! - **Error Recovery**: Robust error handling with retry mechanisms
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use unity_asset_yaml_v2::{AsyncYamlDocument, AsyncUnityDocument};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load YAML file asynchronously
//!     let doc = AsyncYamlDocument::load_from_path("GameObject.yaml").await?;
//!     
//!     // Stream through objects
//!     let mut objects = doc.objects_stream();
//!     while let Some(object) = objects.next().await {
//!         let obj = object?;
//!         println!("Found: {} ({})", obj.name().unwrap_or_else(|| "Unknown".to_string()), obj.class_name());
//!     }
//!     
//!     Ok(())
//! }
//! ```

// Re-export core types
pub use unity_asset_core_v2::*;

// YAML-specific modules
pub mod async_document;
pub mod async_loader;
pub mod python_api;
pub mod stream_parser;
pub mod unity_deserializer;
pub mod yaml_writer;

// Re-export main YAML types
pub use async_document::AsyncYamlDocument;
pub use async_loader::AsyncYamlLoader;
pub use python_api::AsyncPythonApi;
pub use stream_parser::{StreamYamlParser, YamlObjectStream};
pub use unity_deserializer::{DeserializeConfig, UnityDeserializer};
pub use yaml_writer::{AsyncYamlWriter, WriteConfig};

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialize async YAML processor with default settings
pub fn init_async_yaml() -> AsyncYamlLoader {
    AsyncYamlLoader::new()
}

/// Create async YAML processor with custom configuration
pub fn init_async_yaml_with_config(config: async_loader::LoaderConfig) -> AsyncYamlLoader {
    AsyncYamlLoader::with_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_yaml_functionality() {
        let loader = init_async_yaml();
        assert!(!loader.config().preserve_order);
    }
}
