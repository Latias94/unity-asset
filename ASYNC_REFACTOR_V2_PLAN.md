# Unity Asset Parser V2 异步化重构计划

*制定日期: 2025年8月26日*  
*目标: 彻底异步化3个crate，创建v2版本，从底层开始使用tokio*

## 📋 项目概述

### 当前状态分析

**现有3个Crate:**
1. **unity-asset-core** - 核心数据结构和traits
2. **unity-asset-yaml** - YAML文件解析 (100%完成，同步实现)
3. **unity-asset-binary** - 二进制资产解析 (75%完成，同步实现)

**主要同步阻塞点:**
- 文件IO操作 (`std::fs::read`, `File::open`, `BufReader`)
- 大数据块处理 (AssetBundle解压缩)
- 纹理/音频解码 (CPU密集型操作)
- 批量文件处理 (无并发支持)

### 重构目标

✅ **彻底异步化** - 从底层开始使用tokio，不保留同步代码  
✅ **模块化重构** - 避免单文件过大，更好的代码组织  
✅ **性能优化** - 流式处理、并发控制、内存效率  
✅ **向前兼容** - 保持API设计理念，但不考虑向后兼容  

## 🏗️ V2架构设计

### 目录结构

```
unity-asset/
├── v2/                          # 新的v2实现
│   ├── unity-asset-core-v2/     # 异步核心
│   │   ├── src/
│   │   │   ├── async_traits.rs      # 异步trait定义
│   │   │   ├── stream_types.rs      # 流式数据类型
│   │   │   ├── error_recovery.rs    # 错误恢复机制
│   │   │   ├── metrics.rs           # 性能监控
│   │   │   └── lib.rs
│   │   └── Cargo.toml
│   ├── unity-asset-yaml-v2/     # 异步YAML处理
│   │   ├── src/
│   │   │   ├── async_loader.rs      # 异步YAML加载器
│   │   │   ├── stream_parser.rs     # 流式解析器
│   │   │   ├── concurrent_writer.rs # 并发写入器
│   │   │   └── lib.rs
│   │   └── Cargo.toml
│   └── unity-asset-binary-v2/   # 异步二进制处理
│       ├── src/
│       │   ├── async_bundle.rs      # 异步AssetBundle
│       │   ├── stream_reader.rs     # 流式读取器
│       │   ├── concurrent_processor.rs # 并发处理器
│       │   ├── codecs/              # 编解码器模块
│       │   │   ├── texture_async.rs
│       │   │   ├── audio_async.rs
│       │   │   └── mesh_async.rs
│       │   └── lib.rs
│       └── Cargo.toml
└── [现有v1代码保持不变]
```

### 核心异步Traits

```rust
// unity-asset-core-v2/src/async_traits.rs
#[async_trait]
pub trait AsyncUnityDocument {
    async fn load_from_path<P: AsRef<Path> + Send>(path: P) -> Result<Self>;
    async fn load_from_stream<S: AsyncRead + Unpin + Send>(stream: S) -> Result<Self>;
    fn objects_stream(&self) -> impl Stream<Item = Result<UnityObject>>;
    async fn save_to_path<P: AsRef<Path> + Send>(&self, path: P) -> Result<()>;
}

#[async_trait]
pub trait AsyncAssetProcessor {
    type Output;
    async fn process_async(&self, data: &[u8]) -> Result<Self::Output>;
    fn process_stream(&self, stream: impl Stream<Item = Vec<u8>>) 
        -> impl Stream<Item = Result<Self::Output>>;
}
```

## 🚀 实施计划

### Phase 1: 核心异步基础 (Week 1-2)

**1.1 创建unity-asset-core-v2**
- [ ] 异步trait定义 (`AsyncUnityDocument`, `AsyncAssetProcessor`)
- [ ] 流式数据类型 (`UnityObjectStream`, `AssetChunk`)
- [ ] 错误恢复机制 (`AsyncErrorRecovery`, `BackoffStrategy`)
- [ ] 性能监控 (`AsyncMetrics`, `AsyncTracer`)

**1.2 依赖管理**
```toml
[dependencies]
tokio = { version = "1.0", features = ["full"] }
futures = "0.3"
async-trait = "0.1"
tokio-stream = "0.1"
async-stream = "0.3"
pin-project = "1.0"
```

### Phase 2: 异步YAML处理 (Week 3-4)

**2.1 创建unity-asset-yaml-v2**
- [ ] `AsyncYamlLoader` - 异步文件加载
- [ ] `StreamParser` - 流式YAML解析
- [ ] `ConcurrentWriter` - 并发写入支持
- [ ] 内存优化的大文件处理

**2.2 关键实现**
```rust
impl AsyncYamlDocument {
    pub async fn load_yaml<P: AsRef<Path> + Send>(path: P) -> Result<Self> {
        let file = tokio::fs::File::open(path).await?;
        let reader = tokio::io::BufReader::new(file);
        Self::load_from_async_reader(reader).await
    }
    
    pub fn objects_stream(&self) -> impl Stream<Item = Result<UnityClass>> {
        stream! {
            for class in &self.classes {
                yield Ok(class.clone());
            }
        }
    }
}
```

### Phase 3: 异步二进制处理 (Week 5-7)

**3.1 创建unity-asset-binary-v2**
- [ ] `AsyncAssetBundle` - 异步Bundle加载
- [ ] `StreamReader` - 流式二进制读取
- [ ] `ConcurrentProcessor` - 并发资产处理
- [ ] 编解码器模块重构

**3.2 流式处理架构**
```rust
impl AsyncAssetBundle {
    pub async fn from_file<P: AsRef<Path> + Send>(path: P) -> Result<Self> {
        let file = tokio::fs::File::open(path).await?;
        let reader = tokio::io::BufReader::new(file);
        Self::from_async_reader(reader).await
    }
    
    pub fn assets_stream(&self) -> impl Stream<Item = Result<AsyncAsset>> {
        // 流式返回资产，避免一次性加载到内存
    }
}
```

### Phase 4: 编解码器异步化 (Week 8-9)

**4.1 纹理异步处理**
```rust
impl AsyncTexture2D {
    pub async fn decode_image_async(&self) -> Result<DynamicImage> {
        // 使用tokio::task::spawn_blocking处理CPU密集操作
        let data = self.data.clone();
        let format = self.format;
        
        tokio::task::spawn_blocking(move || {
            decode_texture_data(&data, format)
        }).await?
    }
    
    pub async fn export_png_async<P: AsRef<Path> + Send>(&self, path: P) -> Result<()> {
        let image = self.decode_image_async().await?;
        let path = path.as_ref().to_owned();
        
        tokio::task::spawn_blocking(move || {
            image.save_with_format(&path, ImageFormat::Png)
        }).await??;
        
        Ok(())
    }
}
```

**4.2 音频异步处理**
```rust
impl AsyncAudioClip {
    pub async fn decode_samples_async(&self) -> Result<Vec<f32>> {
        let data = self.data.clone();
        let format = self.format;
        
        tokio::task::spawn_blocking(move || {
            decode_audio_samples(&data, format)
        }).await?
    }
}
```

### Phase 5: 高级功能和优化 (Week 10)

**5.1 并发控制**
- [ ] `AsyncBatchProcessor` - 智能并发控制
- [ ] 内存使用监控和限制
- [ ] 背压处理机制

**5.2 错误恢复**
- [ ] 自动重试机制
- [ ] 指数退避策略
- [ ] 部分失败恢复

## 📊 性能目标

### 内存使用
- **流式处理**: 大文件处理内存使用恒定 (<100MB)
- **并发控制**: 智能限制并发数，避免内存爆炸
- **零拷贝**: 尽可能使用引用而非克隆

### 处理速度
- **并发加载**: 多文件并发处理，提升3-5倍速度
- **流式解码**: 边读边处理，减少等待时间
- **CPU优化**: CPU密集操作使用线程池，不阻塞异步运行时

### 错误处理
- **智能重试**: 网络错误自动重试，最多3次
- **部分恢复**: 单个资产失败不影响整体处理
- **详细日志**: 完整的错误追踪和性能指标

## 🔧 开发工具和测试

### 开发依赖
```toml
[dev-dependencies]
tokio-test = "0.4"
futures-test = "0.3"
criterion = { version = "0.5", features = ["async_tokio"] }
proptest = "1.0"
```

### 测试策略
- **单元测试**: 每个异步函数的独立测试
- **集成测试**: 端到端异步流程测试
- **性能测试**: 与v1版本的性能对比
- **压力测试**: 大文件和高并发场景测试

## 📈 迁移指南

### API对比
```rust
// V1 (同步)
let doc = YamlDocument::load_yaml("file.asset", false)?;
let objects = doc.filter(Some(&["GameObject"]), None);

// V2 (异步)
let doc = AsyncYamlDocument::load_yaml("file.asset").await?;
let mut objects = doc.filter_stream(&["GameObject"]);
while let Some(obj) = objects.next().await {
    // 处理对象
}
```

### 渐进式迁移
1. **新项目**: 直接使用v2异步API
2. **现有项目**: 提供同步包装器过渡
3. **性能关键**: 优先迁移IO密集部分

## 🎯 成功指标

- [ ] 所有文件IO操作异步化
- [ ] 大文件处理内存使用恒定
- [ ] 多文件并发处理速度提升3倍以上
- [ ] 单元测试覆盖率 >90%
- [ ] 性能测试通过率 100%
- [ ] 文档完整性 100%

## 🛠️ 详细技术实现

### 异步文件IO模式

```rust
// 替换所有同步文件操作
// 旧: std::fs::read("file.bundle")?
// 新: tokio::fs::read("file.bundle").await?

pub struct AsyncFileLoader {
    buffer_size: usize,
    max_concurrent: usize,
}

impl AsyncFileLoader {
    pub async fn load_with_progress<P, F>(
        &self,
        path: P,
        progress_callback: F
    ) -> Result<Vec<u8>>
    where
        P: AsRef<Path> + Send,
        F: Fn(u64, u64) + Send + Sync,
    {
        let file = tokio::fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        let total_size = metadata.len();

        let mut reader = tokio::io::BufReader::with_capacity(self.buffer_size, file);
        let mut buffer = Vec::with_capacity(total_size as usize);
        let mut bytes_read = 0u64;

        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() { break; }

            let chunk_len = chunk.len();
            buffer.extend_from_slice(chunk);
            reader.consume(chunk_len);

            bytes_read += chunk_len as u64;
            progress_callback(bytes_read, total_size);

            // 让出控制权，避免阻塞其他任务
            tokio::task::yield_now().await;
        }

        Ok(buffer)
    }
}
```

### 流式数据处理架构

```rust
// 核心流式处理trait
#[async_trait]
pub trait AsyncStreamProcessor<Input, Output> {
    async fn process_item(&self, item: Input) -> Result<Output>;

    fn process_stream<S>(&self, input: S) -> impl Stream<Item = Result<Output>>
    where
        S: Stream<Item = Result<Input>> + Send,
    {
        input.then(|item_result| async move {
            match item_result {
                Ok(item) => self.process_item(item).await,
                Err(e) => Err(e),
            }
        })
    }
}

// 批处理流式处理器
pub struct AsyncBatchProcessor<T> {
    batch_size: usize,
    max_concurrent: usize,
    processor: T,
}

impl<T> AsyncBatchProcessor<T>
where
    T: AsyncStreamProcessor<Vec<u8>, ProcessedAsset> + Clone + Send + Sync + 'static,
{
    pub fn process_files<P>(&self, files: Vec<P>) -> impl Stream<Item = Result<ProcessedAsset>>
    where
        P: AsRef<Path> + Send + 'static,
    {
        let semaphore = Arc::new(Semaphore::new(self.max_concurrent));
        let processor = self.processor.clone();

        stream! {
            for file_path in files {
                let permit = semaphore.clone().acquire_owned().await.unwrap();
                let processor = processor.clone();

                let task = tokio::spawn(async move {
                    let _permit = permit; // 持有许可证

                    let data = tokio::fs::read(&file_path).await?;
                    processor.process_item(data).await
                });

                match task.await {
                    Ok(Ok(result)) => yield Ok(result),
                    Ok(Err(e)) => yield Err(e),
                    Err(e) => yield Err(BinaryError::from(e)),
                }
            }
        }
    }
}
```

### 内存优化策略

```rust
// 内存池管理
pub struct AsyncMemoryPool {
    small_buffers: Arc<Mutex<Vec<Vec<u8>>>>,  // < 1MB
    large_buffers: Arc<Mutex<Vec<Vec<u8>>>>,  // >= 1MB
    max_small_buffers: usize,
    max_large_buffers: usize,
}

impl AsyncMemoryPool {
    pub async fn get_buffer(&self, size: usize) -> Vec<u8> {
        if size < 1024 * 1024 {
            // 尝试复用小缓冲区
            let mut small_buffers = self.small_buffers.lock().await;
            if let Some(mut buffer) = small_buffers.pop() {
                buffer.clear();
                buffer.reserve(size);
                return buffer;
            }
        } else {
            // 尝试复用大缓冲区
            let mut large_buffers = self.large_buffers.lock().await;
            if let Some(mut buffer) = large_buffers.pop() {
                buffer.clear();
                buffer.reserve(size);
                return buffer;
            }
        }

        Vec::with_capacity(size)
    }

    pub async fn return_buffer(&self, buffer: Vec<u8>) {
        if buffer.capacity() < 1024 * 1024 {
            let mut small_buffers = self.small_buffers.lock().await;
            if small_buffers.len() < self.max_small_buffers {
                small_buffers.push(buffer);
            }
        } else {
            let mut large_buffers = self.large_buffers.lock().await;
            if large_buffers.len() < self.max_large_buffers {
                large_buffers.push(buffer);
            }
        }
    }
}
```

### 错误恢复和重试机制

```rust
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: usize,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub backoff_factor: f64,
}

pub struct AsyncErrorRecovery {
    config: RetryConfig,
}

impl AsyncErrorRecovery {
    pub async fn retry_with_backoff<F, Fut, T, E>(&self, mut operation: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Debug,
    {
        let mut attempt = 0;
        let mut delay = self.config.base_delay;

        loop {
            attempt += 1;

            match operation().await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    if attempt >= self.config.max_attempts {
                        return Err(error);
                    }

                    // 记录重试日志
                    tracing::warn!(
                        "Operation failed (attempt {}/{}): {:?}, retrying in {:?}",
                        attempt, self.config.max_attempts, error, delay
                    );

                    tokio::time::sleep(delay).await;

                    // 指数退避
                    delay = std::cmp::min(
                        Duration::from_millis(
                            (delay.as_millis() as f64 * self.config.backoff_factor) as u64
                        ),
                        self.config.max_delay,
                    );
                }
            }
        }
    }
}
```

## 📋 实施检查清单

### Phase 1: 核心基础 ✅
- [ ] 创建 `v2/unity-asset-core-v2/` 目录结构
- [ ] 实现 `AsyncUnityDocument` trait
- [ ] 实现 `AsyncStreamProcessor` trait
- [ ] 创建 `AsyncMemoryPool` 内存管理
- [ ] 实现 `AsyncErrorRecovery` 错误恢复
- [ ] 添加 `AsyncMetrics` 性能监控
- [ ] 编写单元测试 (覆盖率 >90%)

### Phase 2: YAML异步化 ✅
- [ ] 创建 `v2/unity-asset-yaml-v2/` 目录结构
- [ ] 实现 `AsyncYamlDocument::load_yaml()`
- [ ] 实现 `objects_stream()` 流式对象访问
- [ ] 实现 `save_to_path_async()` 异步保存
- [ ] 优化大YAML文件的内存使用
- [ ] 添加并发写入支持
- [ ] 性能测试 vs v1版本

### Phase 3: Binary异步化 ✅
- [ ] 创建 `v2/unity-asset-binary-v2/` 目录结构
- [ ] 实现 `AsyncAssetBundle::from_file()`
- [ ] 实现 `assets_stream()` 流式资产访问
- [ ] 重构压缩解码为异步操作
- [ ] 实现流式大文件处理
- [ ] 添加并发资产处理
- [ ] 内存使用优化和测试

### Phase 4: 编解码器异步化 ✅
- [ ] 实现 `AsyncTexture2D::decode_image_async()`
- [ ] 实现 `AsyncAudioClip::decode_samples_async()`
- [ ] 实现 `AsyncMesh::get_vertices_async()`
- [ ] CPU密集操作使用 `spawn_blocking`
- [ ] 添加进度回调支持
- [ ] 性能基准测试

### Phase 5: 高级功能 ✅
- [ ] 实现 `AsyncBatchProcessor` 批处理
- [ ] 添加背压控制机制
- [ ] 实现智能并发限制
- [ ] 添加实时监控和指标
- [ ] 完善错误恢复策略
- [ ] 编写完整文档和示例

## 🎯 质量保证

### 测试覆盖率要求
- **单元测试**: >90% 代码覆盖率
- **集成测试**: 所有主要API流程
- **性能测试**: 与v1版本对比
- **内存测试**: 大文件处理内存稳定性
- **并发测试**: 高并发场景稳定性

### 性能基准
- **文件加载**: 比v1快 2-3倍 (并发场景)
- **内存使用**: 大文件处理 <100MB 恒定内存
- **错误恢复**: 网络错误 <3秒 自动恢复
- **并发处理**: 支持 >100 并发文件处理

---

**下一步**: 开始Phase 1的实施，创建unity-asset-core-v2基础架构。
