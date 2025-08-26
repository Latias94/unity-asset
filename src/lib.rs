//! Unity Asset Parser
//!
//! A Rust implementation of Unity asset parsing, starting with YAML support
//! and designed for future extension to binary assets.
//!
//! This crate provides high-performance, memory-safe parsing of Unity files
//! while maintaining exact compatibility with Unity's formats.
//!
//! # Examples
//!
//! ```rust,no_run
//! use unity_asset::YamlDocument;
//! use unity_asset_core::UnityDocument;
//!
//! // Load a Unity YAML file
//! let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
//!
//! // Access the main object
//! if let Some(settings) = doc.entry() {
//!     println!("Product name: {:?}", settings.get("productName"));
//! }
//!
//! # Ok::<(), unity_asset::UnityAssetError>(())
//! ```

// Re-export from core and YAML crates
pub use unity_asset_core::{
    DocumentFormat, Result, UnityAssetError, UnityClass, UnityClassRegistry, UnityValue,
    constants::*,
};

pub use unity_asset_yaml::YamlDocument;

// TODO: Re-export from unity-binary crate when implemented
// pub use unity_binary::{...};

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
