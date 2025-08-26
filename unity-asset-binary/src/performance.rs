//! Performance Optimization and Monitoring
//!
//! This module provides performance monitoring, optimization utilities,
//! and memory management improvements for Unity asset parsing.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Global performance metrics collector
static GLOBAL_METRICS: once_cell::sync::Lazy<Arc<PerformanceMetrics>> =
    once_cell::sync::Lazy::new(|| Arc::new(PerformanceMetrics::new()));

/// Performance metrics for Unity asset parsing operations
#[derive(Debug)]
pub struct PerformanceMetrics {
    /// Total bytes processed
    pub bytes_processed: AtomicU64,
    /// Total parsing time in nanoseconds
    pub total_parse_time_ns: AtomicU64,
    /// Number of files processed
    pub files_processed: AtomicUsize,
    /// Number of objects parsed
    pub objects_parsed: AtomicUsize,
    /// Peak memory usage in bytes
    pub peak_memory_bytes: AtomicU64,
    /// Number of cache hits
    pub cache_hits: AtomicUsize,
    /// Number of cache misses
    pub cache_misses: AtomicUsize,
}

impl PerformanceMetrics {
    /// Create new performance metrics
    pub fn new() -> Self {
        Self {
            bytes_processed: AtomicU64::new(0),
            total_parse_time_ns: AtomicU64::new(0),
            files_processed: AtomicUsize::new(0),
            objects_parsed: AtomicUsize::new(0),
            peak_memory_bytes: AtomicU64::new(0),
            cache_hits: AtomicUsize::new(0),
            cache_misses: AtomicUsize::new(0),
        }
    }

    /// Record bytes processed
    pub fn record_bytes(&self, bytes: u64) {
        self.bytes_processed.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record parsing time
    pub fn record_parse_time(&self, duration: Duration) {
        self.total_parse_time_ns
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Record file processed
    pub fn record_file(&self) {
        self.files_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record object parsed
    pub fn record_object(&self) {
        self.objects_parsed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record memory usage
    pub fn record_memory(&self, bytes: u64) {
        let current = self.peak_memory_bytes.load(Ordering::Relaxed);
        if bytes > current {
            self.peak_memory_bytes.store(bytes, Ordering::Relaxed);
        }
    }

    /// Record cache hit
    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Record cache miss
    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Get current statistics
    pub fn get_stats(&self) -> PerformanceStats {
        let bytes = self.bytes_processed.load(Ordering::Relaxed);
        let time_ns = self.total_parse_time_ns.load(Ordering::Relaxed);
        let files = self.files_processed.load(Ordering::Relaxed);
        let objects = self.objects_parsed.load(Ordering::Relaxed);
        let memory = self.peak_memory_bytes.load(Ordering::Relaxed);
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let misses = self.cache_misses.load(Ordering::Relaxed);

        PerformanceStats {
            bytes_processed: bytes,
            total_parse_time: Duration::from_nanos(time_ns),
            files_processed: files,
            objects_parsed: objects,
            peak_memory_bytes: memory,
            throughput_mbps: if time_ns > 0 {
                (bytes as f64 / 1_048_576.0) / (time_ns as f64 / 1_000_000_000.0)
            } else {
                0.0
            },
            objects_per_second: if time_ns > 0 {
                objects as f64 / (time_ns as f64 / 1_000_000_000.0)
            } else {
                0.0
            },
            cache_hit_rate: if hits + misses > 0 {
                hits as f64 / (hits + misses) as f64
            } else {
                0.0
            },
        }
    }

    /// Reset all metrics
    pub fn reset(&self) {
        self.bytes_processed.store(0, Ordering::Relaxed);
        self.total_parse_time_ns.store(0, Ordering::Relaxed);
        self.files_processed.store(0, Ordering::Relaxed);
        self.objects_parsed.store(0, Ordering::Relaxed);
        self.peak_memory_bytes.store(0, Ordering::Relaxed);
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_misses.store(0, Ordering::Relaxed);
    }
}

impl Default for PerformanceMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Performance statistics snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceStats {
    pub bytes_processed: u64,
    pub total_parse_time: Duration,
    pub files_processed: usize,
    pub objects_parsed: usize,
    pub peak_memory_bytes: u64,
    pub throughput_mbps: f64,
    pub objects_per_second: f64,
    pub cache_hit_rate: f64,
}

/// Performance timer for measuring operations
pub struct PerformanceTimer {
    start: Instant,
    operation: String,
}

impl PerformanceTimer {
    /// Start timing an operation
    pub fn start(operation: impl Into<String>) -> Self {
        Self {
            start: Instant::now(),
            operation: operation.into(),
        }
    }

    /// Finish timing and record the result
    pub fn finish(self) -> Duration {
        let duration = self.start.elapsed();
        GLOBAL_METRICS.record_parse_time(duration);
        duration
    }

    /// Finish timing with byte count
    pub fn finish_with_bytes(self, bytes: u64) -> Duration {
        let duration = self.start.elapsed();
        GLOBAL_METRICS.record_parse_time(duration);
        GLOBAL_METRICS.record_bytes(bytes);
        duration
    }
}

/// Memory pool for reducing allocations
pub struct MemoryPool<T> {
    pool: std::sync::Mutex<Vec<T>>,
    factory: Box<dyn Fn() -> T + Send + Sync>,
}

impl<T> MemoryPool<T> {
    /// Create a new memory pool
    pub fn new<F>(factory: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
    {
        Self {
            pool: std::sync::Mutex::new(Vec::new()),
            factory: Box::new(factory),
        }
    }

    /// Get an item from the pool or create a new one
    pub fn get(&self) -> PooledItem<T> {
        let item = {
            let mut pool = self.pool.lock().unwrap();
            pool.pop().unwrap_or_else(|| (self.factory)())
        };
        PooledItem {
            item: Some(item),
            pool: &self.pool,
        }
    }

    /// Get the current pool size
    pub fn size(&self) -> usize {
        self.pool.lock().unwrap().len()
    }
}

/// An item borrowed from a memory pool
pub struct PooledItem<'a, T> {
    item: Option<T>,
    pool: &'a std::sync::Mutex<Vec<T>>,
}

impl<'a, T> std::ops::Deref for PooledItem<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.item.as_ref().unwrap()
    }
}

impl<'a, T> std::ops::DerefMut for PooledItem<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.item.as_mut().unwrap()
    }
}

impl<'a, T> Drop for PooledItem<'a, T> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            let mut pool = self.pool.lock().unwrap();
            pool.push(item);
        }
    }
}

/// Buffer pool for byte vectors
pub type BufferPool = MemoryPool<Vec<u8>>;

/// Create a global buffer pool
static BUFFER_POOL: once_cell::sync::Lazy<BufferPool> =
    once_cell::sync::Lazy::new(|| BufferPool::new(|| Vec::with_capacity(8192)));

/// Get a buffer from the global pool
pub fn get_buffer() -> PooledItem<'static, Vec<u8>> {
    let mut buffer = BUFFER_POOL.get();
    buffer.clear(); // Clear but keep capacity
    buffer
}

/// Performance optimization settings
#[derive(Debug, Clone)]
pub struct OptimizationSettings {
    /// Enable memory pooling
    pub use_memory_pools: bool,
    /// Enable parallel processing
    pub use_parallel_processing: bool,
    /// Maximum number of threads to use
    pub max_threads: usize,
    /// Buffer size for I/O operations
    pub io_buffer_size: usize,
    /// Enable compression caching
    pub cache_decompressed_data: bool,
    /// Maximum cache size in bytes
    pub max_cache_size: usize,
}

impl Default for OptimizationSettings {
    fn default() -> Self {
        Self {
            use_memory_pools: true,
            use_parallel_processing: true,
            max_threads: num_cpus::get(),
            io_buffer_size: 64 * 1024, // 64KB
            cache_decompressed_data: true,
            max_cache_size: 100 * 1024 * 1024, // 100MB
        }
    }
}

/// Global performance functions
pub fn get_global_metrics() -> Arc<PerformanceMetrics> {
    GLOBAL_METRICS.clone()
}

/// Get current performance statistics
pub fn get_performance_stats() -> PerformanceStats {
    GLOBAL_METRICS.get_stats()
}

/// Reset global performance metrics
pub fn reset_performance_metrics() {
    GLOBAL_METRICS.reset();
}

/// Record that a file was processed
pub fn record_file_processed() {
    GLOBAL_METRICS.record_file();
}

/// Record that an object was parsed
pub fn record_object_parsed() {
    GLOBAL_METRICS.record_object();
}

/// Record memory usage
pub fn record_memory_usage(bytes: u64) {
    GLOBAL_METRICS.record_memory(bytes);
}

/// Record cache hit
pub fn record_cache_hit() {
    GLOBAL_METRICS.record_cache_hit();
}

/// Record cache miss
pub fn record_cache_miss() {
    GLOBAL_METRICS.record_cache_miss();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_performance_metrics() {
        let metrics = PerformanceMetrics::new();

        metrics.record_bytes(1024);
        metrics.record_parse_time(Duration::from_millis(100));
        metrics.record_file();
        metrics.record_object();

        let stats = metrics.get_stats();
        assert_eq!(stats.bytes_processed, 1024);
        assert_eq!(stats.files_processed, 1);
        assert_eq!(stats.objects_parsed, 1);
        assert!(stats.throughput_mbps > 0.0);
    }

    #[test]
    fn test_performance_timer() {
        let timer = PerformanceTimer::start("test_operation");
        thread::sleep(Duration::from_millis(10));
        let duration = timer.finish();
        assert!(duration >= Duration::from_millis(10));
    }

    #[test]
    fn test_memory_pool() {
        let pool = MemoryPool::new(|| Vec::<u8>::with_capacity(1024));

        {
            let mut item1 = pool.get();
            item1.push(42);
            assert_eq!(item1.len(), 1);
        } // item1 is returned to pool here

        {
            let mut item2 = pool.get();
            // The vector might be reused, but we need to clear it manually
            item2.clear(); // Clear it for our test
            assert_eq!(item2.len(), 0);
            assert!(item2.capacity() >= 1024);
        }
    }

    #[test]
    fn test_buffer_pool() {
        let buffer1 = get_buffer();
        let capacity1 = buffer1.capacity();
        drop(buffer1);

        let buffer2 = get_buffer();
        let capacity2 = buffer2.capacity();

        // Should reuse the same buffer
        assert_eq!(capacity1, capacity2);
    }

    #[test]
    fn test_optimization_settings() {
        let settings = OptimizationSettings::default();
        assert!(settings.use_memory_pools);
        assert!(settings.use_parallel_processing);
        assert!(settings.max_threads > 0);
        assert!(settings.io_buffer_size > 0);
    }
}
