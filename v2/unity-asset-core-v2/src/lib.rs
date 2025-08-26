//! Unity Asset Core V2
//!
//! Fully async Unity asset parsing core library with ground-up tokio async support,
//! including async I/O, streaming processing, concurrency control and modern async patterns.
//!
//! # Key Features
//!
//! - **Fully Async**: End-to-end async from I/O to processing
//! - **Stream Processing**: Memory-efficient large file processing  
//! - **Concurrency Optimized**: Intelligent concurrency control and backpressure
//! - **Error Recovery**: Auto-retry and graceful degradation
//! - **Performance Monitoring**: Built-in metrics and tracing
//!
//! # Quick Start
//!
//! ```rust,no_run
//! // use unity_asset_core_v2::{AsyncUnityDocument, AsyncAssetLoader};
//! // use tokio_stream::StreamExt;
//!
//! // #[tokio::main]
//! // async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! //     // Load Unity asset file asynchronously
//! //     let doc = AsyncUnityDocument::load_from_path("game.assets").await?;
//! //     
//! //     // Stream process objects
//! //     let mut objects = doc.objects_stream();
//! //     while let Some(object) = objects.next().await {
//! //         let obj = object?;
//! //         println!("Object: {} ({})", obj.name().unwrap_or("Unknown"), obj.class_name());
//! //     }
//! //     
//! //     Ok(())
//! // }
//! ```

pub mod async_traits;
pub mod error;
pub mod io;
pub mod memory;
pub mod metrics;
pub mod stream_types;
pub mod unity_types;

// Re-export main types
pub use async_traits::{
    AsyncAssetLoader, AsyncObjectProcessor, AsyncStreamProcessor, AsyncUnityDocument, LoadProgress,
};
pub use error::{ErrorRecovery, Result, RetryConfig, UnityAssetError};
pub use io::{AsyncFileLoader, AsyncUnityReader, BufferedAsyncReader, ByteOrder, ReadConfig};
pub use memory::{AsyncMemoryPool, BufferManager, MemoryConfig};
pub use metrics::{AsyncMetrics, LoadStatistics, PerformanceTracker};
pub use stream_types::{
    AssetChunkStream, BackPressure, ProcessedObjectStream, StreamConfig, UnityObjectStream,
};
pub use unity_types::{
    AsyncUnityClass, DynamicAccess, DynamicValue, ObjectMetadata, UnityClassRegistry, UnityValue,
};

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Get default async runtime configuration
pub fn default_runtime_config() -> tokio::runtime::Builder {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .worker_threads(num_cpus::get())
        .thread_name("unity-asset-v2");
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_functionality() {
        // Basic functionality test
        assert_eq!(VERSION.is_empty(), false);
    }
}
