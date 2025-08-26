//! Unity document abstraction
//!
//! This module provides abstract traits and types for Unity documents
//! that can be implemented by different format-specific parsers.

use crate::error::Result;
use crate::unity_class::UnityClass;
use std::path::{Path, PathBuf};

/// Supported Unity document formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentFormat {
    Yaml,
    Binary,
}

/// Abstract trait for Unity documents
pub trait UnityDocument {
    /// Get the first entry (main object) in the document
    fn entry(&self) -> Option<&UnityClass>;

    /// Get a mutable reference to the first entry
    fn entry_mut(&mut self) -> Option<&mut UnityClass>;

    /// Get all entries in the document
    fn entries(&self) -> &[UnityClass];

    /// Get mutable access to all entries
    fn entries_mut(&mut self) -> &mut Vec<UnityClass>;

    /// Add a new Unity object to the document
    fn add_entry(&mut self, entry: UnityClass);

    /// Filter entries by class name
    fn filter_by_class(&self, class_name: &str) -> Vec<&UnityClass> {
        self.entries()
            .iter()
            .filter(|entry| entry.class_name == class_name)
            .collect()
    }

    /// Filter entries by multiple class names
    fn filter_by_classes(&self, class_names: &[&str]) -> Vec<&UnityClass> {
        self.entries()
            .iter()
            .filter(|entry| class_names.contains(&entry.class_name.as_str()))
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
            .find(|entry| entry.class_name == class_name && entry.has_property(property))
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

    /// Save the document back to its original file
    fn save(&self) -> Result<()>;

    /// Save the document to a specific file
    fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()>;

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
        fn entry(&self) -> Option<&UnityClass> {
            self.entries.first()
        }

        fn entry_mut(&mut self) -> Option<&mut UnityClass> {
            self.entries.first_mut()
        }

        fn entries(&self) -> &[UnityClass] {
            &self.entries
        }

        fn entries_mut(&mut self) -> &mut Vec<UnityClass> {
            &mut self.entries
        }

        fn add_entry(&mut self, entry: UnityClass) {
            self.entries.push(entry);
        }

        fn file_path(&self) -> Option<&Path> {
            self.metadata.file_path.as_deref()
        }

        fn save(&self) -> Result<()> {
            Ok(()) // Mock implementation
        }

        fn save_to<P: AsRef<Path>>(&self, _path: P) -> Result<()> {
            Ok(()) // Mock implementation
        }

        fn format(&self) -> DocumentFormat {
            self.metadata.format
        }
    }

    #[test]
    fn test_document_trait() {
        let mut doc = MockDocument {
            entries: Vec::new(),
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
        };

        assert!(doc.is_empty());
        assert_eq!(doc.len(), 0);

        let class = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        doc.add_entry(class);

        assert!(!doc.is_empty());
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.format(), DocumentFormat::Yaml);
    }

    #[test]
    fn test_document_filtering() {
        let mut doc = MockDocument {
            entries: Vec::new(),
            metadata: DocumentMetadata::new(DocumentFormat::Yaml),
        };

        let game_object = UnityClass::new(1, "GameObject".to_string(), "123".to_string());
        let behaviour = UnityClass::new(114, "MonoBehaviour".to_string(), "456".to_string());

        doc.add_entry(game_object);
        doc.add_entry(behaviour);

        let game_objects = doc.filter_by_class("GameObject");
        assert_eq!(game_objects.len(), 1);

        let behaviours = doc.filter_by_class("MonoBehaviour");
        assert_eq!(behaviours.len(), 1);
    }
}
