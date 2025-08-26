//! 异步Unity资产处理演示
//!
//! 这个示例展示了我们新的异步架构应该如何工作

use futures::StreamExt;
use std::path::Path;
use tokio::fs;
use unity_asset_binary::*;

/// 演示：异步处理单个AssetBundle
#[tokio::main]
async fn demo_single_bundle() -> Result<()> {
    println!("🚀 异步处理单个AssetBundle演示");

    // ✅ 异步加载AssetBundle（非阻塞）
    let mut bundle = AssetBundle::from_file("game.unity3d").await?;
    println!("✓ AssetBundle加载完成: {}", bundle.name());

    // ✅ 流式处理所有资产（内存高效）
    let mut asset_stream = bundle.assets().await?;
    tokio::pin!(asset_stream);

    let mut processed_count = 0;
    while let Some(asset_result) = asset_stream.next().await {
        let asset = asset_result?;
        println!("  处理资产: {}", asset.name());

        // ✅ 异步处理纹理
        if let Some(mut texture_stream) = asset.textures().await? {
            while let Some(texture) = texture_stream.next().await {
                let texture = texture?;
                println!(
                    "    纹理: {} ({}x{})",
                    texture.name, texture.width, texture.height
                );

                // ✅ 异步解码和导出（CPU密集操作不阻塞）
                let image = texture.decode_image_async().await?;
                texture
                    .export_png_async(&format!("output/{}.png", texture.name))
                    .await?;

                processed_count += 1;
            }
        }
    }

    println!("✓ 处理完成，共处理 {} 个纹理", processed_count);
    Ok(())
}

/// 演示：并发处理多个文件
#[tokio::main]
async fn demo_concurrent_processing() -> Result<()> {
    println!("🔥 并发处理多个文件演示");

    // ✅ 创建批处理器（智能并发控制）
    let processor = AsyncBatchProcessor::new(10); // 最大10个并发

    // ✅ 扫描目录，获取所有Unity文件
    let unity_files = fs::read_dir("game_assets/")
        .await?
        .filter_map(|entry| async move {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? == "unity3d" {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .await;

    println!("发现 {} 个Unity文件", unity_files.len());

    // ✅ 并发处理所有文件
    let mut result_stream = processor
        .process_files(unity_files, |path| async move {
            println!("  开始处理: {}", path.display());

            // 异步加载和处理
            let bundle = AssetBundle::from_file(&path).await?;
            let texture_count = bundle.count_textures().await?;
            let audio_count = bundle.count_audio_clips().await?;

            Ok(ProcessResult {
                path: path.clone(),
                texture_count,
                audio_count,
                processing_time: std::time::Instant::now().elapsed(),
            })
        })
        .await?;

    // ✅ 实时显示处理结果
    let mut total_textures = 0;
    let mut total_audio = 0;

    while let Some(result) = result_stream.next().await {
        let result = result?;
        total_textures += result.texture_count;
        total_audio += result.audio_count;

        println!(
            "✓ {}: {} 纹理, {} 音频 ({:?})",
            result.path.file_name().unwrap().to_string_lossy(),
            result.texture_count,
            result.audio_count,
            result.processing_time
        );
    }

    println!(
        "🎉 全部完成！总计: {} 纹理, {} 音频",
        total_textures, total_audio
    );
    Ok(())
}

/// 演示：流式处理超大文件
#[tokio::main]
async fn demo_streaming_large_file() -> Result<()> {
    println!("💾 流式处理超大文件演示");

    // ✅ 流式加载大文件（不会占用大量内存）
    let mut bundle_stream = AssetBundle::stream_from_file("huge_bundle.unity3d").await?;

    let mut processed_mb = 0.0;
    let start_time = std::time::Instant::now();

    // ✅ 逐块处理，内存使用恒定
    while let Some(chunk_result) = bundle_stream.next().await {
        let chunk = chunk_result?;

        // 处理这个数据块
        match chunk {
            AssetChunk::Texture(texture) => {
                // 异步解码纹理
                let image = texture.decode_image_async().await?;
                processed_mb += (image.width() * image.height() * 4) as f64 / 1_000_000.0;

                // 可选：导出到磁盘
                if texture.name.contains("important") {
                    texture
                        .export_png_async(&format!("important/{}.png", texture.name))
                        .await?;
                }
            }
            AssetChunk::Audio(audio) => {
                // 异步处理音频
                let samples = audio.decode_samples_async().await?;
                processed_mb += samples.len() as f64 * 4.0 / 1_000_000.0;
            }
            AssetChunk::Mesh(mesh) => {
                // 异步处理网格
                let vertices = mesh.get_vertices_async().await?;
                processed_mb += vertices.len() as f64 * 12.0 / 1_000_000.0; // 3 floats per vertex
            }
        }

        // 实时显示进度
        let elapsed = start_time.elapsed();
        let speed = processed_mb / elapsed.as_secs_f64();
        println!("  处理进度: {:.1} MB ({:.1} MB/s)", processed_mb, speed);
    }

    println!("✓ 流式处理完成，总计处理 {:.1} MB 数据", processed_mb);
    Ok(())
}

/// 演示：智能错误恢复
#[tokio::main]
async fn demo_error_recovery() -> Result<()> {
    println!("🛡️ 智能错误恢复演示");

    let recovery = AsyncErrorRecovery::new()
        .max_retries(3)
        .backoff_strategy(BackoffStrategy::Exponential);

    // ✅ 带重试的文件处理
    let result = recovery
        .retry_async(|| async {
            // 可能失败的操作
            let bundle = AssetBundle::from_file("unstable_network_file.unity3d").await?;
            let texture_count = bundle.count_textures().await?;
            Ok(texture_count)
        })
        .await;

    match result {
        Ok(count) => println!("✓ 成功处理，发现 {} 个纹理", count),
        Err(e) => println!("❌ 重试失败: {}", e),
    }

    Ok(())
}

/// 演示：实时监控和指标
#[tokio::main]
async fn demo_monitoring() -> Result<()> {
    println!("📊 实时监控和指标演示");

    let mut metrics = AsyncMetrics::new();
    let tracer = AsyncTracer::new();

    // ✅ 带监控的处理流程
    let files = vec!["file1.unity3d", "file2.unity3d", "file3.unity3d"];

    for file in files {
        let result = tracer
            .trace("process_file", || async {
                let bundle = AssetBundle::from_file(file).await?;
                let texture_count = bundle.count_textures().await?;
                Ok(texture_count)
            })
            .await;

        // 更新指标
        metrics.update_operation(&result).await;

        // 实时显示指标
        println!(
            "当前指标: 成功率 {:.1}%, 平均耗时 {:?}",
            metrics.success_rate() * 100.0,
            metrics.average_duration()
        );
    }

    // 生成最终报告
    let report = tracer.generate_report().await;
    println!("📈 性能报告:\n{}", report);

    Ok(())
}

/// 演示：与现有同步代码的兼容性
fn demo_sync_compatibility() -> Result<()> {
    println!("🔄 同步兼容性演示");

    // ✅ 同步API包装器（内部使用异步实现）
    let bundle = AssetBundle::from_file_sync("legacy.unity3d")?;
    println!("✓ 同步API仍然可用: {}", bundle.name());

    // 但建议迁移到异步API以获得更好性能
    println!("💡 建议迁移到异步API以获得更好性能");

    Ok(())
}

// 辅助类型定义
#[derive(Debug)]
struct ProcessResult {
    path: std::path::PathBuf,
    texture_count: usize,
    audio_count: usize,
    processing_time: std::time::Duration,
}

#[derive(Debug)]
enum AssetChunk {
    Texture(AsyncTexture2D),
    Audio(AsyncAudioClip),
    Mesh(AsyncMesh),
}

fn main() {
    println!("🎮 Unity Asset Parser 异步架构演示");
    println!("=====================================");

    // 这些演示展示了我们的异步架构应该如何工作
    // 实际实现将在接下来的10周内完成

    println!("📋 演示场景:");
    println!("1. 异步处理单个AssetBundle");
    println!("2. 并发处理多个文件");
    println!("3. 流式处理超大文件");
    println!("4. 智能错误恢复");
    println!("5. 实时监控和指标");
    println!("6. 与现有同步代码的兼容性");

    println!("\n🚀 开始实施异步架构重构！");
}
