//! Stream-based YAML parser
//!
//! High-performance streaming YAML parser for processing large Unity YAML files.

use bytes::Bytes;
use futures::Stream;
use serde_yaml::Value;
use std::path::Path;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio_stream::StreamExt;
use tracing::instrument;

use unity_asset_core_v2::{stream_types::AssetChunk, AsyncUnityClass, Result, UnityAssetError};

use crate::async_loader::AsyncYamlLoader;

/// Stream of YAML objects
pub type YamlObjectStream = Pin<Box<dyn Stream<Item = Result<AsyncUnityClass>> + Send>>;
#[derive(Debug, Clone)]
pub struct StreamParseConfig {
    /// Buffer size for reading
    pub buffer_size: usize,
    /// Maximum document size to process in memory
    pub max_document_size: usize,
    /// Enable concurrent document parsing
    pub concurrent_parsing: bool,
    /// Maximum number of concurrent parsers
    pub max_concurrent_parsers: usize,
    /// Yield control every N documents
    pub yield_interval: usize,
}

impl Default for StreamParseConfig {
    fn default() -> Self {
        Self {
            buffer_size: 16384,
            max_document_size: 1024 * 1024, // 1MB per document
            concurrent_parsing: true,
            max_concurrent_parsers: 4,
            yield_interval: 50,
        }
    }
}

impl StreamParseConfig {
    /// Configuration for large files with many small documents
    pub fn for_many_small_docs() -> Self {
        Self {
            buffer_size: 8192,
            max_document_size: 64 * 1024, // 64KB per document
            concurrent_parsing: true,
            max_concurrent_parsers: 8,
            yield_interval: 100,
        }
    }

    /// Configuration for few large documents
    pub fn for_large_docs() -> Self {
        Self {
            buffer_size: 32768,
            max_document_size: 10 * 1024 * 1024, // 10MB per document
            concurrent_parsing: false,           // Sequential for large docs
            max_concurrent_parsers: 1,
            yield_interval: 10,
        }
    }
}

/// Stream-based YAML parser
pub struct StreamYamlParser {
    config: StreamParseConfig,
    loader: AsyncYamlLoader,
}

impl StreamYamlParser {
    /// Create new stream parser
    pub fn new() -> Self {
        Self {
            config: StreamParseConfig::default(),
            loader: AsyncYamlLoader::new(),
        }
    }

    /// Create with custom configuration
    pub fn with_config(config: StreamParseConfig) -> Self {
        Self {
            config,
            loader: AsyncYamlLoader::new(),
        }
    }

    /// Parse YAML file as stream
    #[instrument(skip(self), fields(path = %path.as_ref().display()))]
    pub async fn parse_file_stream<P>(&self, path: P) -> Result<YamlObjectStream>
    where
        P: AsRef<Path> + Send + 'static,
    {
        let file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| UnityAssetError::Io(e.to_string()))?;

        self.parse_reader_stream(file).await
    }

    /// Parse from async reader as stream
    #[instrument(skip(self, reader))]
    pub async fn parse_reader_stream<R>(&self, reader: R) -> Result<YamlObjectStream>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let buffered = BufReader::with_capacity(self.config.buffer_size, reader);
        let document_stream = self.create_document_stream(buffered).await?;
        let object_stream = self.create_object_stream(document_stream).await?;

        Ok(Box::pin(object_stream))
    }

    /// Create stream of YAML documents
    #[instrument(skip(self, reader))]
    async fn create_document_stream<R>(
        &self,
        reader: BufReader<R>,
    ) -> Result<impl Stream<Item = Result<String>> + Send>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let config = self.config.clone();
        Ok(async_stream::stream! {
            let reader = reader;
            let mut current_document = String::new();
            let mut in_document = false;
            let mut document_count = 0;

            let mut lines = reader.lines();
            while let Some(line_result) = lines.next_line().await.transpose() {
                match line_result {
                    Ok(line) => {
                        // Process line
                        if line.starts_with("---") {
                            if in_document && !current_document.is_empty() {
                                // End of current document
                                let doc = std::mem::take(&mut current_document);
                                document_count += 1;
                                yield Ok(doc);
                            }
                            in_document = true;
                        } else if line.starts_with("%") {
                            // YAML directive - skip
                            continue;
                        } else if !line.trim().is_empty() {
                            // Content line
                            current_document.push_str(&line);
                            current_document.push('\n');

                            // Check document size limit
                            if current_document.len() > config.max_document_size {
                                yield Err(UnityAssetError::validation(
                                    "document_size",
                                    format!("Document {} exceeds maximum size of {} bytes",
                                        document_count, config.max_document_size)
                                ));
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(UnityAssetError::Io(e.to_string()));
                        return;
                    }
                }
            }

            // Return final document if any
            if !current_document.is_empty() {
                yield Ok(current_document);
            }
        })
    }

    /// Convert document stream to object stream
    #[instrument(skip(self, document_stream))]
    async fn create_object_stream<S>(
        &self,
        document_stream: S,
    ) -> Result<impl Stream<Item = Result<AsyncUnityClass>> + Send>
    where
        S: Stream<Item = Result<String>> + Send + 'static,
    {
        let loader = self.loader.clone();
        let config = self.config.clone();

        Ok(async_stream::stream! {
            tokio::pin!(document_stream);
            let mut doc_count = 0;

            if config.concurrent_parsing {
                // Concurrent parsing mode
                let mut join_set = tokio::task::JoinSet::new();
                let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_parsers));

                while let Some(doc_result) = document_stream.next().await {
                    match doc_result {
                        Ok(doc_content) => {
                            let permit = semaphore.clone().acquire_owned().await.unwrap();
                            let loader_clone = loader.clone();

                            join_set.spawn(async move {
                                let _permit = permit;
                                Self::parse_document_to_class(loader_clone, doc_content, doc_count).await
                            });
                        }
                        Err(e) => yield Err(e),
                    }

                    doc_count += 1;

                    // Process completed tasks
                    while let Some(task_result) = join_set.try_join_next() {
                        match task_result {
                            Ok(Ok(Some(class))) => yield Ok(class),
                            Ok(Ok(None)) => {}, // Skip empty documents
                            Ok(Err(e)) => yield Err(e),
                            Err(e) => yield Err(UnityAssetError::TaskJoin(e.to_string())),
                        }
                    }
                }

                // Process remaining tasks
                while let Some(task_result) = join_set.join_next().await {
                    match task_result {
                        Ok(Ok(Some(class))) => yield Ok(class),
                        Ok(Ok(None)) => {}, // Skip empty documents
                        Ok(Err(e)) => yield Err(e),
                        Err(e) => yield Err(UnityAssetError::TaskJoin(e.to_string())),
                    }
                }
            } else {
                // Sequential parsing mode
                while let Some(doc_result) = document_stream.next().await {
                    match doc_result {
                        Ok(doc_content) => {
                            match Self::parse_document_to_class(loader.clone(), doc_content, doc_count).await {
                                Ok(Some(class)) => yield Ok(class),
                                Ok(None) => {}, // Skip empty documents
                                Err(e) => yield Err(e),
                            }
                        }
                        Err(e) => yield Err(e),
                    }

                    doc_count += 1;

                    // Yield control periodically
                    if doc_count % config.yield_interval == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }
        })
    }

    /// Parse single document to Unity class
    #[instrument(skip(loader, doc_content))]
    async fn parse_document_to_class(
        loader: AsyncYamlLoader,
        doc_content: String,
        doc_index: usize,
    ) -> Result<Option<AsyncUnityClass>> {
        if doc_content.trim().is_empty() {
            return Ok(None);
        }

        // Parse YAML document
        let yaml_value: Value = serde_yaml::from_str(&doc_content).map_err(|e| {
            UnityAssetError::parse_error(format!("YAML parsing failed: {}", e), doc_index as u64)
        })?;

        // Extract Unity class
        loader.extract_unity_class(&yaml_value, doc_index).await
    }

    /// Create chunk-based stream (for very large files)
    pub fn create_chunk_stream<R>(
        &self,
        reader: R,
        chunk_size: usize,
    ) -> impl Stream<Item = Result<AssetChunk>> + Send
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        let buffer_size = self.config.buffer_size;
        async_stream::stream! {
            let mut buffered = BufReader::with_capacity(buffer_size, reader);
            let mut buffer = Vec::with_capacity(chunk_size);
            let mut offset = 0u64;

            loop {
                buffer.clear();
                let mut bytes_read = 0;

                // Read chunk
                while bytes_read < chunk_size {
                    let line_result = buffered.read_until(b'\n', &mut buffer).await;
                    match line_result {
                        Ok(0) => break, // EOF
                        Ok(n) => bytes_read += n,
                        Err(e) => {
                            yield Err(UnityAssetError::Io(e.to_string()));
                            return;
                        }
                    }
                }

                if buffer.is_empty() {
                    break; // EOF reached
                }

                let is_last = bytes_read < chunk_size;
                let chunk = AssetChunk::new(
                    Bytes::copy_from_slice(&buffer),
                    offset,
                    is_last
                );

                yield Ok(chunk);

                offset += bytes_read as u64;

                if is_last {
                    break;
                }

                // Yield control for large files
                tokio::task::yield_now().await;
            }
        }
    }
}

impl Default for StreamYamlParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_document_stream() {
        let yaml_content = r#"
%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
---
GameObject:
  m_Name: "Player"
  m_IsActive: 1
---
Transform:
  m_Position: {x: 1.0, y: 2.0, z: 3.0}
"#;

        let cursor = Cursor::new(yaml_content.as_bytes());
        let buffered = BufReader::new(cursor);
        let parser = StreamYamlParser::new();
        let stream = parser.create_document_stream(buffered).await.unwrap();

        let mut doc_count = 0;
        tokio::pin!(stream);
        while let Some(result) = stream.next().await {
            assert!(result.is_ok());
            let doc = result.unwrap();
            assert!(!doc.trim().is_empty());
            doc_count += 1;
        }

        assert_eq!(doc_count, 2);
    }

    #[tokio::test]
    async fn test_stream_parser() {
        let yaml_content = r#"
---
GameObject:
  m_Name: "TestObject"
  m_IsActive: 1
---
Transform:
  m_Position: {x: 1.0, y: 2.0, z: 3.0}
"#;

        let cursor = Cursor::new(yaml_content.as_bytes());
        let parser = StreamYamlParser::new();
        let mut object_stream = parser.parse_reader_stream(cursor).await.unwrap();

        let mut object_count = 0;
        while let Some(result) = object_stream.next().await {
            assert!(result.is_ok());
            let class = result.unwrap();
            assert!(!class.class_name().is_empty());
            object_count += 1;
        }

        assert_eq!(object_count, 2);
    }

    #[tokio::test]
    async fn test_concurrent_parsing() {
        let yaml_content = r#"
---
GameObject:
  m_Name: "Object1"
---
GameObject:
  m_Name: "Object2"
---
GameObject:
  m_Name: "Object3"
"#;

        let cursor = Cursor::new(yaml_content.as_bytes());
        let config = StreamParseConfig {
            concurrent_parsing: true,
            max_concurrent_parsers: 2,
            ..Default::default()
        };

        let parser = StreamYamlParser::with_config(config);
        let mut object_stream = parser.parse_reader_stream(cursor).await.unwrap();

        let mut objects = Vec::new();
        while let Some(result) = object_stream.next().await {
            assert!(result.is_ok());
            objects.push(result.unwrap());
        }

        assert_eq!(objects.len(), 3);

        // All should be GameObjects
        for obj in objects {
            assert_eq!(obj.class_name(), "GameObject");
        }
    }

    #[tokio::test]
    async fn test_chunk_stream() {
        let yaml_content = "Line 1\nLine 2\nLine 3\nLine 4\n";
        let cursor = Cursor::new(yaml_content.as_bytes());

        let parser = StreamYamlParser::new();
        let chunk_stream = parser.create_chunk_stream(cursor, 10);
        tokio::pin!(chunk_stream);

        let mut chunk_count = 0;
        while let Some(result) = chunk_stream.next().await {
            assert!(result.is_ok());
            let chunk = result.unwrap();
            assert!(!chunk.data.is_empty());
            chunk_count += 1;
        }

        assert!(chunk_count > 0);
    }

    #[tokio::test]
    async fn test_large_document_limit() {
        // Use a simpler approach with a direct string
        let yaml_content = format!("---\nx: {}", "a".repeat(10000));

        let config = StreamParseConfig {
            max_document_size: 1000, // Very small limit
            ..Default::default()
        };

        let parser = StreamYamlParser::with_config(config);
        let cursor = std::io::Cursor::new(yaml_content.into_bytes());
        let buffered = BufReader::new(cursor);

        let stream = parser.create_document_stream(buffered).await.unwrap();
        tokio::pin!(stream);

        let result = stream.next().await.unwrap();
        assert!(result.is_err());

        if let Err(UnityAssetError::Validation { field, .. }) = result {
            assert_eq!(field, "document_size");
        } else {
            panic!("Expected validation error");
        }
    }
}
