//! Memory management utilities
//!
//! Async memory pool and buffer management for Unity asset processing.

use std::sync::Arc;
use tokio::sync::Mutex;

/// Memory configuration
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub max_pool_size: usize,
    pub buffer_size: usize,
    pub enable_pool: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_pool_size: 1024 * 1024 * 100, // 100MB
            buffer_size: 8192,
            enable_pool: true,
        }
    }
}

/// Memory pool
pub struct MemoryPool {
    buffers: Arc<Mutex<Vec<Vec<u8>>>>,
    config: MemoryConfig,
}

impl MemoryPool {
    pub fn new(config: MemoryConfig) -> Self {
        Self {
            buffers: Arc::new(Mutex::new(Vec::new())),
            config,
        }
    }

    pub async fn get_buffer(&self, size: usize) -> Vec<u8> {
        if self.config.enable_pool {
            let mut buffers = self.buffers.lock().await;
            if let Some(mut buffer) = buffers.pop() {
                buffer.clear();
                buffer.reserve(size);
                return buffer;
            }
        }
        Vec::with_capacity(size)
    }

    pub async fn return_buffer(&self, buffer: Vec<u8>) {
        if self.config.enable_pool && buffer.capacity() <= self.config.max_pool_size {
            let mut buffers = self.buffers.lock().await;
            buffers.push(buffer);
        }
    }
}

impl Default for MemoryPool {
    fn default() -> Self {
        Self::new(MemoryConfig::default())
    }
}

/// Buffer manager
pub struct BufferManager {
    pool: MemoryPool,
}

impl BufferManager {
    pub fn new(pool: MemoryPool) -> Self {
        Self { pool }
    }

    pub async fn allocate(&self, size: usize) -> Vec<u8> {
        self.pool.get_buffer(size).await
    }
}

impl Default for BufferManager {
    fn default() -> Self {
        Self::new(MemoryPool::default())
    }
}
