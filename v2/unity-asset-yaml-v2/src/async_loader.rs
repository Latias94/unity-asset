//! Async YAML loader
//!
//! High-performance async YAML loader with streaming support and concurrent processing.

use async_trait::async_trait;
use futures::Stream;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tracing::{debug, info, instrument, warn};

use unity_asset_core_v2::{
    AsyncAssetLoader, AsyncMetrics, AsyncUnityClass, LoadProgress, ObjectMetadata, Result,
    UnityAssetError, UnityValue,
};

use crate::async_document::YamlDocument;

/// Configuration for YAML loading
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Whether to preserve key order in objects
    pub preserve_order: bool,
    /// Maximum concurrent file loading
    pub max_concurrent_loads: usize,
    /// Buffer size for streaming
    pub buffer_size: usize,
    /// Enable progress callbacks
    pub enable_progress: bool,
    /// Parse anchor references
    pub resolve_anchors: bool,
    /// Maximum file size to process (bytes)
    pub max_file_size: usize,
}

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            preserve_order: false,
            max_concurrent_loads: 8,
            buffer_size: 8192,
            enable_progress: true,
            resolve_anchors: true,
            max_file_size: 100 * 1024 * 1024, // 100MB
        }
    }
}

impl LoaderConfig {
    /// Configuration optimized for large files
    pub fn for_large_files() -> Self {
        Self {
            preserve_order: false,
            max_concurrent_loads: 4,
            buffer_size: 32768,
            enable_progress: true,
            resolve_anchors: false,            // Skip for performance
            max_file_size: 1024 * 1024 * 1024, // 1GB
        }
    }

    /// Configuration optimized for small files
    pub fn for_small_files() -> Self {
        Self {
            preserve_order: true,
            max_concurrent_loads: 16,
            buffer_size: 1024,
            enable_progress: false,
            resolve_anchors: true,
            max_file_size: 10 * 1024 * 1024, // 10MB
        }
    }
}

/// YAML loader with streaming support
#[derive(Debug)]
pub struct YamlLoader {
    config: LoaderConfig,
    metrics: Arc<AsyncMetrics>,
}

impl YamlLoader {
    /// Create new loader with default configuration
    pub fn new() -> Self {
        Self {
            config: LoaderConfig::default(),
            metrics: Arc::new(AsyncMetrics::new()),
        }
    }

    /// Create new loader with custom configuration
    pub fn with_config(config: LoaderConfig) -> Self {
        Self {
            config,
            metrics: Arc::new(AsyncMetrics::new()),
        }
    }

    /// Get loader configuration
    pub fn config(&self) -> &LoaderConfig {
        &self.config
    }

    /// Load YAML document from file path
    #[instrument(skip(self), fields(path = %path.as_ref().display()))]
    pub async fn load_from_path<P: AsRef<Path> + Send>(
        &self,
        path: P,
    ) -> Result<YamlDocument> {
        let path = path.as_ref();
        info!("Loading YAML from path: {}", path.display());

        // Check file size
        let metadata = tokio::fs::metadata(path).await?;
        if metadata.len() > self.config.max_file_size as u64 {
            return Err(UnityAssetError::validation(
                "file_size",
                format!(
                    "File size {} exceeds maximum allowed size {}",
                    metadata.len(),
                    self.config.max_file_size
                ),
            ));
        }

        let file = File::open(path).await?;
        self.load_from_reader(file, Some(path.to_path_buf())).await
    }

    /// Load YAML document from async reader
    #[instrument(skip(self, reader))]
    pub async fn load_from_reader<R>(
        &self,
        reader: R,
        file_path: Option<std::path::PathBuf>,
    ) -> Result<YamlDocument>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let buffered = BufReader::with_capacity(self.config.buffer_size, reader);
        let mut lines = buffered.lines();
        let mut content = String::new();
        let mut bytes_read = 0u64;

        // Read all content
        while let Some(line) = lines.next_line().await? {
            content.push_str(&line);
            content.push('\n');
            bytes_read += line.len() as u64 + 1; // +1 for newline

            // Yield control periodically for large files
            if bytes_read % 1024 == 0 {
                tokio::task::yield_now().await;
            }
        }

        debug!("Read {} bytes from YAML source", bytes_read);
        self.metrics.increment_counter("bytes_loaded").await;

        // Parse YAML content
        self.parse_yaml_content(&content, file_path).await
    }

    /// Load with progress callback
    pub async fn load_with_progress<P, F>(
        &self,
        path: P,
        progress_callback: F,
    ) -> Result<YamlDocument>
    where
        P: AsRef<Path> + Send,
        F: Fn(LoadProgress) + Send + Sync + 'static,
    {
        let path = path.as_ref();
        let file_size = tokio::fs::metadata(path).await?.len();

        let file = File::open(path).await?;
        let mut buffered = BufReader::with_capacity(self.config.buffer_size, file);
        let mut content = String::new();
        let mut bytes_read = 0u64;

        // Read with progress reporting
        let mut buffer = String::with_capacity(self.config.buffer_size);
        loop {
            buffer.clear();
            let read = buffered.read_line(&mut buffer).await?;
            if read == 0 {
                break;
            }

            content.push_str(&buffer);
            bytes_read += read as u64;

            if self.config.enable_progress {
                let progress = LoadProgress {
                    bytes_loaded: bytes_read,
                    total_bytes: Some(file_size),
                    objects_processed: 0,
                    estimated_total_objects: None,
                    stage: "Reading YAML file".to_string(),
                };
                progress_callback(progress);
            }

            // Yield for large files
            if bytes_read % (64 * 1024) == 0 {
                tokio::task::yield_now().await;
            }
        }

        // Parse with final progress update
        if self.config.enable_progress {
            let progress = LoadProgress {
                bytes_loaded: bytes_read,
                total_bytes: Some(file_size),
                objects_processed: 0,
                estimated_total_objects: None,
                stage: "Parsing YAML content".to_string(),
            };
            progress_callback(progress);
        }

        self.parse_yaml_content(&content, Some(path.to_path_buf()))
            .await
    }

    /// Parse YAML content string
    #[instrument(skip(self, content))]
    async fn parse_yaml_content(
        &self,
        content: &str,
        file_path: Option<std::path::PathBuf>,
    ) -> Result<YamlDocument> {
        // Pre-process Unity YAML format
        let processed_content = self.preprocess_unity_yaml(content).await?;

        // Parse YAML documents
        let documents = self.parse_yaml_documents(&processed_content).await?;

        // Convert to Unity classes
        let classes = self.convert_to_unity_classes(documents).await?;

        // Create document metadata
        let metadata = ObjectMetadata {
            file_path: file_path.map(|p| p.to_string_lossy().to_string()),
            size_bytes: content.len() as u64,
            created_at: Some(std::time::SystemTime::now()),
            modified_at: Some(std::time::SystemTime::now()),
            properties: HashMap::new(),
        };

        Ok(YamlDocument::new(classes, metadata))
    }

    /// Pre-process Unity YAML format (based on V1 implementation)
    #[instrument(skip(self, content))]
    async fn preprocess_unity_yaml(&self, content: &str) -> Result<String> {
        let mut processed = String::new();
        let mut in_document = false;
        let mut current_class_info: Option<(i32, String)> = None;

        for line in content.lines() {
            let trimmed = line.trim();

            // Handle YAML directives
            if trimmed.starts_with('%') {
                processed.push_str(line);
                processed.push('\n');
                continue;
            }

            // Handle document separators
            if trimmed.starts_with("---") {
                in_document = true;

                // Parse Unity document header: --- !u!129 &1
                if let Some(unity_info) = self.parse_unity_document_header(trimmed) {
                    current_class_info = Some(unity_info);
                    // Convert to standard YAML document separator
                    processed.push_str("---\n");
                } else {
                    processed.push_str(line);
                    processed.push('\n');
                }
                continue;
            }

            // Handle the first line after document separator (class name)
            if in_document
                && !trimmed.is_empty()
                && !trimmed.starts_with(' ')
                && trimmed.ends_with(':')
            {
                if let Some((class_id, anchor)) = &current_class_info {
                    // Add Unity metadata as special properties
                    let class_name = trimmed.trim_end_matches(':');
                    processed.push_str(&format!("{}:\n", class_name));
                    processed.push_str(&format!("  __unity_class_id__: {}\n", class_id));
                    processed.push_str(&format!("  __unity_anchor__: \"{}\"\n", anchor));
                    current_class_info = None;
                } else {
                    processed.push_str(line);
                    processed.push('\n');
                }
                continue;
            }

            // Regular line
            processed.push_str(line);
            processed.push('\n');
        }

        Ok(processed)
    }

    /// Parse Unity document header like "--- !u!129 &1" (from V1)
    fn parse_unity_document_header(&self, line: &str) -> Option<(i32, String)> {
        let parts: Vec<&str> = line.split_whitespace().collect();

        let mut class_id = 0;
        let mut anchor = "0".to_string();

        for part in parts {
            if let Some(stripped) = part.strip_prefix("!u!") {
                if let Ok(id) = stripped.parse::<i32>() {
                    class_id = id;
                }
            } else if let Some(stripped) = part.strip_prefix('&') {
                anchor = stripped.to_string();
            }
        }

        if class_id > 0 {
            Some((class_id, anchor))
        } else {
            None
        }
    }

    /// Parse YAML documents from content (fixed to use proper multi-document parsing)
    #[instrument(skip(self, content))]
    async fn parse_yaml_documents(&self, content: &str) -> Result<Vec<Value>> {
        // Use serde_yaml's proper multi-document parsing (like V1)
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(content)
            .map(Value::deserialize)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| UnityAssetError::parse_error(format!("YAML parsing error: {}", e), 0))?;

        debug!("Parsed {} YAML documents", documents.len());
        Ok(documents)
    }

    /// Convert YAML documents to Unity classes
    #[instrument(skip(self, documents))]
    async fn convert_to_unity_classes(
        &self,
        documents: Vec<Value>,
    ) -> Result<Vec<AsyncUnityClass>> {
        let mut classes = Vec::with_capacity(documents.len());

        for (doc_index, document) in documents.into_iter().enumerate() {
            // Extract Unity class information from document
            if let Some(class) = self.extract_unity_class(&document, doc_index).await? {
                classes.push(class);
            }

            // Yield control for large sets
            if doc_index % 50 == 49 {
                tokio::task::yield_now().await;
            }
        }

        info!("Converted {} Unity classes", classes.len());
        Ok(classes)
    }

    /// Extract Unity class from YAML document
    #[instrument(skip(self, document))]
    pub async fn extract_unity_class(
        &self,
        document: &Value,
        doc_index: usize,
    ) -> Result<Option<AsyncUnityClass>> {
        match document {
            Value::Mapping(map) => {
                // Unity YAML typically has class name as key
                if map.len() != 1 {
                    warn!(
                        "Expected single-key mapping for Unity class at document {}",
                        doc_index
                    );
                    return Ok(None);
                }

                let (class_key, class_data) = map.iter().next().unwrap();
                let class_name = match class_key {
                    Value::String(s) => s.clone(),
                    _ => {
                        warn!("Expected string class name at document {}", doc_index);
                        return Ok(None);
                    }
                };

                // Convert class data to Unity value
                let unity_data = self.convert_value_to_unity_value(class_data).await?;

                // Extract Unity metadata from the processed YAML
                let mut class_id = -1;
                let mut anchor = format!("doc_{}", doc_index);
                let mut properties = IndexMap::new();

                // Extract metadata and properties from unity_data
                if let UnityValue::Object(obj) = unity_data {
                    for (key, value) in obj {
                        match key.as_str() {
                            "__unity_class_id__" => {
                                if let UnityValue::Int(id) = value {
                                    class_id = id as i32;
                                }
                            }
                            "__unity_anchor__" => {
                                if let UnityValue::String(a) = value {
                                    anchor = a;
                                }
                            }
                            _ => {
                                // Regular property
                                properties.insert(key, value);
                            }
                        }
                    }
                } else {
                    // If not an object, store as single property
                    properties.insert("data".to_string(), unity_data);
                }

                // Fallback to class registry if no class ID found
                if class_id == -1 {
                    class_id = unity_asset_core_v2::unity_types::global_class_registry()
                        .get_class_id(&class_name)
                        .unwrap_or(0);
                }

                // Create AsyncUnityClass with the new structure
                let mut class = AsyncUnityClass::new(class_id, class_name, anchor);
                *class.properties_mut() = properties;

                Ok(Some(class))
            }
            _ => {
                warn!("Expected mapping for Unity class at document {}", doc_index);
                Ok(None)
            }
        }
    }

    /// Convert serde_yaml::Value to UnityValue (fixed recursion issue)
    #[instrument(skip(self, value))]
    async fn convert_value_to_unity_value(&self, value: &Value) -> Result<UnityValue> {
        // Use iterative approach to avoid stack overflow with deeply nested structures
        self.convert_value_iterative(value)
    }

    /// Iterative conversion to avoid recursion stack overflow
    fn convert_value_iterative(&self, root_value: &Value) -> Result<UnityValue> {
        use std::collections::VecDeque;

        #[derive(Debug)]
        enum ConversionTask {
            Simple(Value, usize),               // value, result_index
            Array(Vec<Value>, usize),           // items, result_index
            Object(serde_yaml::Mapping, usize), // map, result_index
        }

        let mut stack = VecDeque::new();
        let mut results = Vec::new();

        // Start with the root value
        stack.push_back(ConversionTask::Simple(root_value.clone(), 0));
        results.push(UnityValue::Null); // placeholder for root result

        while let Some(task) = stack.pop_back() {
            match task {
                ConversionTask::Simple(value, result_idx) => {
                    let converted = match value {
                        Value::Null => UnityValue::Null,
                        Value::Bool(b) => UnityValue::Bool(b),
                        Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                UnityValue::Int(i)
                            } else if let Some(f) = n.as_f64() {
                                UnityValue::Float(f)
                            } else {
                                UnityValue::Int(0)
                            }
                        }
                        Value::String(s) => UnityValue::String(s),
                        Value::Sequence(seq) => {
                            // For arrays, we need to process each item
                            let array_idx = results.len();
                            results.push(UnityValue::Null); // placeholder
                            stack.push_back(ConversionTask::Array(seq, array_idx));
                            continue;
                        }
                        Value::Mapping(map) => {
                            // For objects, we need to process each key-value pair
                            let obj_idx = results.len();
                            results.push(UnityValue::Null); // placeholder
                            stack.push_back(ConversionTask::Object(map, obj_idx));
                            continue;
                        }
                        Value::Tagged(_) => UnityValue::Null,
                    };

                    if result_idx < results.len() {
                        results[result_idx] = converted;
                    }
                }
                ConversionTask::Array(seq, result_idx) => {
                    let mut array = Vec::with_capacity(seq.len());
                    for item in seq {
                        let converted = self.convert_simple_value(&item)?;
                        array.push(converted);
                    }
                    if result_idx < results.len() {
                        results[result_idx] = UnityValue::Array(array);
                    }
                }
                ConversionTask::Object(map, result_idx) => {
                    let mut object = IndexMap::with_capacity(map.len());
                    for (k, v) in map {
                        let key = match k {
                            Value::String(s) => s,
                            _ => format!("{:?}", k),
                        };
                        let value = self.convert_simple_value(&v)?;
                        object.insert(key, value);
                    }
                    if result_idx < results.len() {
                        results[result_idx] = UnityValue::Object(object);
                    }
                }
            }
        }

        Ok(results.into_iter().next().unwrap_or(UnityValue::Null))
    }

    /// Convert simple values (non-recursive)
    fn convert_simple_value(&self, value: &Value) -> Result<UnityValue> {
        let result = match value {
            Value::Null => UnityValue::Null,
            Value::Bool(b) => UnityValue::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    UnityValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    UnityValue::Float(f)
                } else {
                    UnityValue::Int(0)
                }
            }
            Value::String(s) => UnityValue::String(s.clone()),
            Value::Sequence(seq) => {
                let mut array = Vec::with_capacity(seq.len());
                for item in seq {
                    array.push(self.convert_simple_value(item)?);
                }
                UnityValue::Array(array)
            }
            Value::Mapping(map) => {
                let mut object = IndexMap::with_capacity(map.len());
                for (k, v) in map {
                    let key = match k {
                        Value::String(s) => s.clone(),
                        _ => format!("{:?}", k),
                    };
                    let value = self.convert_simple_value(v)?;
                    object.insert(key, value);
                }
                UnityValue::Object(object)
            }
            Value::Tagged(_) => UnityValue::Null,
        };
        Ok(result)
    }
}

#[async_trait]
impl AsyncAssetLoader for YamlLoader {
    type Output = YamlDocument;
    type Config = LoaderConfig;

    async fn load_asset<P>(&self, path: P, _config: Self::Config) -> Result<Self::Output>
    where
        P: AsRef<Path> + Send,
    {
        self.load_from_path(path).await
    }

    async fn load_assets<P>(
        &self,
        paths: Vec<P>,
        _config: Self::Config,
    ) -> impl Stream<Item = Result<Self::Output>> + Send
    where
        P: AsRef<Path> + Send + 'static,
    {
        let loader = Arc::new(self.clone());
        let semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_concurrent_loads,
        ));

        async_stream::stream! {
            let mut join_set = tokio::task::JoinSet::new();

            for path in paths {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let loader_clone = loader.clone();

                join_set.spawn(async move {
                    let _permit = permit; // Hold permit until task completes
                    loader_clone.load_from_path(path).await
                });
            }

            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(document)) => yield Ok(document),
                    Ok(Err(e)) => yield Err(e),
                    Err(e) => yield Err(UnityAssetError::TaskJoin(e.to_string())),
                }
            }
        }
    }

    fn max_concurrent_loads(&self) -> usize {
        self.config.max_concurrent_loads
    }

    fn set_max_concurrent_loads(&mut self, max: usize) {
        self.config.max_concurrent_loads = max;
    }
}

impl Clone for YamlLoader {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

impl Default for YamlLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_load_simple_yaml() {
        let yaml_content = r#"
GameObject:
  m_Name: "TestObject"
  m_IsActive: 1
  m_Tag: "Player"
"#;

        let temp_file = NamedTempFile::new().unwrap();
        tokio::fs::write(temp_file.path(), yaml_content)
            .await
            .unwrap();

        let loader = YamlLoader::new();
        let document = loader.load_from_path(temp_file.path()).await.unwrap();

        assert_eq!(document.classes().len(), 1);
        let class = &document.classes()[0];
        assert_eq!(class.class_name(), "GameObject");
        assert_eq!(class.name(), Some("TestObject".to_string()));
    }

    #[tokio::test]
    async fn test_loader_config() {
        let config = LoaderConfig::for_large_files();
        assert_eq!(config.max_concurrent_loads, 4);
        assert_eq!(config.buffer_size, 32768);
        assert!(!config.resolve_anchors);

        let small_config = LoaderConfig::for_small_files();
        assert_eq!(small_config.max_concurrent_loads, 16);
        assert!(small_config.preserve_order);
    }

    #[tokio::test]
    async fn test_concurrent_loading() {
        let yaml_content = r#"
Transform:
  m_Position: {x: 1.0, y: 2.0, z: 3.0}
"#;

        // Create multiple temp files
        let mut temp_files = Vec::new();
        for _ in 0..3 {
            let temp_file = NamedTempFile::new().unwrap();
            tokio::fs::write(temp_file.path(), yaml_content)
                .await
                .unwrap();
            temp_files.push(temp_file);
        }

        let loader = YamlLoader::new();
        let paths: Vec<_> = temp_files.iter().map(|f| f.path().to_path_buf()).collect();

        let stream = loader.load_assets(paths, LoaderConfig::default()).await;
        tokio::pin!(stream);
        let mut count = 0;

        while let Some(result) = stream.next().await {
            assert!(result.is_ok());
            count += 1;
        }

        assert_eq!(count, 3);
    }
}
