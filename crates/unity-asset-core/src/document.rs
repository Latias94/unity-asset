//! Unity document abstraction
//!
//! This module provides abstract traits and types for Unity documents
//! that can be implemented by different format-specific parsers.

use crate::unity_class::UnityClass;
use std::path::{Path, PathBuf};

#[cfg(feature = "async")]
use crate::{AssetLoadBudget, error::Result};
#[cfg(feature = "async")]
use async_trait::async_trait;
#[cfg(feature = "async")]
use futures::Stream;

/// Supported Unity document formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    Yaml,
    Binary,
}

/// Abstract trait for Unity documents
pub trait UnityDocument {
    /// Get the first entry (main object) in the document
    fn entry(&self) -> Option<&UnityClass> {
        self.entries().first()
    }

    /// Get all entries in the document
    fn entries(&self) -> &[UnityClass];

    /// Filter entries by class name
    fn filter_by_class(&self, class_name: &str) -> Vec<&UnityClass> {
        self.entries()
            .iter()
            .filter(|entry| entry.class_name() == class_name)
            .collect()
    }

    /// Filter entries by multiple class names
    fn filter_by_classes(&self, class_names: &[&str]) -> Vec<&UnityClass> {
        self.entries()
            .iter()
            .filter(|entry| class_names.contains(&entry.class_name()))
            .collect()
    }

    /// Filter entries by a custom predicate
    fn filter<F>(&self, predicate: F) -> Vec<&UnityClass>
    where
        F: Fn(&UnityClass) -> bool,
    {
        self.entries()
            .iter()
            .filter(|entry| predicate(entry))
            .collect()
    }

    /// Find a single entry by class name and optional property filter
    fn find_by_class_and_property(&self, class_name: &str, property: &str) -> Option<&UnityClass> {
        self.entries()
            .iter()
            .find(|entry| entry.class_name() == class_name && entry.has_property(property))
    }

    /// Get the file path this document was loaded from
    fn file_path(&self) -> Option<&Path>;

    /// Check if the document is empty
    fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    /// Get the number of entries in the document
    fn len(&self) -> usize {
        self.entries().len()
    }

    /// Get the document format
    fn format(&self) -> DocumentFormat;
}

/// Document metadata
#[derive(Debug, Clone)]
pub struct DocumentMetadata {
    /// Path to the source file
    pub file_path: Option<PathBuf>,
    /// Document format
    pub format: DocumentFormat,
    /// Format-specific version information
    pub version: Option<String>,
    /// Format-specific tags or metadata
    pub metadata: std::collections::HashMap<String, String>,
}

impl DocumentMetadata {
    /// Create new metadata
    pub fn new(format: DocumentFormat) -> Self {
        Self {
            file_path: None,
            format,
            version: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    /// Set file path
    pub fn with_file_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.file_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set version
    pub fn with_version<S: Into<String>>(mut self, version: S) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Add metadata entry
    pub fn with_metadata<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Async version of UnityDocument trait for non-blocking I/O operations
#[cfg(feature = "async")]
#[async_trait]
pub trait AsyncUnityDocument: Send + Sync {
    type LoadError: std::error::Error + Send + Sync + 'static;

    /// Loads a document from a file path under one caller-owned budget.
    async fn load_from_path_async(
        path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> std::result::Result<Self, Self::LoadError>
    where
        Self: Sized;

    /// Get all entries in the document (sync access for already loaded data)
    fn entries(&self) -> &[UnityClass];

    /// Get the first entry (if any) (sync access)
    fn entry(&self) -> Option<&UnityClass> {
        self.entries().first()
    }

    /// Get document file path (sync access)
    fn file_path(&self) -> Option<&Path>;

    /// Stream entries for processing large documents without loading all into memory
    fn entries_stream(&self) -> impl Stream<Item = &UnityClass> + Send {
        futures::stream::iter(self.entries())
    }

    /// Process entries with an async function
    async fn process_entries<F, Fut>(&self, mut processor: F) -> Result<()>
    where
        F: FnMut(&UnityClass) -> Fut + Send,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        for entry in self.entries() {
            processor(entry).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unity_class::UnityClass;

    // Mock implementation for testing
    struct MockDocument {
        entries: Vec<UnityClass>,
        metadata: DocumentMetadata,
    }

    impl UnityDocument for MockDocument {
        fn entries(&self) -> &[UnityClass] {
            &self.entries
        }

        fn file_path(&self) -> Option<&Path> {
            self.metadata.file_path.as_deref()
        }

        fn format(&self) -> DocumentFormat {
            self.metadata.format
        }
    }

    #[test]
    fn test_document_trait() {
        let doc = MockDocument {
            entries: vec![UnityClass::new(
                1,
                "GameObject".to_string(),
                "123".to_string(),
            )],
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
        };

        assert!(!doc.is_empty());
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.format(), DocumentFormat::Yaml);
    }

    #[test]
    fn test_document_filtering() {
        let doc = MockDocument {
            entries: vec![
                UnityClass::new(1, "GameObject".to_string(), "123".to_string()),
                UnityClass::new(114, "MonoBehaviour".to_string(), "456".to_string()),
            ],
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
        };

        let game_objects = doc.filter_by_class("GameObject");
        assert_eq!(game_objects.len(), 1);

        let behaviours = doc.filter_by_class("MonoBehaviour");
        assert_eq!(behaviours.len(), 1);
    }
}
