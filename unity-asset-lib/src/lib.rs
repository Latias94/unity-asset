//! Unity Asset Parser
//!
//! A comprehensive Rust library for parsing Unity asset files, supporting both YAML and binary formats.
//!
//! This crate provides high-performance, memory-safe parsing of Unity files
//! while maintaining exact compatibility with Unity's formats.
//!
//! # Features
//!
//! - **YAML Processing**: Complete Unity YAML format support with multi-document parsing
//! - **Binary Assets**: AssetBundle and SerializedFile parsing with compression support
//! - **Async Support**: Optional async/await API for concurrent processing (enable with `async` feature)
//! - **Type Safety**: Rust's type system prevents common parsing vulnerabilities
//! - **Performance**: Zero-cost abstractions and memory-efficient parsing
//!
//! # Examples
//!
//! ## Basic YAML Processing
//!
//! ```rust,no_run
//! use unity_asset::{YamlDocument, UnityDocument};
//!
//! // Load a Unity YAML file
//! let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
//!
//! // Access and filter objects
//! let settings = doc.get(Some("PlayerSettings"), None)?;
//! println!("Product name: {:?}", settings.get("productName"));
//!
//! # Ok::<(), unity_asset::UnityAssetError>(())
//! ```
//!
//! ## Binary Asset Processing
//!
//! ```rust,no_run
//! use unity_asset::load_bundle_from_memory;
//!
//! // Load and parse AssetBundle
//! let data = std::fs::read("game.bundle")?;
//! let bundle = load_bundle_from_memory(data)?;
//!
//! // Process assets
//! for asset in &bundle.assets {
//!     println!("Found asset with {} objects", asset.object_count());
//! }
//!
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Async Processing (requires `async` feature)
//!
//! ```rust,no_run
//! # #[cfg(feature = "async")]
//! # {
//! use unity_asset::{YamlDocument, AsyncUnityDocument};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load file asynchronously
//!     let doc = YamlDocument::load_yaml_async("ProjectSettings.asset", false).await?;
//!
//!     // Same API as sync version
//!     let settings = doc.get(Some("PlayerSettings"), None)?;
//!     println!("Product name: {:?}", settings.get("productName"));
//!
//!     Ok(())
//! }
//! # }
//! ```

// Re-export from core crate
pub use unity_asset_core::{
    DocumentFormat, Result, UnityAssetError, UnityClass, UnityClassRegistry, UnityDocument,
    UnityValue, constants::*,
};

// Re-export from YAML crate
pub use unity_asset_yaml::YamlDocument;

// Re-export from binary crate
pub use unity_asset_binary::{
    AssetBundle, SerializedFile, load_bundle, load_bundle_from_memory, load_bundle_with_options,
};

// Re-export async traits when async feature is enabled
#[cfg(feature = "async")]
pub use unity_asset_core::document::AsyncUnityDocument;

/// Environment for managing multiple Unity assets
pub mod environment {
    use crate::{Result, UnityClass, YamlDocument};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use unity_asset_core::{UnityAssetError, UnityDocument};

    /// Unified environment for managing Unity assets
    pub struct Environment {
        /// Loaded YAML documents
        yaml_documents: HashMap<PathBuf, YamlDocument>,
        /// Base path for relative file resolution
        #[allow(dead_code)]
        base_path: PathBuf,
    }

    impl Environment {
        /// Create a new environment
        pub fn new() -> Self {
            Self {
                yaml_documents: HashMap::new(),
                base_path: std::env::current_dir().unwrap_or_default(),
            }
        }

        /// Load assets from a path (file or directory)
        pub fn load<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            if path.is_file() {
                self.load_file(path)?;
            } else if path.is_dir() {
                self.load_directory(path)?;
            }

            Ok(())
        }

        /// Load a single file
        pub fn load_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            // Check file extension to determine type
            if let Some(ext) = path.extension() {
                match ext.to_str() {
                    Some("asset") | Some("prefab") | Some("unity") | Some("meta") => {
                        let doc = YamlDocument::load_yaml(path, false)?;
                        self.yaml_documents.insert(path.to_path_buf(), doc);
                    }
                    _ => {
                        // For now, skip unknown file types
                        // Future: Add binary asset support (.bundle, .assets, etc.)
                    }
                }
            }

            Ok(())
        }

        /// Load all supported files from a directory
        pub fn load_directory<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
            let path = path.as_ref();

            if !path.exists() {
                return Err(UnityAssetError::format(format!(
                    "Directory does not exist: {:?}",
                    path
                )));
            }

            if !path.is_dir() {
                return Err(UnityAssetError::format(format!(
                    "Path is not a directory: {:?}",
                    path
                )));
            }

            // Recursively traverse directory
            self.traverse_directory(path)?;

            Ok(())
        }

        /// Recursively traverse directory and load Unity files
        fn traverse_directory(&mut self, dir: &Path) -> Result<()> {
            let entries = std::fs::read_dir(dir).map_err(|e| {
                UnityAssetError::format(format!("Failed to read directory {:?}: {}", dir, e))
            })?;

            for entry in entries {
                let entry = entry.map_err(|e| {
                    UnityAssetError::format(format!("Failed to read directory entry: {}", e))
                })?;
                let path = entry.path();

                if path.is_dir() {
                    // Skip common Unity directories that don't contain assets
                    if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                        match dir_name {
                            "Library" | "Temp" | "Logs" | ".git" | ".vs" | "obj" | "bin" => {
                                continue; // Skip these directories
                            }
                            _ => {
                                // Recursively process subdirectory
                                self.traverse_directory(&path)?;
                            }
                        }
                    }
                } else if path.is_file() {
                    // Try to load the file
                    if let Err(e) = self.load_file(&path) {
                        // Log error but continue processing other files
                        eprintln!("Warning: Failed to load {:?}: {}", path, e);
                    }
                }
            }

            Ok(())
        }

        /// Get all Unity objects from all loaded documents
        pub fn objects(&self) -> impl Iterator<Item = &UnityClass> {
            self.yaml_documents.values().flat_map(|doc| doc.entries())
        }

        /// Filter objects by class name
        pub fn filter_by_class(&self, class_name: &str) -> Vec<&UnityClass> {
            self.objects()
                .filter(|obj| obj.class_name == class_name)
                .collect()
        }

        /// Get loaded YAML documents
        pub fn yaml_documents(&self) -> &HashMap<PathBuf, YamlDocument> {
            &self.yaml_documents
        }
    }

    impl Default for Environment {
        fn default() -> Self {
            Self::new()
        }
    }
}
