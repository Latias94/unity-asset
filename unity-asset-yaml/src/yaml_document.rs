//! YAML-specific Unity document implementation
//!
//! This module provides the concrete implementation of UnityDocument
//! for YAML format files.

use crate::unity_yaml_serializer::UnityYamlSerializer;
use std::fs;
use std::path::Path;
use unity_asset_core::{
    DocumentFormat, LineEnding, Result, UnityAssetError, UnityClass, UnityDocument,
    document::DocumentMetadata,
};

/// A Unity YAML document containing one or more Unity objects
#[derive(Debug)]
pub struct YamlDocument {
    /// The Unity objects in this document
    data: Vec<UnityClass>,
    /// Document metadata
    metadata: DocumentMetadata,
    /// Line ending style used in the original file
    newline: LineEnding,
}

impl YamlDocument {
    /// Create a new empty YAML document
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
            newline: LineEnding::default(),
        }
    }

    /// Load a Unity YAML file
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the YAML file to load
    /// * `preserve_types` - If true, try to preserve int/float types instead of converting all to strings
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn load_yaml<P: AsRef<Path>>(path: P, _preserve_types: bool) -> Result<Self> {
        use crate::serde_unity_loader::SerdeUnityLoader;
        use std::fs::File;
        use std::io::BufReader;

        let path = path.as_ref();

        // Read the file
        let file = File::open(path).map_err(|e| {
            UnityAssetError::format(format!("Failed to open file {}: {}", path.display(), e))
        })?;
        let reader = BufReader::new(file);

        // Use serde-based loader
        let loader = SerdeUnityLoader::new();
        let unity_classes = loader.load_from_reader(reader)?;

        // Create YamlDocument with metadata
        let mut yaml_doc = YamlDocument::new();
        yaml_doc.metadata.file_path = Some(path.to_path_buf());

        // Add all loaded classes
        for unity_class in unity_classes {
            yaml_doc.add_entry(unity_class);
        }

        Ok(yaml_doc)
    }

    /// Get the line ending style
    pub fn line_ending(&self) -> LineEnding {
        self.newline
    }

    /// Set the line ending style
    pub fn set_line_ending(&mut self, newline: LineEnding) {
        self.newline = newline;
    }

    /// Get the YAML version
    pub fn version(&self) -> Option<&str> {
        self.metadata.version.as_deref()
    }

    /// Get the YAML metadata
    pub fn yaml_metadata(&self) -> &std::collections::HashMap<String, String> {
        &self.metadata.metadata
    }

    /// Save document to its original file
    ///
    /// This method saves the document back to the file it was loaded from.
    /// If the document was not loaded from a file, this will return an error.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let mut doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
    /// // ... modify the document ...
    /// doc.save()?;  // Save back to original file
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn save(&self) -> Result<()> {
        if let Some(path) = &self.metadata.file_path {
            self.save_to(path)
        } else {
            Err(UnityAssetError::format(
                "Cannot save document: no file path available. Use save_to() instead.".to_string(),
            ))
        }
    }

    /// Save document to a specific file
    ///
    /// This method serializes the document to Unity YAML format and saves it
    /// to the specified file path.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
    /// doc.save_to("ProjectSettings_backup.asset")?;
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();

        // Create serializer with document settings
        let mut serializer = UnityYamlSerializer::new().with_line_ending(self.newline);

        // Serialize to string
        let yaml_content = serializer.serialize_to_string(&self.data)?;

        // Write to file
        fs::write(path, yaml_content).map_err(UnityAssetError::from)?;

        Ok(())
    }

    /// Get YAML content as string
    ///
    /// This method serializes the document to Unity YAML format and returns
    /// it as a string without writing to a file.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;
    /// let yaml_string = doc.dump_yaml()?;
    /// println!("{}", yaml_string);
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn dump_yaml(&self) -> Result<String> {
        let mut serializer = UnityYamlSerializer::new().with_line_ending(self.newline);

        serializer.serialize_to_string(&self.data)
    }

    /// Filter entries by class names and/or attributes
    ///
    /// This method provides advanced filtering capabilities similar to the
    /// Python reference library's filter() method.
    ///
    /// # Arguments
    ///
    /// * `class_names` - Optional list of class names to filter by
    /// * `attributes` - Optional list of attribute names that entries must have
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let doc = YamlDocument::load_yaml("scene.unity", false)?;
    ///
    /// // Find all GameObjects
    /// let gameobjects = doc.filter(Some(&["GameObject"]), None);
    ///
    /// // Find all objects with m_Enabled property
    /// let enabled_objects = doc.filter(None, Some(&["m_Enabled"]));
    ///
    /// // Find MonoBehaviours with m_Script property
    /// let scripts = doc.filter(Some(&["MonoBehaviour"]), Some(&["m_Script"]));
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn filter(
        &self,
        class_names: Option<&[&str]>,
        attributes: Option<&[&str]>,
    ) -> Vec<&UnityClass> {
        self.data
            .iter()
            .filter(|entry| {
                // Check class name filter
                if let Some(names) = class_names {
                    if !names.is_empty() && !names.contains(&entry.class_name.as_str()) {
                        return false;
                    }
                }

                // Check attribute filter
                if let Some(attrs) = attributes {
                    if !attrs.is_empty() {
                        for attr in attrs {
                            if !entry.has_property(attr) {
                                return false;
                            }
                        }
                    }
                }

                true
            })
            .collect()
    }

    /// Get a single entry by class name and/or attributes
    ///
    /// This method returns the first entry that matches the criteria.
    /// Returns an error if no matching entry is found or if multiple entries match.
    ///
    /// # Arguments
    ///
    /// * `class_name` - Optional class name to match
    /// * `attributes` - Optional list of attribute names that the entry must have
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use unity_asset_yaml::YamlDocument;
    ///
    /// let doc = YamlDocument::load_yaml("scene.unity", false)?;
    ///
    /// // Get the first GameObject
    /// let gameobject = doc.get(Some("GameObject"), None)?;
    ///
    /// // Get an object with specific attributes
    /// let script = doc.get(Some("MonoBehaviour"), Some(&["m_Script", "m_Enabled"]))?;
    /// # Ok::<(), unity_asset_core::UnityAssetError>(())
    /// ```
    pub fn get(
        &self,
        class_name: Option<&str>,
        attributes: Option<&[&str]>,
    ) -> Result<&UnityClass> {
        let class_names = class_name.map(|name| vec![name]);
        let filtered = self.filter(class_names.as_deref(), attributes);

        match filtered.len() {
            0 => Err(UnityAssetError::format(format!(
                "No entry found matching criteria: class_name={:?}, attributes={:?}",
                class_name, attributes
            ))),
            1 => Ok(filtered[0]),
            n => Err(UnityAssetError::format(format!(
                "Multiple entries ({}) found matching criteria: class_name={:?}, attributes={:?}. Use filter() instead.",
                n, class_name, attributes
            ))),
        }
    }
}

impl UnityDocument for YamlDocument {
    fn entry(&self) -> Option<&UnityClass> {
        self.data.first()
    }

    fn entry_mut(&mut self) -> Option<&mut UnityClass> {
        self.data.first_mut()
    }

    fn entries(&self) -> &[UnityClass] {
        &self.data
    }

    fn entries_mut(&mut self) -> &mut Vec<UnityClass> {
        &mut self.data
    }

    fn add_entry(&mut self, entry: UnityClass) {
        self.data.push(entry);
    }

    fn file_path(&self) -> Option<&Path> {
        self.metadata.file_path.as_deref()
    }

    fn save(&self) -> Result<()> {
        match &self.metadata.file_path {
            Some(path) => self.save_to(path),
            None => Err(UnityAssetError::format("No file path specified for save")),
        }
    }

    fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();

        // Serialize the document to YAML format
        let yaml_content = self.dump_yaml()?;

        // Write to file
        std::fs::write(path, yaml_content)
            .map_err(|e| UnityAssetError::format(format!("Failed to write YAML file: {}", e)))?;

        Ok(())
    }

    fn format(&self) -> DocumentFormat {
        DocumentFormat::Yaml
    }
}

impl Default for YamlDocument {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::UnityClass;

    #[test]
    fn test_yaml_document_creation() {
        let doc = YamlDocument::new();
        assert!(doc.is_empty());
        assert_eq!(doc.len(), 0);
        assert_eq!(doc.format(), DocumentFormat::Yaml);
    }

    #[test]
    fn test_yaml_document_add_entry() {
        let mut doc = YamlDocument::new();
        let class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());

        doc.add_entry(class);
        assert_eq!(doc.len(), 1);
        assert!(!doc.is_empty());
    }

    #[test]
    fn test_yaml_document_filter() {
        let mut doc = YamlDocument::new();

        let class1 = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        let class2 = UnityClass::new(114, "MonoBehaviour".to_string(), "456".to_string());

        doc.add_entry(class1);
        doc.add_entry(class2);

        let game_objects = doc.filter_by_class("GameObject");
        assert_eq!(game_objects.len(), 1);

        let behaviours = doc.filter_by_class("MonoBehaviour");
        assert_eq!(behaviours.len(), 1);
    }

    #[test]
    fn test_yaml_document_metadata() {
        let doc = YamlDocument::new();
        assert_eq!(doc.format(), DocumentFormat::Yaml);
        assert_eq!(doc.line_ending(), LineEnding::default());
        assert!(doc.version().is_none());
    }
}
