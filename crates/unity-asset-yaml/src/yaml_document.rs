//! YAML-specific Unity document implementation
//!
//! This module provides the concrete implementation of UnityDocument
//! for YAML format files.

use std::path::{Path, PathBuf};
use unity_asset_core::{
    DocumentFormat, LineEnding, Result, UnityAssetError, UnityClass, UnityDocument,
    document::DocumentMetadata,
};

#[cfg(feature = "async")]
use async_trait::async_trait;
#[cfg(feature = "async")]
use unity_asset_core::{AssetLoadBudget, document::AsyncUnityDocument};

/// A Unity YAML document containing one or more Unity objects
#[derive(Debug, Clone)]
pub struct YamlDocument {
    /// The Unity objects in this document
    data: Vec<UnityClass>,
    /// Document metadata
    metadata: DocumentMetadata,
    /// Line ending style used in the original file
    newline: LineEnding,
}

impl YamlDocument {
    /// Creates an immutable YAML document from fully constructed entries.
    pub fn from_entries(data: Vec<UnityClass>) -> Self {
        Self {
            data,
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
            newline: LineEnding::default(),
        }
    }

    /// Returns the primary object when the document is not empty.
    pub fn entry(&self) -> Option<&UnityClass> {
        self.data.first()
    }

    /// Returns every object in document order.
    pub fn entries(&self) -> &[UnityClass] {
        &self.data
    }

    /// Returns the path used to load this document, when available.
    pub fn file_path(&self) -> Option<&Path> {
        self.metadata.file_path.as_deref()
    }

    /// Returns whether the document contains no objects.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the number of objects in the document.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns the concrete document format.
    pub const fn format(&self) -> DocumentFormat {
        DocumentFormat::Yaml
    }

    pub(crate) fn set_file_path(&mut self, path: PathBuf) {
        self.metadata.file_path = Some(path);
    }

    /// Get the line ending style
    pub fn line_ending(&self) -> LineEnding {
        self.newline
    }

    /// Get the YAML version
    pub fn version(&self) -> Option<&str> {
        self.metadata.version.as_deref()
    }

    /// Get the YAML metadata
    pub fn yaml_metadata(&self) -> &std::collections::HashMap<String, String> {
        &self.metadata.metadata
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
    /// use unity_asset_core::AssetLoadBudget;
    /// use unity_asset_yaml::load_budgeted_yaml_path;
    ///
    /// let mut budget = AssetLoadBudget::default();
    /// let source = load_budgeted_yaml_path("scene.unity", &mut budget)?;
    /// let doc = source.document();
    ///
    /// // Find all GameObjects
    /// let gameobjects = doc.filter(Some(&["GameObject"]), None);
    ///
    /// // Find all objects with m_Enabled property
    /// let enabled_objects = doc.filter(None, Some(&["m_Enabled"]));
    ///
    /// // Find MonoBehaviours with m_Script property
    /// let scripts = doc.filter(Some(&["MonoBehaviour"]), Some(&["m_Script"]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
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
                if let Some(names) = class_names
                    && !names.is_empty()
                    && !names.contains(&entry.class_name())
                {
                    return false;
                }

                // Check attribute filter
                if let Some(attrs) = attributes
                    && !attrs.is_empty()
                {
                    for attr in attrs {
                        if !entry.has_property(attr) {
                            return false;
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
    /// use unity_asset_core::AssetLoadBudget;
    /// use unity_asset_yaml::load_budgeted_yaml_path;
    ///
    /// let mut budget = AssetLoadBudget::default();
    /// let source = load_budgeted_yaml_path("scene.unity", &mut budget)?;
    /// let doc = source.document();
    ///
    /// // Get the first GameObject
    /// let gameobject = doc.get(Some("GameObject"), None)?;
    ///
    /// // Get an object with specific attributes
    /// let script = doc.get(Some("MonoBehaviour"), Some(&["m_Script", "m_Enabled"]))?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
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
    fn entries(&self) -> &[UnityClass] {
        &self.data
    }

    fn file_path(&self) -> Option<&Path> {
        self.metadata.file_path.as_deref()
    }

    fn format(&self) -> DocumentFormat {
        DocumentFormat::Yaml
    }
}

/// Async implementation of UnityDocument trait for YamlDocument
#[cfg(feature = "async")]
#[async_trait]
impl AsyncUnityDocument for YamlDocument {
    type LoadError = crate::BudgetedYamlError;

    async fn load_from_path_async(
        path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Self, Self::LoadError>
    where
        Self: Sized,
    {
        let source = crate::load_budgeted_yaml_path_async(path, budget).await?;
        let (_, document) = source.into_budgeted_parts(budget)?;
        Ok(std::sync::Arc::try_unwrap(document)
            .expect("a newly loaded YAML source uniquely owns its document"))
    }

    fn entries(&self) -> &[UnityClass] {
        &self.data
    }

    fn file_path(&self) -> Option<&Path> {
        self.metadata.file_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::UnityClass;

    #[test]
    fn test_yaml_document_creation() {
        let doc = YamlDocument::from_entries(Vec::new());
        assert!(doc.is_empty());
        assert_eq!(doc.len(), 0);
        assert_eq!(doc.format(), DocumentFormat::Yaml);
    }

    #[test]
    fn test_yaml_document_from_entries() {
        let class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        let doc = YamlDocument::from_entries(vec![class]);
        assert_eq!(doc.len(), 1);
        assert!(!doc.is_empty());
    }

    #[test]
    fn test_yaml_document_filter() {
        let class1 = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        let class2 = UnityClass::new(114, "MonoBehaviour".to_string(), "456".to_string());
        let doc = YamlDocument::from_entries(vec![class1, class2]);

        let game_objects = doc.filter_by_class("GameObject");
        assert_eq!(game_objects.len(), 1);

        let behaviours = doc.filter_by_class("MonoBehaviour");
        assert_eq!(behaviours.len(), 1);
    }

    #[test]
    fn test_yaml_document_metadata() {
        let doc = YamlDocument::from_entries(Vec::new());
        assert_eq!(doc.format(), DocumentFormat::Yaml);
        assert_eq!(doc.line_ending(), LineEnding::default());
        assert!(doc.version().is_none());
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn test_async_yaml_document_creation() {
        use futures::StreamExt;
        use unity_asset_core::document::AsyncUnityDocument;

        // Test that the async trait methods compile and work
        let doc = YamlDocument::from_entries(Vec::new());
        assert!(AsyncUnityDocument::entries(&doc).is_empty());
        assert!(AsyncUnityDocument::entry(&doc).is_none());
        assert!(AsyncUnityDocument::file_path(&doc).is_none());

        // Test stream functionality
        let mut stream = doc.entries_stream();
        assert!(stream.next().await.is_none());
    }
}
