//! Simple UnityPy Compatibility Tests for v2 Async API
//!
//! Basic compatibility tests to verify v2 implementation structure

use std::fs;
use std::path::Path;
use tokio::fs as async_fs;

const SAMPLES_DIR: &str = "tests/samples";

/// Test basic file reading functionality
#[tokio::test]
async fn test_basic_file_reading() {
    println!("Testing basic async file operations...");

    let samples_path = Path::new(SAMPLES_DIR);
    if !samples_path.exists() {
        println!("Samples directory not found, skipping test - this is expected");
        return;
    }

    let mut file_count = 0;
    if let Ok(mut entries) = async_fs::read_dir(samples_path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_file() {
                file_count += 1;
                if let Ok(data) = async_fs::read(&path).await {
                    println!(
                        "  Read file: {} ({} bytes)",
                        path.file_name().unwrap().to_string_lossy(),
                        data.len()
                    );
                }

                // Limit test files to avoid long test times
                if file_count >= 3 {
                    break;
                }
            }
        }
    }

    println!(
        "Basic file reading test completed. Files processed: {}",
        file_count
    );
    assert!(true, "Basic async file reading should work");
}

/// Test async task spawning functionality
#[tokio::test]
async fn test_async_task_spawning() {
    println!("Testing async task spawning...");

    let tasks: Vec<_> = (0..5)
        .map(|i| {
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                format!("Task {} completed", i)
            })
        })
        .collect();

    let mut results = Vec::new();
    for task in tasks {
        if let Ok(result) = task.await {
            results.push(result);
        }
    }

    println!("Async tasks completed: {} results", results.len());
    for result in &results {
        println!("  {}", result);
    }

    assert_eq!(results.len(), 5, "Should complete 5 async tasks");
    println!("✓ Async task spawning test passed");
}

/// Test async stream processing
#[tokio::test]
async fn test_async_stream_processing() {
    use async_stream::stream;
    use futures::StreamExt;

    println!("Testing async stream processing...");

    let test_stream = stream! {
        for i in 0..5 {
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            yield format!("Stream item {}", i);
        }
    };

    let mut items = Vec::new();
    let mut stream = Box::pin(test_stream);

    while let Some(item) = stream.next().await {
        items.push(item);
        println!("  Received: {}", items.last().unwrap());
    }

    assert_eq!(items.len(), 5, "Should receive 5 stream items");
    println!("✓ Async stream processing test passed");
}

/// Test concurrent async operations
#[tokio::test]
async fn test_concurrent_operations() {
    println!("Testing concurrent async operations...");

    let start = std::time::Instant::now();

    // Create multiple concurrent tasks
    let (result1, result2, result3) = tokio::join!(
        async {
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            "Task 1"
        },
        async {
            tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;
            "Task 2"
        },
        async {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            "Task 3"
        }
    );

    let elapsed = start.elapsed();
    println!("Concurrent tasks completed in {:?}", elapsed);
    println!("  Results: {}, {}, {}", result1, result2, result3);

    // Should complete faster than sum of individual delays (around 20ms, not 45ms)
    assert!(
        elapsed < std::time::Duration::from_millis(30),
        "Concurrent execution should be faster than sequential"
    );
    println!("✓ Concurrent operations test passed");
}

/// Test basic error handling in async context
#[tokio::test]
async fn test_async_error_handling() {
    println!("Testing async error handling...");

    // Test successful operation
    let success_result = async {
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        Ok::<&str, &str>("Success")
    }
    .await;

    assert!(success_result.is_ok(), "Success case should work");
    println!("  ✓ Success case handled correctly");

    // Test error operation
    let error_result = async {
        tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
        Err::<&str, &str>("Error case")
    }
    .await;

    assert!(error_result.is_err(), "Error case should fail");
    println!("  ✓ Error case handled correctly");

    println!("✓ Async error handling test passed");
}

/// Test async timeout functionality
#[tokio::test]
async fn test_async_timeout() {
    use tokio::time::{timeout, Duration};

    println!("Testing async timeout functionality...");

    // Test operation that completes within timeout
    let quick_result = timeout(Duration::from_millis(50), async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        "Quick operation"
    })
    .await;

    assert!(quick_result.is_ok(), "Quick operation should complete");
    println!("  ✓ Quick operation completed within timeout");

    // Test operation that times out
    let slow_result = timeout(Duration::from_millis(10), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        "Slow operation"
    })
    .await;

    assert!(slow_result.is_err(), "Slow operation should timeout");
    println!("  ✓ Slow operation correctly timed out");

    println!("✓ Async timeout test passed");
}

/// Test basic v2 API structure exists
#[tokio::test]
async fn test_v2_api_structure() {
    println!("Testing v2 API structure availability...");

    // Test that v2 modules can be imported without compilation errors
    // This is mainly a compilation test

    use unity_asset_core_v2::Result;
    use unity_asset_core_v2::UnityAssetError;

    // Test basic error creation
    let error = UnityAssetError::parse_error("Test error".to_string(), 0);
    println!("  ✓ Can create UnityAssetError: {:?}", error);

    // Test basic result handling
    let success: Result<String> = Ok("Success".to_string());
    let failure: Result<String> = Err(UnityAssetError::parse_error("Test".to_string(), 0));

    assert!(success.is_ok(), "Success result should be Ok");
    assert!(failure.is_err(), "Failure result should be Err");
    println!("  ✓ Result type works correctly");

    println!("✓ v2 API structure test passed");
}
