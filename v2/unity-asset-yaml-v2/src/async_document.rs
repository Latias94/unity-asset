//! Async YAML document
//!
//! Core async YAML document implementation with streaming object access.

use async_trait::async_trait;
use futures::Stream;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncSeek};
use tracing::{info, instrument};

use unity_asset_core_v2::{
    AsyncUnityClass, AsyncUnityDocument, LoadProgress, ObjectMetadata, Result, UnityAssetError,
    UnityValue,
};

use crate::async_loader::YamlLoader;

/// Async YAML document with streaming capabilities
#[derive(Debug, Clone)]
pub struct YamlDocument {
    classes: Vec<AsyncUnityClass>,
    metadata: ObjectMetadata,
    loader: Arc<YamlLoader>,
}

impl YamlDocument {
    /// Create new YAML document
    pub fn new(classes: Vec<AsyncUnityClass>, metadata: ObjectMetadata) -> Self {
        Self {
            classes,
            metadata,
            loader: Arc::new(YamlLoader::new()),
        }
    }

    /// Create with custom loader
    pub fn with_loader(
        classes: Vec<AsyncUnityClass>,
        metadata: ObjectMetadata,
        loader: YamlLoader,
    ) -> Self {
        Self {
            classes,
            metadata,
            loader: Arc::new(loader),
        }
    }

    /// Get all classes (synchronous access)
    pub fn classes(&self) -> &[AsyncUnityClass] {
        &self.classes
    }

    /// Get classes by type
    pub fn classes_by_type(&self, class_name: &str) -> Vec<&AsyncUnityClass> {
        self.classes
            .iter()
            .filter(|class| class.class_name() == class_name)
            .collect()
    }

    /// Find class by name
    pub fn find_class_by_name(&self, name: &str) -> Option<&AsyncUnityClass> {
        self.classes
            .iter()
            .find(|class| class.name().as_deref() == Some(name))
    }

    /// Get class count
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// Check if document is empty
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Get unique class types
    pub fn class_types(&self) -> Vec<String> {
        let mut types: Vec<String> = self
            .classes
            .iter()
            .map(|c| c.class_name().to_string())
            .collect();
        types.sort();
        types.dedup();
        types
    }

    /// Get statistics
    pub fn statistics(&self) -> DocumentStatistics {
        let mut stats = DocumentStatistics::default();
        stats.total_classes = self.classes.len();

        let mut type_counts = std::collections::HashMap::new();
        for class in &self.classes {
            *type_counts
                .entry(class.class_name().to_string())
                .or_insert(0) += 1;
        }
        stats.class_type_counts = type_counts;

        stats.file_size = self.metadata.size_bytes;
        stats
    }

    /// Async save to file
    pub async fn save_to_file<P: AsRef<Path> + Send>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        info!("Saving YAML document to: {}", path.display());

        let yaml_content = self.serialize_to_yaml().await?;
        tokio::fs::write(path, yaml_content)
            .await
            .map_err(|e| UnityAssetError::Io(e.to_string()))?;

        info!("Successfully saved {} classes to YAML", self.classes.len());
        Ok(())
    }

    /// Serialize to YAML string
    #[instrument(skip(self))]
    pub async fn serialize_to_yaml(&self) -> Result<String> {
        let mut yaml_content = String::new();

        // Add YAML header
        yaml_content.push_str("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");

        for (index, class) in self.classes.iter().enumerate() {
            if index > 0 {
                yaml_content.push_str("---\n");
            }

            // Serialize individual class
            let class_yaml = self.serialize_class(class).await?;
            yaml_content.push_str(&class_yaml);
            yaml_content.push('\n');

            // Yield for large documents
            if index % 100 == 99 {
                tokio::task::yield_now().await;
            }
        }

        Ok(yaml_content)
    }

    /// Serialize single class to YAML (improved based on V1)
    #[instrument(skip(self, class))]
    async fn serialize_class(&self, class: &AsyncUnityClass) -> Result<String> {
        let mut output = String::new();

        // Write document separator with Unity tag and anchor
        output.push_str(&format!("--- !u!{} &{}", class.class_id, class.anchor));

        // Write extra anchor data if present
        if !class.extra_anchor_data.is_empty() {
            output.push_str(&format!(" {}", class.extra_anchor_data));
        }
        output.push('\n');

        // Write class name
        output.push_str(&format!("{}:\n", class.class_name()));

        // Serialize properties with proper indentation
        for (key, value) in class.properties() {
            let serialized_value = self.serialize_unity_value(value, 1).await?;
            output.push_str(&format!("  {}: {}", key, serialized_value));
        }

        Ok(output)
    }

    /// Serialize UnityValue with proper Unity YAML formatting
    fn serialize_unity_value<'a>(
        &'a self,
        value: &'a UnityValue,
        indent_level: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let indent = "  ".repeat(indent_level);

            match value {
                UnityValue::Null => Ok("{fileID: 0}\n".to_string()),
                UnityValue::Bool(b) => Ok(format!("{}\n", if *b { "1" } else { "0" })),
                UnityValue::Int(i) => Ok(format!("{}\n", i)),
                UnityValue::Int32(i) => Ok(format!("{}\n", i)),
                UnityValue::UInt32(u) => Ok(format!("{}\n", u)),
                UnityValue::Int64(i) => Ok(format!("{}\n", i)),
                UnityValue::UInt64(u) => Ok(format!("{}\n", u)),
                UnityValue::Float(f) => Ok(format!("{}\n", f)),
                UnityValue::Double(d) => Ok(format!("{}\n", d)),
                UnityValue::String(s) => {
                    if self.needs_quoting(s) {
                        Ok(format!("\"{}\"\n", self.escape_string(s)))
                    } else {
                        Ok(format!("{}\n", s))
                    }
                }
                UnityValue::Array(arr) => {
                    let mut result = String::new();
                    result.push('\n');
                    for (i, item) in arr.iter().enumerate() {
                        result.push_str(&format!("{}  - ", indent));
                        let item_str = self.serialize_unity_value(item, indent_level + 1).await?;
                        result.push_str(&item_str.trim_start());
                    }
                    Ok(result)
                }
                UnityValue::Object(obj) => {
                    let mut result = String::new();
                    result.push('\n');
                    for (key, val) in obj {
                        result.push_str(&format!("{}  {}: ", indent, key));
                        let val_str = self.serialize_unity_value(val, indent_level + 1).await?;
                        result.push_str(&val_str.trim_start());
                    }
                    Ok(result)
                }
                UnityValue::Bytes(_) => Ok("{fileID: 0}\n".to_string()), // Simplified for now
            }
        })
    }

    /// Check if string needs quoting (based on V1 logic)
    fn needs_quoting(&self, s: &str) -> bool {
        s.is_empty()
            || s.contains('\n')
            || s.contains('\r')
            || s.contains('"')
            || s.contains('\'')
            || s.starts_with(' ')
            || s.ends_with(' ')
            || s.contains(':')
            || s.contains('#')
    }

    /// Escape string for YAML (based on V1 logic)
    fn escape_string(&self, s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }

    /// Create filtered copy with only specified class types
    pub async fn filter_by_types(&self, class_types: &[&str]) -> Result<YamlDocument> {
        let filtered_classes: Vec<AsyncUnityClass> = self
            .classes
            .iter()
            .filter(|class| class_types.contains(&class.class_name()))
            .cloned()
            .collect();

        let mut new_metadata = self.metadata.clone();
        new_metadata
            .properties
            .insert("filtered_types".to_string(), class_types.join(","));

        Ok(YamlDocument::with_loader(
            filtered_classes,
            new_metadata,
            (*self.loader).clone(),
        ))
    }

    /// Merge with another document
    pub async fn merge_with(&self, other: &YamlDocument) -> Result<YamlDocument> {
        let mut merged_classes = self.classes.clone();
        merged_classes.extend(other.classes.iter().cloned());

        let mut new_metadata = self.metadata.clone();
        new_metadata.properties.insert(
            "merged_with".to_string(),
            format!("{} classes", other.classes.len()),
        );
        new_metadata.size_bytes += other.metadata.size_bytes;

        Ok(YamlDocument::with_loader(
            merged_classes,
            new_metadata,
            (*self.loader).clone(),
        ))
    }

    /// Reload document from file
    #[instrument(skip(self))]
    pub async fn reload(&mut self) -> Result<()> {
        if let Some(file_path) = self.metadata.file_path.clone() {
            let new_doc = self.loader.load_from_path(&file_path).await?;
            self.classes = new_doc.classes;
            self.metadata = new_doc.metadata;
            info!("Reloaded document from {}", file_path);
        } else {
            return Err(UnityAssetError::validation(
                "file_path",
                "No file path available for reload",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl AsyncUnityDocument for YamlDocument {
    /// Load from file path asynchronously
    async fn load_from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path> + Send,
        Self: Sized,
    {
        let loader = YamlLoader::new();
        loader.load_from_path(path).await
    }

    /// Load from async stream
    async fn load_from_stream<S>(stream: S) -> Result<Self>
    where
        S: AsyncRead + AsyncSeek + Send + Unpin + 'static,
        Self: Sized,
    {
        let loader = YamlLoader::new();
        loader.load_from_reader(stream, None).await
    }

    /// Load with progress callback
    async fn load_with_progress<P, F>(path: P, progress_callback: F) -> Result<Self>
    where
        P: AsRef<Path> + Send,
        F: Fn(LoadProgress) + Send + Sync + 'static,
        Self: Sized,
    {
        let loader = YamlLoader::new();
        loader.load_with_progress(path, progress_callback).await
    }

    /// Get object stream
    fn objects_stream(&self) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_ {
        futures::stream::iter(self.classes.iter().cloned().map(Ok))
    }

    /// Filter objects stream by class types
    fn filter_objects_stream(
        &self,
        class_names: &[&str],
    ) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_ {
        let class_names_owned: Vec<String> = class_names.iter().map(|s| s.to_string()).collect();

        futures::stream::iter(
            self.classes
                .iter()
                .filter(move |class| {
                    class_names_owned
                        .iter()
                        .any(|name| class.class_name() == name)
                })
                .cloned()
                .map(Ok),
        )
    }

    /// Save to file asynchronously
    async fn save_to_path<P>(&self, path: P) -> Result<()>
    where
        P: AsRef<Path> + Send,
    {
        self.save_to_file(path).await
    }

    /// Get document metadata
    fn metadata(&self) -> &ObjectMetadata {
        &self.metadata
    }

    /// Get object count
    fn object_count(&self) -> u64 {
        self.classes.len() as u64
    }
}

/// Document statistics
#[derive(Debug, Default, Clone)]
pub struct DocumentStatistics {
    pub total_classes: usize,
    pub class_type_counts: std::collections::HashMap<String, usize>,
    pub file_size: u64,
}

impl DocumentStatistics {
    /// Get most common class type
    pub fn most_common_type(&self) -> Option<(&String, &usize)> {
        self.class_type_counts
            .iter()
            .max_by_key(|(_, &count)| count)
    }

    /// Get class type percentage
    pub fn type_percentage(&self, class_type: &str) -> f32 {
        if self.total_classes == 0 {
            return 0.0;
        }

        let count = self.class_type_counts.get(class_type).unwrap_or(&0);
        (*count as f32 / self.total_classes as f32) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use indexmap::IndexMap;
    use unity_asset_core_v2::{ObjectMetadata, UnityValue};

    fn create_test_class(class_name: &str, object_name: &str) -> AsyncUnityClass {
        let mut data = IndexMap::new();
        data.insert(
            "m_Name".to_string(),
            UnityValue::String(object_name.to_string()),
        );

        let mut class = AsyncUnityClass::new(1, class_name.to_string(), "0".to_string());
        *class.properties_mut() = data;
        class
    }

    #[tokio::test]
    async fn test_document_creation() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("Transform", "PlayerTransform"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        assert_eq!(doc.class_count(), 2);
        assert!(!doc.is_empty());
        assert_eq!(doc.class_types(), vec!["GameObject", "Transform"]);
    }

    #[tokio::test]
    async fn test_class_filtering() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("Transform", "PlayerTransform"),
            create_test_class("GameObject", "Enemy"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        let game_objects = doc.classes_by_type("GameObject");
        assert_eq!(game_objects.len(), 2);

        let player = doc.find_class_by_name("Player");
        assert!(player.is_some());
        assert_eq!(player.unwrap().class_name(), "GameObject");
    }

    #[tokio::test]
    async fn test_document_statistics() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("GameObject", "Enemy"),
            create_test_class("Transform", "PlayerTransform"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        let stats = doc.statistics();
        assert_eq!(stats.total_classes, 3);
        assert_eq!(stats.class_type_counts.get("GameObject"), Some(&2));
        assert_eq!(stats.class_type_counts.get("Transform"), Some(&1));

        let (most_common, count) = stats.most_common_type().unwrap();
        assert_eq!(most_common, "GameObject");
        assert_eq!(*count, 2);

        assert!((stats.type_percentage("GameObject") - 66.666664).abs() < 0.0001);
    }

    #[tokio::test]
    async fn test_document_streaming() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("Transform", "PlayerTransform"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        let mut stream = doc.objects_stream();
        let mut count = 0;

        while let Some(result) = stream.next().await {
            assert!(result.is_ok());
            count += 1;
        }

        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_filtered_streaming() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("Transform", "PlayerTransform"),
            create_test_class("GameObject", "Enemy"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        let mut stream = doc.filter_objects_stream(&["GameObject"]);
        let mut count = 0;

        while let Some(result) = stream.next().await {
            let class = result.unwrap();
            assert_eq!(class.class_name(), "GameObject");
            count += 1;
        }

        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_document_merge() {
        let classes1 = vec![create_test_class("GameObject", "Player")];
        let classes2 = vec![create_test_class("Transform", "PlayerTransform")];

        let doc1 = YamlDocument::new(classes1, ObjectMetadata::default());
        let doc2 = YamlDocument::new(classes2, ObjectMetadata::default());

        let merged = doc1.merge_with(&doc2).await.unwrap();
        assert_eq!(merged.class_count(), 2);
        assert_eq!(merged.class_types(), vec!["GameObject", "Transform"]);
    }

    #[tokio::test]
    async fn test_document_filtering() {
        let classes = vec![
            create_test_class("GameObject", "Player"),
            create_test_class("Transform", "PlayerTransform"),
            create_test_class("GameObject", "Enemy"),
        ];

        let metadata = ObjectMetadata::default();
        let doc = YamlDocument::new(classes, metadata);

        let filtered = doc.filter_by_types(&["GameObject"]).await.unwrap();
        assert_eq!(filtered.class_count(), 2);
        assert_eq!(filtered.class_types(), vec!["GameObject"]);
    }
}
