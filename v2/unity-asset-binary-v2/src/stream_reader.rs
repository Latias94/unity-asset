//! Async Stream Reader
//!
//! High-performance async binary reader with backpressure control and streaming support.
//! Optimized for Unity asset parsing with zero-copy operations where possible.

use crate::binary_types::{AsyncBinaryData, AsyncBinaryReader, StreamPosition};
use bytes::{Bytes, BytesMut};
use std::io::SeekFrom;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, BufReader, ReadBuf};
use tokio::time::{timeout, Duration};
use unity_asset_core_v2::{Result, UnityAssetError};

/// Configuration for async binary reader
#[derive(Debug, Clone)]
pub struct ReaderConfig {
    /// Buffer size for buffered reading
    pub buffer_size: usize,
    /// Read timeout in milliseconds
    pub timeout_ms: u64,
    /// Whether to use zero-copy operations when possible
    pub zero_copy: bool,
    /// Maximum single read size
    pub max_read_size: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65536, // 64KB default
            timeout_ms: 30000,  // 30 seconds
            zero_copy: true,
            max_read_size: 1048576, // 1MB max single read
        }
    }
}

/// Async binary reader with streaming and buffering support
pub struct AsyncStreamReader<R> {
    /// Underlying reader
    inner: BufReader<R>,
    /// Reader configuration
    config: ReaderConfig,
    /// Current position tracking
    position: StreamPosition,
    /// Read statistics
    stats: ReaderStats,
}

impl<R> AsyncStreamReader<R>
where
    R: AsyncRead + AsyncSeek + Send + Sync + Unpin,
{
    /// Create new async stream reader
    pub fn new(reader: R) -> Self {
        Self::with_config(reader, ReaderConfig::default())
    }

    /// Create new async stream reader with configuration
    pub fn with_config(reader: R, config: ReaderConfig) -> Self {
        let inner = BufReader::with_capacity(config.buffer_size, reader);
        Self {
            inner,
            config,
            position: StreamPosition::new(0, 0, 0),
            stats: ReaderStats::default(),
        }
    }

    /// Get current reader configuration
    pub fn config(&self) -> &ReaderConfig {
        &self.config
    }

    /// Get reading statistics
    pub fn stats(&self) -> &ReaderStats {
        &self.stats
    }

    /// Read bytes with automatic retry on partial reads
    pub async fn read_bytes_retry(&mut self, count: usize, max_retries: u32) -> Result<Bytes> {
        let mut retries = 0;
        let mut buffer = BytesMut::with_capacity(count);

        while buffer.len() < count && retries < max_retries {
            let remaining = count - buffer.len();
            let chunk_size = remaining.min(self.config.max_read_size);

            match timeout(
                Duration::from_millis(self.config.timeout_ms),
                self.read_chunk(chunk_size),
            )
            .await
            {
                Ok(Ok(chunk)) => {
                    if chunk.is_empty() {
                        return Err(UnityAssetError::UnexpectedEof);
                    }
                    buffer.extend_from_slice(&chunk);
                    self.position.advance(chunk.len() as u64);
                    self.stats.bytes_read += chunk.len() as u64;
                }
                Ok(Err(e)) => {
                    retries += 1;
                    self.stats.retry_count += 1;
                    if retries >= max_retries {
                        return Err(e);
                    }
                    // Brief delay before retry
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(_) => {
                    self.stats.timeout_count += 1;
                    return Err(UnityAssetError::timeout(Duration::from_millis(
                        self.config.timeout_ms,
                    )));
                }
            }
        }

        if buffer.len() < count {
            return Err(UnityAssetError::UnexpectedEof);
        }

        self.stats.read_operations += 1;
        Ok(buffer.freeze())
    }

    /// Read a chunk of data
    async fn read_chunk(&mut self, size: usize) -> Result<Bytes> {
        let mut buffer = vec![0u8; size];
        let bytes_read = self
            .inner
            .read(&mut buffer)
            .await
            .map_err(|e| UnityAssetError::parse_error(format!("Read error: {}", e), 0))?;

        buffer.truncate(bytes_read);
        Ok(Bytes::from(buffer))
    }

    /// Peek ahead without advancing position
    pub async fn peek(&mut self, count: usize) -> Result<Bytes> {
        let current_pos = self.current_position().await?;

        // Read the data
        let data = self.read_exact_bytes(count).await?;

        // Seek back to original position
        self.inner
            .seek(SeekFrom::Start(current_pos))
            .await
            .map_err(|e| {
                UnityAssetError::parse_error(format!("Seek error during peek: {}", e), 0)
            })?;

        // Reset position tracking
        self.position = StreamPosition::new(
            current_pos,
            self.position.relative,
            self.position.section_id,
        );

        Ok(data)
    }

    /// Skip bytes by seeking forward
    pub async fn skip_bytes(&mut self, count: u64) -> Result<()> {
        let current_pos = self.current_position().await?;
        let new_pos = current_pos + count;

        self.inner
            .seek(SeekFrom::Start(new_pos))
            .await
            .map_err(|e| UnityAssetError::parse_error(format!("Skip seek error: {}", e), 0))?;

        self.position.advance(count);
        self.stats.bytes_skipped += count;

        Ok(())
    }

    /// Align to boundary (typically used for Unity data alignment)
    pub async fn align_to(&mut self, boundary: u64) -> Result<()> {
        let current_pos = self.current_position().await?;
        let remainder = current_pos % boundary;

        if remainder != 0 {
            let skip_amount = boundary - remainder;
            self.skip_bytes(skip_amount).await?;
        }

        Ok(())
    }

    /// Read null-terminated string (C-style)
    pub async fn read_null_terminated_string(&mut self) -> Result<String> {
        let mut buffer = Vec::new();
        let mut byte_buffer = [0u8; 1];

        loop {
            let bytes_read = self.inner.read(&mut byte_buffer).await.map_err(|e| {
                UnityAssetError::parse_error(format!("String read error: {}", e), 0)
            })?;

            if bytes_read == 0 {
                break; // EOF reached
            }

            let byte = byte_buffer[0];
            if byte == 0 {
                break; // Null terminator found
            }

            buffer.push(byte);
            self.position.advance(1);

            // Prevent infinite loops with very long strings
            if buffer.len() > 1024 * 1024 {
                // 1MB limit
                return Err(UnityAssetError::parse_error(
                    "String too long (>1MB)".to_string(),
                    0,
                ));
            }
        }

        // Skip the null terminator if we read it
        if !buffer.is_empty() || byte_buffer[0] == 0 {
            self.position.advance(1);
        }

        self.stats.string_reads += 1;
        self.stats.bytes_read += buffer.len() as u64 + 1;

        String::from_utf8(buffer)
            .map_err(|e| UnityAssetError::parse_error(format!("Invalid UTF-8 string: {}", e), 0))
    }

    /// Read length-prefixed string (Pascal-style)
    pub async fn read_length_prefixed_string(&mut self) -> Result<String> {
        // Read length (typically u32 for Unity)
        let length = self.read_u32().await?;

        if length > 1024 * 1024 {
            // 1MB sanity check
            return Err(UnityAssetError::parse_error(
                format!("String length too large: {} bytes", length),
                0,
            ));
        }

        if length == 0 {
            return Ok(String::new());
        }

        // Read string data
        let string_bytes = self.read_exact_bytes(length as usize).await?;
        self.stats.string_reads += 1;

        String::from_utf8(string_bytes.to_vec())
            .map_err(|e| UnityAssetError::parse_error(format!("Invalid UTF-8 string: {}", e), 0))
    }

    /// Read primitive types with proper endianness
    pub async fn read_u8(&mut self) -> Result<u8> {
        let bytes = self.read_exact_bytes(1).await?;
        Ok(bytes[0])
    }

    pub async fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_exact_bytes(2).await?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub async fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub async fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub async fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub async fn read_f32(&mut self) -> Result<f32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub async fn read_f64(&mut self) -> Result<f64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub async fn read_i64(&mut self) -> Result<i64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read big-endian variants (for some Unity formats)
    pub async fn read_u32_be(&mut self) -> Result<u32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub async fn read_u64_be(&mut self) -> Result<u64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Create binary data container from current position
    pub async fn create_binary_data(&mut self, size: usize) -> Result<AsyncBinaryData> {
        let current_offset = self.current_position().await?;
        let data = self.read_exact_bytes(size).await?;

        Ok(AsyncBinaryData::new(data, current_offset))
    }

    /// Get remaining bytes in stream (if size is known)
    pub fn remaining_bytes(&self) -> Option<u64> {
        if let Some(total_size) = self.total_size() {
            Some(total_size.saturating_sub(self.position.absolute))
        } else {
            None
        }
    }
}

impl<R> AsyncBinaryReader for AsyncStreamReader<R>
where
    R: AsyncRead + AsyncSeek + Send + Sync + Unpin,
{
    async fn read_exact_bytes(&mut self, count: usize) -> Result<Bytes> {
        self.read_bytes_retry(count, 3).await
    }

    async fn read_exact_bytes_timeout(&mut self, count: usize, timeout_ms: u64) -> Result<Bytes> {
        timeout(
            Duration::from_millis(timeout_ms),
            self.read_exact_bytes(count),
        )
        .await
        .map_err(|_| UnityAssetError::timeout(std::time::Duration::from_millis(timeout_ms)))?
    }

    async fn current_position(&mut self) -> Result<u64> {
        self.inner
            .stream_position()
            .await
            .map_err(|e| UnityAssetError::parse_error(format!("Position query error: {}", e), 0))
    }

    fn total_size(&self) -> Option<u64> {
        // This would need to be set externally or determined from the underlying reader
        None
    }

    async fn is_at_end(&mut self) -> Result<bool> {
        // Try to peek one byte ahead
        match self.peek(1).await {
            Ok(_) => Ok(false),
            Err(UnityAssetError::UnexpectedEof) => Ok(true),
            Err(e) => Err(e),
        }
    }

    async fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    async fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    async fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    async fn read_i64(&mut self) -> Result<i64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    async fn read_u8(&mut self) -> Result<u8> {
        let bytes = self.read_exact_bytes(1).await?;
        Ok(bytes[0])
    }

    async fn read_f32(&mut self) -> Result<f32> {
        let bytes = self.read_exact_bytes(4).await?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    async fn read_f64(&mut self) -> Result<f64> {
        let bytes = self.read_exact_bytes(8).await?;
        Ok(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    async fn read_null_terminated_string(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.read_u8().await?;
            if byte == 0 {
                break;
            }
            bytes.push(byte);
        }
        String::from_utf8(bytes)
            .map_err(|e| UnityAssetError::parse_error(format!("Invalid UTF-8 in string: {}", e), 0))
    }

    async fn read_length_prefixed_string(&mut self) -> Result<String> {
        let length = self.read_u32().await? as usize;
        let bytes = self.read_exact_bytes(length).await?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| UnityAssetError::parse_error(format!("Invalid UTF-8 in string: {}", e), 0))
    }

    async fn seek(&mut self, pos: u64) -> Result<u64> {
        self.inner
            .seek(SeekFrom::Start(pos))
            .await
            .map_err(|e| UnityAssetError::parse_error(format!("Seek error: {}", e), pos))
    }
}

impl<R> AsyncRead for AsyncStreamReader<R>
where
    R: AsyncRead + AsyncSeek + Send + Sync + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<R> AsyncSeek for AsyncStreamReader<R>
where
    R: AsyncRead + AsyncSeek + Send + Sync + Unpin,
{
    fn start_seek(mut self: Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.inner).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        match Pin::new(&mut self.inner).poll_complete(cx) {
            Poll::Ready(Ok(pos)) => {
                // Update position tracking
                self.position = StreamPosition::new(pos, 0, self.position.section_id);
                Poll::Ready(Ok(pos))
            }
            other => other,
        }
    }
}

/// Reader performance statistics
#[derive(Debug, Default, Clone)]
pub struct ReaderStats {
    /// Total bytes read
    pub bytes_read: u64,
    /// Total bytes skipped
    pub bytes_skipped: u64,
    /// Number of read operations
    pub read_operations: u64,
    /// Number of string reads
    pub string_reads: u64,
    /// Number of retry attempts
    pub retry_count: u32,
    /// Number of timeouts
    pub timeout_count: u32,
}

impl ReaderStats {
    /// Calculate average bytes per read operation
    pub fn average_read_size(&self) -> f64 {
        if self.read_operations == 0 {
            0.0
        } else {
            self.bytes_read as f64 / self.read_operations as f64
        }
    }

    /// Calculate retry rate
    pub fn retry_rate(&self) -> f64 {
        if self.read_operations == 0 {
            0.0
        } else {
            self.retry_count as f64 / self.read_operations as f64
        }
    }

    /// Check if performance is healthy
    pub fn is_healthy(&self) -> bool {
        self.retry_rate() < 0.1 && self.timeout_count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio_test;

    #[tokio::test]
    async fn test_basic_reading() {
        let data = vec![1, 2, 3, 4, 5];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        let bytes = reader.read_exact_bytes(3).await.unwrap();
        assert_eq!(bytes.as_ref(), &[1, 2, 3]);

        let remaining = reader.read_exact_bytes(2).await.unwrap();
        assert_eq!(remaining.as_ref(), &[4, 5]);
    }

    #[tokio::test]
    async fn test_primitive_reading() {
        let data = vec![
            0x12, 0x34, 0x56, 0x78, // u32: 0x78563412 (little-endian)
            0x3f, 0x80, 0x00, 0x00, // f32: 1.0 (little-endian)
        ];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        let value = reader.read_u32().await.unwrap();
        assert_eq!(value, 0x78563412);

        let float_val = reader.read_f32().await.unwrap();
        assert_eq!(float_val, 1.0);
    }

    #[tokio::test]
    async fn test_string_reading() {
        // Length-prefixed string: length=5, "hello"
        let data = vec![
            5, 0, 0, 0, // length (u32 little-endian)
            b'h', b'e', b'l', b'l', b'o',
        ];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        let string = reader.read_length_prefixed_string().await.unwrap();
        assert_eq!(string, "hello");
    }

    #[tokio::test]
    async fn test_null_terminated_string() {
        let data = vec![b'w', b'o', b'r', b'l', b'd', 0];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        let string = reader.read_null_terminated_string().await.unwrap();
        assert_eq!(string, "world");
    }

    #[tokio::test]
    async fn test_peek_functionality() {
        let data = vec![1, 2, 3, 4, 5];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        // Peek at first 3 bytes
        let peeked = reader.peek(3).await.unwrap();
        assert_eq!(peeked.as_ref(), &[1, 2, 3]);

        // Position should be unchanged, so reading should get the same data
        let read_data = reader.read_exact_bytes(3).await.unwrap();
        assert_eq!(read_data.as_ref(), &[1, 2, 3]);
    }

    #[tokio::test]
    async fn test_skip_and_align() {
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        // Skip 2 bytes
        reader.skip_bytes(2).await.unwrap();

        // Should be at position 2, align to 4-byte boundary (position 4)
        reader.align_to(4).await.unwrap();

        // Read should get byte at position 4 (value 5)
        let value = reader.read_u8().await.unwrap();
        assert_eq!(value, 5);
    }

    #[tokio::test]
    async fn test_reader_stats() {
        let data = vec![1, 2, 3, 4, 5];
        let cursor = Cursor::new(data);
        let mut reader = AsyncStreamReader::new(cursor);

        reader.read_exact_bytes(3).await.unwrap();
        reader.skip_bytes(2).await.unwrap();

        let stats = reader.stats();
        assert_eq!(stats.bytes_read, 3);
        assert_eq!(stats.bytes_skipped, 2);
        assert!(stats.read_operations > 0);
    }
}
