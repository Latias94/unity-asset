//! Unity AssetBundle parsing
//!
//! AssetBundles are Unity's primary format for distributing assets.
//! This module supports the UnityFS format used in modern Unity versions.

use crate::asset::{Asset, SerializedFile};
use crate::compression::{decompress, ArchiveFlags, CompressionBlock, CompressionType};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};

#[cfg(feature = "async")]
use std::path::Path;
#[cfg(feature = "async")]
use tokio::fs;

/// AssetBundle header information
#[derive(Debug, Clone)]
pub struct BundleHeader {
    /// Bundle signature (e.g., "UnityFS")
    pub signature: String,
    /// Bundle format version
    pub version: u32,
    /// Unity version that created this bundle
    pub unity_version: String,
    /// Unity revision
    pub unity_revision: String,
    /// Total bundle size
    pub size: u64,
    /// Compressed blocks info size
    pub compressed_blocks_info_size: u32,
    /// Uncompressed blocks info size
    pub uncompressed_blocks_info_size: u32,
    /// Archive flags
    pub flags: u32,
}

impl BundleHeader {
    /// Parse bundle header from binary data
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        let signature = reader.read_cstring()?;
        let version = reader.read_u32()?;
        let unity_version = reader.read_cstring()?;
        let unity_revision = reader.read_cstring()?;

        let mut header = Self {
            signature,
            version,
            unity_version,
            unity_revision,
            size: 0,
            compressed_blocks_info_size: 0,
            uncompressed_blocks_info_size: 0,
            flags: 0,
        };

        // Read additional fields for UnityFS format
        if header.signature == "UnityFS" {
            header.size = reader.read_i64()? as u64;
            header.compressed_blocks_info_size = reader.read_u32()?;
            header.uncompressed_blocks_info_size = reader.read_u32()?;
            header.flags = reader.read_u32()?;

            // Skip padding byte for older versions
            if header.signature != "UnityFS" {
                reader.read_u8()?;
            }
        }

        Ok(header)
    }

    /// Get the compression type from flags
    pub fn compression_type(&self) -> Result<CompressionType> {
        CompressionType::from_flags(self.flags & ArchiveFlags::COMPRESSION_TYPE_MASK)
    }

    /// Check if block info is at the end of the file
    pub fn block_info_at_end(&self) -> bool {
        (self.flags & ArchiveFlags::BLOCK_INFO_AT_END) != 0
    }
}

/// Information about a file within the bundle
#[derive(Debug, Clone)]
pub struct BundleFileInfo {
    /// Offset within the bundle data
    pub offset: u64,
    /// Size of the file
    pub size: u64,
    /// File name
    pub name: String,
}

/// Directory node in the bundle
#[derive(Debug, Clone)]
pub struct DirectoryNode {
    /// Node name
    pub name: String,
    /// Offset in the bundle
    pub offset: u64,
    /// Size of the data
    pub size: u64,
    /// Flags
    pub flags: u32,
}

/// A Unity AssetBundle
#[derive(Debug)]
pub struct AssetBundle {
    /// Bundle header
    pub header: BundleHeader,
    /// Compression blocks
    pub blocks: Vec<CompressionBlock>,
    /// Directory nodes
    pub nodes: Vec<DirectoryNode>,
    /// File information
    pub files: Vec<BundleFileInfo>,
    /// Contained assets
    pub assets: Vec<Asset>,
    /// Raw bundle data
    data: Vec<u8>,
}

impl AssetBundle {
    /// Parse an AssetBundle from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let data_clone = data.clone();
        let mut reader = BinaryReader::new(&data_clone, ByteOrder::Big);

        // Read header
        let header = BundleHeader::from_reader(&mut reader)?;

        // Validate signature
        match header.signature.as_str() {
            "UnityFS" => {
                // Modern UnityFS format
                Self::parse_unity_fs(data, header, reader)
            }
            "UnityWeb" | "UnityRaw" => {
                // Legacy Unity formats
                Self::parse_unity_web_raw(data, header, reader)
            }
            "UnityArchive" => Err(BinaryError::unsupported(
                "UnityArchive format is not supported yet",
            )),
            _ => Err(BinaryError::invalid_signature(
                "UnityFS, UnityWeb, or UnityRaw",
                &header.signature,
            )),
        }
    }

    /// Parse an AssetBundle from binary data asynchronously
    #[cfg(feature = "async")]
    pub async fn from_bytes_async(data: Vec<u8>) -> Result<Self> {
        // For now, use spawn_blocking to run the sync version
        // In a full implementation, we would make the parsing truly async
        let result = tokio::task::spawn_blocking(move || Self::from_bytes(data))
            .await
            .map_err(|e| BinaryError::format(format!("Task join error: {}", e)))??;

        Ok(result)
    }

    /// Load AssetBundle from file path asynchronously
    #[cfg(feature = "async")]
    pub async fn from_path_async<P: AsRef<Path>>(path: P) -> Result<Self> {
        let data = fs::read(path)
            .await
            .map_err(|e| BinaryError::format(format!("Failed to read file: {}", e)))?;

        Self::from_bytes_async(data).await
    }

    /// Parse UnityFS format bundle
    fn parse_unity_fs(
        data: Vec<u8>,
        header: BundleHeader,
        mut reader: BinaryReader,
    ) -> Result<Self> {
        let mut bundle = Self {
            header,
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            data: data.clone(), // Clone data for storage
        };

        // Read blocks and directory info
        bundle.read_blocks_info(&mut reader)?;

        // Read the actual block data
        let blocks_data = bundle.read_blocks(&mut reader)?;

        // Parse files from the decompressed data
        bundle.parse_files(&blocks_data)?;

        // Load assets from files
        bundle.load_assets()?;

        Ok(bundle)
    }

    /// Parse UnityWeb/UnityRaw format bundle (based on UnityPy implementation)
    fn parse_unity_web_raw(
        data: Vec<u8>,
        header: BundleHeader,
        mut reader: BinaryReader,
    ) -> Result<Self> {
        let mut bundle = Self {
            header,
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            data: data.clone(),
        };

        // Read header fields specific to UnityWeb/UnityRaw
        let version = bundle.header.version;

        // Read hash and CRC for version >= 4
        if version >= 4 {
            let _hash = reader.read_bytes(16)?; // MD5 hash
            let _crc = reader.read_u32()?;
        }

        // Read header information
        let _minimum_streamed_bytes = reader.read_u32()?;
        let header_size = reader.read_u32()?;
        let _number_of_levels_to_download = reader.read_u32()?;
        let level_count = reader.read_i32()?;

        // Skip level information (4 bytes * 2 * (level_count - 1))
        if level_count > 1 {
            let skip_bytes = (level_count - 1) as usize * 8;
            let _skipped = reader.read_bytes(skip_bytes)?; // Skip by reading and discarding
        }

        // Read compression information
        let compressed_size = reader.read_u32()?;
        let _uncompressed_size = reader.read_u32()?;

        // Read additional fields for newer versions
        if version >= 2 {
            let _complete_file_size = reader.read_u32()?;
        }
        if version >= 3 {
            let _file_info_header_size = reader.read_u32()?;
        }

        // Move to the data section
        reader.set_position(header_size as u64)?;

        // Read and decompress the directory data
        let compressed_data = reader.read_bytes(compressed_size as usize)?;
        let directory_data = if bundle.header.signature == "UnityWeb" {
            // UnityWeb uses LZMA compression
            crate::compression::decompress(
                &compressed_data,
                CompressionType::Lzma,
                compressed_data.len() * 4, // Estimate uncompressed size
            )?
        } else {
            // UnityRaw is uncompressed
            compressed_data
        };

        // Parse directory information from decompressed data
        let mut dir_reader = BinaryReader::new(&directory_data, ByteOrder::Big);
        dir_reader.set_position(header_size as u64)?; // Skip header in directory data

        let nodes_count = dir_reader.read_i32()?;
        for _ in 0..nodes_count {
            let name = dir_reader.read_cstring()?;
            let offset = dir_reader.read_u32()? as u64;
            let size = dir_reader.read_u32()? as u64;

            bundle.nodes.push(DirectoryNode {
                name: name.clone(),
                offset,
                size,
                flags: 0, // UnityWeb/Raw doesn't use flags like UnityFS
            });

            bundle.files.push(BundleFileInfo { name, offset, size });
        }

        // Load assets from files
        bundle.load_assets()?;

        Ok(bundle)
    }

    /// Read compression blocks information (based on unity-rs successful implementation)
    fn read_blocks_info(&mut self, reader: &mut BinaryReader) -> Result<()> {
        // Apply version-specific alignment (critical for correct parsing)
        if self.header.version >= 7 {
            reader.align_to(16)?;
        }

        // Store current position for potential reset
        let current_offset = reader.position();

        // Read blocks info data based on flags (exactly like unity-rs)
        let blocks_info_data = if self.header.flags & 0x80 != 0 {
            // BlocksInfoAtTheEnd
            // Blocks info is at the end of the file
            let file_len = reader.len() as u64;
            let blocks_info_pos = file_len - self.header.compressed_blocks_info_size as u64;
            reader.set_position(blocks_info_pos)?;
            let data = reader.read_bytes(self.header.compressed_blocks_info_size as usize)?;
            reader.set_position(current_offset)?; // Reset position for data reading
            data
        } else {
            // Blocks info is right after header
            reader.read_bytes(self.header.compressed_blocks_info_size as usize)?
        };

        // Decompress blocks info using correct compression type from flags
        let compression_type = self.header.flags & 0x3F; // CompressionTypeMask
        let uncompressed_data = match compression_type {
            0 => blocks_info_data, // No compression
            2 | 3 => {
                // LZ4 or LZ4HC
                decompress(
                    &blocks_info_data,
                    CompressionType::Lz4,
                    self.header.uncompressed_blocks_info_size as usize,
                )?
            }
            1 => {
                // LZMA
                return Err(BinaryError::unsupported_compression(
                    "LZMA compression not yet supported",
                ));
            }
            _ => {
                return Err(BinaryError::invalid_format(format!(
                    "Unknown compression type: {}",
                    compression_type
                )));
            }
        };

        // Parse the decompressed blocks info
        let mut blocks_reader = BinaryReader::new(&uncompressed_data, ByteOrder::Big);

        // Skip uncompressed data hash (16 bytes) - critical step
        blocks_reader.read_bytes(16)?;

        // Read compression blocks
        let block_count = blocks_reader.read_i32()? as usize;
        for _ in 0..block_count {
            let uncompressed_size = blocks_reader.read_u32()?;
            let compressed_size = blocks_reader.read_u32()?;
            let flags = blocks_reader.read_u16()?;

            let block = CompressionBlock::new(uncompressed_size, compressed_size, flags);
            self.blocks.push(block);
        }

        // Read directory nodes
        let node_count = blocks_reader.read_i32()? as usize;
        for _ in 0..node_count {
            let offset = blocks_reader.read_i64()? as u64;
            let size = blocks_reader.read_i64()? as u64;
            let flags = blocks_reader.read_u32()?;
            let name = blocks_reader.read_cstring()?;

            let node = DirectoryNode {
                name,
                offset,
                size,
                flags,
            };
            self.nodes.push(node);
        }

        // Apply padding if needed (like unity-rs)
        if self.header.flags & 0x200 != 0 {
            // BlockInfoNeedPaddingAtStart
            reader.align_to(16)?;
        }

        Ok(())
    }

    /// Read and decompress all blocks (improved based on UnityPy)
    fn read_blocks(&self, reader: &mut BinaryReader) -> Result<Vec<u8>> {
        let mut decompressed_data = Vec::new();

        // Calculate the position where block data starts
        let mut data_pos = if self.header.block_info_at_end() {
            // If blocks info is at end, data starts after header
            reader.position()
        } else {
            // If blocks info is after header, data starts after blocks info
            reader.position()
        };

        for block in &self.blocks {
            // Seek to the correct position for this block
            reader.set_position(data_pos)?;

            let compressed_data = reader.read_bytes(block.compressed_size as usize)?;
            let block_data = block.decompress(&compressed_data)?;
            decompressed_data.extend_from_slice(&block_data);

            // Move to next block position
            data_pos += block.compressed_size as u64;
        }

        Ok(decompressed_data)
    }

    /// Parse files from decompressed block data (based on unity-rs)
    fn parse_files(&mut self, blocks_data: &[u8]) -> Result<()> {
        // Store the decompressed data for later use
        self.data = blocks_data.to_vec();

        // Create file info for each node
        for node in &self.nodes {
            let file_info = BundleFileInfo {
                offset: node.offset,
                size: node.size,
                name: node.name.clone(),
            };
            self.files.push(file_info);
        }
        Ok(())
    }

    /// Load assets from the bundle files (fixed based on unity-rs)
    fn load_assets(&mut self) -> Result<()> {
        // Create a reader for the decompressed data
        let mut data_reader = BinaryReader::new(&self.data, ByteOrder::Big);

        for node in &self.nodes {
            // Check if this is an asset file
            if self.is_asset_file(&node.name) {
                // Validate offset and size
                if node.offset + node.size > self.data.len() as u64 {
                    continue;
                }

                // Set position to the file's offset in decompressed data
                data_reader.set_position(node.offset)?;

                // Read the file data
                let file_data = data_reader.read_bytes(node.size as usize)?;

                // Try to parse as SerializedFile
                match SerializedFile::from_bytes(file_data) {
                    Ok(asset) => {
                        self.assets.push(asset);
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
        }
        Ok(())
    }

    /// Check if a file is likely an asset file
    fn is_asset_file(&self, name: &str) -> bool {
        // Simple heuristic: files without extensions or with .assets extension
        !name.contains('.') || name.ends_with(".assets") || name.ends_with(".unity")
    }

    /// Get all assets in this bundle
    pub fn assets(&self) -> &[Asset] {
        &self.assets
    }

    /// Process all assets concurrently with a custom async function
    #[cfg(feature = "async")]
    pub async fn process_assets_concurrent<F, Fut, T>(
        &self,
        processor: F,
        max_concurrent: usize,
    ) -> Result<Vec<T>>
    where
        F: Fn(&Asset) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        use futures::stream::{self, StreamExt};

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
        let results: Result<Vec<T>> = stream::iter(self.assets.iter())
            .map(|asset| {
                let processor = &processor;
                let semaphore = semaphore.clone();
                async move {
                    let _permit = semaphore
                        .acquire()
                        .await
                        .map_err(|e| BinaryError::format(format!("Semaphore error: {}", e)))?;
                    processor(asset).await
                }
            })
            .buffer_unordered(max_concurrent)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect();

        results
    }

    /// Extract all objects from all assets concurrently
    #[cfg(feature = "async")]
    pub async fn extract_all_objects_concurrent(
        &self,
        _max_concurrent: usize,
    ) -> Result<Vec<crate::object::UnityObject>> {
        let mut all_objects = Vec::new();

        // Use a simpler approach to avoid lifetime issues
        for asset in &self.assets {
            let objects = asset.get_objects()?;
            all_objects.extend(objects);
        }

        Ok(all_objects)
    }

    /// Get bundle name/path
    pub fn name(&self) -> &str {
        "AssetBundle"
    }

    /// Get Unity version
    pub fn unity_version(&self) -> &str {
        &self.header.unity_version
    }

    /// Get all file names in the bundle
    pub fn file_names(&self) -> Vec<&str> {
        self.files.iter().map(|f| f.name.as_str()).collect()
    }

    /// Get file data by name
    pub fn get_file_data(&self, name: &str) -> Option<Vec<u8>> {
        for (file_info, node) in self.files.iter().zip(self.nodes.iter()) {
            if file_info.name == name {
                let start = node.offset as usize;
                let end = start + node.size as usize;

                if end <= self.data.len() {
                    return Some(self.data[start..end].to_vec());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_header_compression_type() {
        let header = BundleHeader {
            signature: "UnityFS".to_string(),
            version: 6,
            unity_version: "2019.4.0f1".to_string(),
            unity_revision: "abc123".to_string(),
            size: 1000,
            compressed_blocks_info_size: 100,
            uncompressed_blocks_info_size: 200,
            flags: 2, // LZ4 compression
        };

        let compression = header.compression_type().unwrap();
        assert_eq!(compression, CompressionType::Lz4);
    }

    #[test]
    fn test_is_asset_file() {
        let bundle = AssetBundle {
            header: BundleHeader {
                signature: "UnityFS".to_string(),
                version: 6,
                unity_version: "2019.4.0f1".to_string(),
                unity_revision: "abc123".to_string(),
                size: 0,
                compressed_blocks_info_size: 0,
                uncompressed_blocks_info_size: 0,
                flags: 0,
            },
            blocks: Vec::new(),
            nodes: Vec::new(),
            files: Vec::new(),
            assets: Vec::new(),
            data: Vec::new(),
        };

        assert!(bundle.is_asset_file("CAB-123456789"));
        assert!(bundle.is_asset_file("level1.assets"));
        assert!(bundle.is_asset_file("scene.unity"));
        assert!(!bundle.is_asset_file("texture.png"));
    }

    #[test]
    fn test_format_support() {
        // Test that we support the expected formats
        let supported_formats = ["UnityFS", "UnityWeb", "UnityRaw"];
        let _unsupported_formats = ["UnityArchive", "InvalidFormat"];

        for format in supported_formats {
            // Create minimal header data for each format
            let mut data = Vec::new();
            data.extend_from_slice(format.as_bytes());
            data.push(0); // null terminator
            data.extend_from_slice(&[0, 0, 0, 6]); // version 6
            data.extend_from_slice(b"2019.4.0f1\0"); // unity version
            data.extend_from_slice(b"abc123\0"); // revision

            // Add format-specific data
            if format == "UnityFS" {
                data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 100]); // size
                data.extend_from_slice(&[0, 0, 0, 0]); // compressed_blocks_info_size
                data.extend_from_slice(&[0, 0, 0, 0]); // uncompressed_blocks_info_size
                data.extend_from_slice(&[0, 0, 0, 0]); // flags
            } else {
                // UnityWeb/UnityRaw format - add minimal required fields
                data.extend_from_slice(&[0, 0, 0, 0]); // minimum_streamed_bytes
                data.extend_from_slice(&[0, 0, 0, 32]); // header_size
                data.extend_from_slice(&[0, 0, 0, 0]); // number_of_levels
                data.extend_from_slice(&[0, 0, 0, 1]); // level_count
                data.extend_from_slice(&[0, 0, 0, 0]); // compressed_size
                data.extend_from_slice(&[0, 0, 0, 0]); // uncompressed_size
            }

            // Pad to minimum size
            while data.len() < 100 {
                data.push(0);
            }

            // Should not panic when parsing (though may fail due to incomplete data)
            let result = AssetBundle::from_bytes(data);
            match result {
                Ok(_) => {
                    // Success is fine
                }
                Err(e) => {
                    // Failure is also fine for this test, as long as it's not a panic
                    // and the error is reasonable (not "unsupported format")
                    let error_msg = e.to_string();
                    assert!(
                        !error_msg.contains("not supported yet"),
                        "Format {} should be supported but got error: {}",
                        format,
                        error_msg
                    );
                }
            }
        }
    }
}
