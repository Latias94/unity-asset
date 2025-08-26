//! Async trait definitions
//!
//! Core async interfaces for Unity Asset Parser V2, providing ground-up async support.

use crate::{
    error::Result,
    unity_types::{AsyncUnityClass, ObjectMetadata},
};
use async_trait::async_trait;
use futures::Stream;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncSeek};

/// Load progress information
#[derive(Debug, Clone)]
pub struct LoadProgress {
    /// Number of bytes loaded
    pub bytes_loaded: u64,
    /// Total bytes (if known)
    pub total_bytes: Option<u64>,
    /// Number of objects processed
    pub objects_processed: u64,
    /// Estimated total objects (if known)
    pub estimated_total_objects: Option<u64>,
    /// Current stage description
    pub stage: String,
}

impl LoadProgress {
    /// Calculate completion ratio (0.0 - 1.0)
    pub fn completion_ratio(&self) -> Option<f32> {
        self.total_bytes.map(|total| {
            if total == 0 {
                1.0
            } else {
                self.bytes_loaded as f32 / total as f32
            }
        })
    }
}

/// Core async Unity document trait
#[async_trait]
pub trait AsyncUnityDocument: Send + Sync {
    /// Load from file path asynchronously
    async fn load_from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path> + Send,
        Self: Sized;

    /// Load from async stream
    async fn load_from_stream<S>(stream: S) -> Result<Self>
    where
        S: AsyncRead + AsyncSeek + Send + Unpin + 'static,
        Self: Sized;

    /// Load with progress callback
    async fn load_with_progress<P, F>(path: P, progress_callback: F) -> Result<Self>
    where
        P: AsRef<Path> + Send,
        F: Fn(LoadProgress) + Send + Sync + 'static,
        Self: Sized;

    /// Get object stream
    fn objects_stream(&self) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_;

    /// Filter objects stream by class types
    fn filter_objects_stream(
        &self,
        class_names: &[&str],
    ) -> impl Stream<Item = Result<AsyncUnityClass>> + Send + '_;

    /// Save to file asynchronously
    async fn save_to_path<P>(&self, path: P) -> Result<()>
    where
        P: AsRef<Path> + Send;

    /// Get document metadata
    fn metadata(&self) -> &ObjectMetadata;

    /// Get object count
    fn object_count(&self) -> u64;
}

/// Async asset loader trait
#[async_trait]
pub trait AsyncAssetLoader: Send + Sync {
    type Output: Send;
    type Config: Send + Sync;

    /// Load single asset
    async fn load_asset<P>(&self, path: P, config: Self::Config) -> Result<Self::Output>
    where
        P: AsRef<Path> + Send;

    /// Batch load assets
    async fn load_assets<P>(
        &self,
        paths: Vec<P>,
        config: Self::Config,
    ) -> impl Stream<Item = Result<Self::Output>> + Send
    where
        P: AsRef<Path> + Send + 'static;

    /// Maximum concurrent loads
    fn max_concurrent_loads(&self) -> usize;

    /// Set concurrency level
    fn set_max_concurrent_loads(&mut self, max: usize);
}

/// Stream processor trait
#[async_trait]
pub trait AsyncStreamProcessor<Input, Output>: Send + Sync
where
    Input: Send,
    Output: Send,
{
    /// Process single item
    async fn process_item(&self, item: Input) -> Result<Output>;

    /// Process stream
    fn process_stream<S>(&self, input: S) -> impl Stream<Item = Result<Output>> + Send
    where
        S: Stream<Item = Result<Input>> + Send + 'static;

    /// Batch size
    fn batch_size(&self) -> usize {
        1
    }

    /// Whether supports parallel processing
    fn supports_parallel(&self) -> bool {
        false
    }
}

/// Async object processor trait
#[async_trait]
pub trait AsyncObjectProcessor: Send + Sync {
    type Input: Send;
    type Output: Send;
    type Config: Send + Sync + Clone;

    /// Process object
    async fn process_object(
        &self,
        input: Self::Input,
        config: Self::Config,
    ) -> Result<Self::Output>;

    /// Batch process objects
    async fn process_objects(
        &self,
        inputs: Vec<Self::Input>,
        config: Self::Config,
    ) -> impl Stream<Item = Result<Self::Output>> + Send;

    /// Supported concurrency level
    fn max_concurrent_processes(&self) -> usize;
}

/// Asset cache trait
#[async_trait]
pub trait AsyncAssetCache: Send + Sync {
    type Key: Send + Sync + Clone;
    type Value: Send + Sync + Clone;

    /// Get cache item asynchronously
    async fn get(&self, key: &Self::Key) -> Option<Self::Value>;

    /// Set cache item asynchronously
    async fn set(&self, key: Self::Key, value: Self::Value) -> Result<()>;

    /// Remove cache item asynchronously
    async fn remove(&self, key: &Self::Key) -> Result<Option<Self::Value>>;

    /// Clear cache
    async fn clear(&self) -> Result<()>;

    /// Get cache size
    async fn size(&self) -> u64;
}

/// Performance metrics trait
pub trait AsyncMetricsCollector: Send + Sync {
    /// Record processing time
    fn record_processing_time(&self, operation: &str, duration: std::time::Duration);

    /// Record memory usage
    fn record_memory_usage(&self, bytes: u64);

    /// Record error
    fn record_error(&self, error: &str);

    /// Increment counter
    fn increment_counter(&self, name: &str);

    /// Set gauge value
    fn set_gauge(&self, name: &str, value: f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_test;

    #[tokio::test]
    async fn test_load_progress() {
        let progress = LoadProgress {
            bytes_loaded: 50,
            total_bytes: Some(100),
            objects_processed: 10,
            estimated_total_objects: Some(20),
            stage: "Loading".to_string(),
        };

        assert_eq!(progress.completion_ratio(), Some(0.5));
    }

    #[tokio::test]
    async fn test_load_progress_unknown_total() {
        let progress = LoadProgress {
            bytes_loaded: 50,
            total_bytes: None,
            objects_processed: 10,
            estimated_total_objects: None,
            stage: "Loading".to_string(),
        };

        assert_eq!(progress.completion_ratio(), None);
    }
}
