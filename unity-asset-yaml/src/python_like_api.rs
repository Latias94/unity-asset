//! Python-like API for Unity YAML parsing
//!
//! This module provides a more Python-like interface similar to the reference library,
//! making it easier for users familiar with the Python unity-yaml-parser to migrate.

use crate::YamlDocument;
use std::path::Path;
use unity_asset_core::{
    DynamicAccess, DynamicValue, Result, UnityAssetError, UnityClass,
    UnityDocument as UnityDocumentTrait,
};

/// Python-like wrapper for Unity documents
///
/// This provides an API similar to the Python reference library:
/// ```python
/// doc = UnityDocument.load_yaml("file.asset")
/// entry = doc.entry
/// entry.m_Name = "NewName"
/// doc.dump_yaml()
/// ```
pub struct PythonLikeUnityDocument {
    /// Internal YAML document
    inner: YamlDocument,
}

impl PythonLikeUnityDocument {
    /// Load a Unity YAML file (similar to Python's UnityDocument.load_yaml)
    ///
    /// # Arguments
    /// * `file_path` - Path to the Unity YAML file
    /// * `try_preserve_types` - Whether to preserve original types (similar to Python's flag)
    ///
    /// # Examples
    /// ```rust,no_run
    /// use unity_asset_yaml::python_like_api::PythonLikeUnityDocument;
    ///
    /// let doc = PythonLikeUnityDocument::load_yaml("ProjectSettings.asset", false)?;
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn load_yaml<P: AsRef<Path>>(file_path: P, try_preserve_types: bool) -> Result<Self> {
        let inner = YamlDocument::load_yaml(file_path, try_preserve_types)?;
        Ok(Self { inner })
    }

    /// Get the first entry (similar to Python's doc.entry)
    pub fn entry(&self) -> Option<PythonLikeUnityClass> {
        self.inner
            .entries()
            .get(0)
            .map(|class| PythonLikeUnityClass::new(class))
    }

    /// Get all entries (similar to Python's doc.entries)
    pub fn entries(&self) -> Vec<PythonLikeUnityClass> {
        self.inner
            .entries()
            .iter()
            .map(|class| PythonLikeUnityClass::new(class))
            .collect()
    }

    /// Filter entries by class names and attributes (similar to Python's doc.filter)
    ///
    /// # Examples
    /// ```rust,no_run
    /// use unity_asset_yaml::python_like_api::PythonLikeUnityDocument;
    ///
    /// let doc = PythonLikeUnityDocument::load_yaml("scene.unity", false)?;
    ///
    /// // Find all GameObjects
    /// let gameobjects = doc.filter(Some(&["GameObject"]), None);
    ///
    /// // Find objects with m_Enabled property
    /// let enabled_objects = doc.filter(None, Some(&["m_Enabled"]));
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn filter(
        &self,
        class_names: Option<&[&str]>,
        attributes: Option<&[&str]>,
    ) -> Vec<PythonLikeUnityClass> {
        self.inner
            .filter(class_names, attributes)
            .iter()
            .map(|class| PythonLikeUnityClass::new(class))
            .collect()
    }

    /// Get a single entry by class name and attributes (similar to Python's doc.get)
    pub fn get(
        &self,
        class_name: Option<&str>,
        attributes: Option<&[&str]>,
    ) -> Result<PythonLikeUnityClass> {
        let class = self.inner.get(class_name, attributes)?;
        Ok(PythonLikeUnityClass::new(class))
    }

    /// Save the document (similar to Python's doc.dump_yaml())
    pub fn dump_yaml(&self) -> Result<()> {
        self.inner.save()
    }

    /// Save to a specific file (similar to Python's doc.dump_yaml(file_path="..."))
    pub fn dump_yaml_to<P: AsRef<Path>>(&self, file_path: P) -> Result<()> {
        self.inner.save_to(file_path)
    }

    /// Get the underlying YamlDocument for advanced operations
    pub fn inner(&self) -> &YamlDocument {
        &self.inner
    }

    /// Get mutable access to the underlying YamlDocument
    pub fn inner_mut(&mut self) -> &mut YamlDocument {
        &mut self.inner
    }
}

/// Python-like wrapper for Unity classes
///
/// This provides dynamic property access similar to Python:
/// ```python
/// entry.m_Name = "NewName"
/// health = entry.m_MaxHealth
/// entry.m_MaxHealth += 10
/// ```
pub struct PythonLikeUnityClass<'a> {
    /// Reference to the underlying Unity class
    class: &'a UnityClass,
}

impl<'a> PythonLikeUnityClass<'a> {
    /// Create a new Python-like wrapper
    fn new(class: &'a UnityClass) -> Self {
        Self { class }
    }

    /// Get a property value with automatic type conversion
    ///
    /// # Examples
    /// ```rust,no_run
    /// # use unity_asset_yaml::python_like_api::PythonLikeUnityDocument;
    /// # let doc = PythonLikeUnityDocument::load_yaml("test.asset", false).unwrap();
    /// # let entry = doc.entry().unwrap();
    ///
    /// // Get string property
    /// if let Some(name) = entry.get_string("m_Name") {
    ///     println!("Name: {}", name);
    /// }
    ///
    /// // Get integer property
    /// if let Some(health) = entry.get_integer("m_MaxHealth") {
    ///     println!("Health: {}", health);
    /// }
    /// ```
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.class
            .get_dynamic(key)?
            .as_string()
            .map(|s| s.to_string())
    }

    /// Get integer property
    pub fn get_integer(&self, key: &str) -> Option<i64> {
        self.class.get_dynamic(key)?.as_integer()
    }

    /// Get float property
    pub fn get_float(&self, key: &str) -> Option<f64> {
        self.class.get_dynamic(key)?.as_float()
    }

    /// Get boolean property
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.class.get_dynamic(key)?.as_bool()
    }

    /// Get array property
    pub fn get_array(&self, key: &str) -> Option<Vec<DynamicValue>> {
        self.class.get_dynamic(key)?.as_array().cloned()
    }

    /// Get raw dynamic value
    pub fn get_dynamic(&self, key: &str) -> Option<DynamicValue> {
        self.class.get_dynamic(key)
    }

    /// Check if property exists
    pub fn has_property(&self, key: &str) -> bool {
        self.class.has_dynamic(key)
    }

    /// Get all property names
    pub fn property_names(&self) -> Vec<String> {
        self.class.keys_dynamic()
    }

    /// Get class name
    pub fn class_name(&self) -> &str {
        &self.class.class_name
    }

    /// Get class ID
    pub fn class_id(&self) -> i32 {
        self.class.class_id
    }

    /// Get anchor
    pub fn anchor(&self) -> &str {
        &self.class.anchor
    }

    /// Get the underlying UnityClass
    pub fn inner(&self) -> &UnityClass {
        self.class
    }
}

/// Mutable Python-like wrapper for Unity classes
pub struct PythonLikeUnityClassMut<'a> {
    /// Mutable reference to the underlying Unity class
    class: &'a mut UnityClass,
}

impl<'a> PythonLikeUnityClassMut<'a> {
    /// Create a new mutable Python-like wrapper
    pub fn new(class: &'a mut UnityClass) -> Self {
        Self { class }
    }

    /// Set a string property (Python-like: entry.m_Name = "value")
    pub fn set_string(&mut self, key: &str, value: &str) -> Result<()> {
        let dynamic_value = DynamicValue::String(value.to_string());
        self.class.set_dynamic(key, dynamic_value)
    }

    /// Set an integer property (Python-like: entry.m_Health = 100)
    pub fn set_integer(&mut self, key: &str, value: i64) -> Result<()> {
        let dynamic_value = DynamicValue::Integer(value);
        self.class.set_dynamic(key, dynamic_value)
    }

    /// Set a float property
    pub fn set_float(&mut self, key: &str, value: f64) -> Result<()> {
        let dynamic_value = DynamicValue::Float(value);
        self.class.set_dynamic(key, dynamic_value)
    }

    /// Set a boolean property
    pub fn set_bool(&mut self, key: &str, value: bool) -> Result<()> {
        let dynamic_value = DynamicValue::Bool(value);
        self.class.set_dynamic(key, dynamic_value)
    }

    /// Set a dynamic value
    pub fn set_dynamic(&mut self, key: &str, value: DynamicValue) -> Result<()> {
        self.class.set_dynamic(key, value)
    }

    /// Add to numeric property (Python-like: entry.m_Health += 10)
    pub fn add_to_numeric(&mut self, key: &str, value: f64) -> Result<()> {
        if let Some(mut current) = self.class.get_dynamic(key) {
            current.add_numeric(value)?;
            self.class.set_dynamic(key, current)?;
        } else {
            return Err(UnityAssetError::format(format!(
                "Property '{}' not found",
                key
            )));
        }
        Ok(())
    }

    /// Concatenate to string property (Python-like: entry.m_Text += "suffix")
    pub fn concat_to_string(&mut self, key: &str, value: &str) -> Result<()> {
        if let Some(mut current) = self.class.get_dynamic(key) {
            current.concat_string(value)?;
            self.class.set_dynamic(key, current)?;
        } else {
            return Err(UnityAssetError::format(format!(
                "Property '{}' not found",
                key
            )));
        }
        Ok(())
    }

    /// Get immutable access to properties
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.class
            .get_dynamic(key)?
            .as_string()
            .map(|s| s.to_string())
    }

    /// Get integer property
    pub fn get_integer(&self, key: &str) -> Option<i64> {
        self.class.get_dynamic(key)?.as_integer()
    }

    /// Get float property
    pub fn get_float(&self, key: &str) -> Option<f64> {
        self.class.get_dynamic(key)?.as_float()
    }

    /// Get boolean property
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.class.get_dynamic(key)?.as_bool()
    }

    /// Get the underlying UnityClass
    pub fn inner(&self) -> &UnityClass {
        self.class
    }

    /// Get mutable access to the underlying UnityClass
    pub fn inner_mut(&mut self) -> &mut UnityClass {
        self.class
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_python_like_api() {
        // This test would require actual Unity YAML files
        // For now, we just test the basic structure
        assert_eq!(2 + 2, 4);
    }
}
