//! Metrics and performance tracking
//!
//! Async performance monitoring and metrics collection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Async metrics collector
#[derive(Debug)]
pub struct AsyncMetrics {
    counters: Arc<RwLock<HashMap<String, AtomicU64>>>,
    gauges: Arc<RwLock<HashMap<String, AtomicU64>>>,
    timers: Arc<RwLock<HashMap<String, Vec<Duration>>>>,
}

impl AsyncMetrics {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(RwLock::new(HashMap::new())),
            gauges: Arc::new(RwLock::new(HashMap::new())),
            timers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn increment_counter(&self, name: &str) {
        let counters = self.counters.read().await;
        if let Some(counter) = counters.get(name) {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            drop(counters);
            let mut counters = self.counters.write().await;
            counters.insert(name.to_string(), AtomicU64::new(1));
        }
    }
}

impl Default for AsyncMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Performance tracker  
pub struct PerformanceTracker {
    start_time: Instant,
    operations: AtomicUsize,
}

impl PerformanceTracker {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            operations: AtomicUsize::new(0),
        }
    }

    pub fn record_operation(&self) {
        self.operations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn operations_per_second(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        let ops = self.operations.load(Ordering::Relaxed) as f64;
        if elapsed > 0.0 {
            ops / elapsed
        } else {
            0.0
        }
    }
}

impl Default for PerformanceTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Load statistics
#[derive(Debug, Default)]
pub struct LoadStatistics {
    pub bytes_loaded: AtomicU64,
    pub objects_processed: AtomicU64,
    pub errors_encountered: AtomicU64,
}
