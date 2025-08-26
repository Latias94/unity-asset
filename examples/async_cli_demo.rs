//! Async CLI Demo
//!
//! Demonstrates the performance benefits of the async CLI tool

use std::process::Command;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Unity Asset Parser CLI Performance Demo");
    println!("==========================================\n");

    // Test directory with YAML files
    let test_dir = "test_output";

    println!("📂 Testing directory: {}", test_dir);
    println!("🔄 Running performance comparison...\n");

    // Test sync version
    println!("1️⃣  Testing Sync CLI (unity_asset):");
    let sync_start = Instant::now();
    let sync_output = Command::new("cargo")
        .args(&[
            "run",
            "--bin",
            "unity_asset",
            "--",
            "parse-yaml",
            "-i",
            test_dir,
            "-f",
            "summary",
        ])
        .output()?;
    let sync_duration = sync_start.elapsed();

    if sync_output.status.success() {
        println!("   ✅ Success in {:.3}s", sync_duration.as_secs_f64());
    } else {
        println!(
            "   ❌ Failed: {}",
            String::from_utf8_lossy(&sync_output.stderr)
        );
    }

    // Test async version with different concurrency levels
    let concurrency_levels = [1, 2, 4, 8];

    for &concurrency in &concurrency_levels {
        println!(
            "\n2️⃣  Testing Async CLI (unity_asset_async) - Concurrency {}:",
            concurrency
        );
        let async_start = Instant::now();
        let async_output = Command::new("cargo")
            .args(&[
                "run",
                "--features",
                "async",
                "--bin",
                "unity_asset_async",
                "--",
                "parse-yaml",
                "-i",
                test_dir,
                "-f",
                "summary",
                "--concurrency",
                &concurrency.to_string(),
            ])
            .output()?;
        let async_duration = async_start.elapsed();

        if async_output.status.success() {
            println!("   ✅ Success in {:.3}s", async_duration.as_secs_f64());

            if concurrency == 4 {
                let speedup = sync_duration.as_secs_f64() / async_duration.as_secs_f64();
                println!("   🚀 Speedup vs sync: {:.2}x", speedup);
            }
        } else {
            println!(
                "   ❌ Failed: {}",
                String::from_utf8_lossy(&async_output.stderr)
            );
        }
    }

    println!("\n📊 Performance Summary:");
    println!("   • Sync version: Single-threaded processing");
    println!("   • Async version: Concurrent processing with configurable parallelism");
    println!("   • Progress bars: Real-time feedback for long operations");
    println!("   • Error handling: Graceful handling of individual file failures");

    println!("\n🎯 Key Benefits of Async CLI:");
    println!("   ✅ Concurrent file processing");
    println!("   ✅ Progress visualization");
    println!("   ✅ Configurable concurrency");
    println!("   ✅ Better resource utilization");
    println!("   ✅ Responsive user experience");

    Ok(())
}
