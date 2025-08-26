//! Async I/O utilities
//!
//! Async file readers and utilities for Unity asset processing.

use crate::error::{Result, UnityAssetError};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, BufReader};

/// Byte order for reading binary data  
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

impl Default for ByteOrder {
    fn default() -> Self {
        ByteOrder::Little
    }
}

/// Read configuration
#[derive(Debug, Clone)]
pub struct ReadConfig {
    pub buffer_size: usize,
    pub timeout: std::time::Duration,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self {
            buffer_size: 8192,
            timeout: std::time::Duration::from_secs(30),
        }
    }
}

/// Async Unity reader
pub struct AsyncUnityReader<R> {
    reader: R,
    byte_order: ByteOrder,
}

impl<R> AsyncUnityReader<R>
where
    R: AsyncRead + AsyncSeek + Send + Unpin,
{
    pub fn new(reader: R, byte_order: ByteOrder) -> Self {
        Self { reader, byte_order }
    }

    pub async fn read_u32(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.reader.read_exact(&mut buf).await?;
        Ok(match self.byte_order {
            ByteOrder::Little => u32::from_le_bytes(buf),
            ByteOrder::Big => u32::from_be_bytes(buf),
        })
    }

    pub async fn read_cstring(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1];

        loop {
            self.reader.read_exact(&mut buf).await?;
            if buf[0] == 0 {
                break;
            }
            bytes.push(buf[0]);
        }

        String::from_utf8(bytes)
            .map_err(|e| UnityAssetError::Custom(format!("Invalid UTF-8: {}", e)))
    }
}

/// Async file loader
pub struct AsyncFileLoader {
    config: ReadConfig,
}

impl AsyncFileLoader {
    pub fn new(config: ReadConfig) -> Self {
        Self { config }
    }

    pub async fn load_bytes<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        tokio::fs::read(path)
            .await
            .map_err(|e| UnityAssetError::Io(e.to_string()))
    }
}

/// Buffered async reader
pub type BufferedAsyncReader<R> = BufReader<R>;
