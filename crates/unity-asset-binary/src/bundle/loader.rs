//! Bundle resource loading and management
//!
//! This module provides functionality for loading and managing
//! resources from Unity AssetBundles.

use super::parser::BundleParser;
use super::types::{AssetBundle, BundleLoadOptions};
use crate::asset::Asset;
use crate::error::{BinaryError, Result};
use std::collections::HashMap;
use std::path::Path;

#[cfg(feature = "async")]
use tokio::fs;

/// Bundle resource loader
///
/// This struct provides high-level functionality for loading and managing
/// AssetBundle resources, including caching and async loading support.
pub struct BundleLoader {
    /// Loaded bundles cache
    bundles: HashMap<String, AssetBundle>,
    /// Loading options
    options: BundleLoadOptions,
}

impl BundleLoader {
    /// Create a new bundle loader
    pub fn new() -> Self {
        Self {
            bundles: HashMap::new(),
            options: BundleLoadOptions::default(),
        }
    }

    /// Create a new bundle loader with options
    pub fn with_options(options: BundleLoadOptions) -> Self {
        Self {
            bundles: HashMap::new(),
            options,
        }
    }

    /// Load a bundle from file path
    pub fn load_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<&AssetBundle> {
        let path_ref = path.as_ref();
        let path_str = path_ref.to_string_lossy().to_string();

        // Check if already loaded
        if self.bundles.contains_key(&path_str) {
            return Ok(self.bundles.get(&path_str).unwrap());
        }

        // Read file data
        let data = std::fs::read(path_ref)
            .map_err(|e| BinaryError::generic(format!("Failed to read bundle file: {}", e)))?;

        // Parse bundle
        let bundle = BundleParser::from_bytes_with_options(data, self.options.clone())?;

        // Cache and return
        self.bundles.insert(path_str.clone(), bundle);
        Ok(self.bundles.get(&path_str).unwrap())
    }

    /// Load a bundle from memory
    pub fn load_from_memory(&mut self, name: String, data: Vec<u8>) -> Result<&AssetBundle> {
        // Check if already loaded
        if self.bundles.contains_key(&name) {
            return Ok(self.bundles.get(&name).unwrap());
        }

        // Parse bundle
        let bundle = BundleParser::from_bytes_with_options(data, self.options.clone())?;

        // Cache and return
        self.bundles.insert(name.clone(), bundle);
        Ok(self.bundles.get(&name).unwrap())
    }

    /// Async load a bundle from file path
    #[cfg(feature = "async")]
    pub async fn load_from_file_async<P: AsRef<Path>>(&mut self, path: P) -> Result<&AssetBundle> {
        let path_ref = path.as_ref();
        let path_str = path_ref.to_string_lossy().to_string();

        // Check if already loaded
        if self.bundles.contains_key(&path_str) {
            return Ok(self.bundles.get(&path_str).unwrap());
        }

        // Read file data asynchronously
        let data = fs::read(path_ref)
            .await
            .map_err(|e| BinaryError::generic(format!("Failed to read bundle file: {}", e)))?;

        // Parse bundle
        let bundle = BundleParser::from_bytes_with_options(data, self.options.clone())?;

        // Cache and return
        self.bundles.insert(path_str.clone(), bundle);
        Ok(self.bundles.get(&path_str).unwrap())
    }

    /// Get a loaded bundle by name
    pub fn get_bundle(&self, name: &str) -> Option<&AssetBundle> {
        self.bundles.get(name)
    }

    /// Get a mutable reference to a loaded bundle
    pub fn get_bundle_mut(&mut self, name: &str) -> Option<&mut AssetBundle> {
        self.bundles.get_mut(name)
    }

    /// Unload a bundle
    pub fn unload_bundle(&mut self, name: &str) -> bool {
        self.bundles.remove(name).is_some()
    }

    /// Unload all bundles
    pub fn unload_all(&mut self) {
        self.bundles.clear();
    }

    /// Get list of loaded bundle names
    pub fn loaded_bundles(&self) -> Vec<&str> {
        self.bundles.keys().map(|s| s.as_str()).collect()
    }

    /// Get total memory usage of loaded bundles
    pub fn memory_usage(&self) -> usize {
        self.bundles
            .values()
            .map(|bundle| bundle.size() as usize)
            .sum()
    }

    /// Find assets by name across all loaded bundles
    pub fn find_assets_by_name(&self, name: &str) -> Vec<(&str, &Asset)> {
        let mut results = Vec::new();

        for (bundle_name, bundle) in &self.bundles {
            for asset in &bundle.assets {
                // SerializedFile doesn't have a name field, use bundle name instead
                if bundle_name.contains(name) {
                    results.push((bundle_name.as_str(), asset));
                }
            }
        }

        results
    }

    /// Find assets by type ID across all loaded bundles
    pub fn find_assets_by_type(&self, _type_id: i32) -> Vec<(&str, &Asset)> {
        let mut results = Vec::new();

        for (bundle_name, bundle) in &self.bundles {
            for asset in &bundle.assets {
                // SerializedFile doesn't have a type_id field directly
                // We'll skip this for now or implement differently
                // TODO: Implement proper type filtering for SerializedFile
                results.push((bundle_name.as_str(), asset));
            }
        }

        results
    }

    /// Get bundle statistics
    pub fn get_statistics(&self) -> LoaderStatistics {
        let bundle_count = self.bundles.len();
        let total_size = self.memory_usage();
        let total_assets: usize = self.bundles.values().map(|b| b.asset_count()).sum();
        let total_files: usize = self.bundles.values().map(|b| b.file_count()).sum();

        LoaderStatistics {
            bundle_count,
            total_size,
            total_assets,
            total_files,
            average_bundle_size: if bundle_count > 0 {
                total_size / bundle_count
            } else {
                0
            },
        }
    }

    /// Validate all loaded bundles
    pub fn validate_all(&self) -> Result<()> {
        for (name, bundle) in &self.bundles {
            bundle.validate().map_err(|e| {
                BinaryError::generic(format!("Bundle '{}' validation failed: {}", name, e))
            })?;
        }
        Ok(())
    }

    /// Set loading options
    pub fn set_options(&mut self, options: BundleLoadOptions) {
        self.options = options;
    }

    /// Get current loading options
    pub fn options(&self) -> &BundleLoadOptions {
        &self.options
    }
}

impl Default for BundleLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Bundle resource manager
///
/// This struct provides advanced resource management functionality,
/// including dependency tracking and resource lifecycle management.
pub struct BundleResourceManager {
    loader: BundleLoader,
    dependencies: HashMap<String, Vec<String>>,
    reference_counts: HashMap<String, usize>,
}

impl BundleResourceManager {
    /// Create a new resource manager
    pub fn new() -> Self {
        Self {
            loader: BundleLoader::new(),
            dependencies: HashMap::new(),
            reference_counts: HashMap::new(),
        }
    }

    /// Load a bundle with dependency tracking
    pub fn load_bundle<P: AsRef<Path>>(
        &mut self,
        path: P,
        dependencies: Vec<String>,
    ) -> Result<()> {
        let path_str = path.as_ref().to_string_lossy().to_string();

        // Load dependencies first
        for dep in &dependencies {
            if !self.loader.bundles.contains_key(dep) {
                return Err(BinaryError::generic(format!(
                    "Dependency '{}' not loaded",
                    dep
                )));
            }
            // Increment reference count
            *self.reference_counts.entry(dep.clone()).or_insert(0) += 1;
        }

        // Load the bundle
        self.loader.load_from_file(path)?;

        // Track dependencies
        self.dependencies.insert(path_str.clone(), dependencies);
        self.reference_counts.insert(path_str, 1);

        Ok(())
    }

    /// Unload a bundle with dependency management
    pub fn unload_bundle(&mut self, name: &str) -> Result<()> {
        // Check if bundle exists
        if !self.reference_counts.contains_key(name) {
            return Err(BinaryError::generic(format!(
                "Bundle '{}' not loaded",
                name
            )));
        }

        // Decrease reference count
        let ref_count = self.reference_counts.get_mut(name).unwrap();
        *ref_count -= 1;

        // If reference count reaches zero, unload
        if *ref_count == 0 {
            // Unload dependencies
            if let Some(deps) = self.dependencies.remove(name) {
                for dep in deps {
                    self.unload_bundle(&dep)?;
                }
            }

            // Unload the bundle itself
            self.loader.unload_bundle(name);
            self.reference_counts.remove(name);
        }

        Ok(())
    }

    /// Get the underlying loader
    pub fn loader(&self) -> &BundleLoader {
        &self.loader
    }

    /// Get mutable access to the underlying loader
    pub fn loader_mut(&mut self) -> &mut BundleLoader {
        &mut self.loader
    }

    /// Get dependency information
    pub fn get_dependencies(&self, name: &str) -> Option<&Vec<String>> {
        self.dependencies.get(name)
    }

    /// Get reference count for a bundle
    pub fn get_reference_count(&self, name: &str) -> usize {
        self.reference_counts.get(name).copied().unwrap_or(0)
    }
}

impl Default for BundleResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Loader statistics
#[derive(Debug, Clone)]
pub struct LoaderStatistics {
    pub bundle_count: usize,
    pub total_size: usize,
    pub total_assets: usize,
    pub total_files: usize,
    pub average_bundle_size: usize,
}

/// Convenience functions for quick bundle loading
/// Load a single bundle from file
pub fn load_bundle<P: AsRef<Path>>(path: P) -> Result<AssetBundle> {
    let data = std::fs::read(path)
        .map_err(|e| BinaryError::generic(format!("Failed to read bundle file: {}", e)))?;
    BundleParser::from_bytes(data)
}

/// Load a bundle from memory
pub fn load_bundle_from_memory(data: Vec<u8>) -> Result<AssetBundle> {
    BundleParser::from_bytes(data)
}

/// Load a bundle with specific options
pub fn load_bundle_with_options<P: AsRef<Path>>(
    path: P,
    options: BundleLoadOptions,
) -> Result<AssetBundle> {
    let data = std::fs::read(path)
        .map_err(|e| BinaryError::generic(format!("Failed to read bundle file: {}", e)))?;
    BundleParser::from_bytes_with_options(data, options)
}

#[cfg(feature = "async")]
/// Async load a single bundle from file
pub async fn load_bundle_async<P: AsRef<Path>>(path: P) -> Result<AssetBundle> {
    let data = fs::read(path)
        .await
        .map_err(|e| BinaryError::generic(format!("Failed to read bundle file: {}", e)))?;
    BundleParser::from_bytes(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loader_creation() {
        let loader = BundleLoader::new();
        assert_eq!(loader.loaded_bundles().len(), 0);
        assert_eq!(loader.memory_usage(), 0);
    }

    #[test]
    fn test_resource_manager_creation() {
        let manager = BundleResourceManager::new();
        assert_eq!(manager.loader().loaded_bundles().len(), 0);
    }
}
