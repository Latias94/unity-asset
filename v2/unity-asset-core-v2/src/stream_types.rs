//! Stream types and utilities
//!
//! Async stream types for Unity asset processing with backpressure and flow control.

use futures::Stream;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{error::Result, unity_types::AsyncUnityClass};

/// Configuration for streams
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Buffer size for stream processing
    pub buffer_size: usize,
    /// Maximum items in memory at once
    pub max_buffer_items: usize,
    /// Enable backpressure control
    pub enable_backpressure: bool,
    /// Timeout for stream operations
    pub timeout: std::time::Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            buffer_size: 1024,
            max_buffer_items: 1000,
            enable_backpressure: true,
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl StreamConfig {
    /// Create config for large files
    pub fn for_large_files() -> Self {
        Self {
            buffer_size: 8192,
            max_buffer_items: 100, // Limit memory usage
            enable_backpressure: true,
            timeout: std::time::Duration::from_secs(300),
        }
    }

    /// Create config for small files
    pub fn for_small_files() -> Self {
        Self {
            buffer_size: 512,
            max_buffer_items: 10000,
            enable_backpressure: false,
            timeout: std::time::Duration::from_secs(10),
        }
    }
}

/// Stream of Unity objects
pub type UnityObjectStream = Pin<Box<dyn Stream<Item = Result<AsyncUnityClass>> + Send>>;

/// Stream of asset chunks for processing
pub type AssetChunkStream = Pin<Box<dyn Stream<Item = Result<AssetChunk>> + Send>>;

/// Stream of processed objects
pub type ProcessedObjectStream<T> = Pin<Box<dyn Stream<Item = Result<T>> + Send>>;

/// Asset chunk for streaming processing
#[derive(Debug, Clone)]
pub struct AssetChunk {
    /// Chunk data
    pub data: bytes::Bytes,
    /// Chunk offset in original file
    pub offset: u64,
    /// Chunk size
    pub size: usize,
    /// Whether this is the last chunk
    pub is_last: bool,
    /// Chunk metadata
    pub metadata: ChunkMetadata,
}

impl AssetChunk {
    /// Create new asset chunk
    pub fn new(data: bytes::Bytes, offset: u64, is_last: bool) -> Self {
        let size = data.len();
        Self {
            data,
            offset,
            size,
            is_last,
            metadata: ChunkMetadata::default(),
        }
    }

    /// Get chunk ID for tracking
    pub fn chunk_id(&self) -> String {
        format!("chunk_{}_{}", self.offset, self.size)
    }
}

/// Chunk metadata
#[derive(Debug, Clone, Default)]
pub struct ChunkMetadata {
    /// Processing start time
    pub started_at: Option<std::time::Instant>,
    /// Processing duration
    pub processing_duration: Option<std::time::Duration>,
    /// Number of objects in this chunk
    pub object_count: u32,
    /// Compression ratio if compressed
    pub compression_ratio: Option<f32>,
}

/// Backpressure controller for streams
#[derive(Debug)]
pub struct BackPressure {
    current_items: std::sync::atomic::AtomicUsize,
    max_items: usize,
    waiting: std::sync::atomic::AtomicUsize,
}

impl BackPressure {
    /// Create new backpressure controller
    pub fn new(max_items: usize) -> Self {
        Self {
            current_items: std::sync::atomic::AtomicUsize::new(0),
            max_items,
            waiting: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Check if we can accept more items
    pub fn can_accept(&self) -> bool {
        self.current_items
            .load(std::sync::atomic::Ordering::Relaxed)
            < self.max_items
    }

    /// Acquire slot (blocking if necessary)
    pub async fn acquire(&self) -> BackPressureGuard<'_> {
        use std::sync::atomic::Ordering;

        // Increment waiting counter
        self.waiting.fetch_add(1, Ordering::Relaxed);

        // Wait for available slot
        loop {
            if self.current_items.load(Ordering::Relaxed) < self.max_items {
                self.current_items.fetch_add(1, Ordering::Relaxed);
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                break;
            }
            tokio::task::yield_now().await;
        }

        BackPressureGuard { controller: self }
    }

    /// Get current load
    pub fn current_load(&self) -> f32 {
        let current = self
            .current_items
            .load(std::sync::atomic::Ordering::Relaxed);
        current as f32 / self.max_items as f32
    }

    /// Get waiting count
    pub fn waiting_count(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// RAII guard for backpressure slots
pub struct BackPressureGuard<'a> {
    controller: &'a BackPressure,
}

impl Drop for BackPressureGuard<'_> {
    fn drop(&mut self) {
        self.controller
            .current_items
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Buffered stream adapter with backpressure
#[pin_project]
pub struct BufferedStream<S> {
    #[pin]
    inner: S,
    buffer: Vec<Result<AsyncUnityClass>>,
    config: StreamConfig,
    backpressure: Option<BackPressure>,
}

impl<S> BufferedStream<S>
where
    S: Stream<Item = Result<AsyncUnityClass>>,
{
    /// Create new buffered stream
    pub fn new(stream: S, config: StreamConfig) -> Self {
        let backpressure = if config.enable_backpressure {
            Some(BackPressure::new(config.max_buffer_items))
        } else {
            None
        };

        Self {
            inner: stream,
            buffer: Vec::with_capacity(config.buffer_size),
            config,
            backpressure,
        }
    }

    /// Get current buffer utilization
    pub fn buffer_utilization(&self) -> f32 {
        self.buffer.len() as f32 / self.config.buffer_size as f32
    }

    /// Check if backpressure is active
    pub fn is_backpressure_active(&self) -> bool {
        self.backpressure
            .as_ref()
            .map(|bp| bp.current_load() > 0.8)
            .unwrap_or(false)
    }
}

impl<S> Stream for BufferedStream<S>
where
    S: Stream<Item = Result<AsyncUnityClass>>,
{
    type Item = Result<AsyncUnityClass>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        // Return from buffer first if available
        if !this.buffer.is_empty() {
            return Poll::Ready(Some(this.buffer.remove(0)));
        }

        // Check backpressure
        if let Some(bp) = this.backpressure {
            if !bp.can_accept() {
                // Apply backpressure by not polling inner stream
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }

        // Poll inner stream
        match this.inner.poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Utility functions for creating streams
pub mod stream_utils {
    use super::*;
    use futures::{stream, StreamExt};

    /// Create empty object stream
    pub fn empty_object_stream() -> UnityObjectStream {
        Box::pin(stream::empty())
    }

    /// Create stream from iterator
    pub fn from_iter<I>(iter: I) -> UnityObjectStream
    where
        I: IntoIterator<Item = Result<AsyncUnityClass>> + Send + 'static,
        I::IntoIter: Send,
    {
        Box::pin(stream::iter(iter))
    }

    /// Create stream from async generator
    pub fn from_async_fn<F, Fut>(mut generator: F) -> UnityObjectStream
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Option<Result<AsyncUnityClass>>> + Send + 'static,
    {
        Box::pin(async_stream::stream! {
            loop {
                match generator().await {
                    Some(item) => yield item,
                    None => break,
                }
            }
        })
    }

    /// Create stream from channel receiver
    pub fn from_receiver(receiver: mpsc::Receiver<Result<AsyncUnityClass>>) -> UnityObjectStream {
        Box::pin(ReceiverStream::new(receiver))
    }

    /// Filter stream by class names
    pub fn filter_by_class_names(
        stream: UnityObjectStream,
        class_names: Vec<String>,
    ) -> UnityObjectStream {
        Box::pin(async_stream::stream! {
            tokio::pin!(stream);
            while let Some(item) = stream.as_mut().next().await {
                match item {
                    Ok(obj) => {
                        if class_names.iter().any(|name| obj.class_name() == name) {
                            yield Ok(obj);
                        }
                    }
                    Err(e) => yield Err(e),
                }
            }
        })
    }

    /// Batch items from stream
    pub fn batch_stream(
        stream: UnityObjectStream,
        batch_size: usize,
    ) -> Pin<Box<dyn Stream<Item = Result<Vec<AsyncUnityClass>>> + Send>> {
        Box::pin(async_stream::stream! {
            let mut batch = Vec::with_capacity(batch_size);
            tokio::pin!(stream);

            while let Some(item) = stream.as_mut().next().await {
                match item {
                    Ok(obj) => {
                        batch.push(obj);
                        if batch.len() >= batch_size {
                            yield Ok(std::mem::take(&mut batch));
                        }
                    }
                    Err(e) => {
                        if !batch.is_empty() {
                            yield Ok(std::mem::take(&mut batch));
                        }
                        yield Err(e);
                        break;
                    }
                }
            }

            if !batch.is_empty() {
                yield Ok(batch);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio_test;

    #[tokio::test]
    async fn test_backpressure() {
        let bp = BackPressure::new(2);

        assert!(bp.can_accept());
        assert_eq!(bp.current_load(), 0.0);

        let _guard1 = bp.acquire().await;
        assert_eq!(bp.current_load(), 0.5);

        let _guard2 = bp.acquire().await;
        assert_eq!(bp.current_load(), 1.0);
        assert!(!bp.can_accept());

        drop(_guard1);
        assert!(bp.can_accept());
    }

    #[tokio::test]
    async fn test_stream_utils() {
        use crate::unity_types::AsyncUnityClass;

        // Create test objects
        let obj1 = AsyncUnityClass::new(1, "GameObject".to_string(), "0".to_string());

        let obj2 = AsyncUnityClass::new(4, "Transform".to_string(), "1".to_string());

        let stream = stream_utils::from_iter(vec![Ok(obj1), Ok(obj2)]);
        let filtered = stream_utils::filter_by_class_names(stream, vec!["GameObject".to_string()]);

        let items: Vec<_> = filtered.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_ref().unwrap().class_name(), "GameObject");
    }

    #[tokio::test]
    async fn test_buffered_stream() {
        let stream = stream_utils::from_iter(vec![
            Ok(AsyncUnityClass::new(1, "A".to_string(), "0".to_string())),
            Ok(AsyncUnityClass::new(2, "B".to_string(), "1".to_string())),
        ]);

        let buffered = BufferedStream::new(stream, StreamConfig::default());
        let items: Vec<_> = buffered.collect().await;

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.is_ok()));
    }

    #[test]
    fn test_asset_chunk() {
        let data = bytes::Bytes::from("test data");
        let chunk = AssetChunk::new(data.clone(), 100, false);

        assert_eq!(chunk.offset, 100);
        assert_eq!(chunk.size, data.len());
        assert!(!chunk.is_last);
        assert_eq!(chunk.chunk_id(), format!("chunk_100_{}", data.len()));
    }

    #[test]
    fn test_stream_config() {
        let config = StreamConfig::for_large_files();
        assert_eq!(config.buffer_size, 8192);
        assert_eq!(config.max_buffer_items, 100);
        assert!(config.enable_backpressure);

        let small_config = StreamConfig::for_small_files();
        assert_eq!(small_config.buffer_size, 512);
        assert!(!small_config.enable_backpressure);
    }
}
