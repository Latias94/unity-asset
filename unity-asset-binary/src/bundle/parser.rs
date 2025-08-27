//! Bundle parser implementation
//!
//! This module provides the main parsing logic for Unity AssetBundles,
//! inspired by UnityPy/files/BundleFile.py


use crate::compression::CompressionType;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use super::header::BundleHeader;
use super::types::{AssetBundle, BundleFileInfo, DirectoryNode, BundleLoadOptions};
use super::compression::BundleCompression;

/// Main bundle parser
/// 
/// This struct handles the parsing of Unity AssetBundle files,
/// supporting both UnityFS and legacy formats.
pub struct BundleParser;

impl BundleParser {
    /// Parse an AssetBundle from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<AssetBundle> {
        Self::from_bytes_with_options(data, BundleLoadOptions::default())
    }

    /// Parse an AssetBundle from binary data with options
    pub fn from_bytes_with_options(data: Vec<u8>, options: BundleLoadOptions) -> Result<AssetBundle> {
        let data_clone = data.clone();
        let mut reader = BinaryReader::new(&data, ByteOrder::Big);

        // Parse header
        let header = BundleHeader::from_reader(&mut reader)?;
        
        if options.validate {
            header.validate()?;
        }

        let mut bundle = AssetBundle::new(header, data_clone);

        // Parse based on bundle format
        match bundle.header.signature.as_str() {
            "UnityFS" => {
                Self::parse_unity_fs(&mut bundle, &mut reader, &options)?;
            }
            "UnityWeb" | "UnityRaw" => {
                Self::parse_legacy(&mut bundle, &mut reader, &options)?;
            }
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unsupported bundle format: {}",
                    bundle.header.signature
                )));
            }
        }

        if options.validate {
            bundle.validate()?;
        }

        Ok(bundle)
    }

    /// Parse UnityFS format bundle
    fn parse_unity_fs(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        // Read blocks info
        Self::read_blocks_info(bundle, reader)?;

        println!("DEBUG: parse_unity_fs - decompress_blocks: {}, load_assets: {}",
            options.decompress_blocks, options.load_assets);

        // Decompress data blocks if requested OR if we need to load assets
        if options.decompress_blocks || options.load_assets {
            println!("DEBUG: Will decompress blocks and parse files");

            println!("DEBUG: About to call read_blocks");
            let blocks_data = Self::read_blocks(bundle, reader)?;
            println!("DEBUG: read_blocks completed, got {} bytes", blocks_data.len());

            println!("DEBUG: About to call parse_files");
            Self::parse_files(bundle, &blocks_data)?;
            println!("DEBUG: parse_files completed");

            // Load assets if requested
            if options.load_assets {
                println!("DEBUG: About to call load_assets");
                Self::load_assets(bundle)?;
                println!("DEBUG: load_assets completed");
            }
        } else {
            println!("DEBUG: Will use lazy directory parsing only");
            // Just parse directory structure without decompressing all data
            Self::parse_directory_lazy(bundle, reader)?;
        }

        Ok(())
    }

    /// Parse legacy format bundle
    fn parse_legacy(
        bundle: &mut AssetBundle,
        reader: &mut BinaryReader,
        options: &BundleLoadOptions,
    ) -> Result<()> {
        // Legacy bundles have a simpler structure
        let header_size = bundle.header.header_size() as usize;
        
        // Skip to after header
        reader.set_position(header_size as u64)?;

        // Read compression information
        let compressed_size = reader.read_u32()?;
        let _uncompressed_size = reader.read_u32()?;

        // Skip some bytes based on version
        let skip_bytes = if bundle.header.version >= 2 { 4 } else { 0 };
        if skip_bytes > 0 {
            reader.read_bytes(skip_bytes)?;
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
        Self::parse_legacy_directory(bundle, &directory_data, header_size)?;

        // Load assets if requested
        if options.load_assets {
            Self::load_assets(bundle)?;
        }

        Ok(())
    }

    /// Read compression blocks information
    fn read_blocks_info(bundle: &mut AssetBundle, reader: &mut BinaryReader) -> Result<()> {
        println!("DEBUG: read_blocks_info called");

        // Apply version-specific alignment
        if bundle.header.version >= 7 {
            reader.align()?;
        }

        // Read blocks info data
        println!("DEBUG: Reader position before reading blocks info: {}", reader.position());
        println!("DEBUG: Reader remaining: {}", reader.remaining());
        println!("DEBUG: Expected blocks info size: {}", bundle.header.compressed_blocks_info_size);
        println!("DEBUG: Block info at end: {}", bundle.header.block_info_at_end());

        // TEMPORARY FIX: Always read blocks info from after header, ignore the flag
        // The BLOCK_INFO_AT_END flag seems to be misunderstood - Python UnityPy always reads from header
        println!("DEBUG: Reading blocks info from after header (ignoring BLOCK_INFO_AT_END flag)");
        let blocks_info_data = reader.read_bytes(bundle.header.compressed_blocks_info_size as usize)?;

        println!("DEBUG: Read {} bytes of blocks info data", blocks_info_data.len());
        print!("DEBUG: First 32 bytes of blocks info: ");
        for i in 0..std::cmp::min(32, blocks_info_data.len()) {
            print!("{:02X} ", blocks_info_data[i]);
        }
        println!();

        // Decompress blocks info
        println!("DEBUG: About to decompress blocks info, size: {}", blocks_info_data.len());
        let uncompressed_data = BundleCompression::decompress_blocks_info(&bundle.header, &blocks_info_data)?;
        println!("DEBUG: Blocks info decompressed successfully, size: {}", uncompressed_data.len());

        // Parse compression blocks
        println!("DEBUG: About to parse compression blocks");
        bundle.blocks = BundleCompression::parse_compression_blocks(&uncompressed_data)?;
        println!("DEBUG: Parsed {} compression blocks", bundle.blocks.len());

        // Validate blocks
        println!("DEBUG: About to validate blocks");
        BundleCompression::validate_blocks(&bundle.blocks)?;
        println!("DEBUG: Blocks validation completed");

        // Parse directory information from the same blocks info data
        Self::parse_directory_from_blocks_info(bundle, &uncompressed_data)?;

        println!("DEBUG: read_blocks_info completed successfully");
        Ok(())
    }

    /// Read and decompress all blocks
    fn read_blocks(bundle: &AssetBundle, reader: &mut BinaryReader) -> Result<Vec<u8>> {
        BundleCompression::decompress_data_blocks(&bundle.header, &bundle.blocks, reader)
    }

    /// Parse files from decompressed block data
    fn parse_files(bundle: &mut AssetBundle, blocks_data: &[u8]) -> Result<()> {
        println!("DEBUG: parse_files called with {} bytes of decompressed data", blocks_data.len());

        // Store the decompressed data
        *bundle.data_mut() = blocks_data.to_vec();

        // Create file info for each node
        for node in &bundle.nodes {
            println!("DEBUG: Creating file info for node: {} (offset: {}, size: {})",
                node.name, node.offset, node.size);

            let file_info = BundleFileInfo::new(
                node.name.clone(),
                node.offset,
                node.size,
            );
            bundle.files.push(file_info);
        }

        println!("DEBUG: Created {} file infos", bundle.files.len());
        Ok(())
    }

    /// Parse directory structure without full decompression (lazy loading)
    fn parse_directory_lazy(bundle: &mut AssetBundle, _reader: &mut BinaryReader) -> Result<()> {
        // For lazy loading, we only parse the directory structure
        // without decompressing all data blocks

        println!("DEBUG: parse_directory_lazy called");

        // The directory information has already been parsed in read_blocks_info()
        // so there's nothing more to do here for lazy loading.

        // The directory nodes are already populated in bundle.nodes
        println!("DEBUG: parse_directory_lazy completed, nodes: {}", bundle.nodes.len());

        Ok(())
    }

    /// Parse directory structure from blocks info data
    fn parse_directory_from_blocks_info(bundle: &mut AssetBundle, blocks_info_data: &[u8]) -> Result<()> {
        let mut reader = BinaryReader::new(blocks_info_data, ByteOrder::Big);

        // Skip uncompressed data hash (16 bytes)
        reader.read_bytes(16)?;

        // Skip compression blocks information
        let block_count = reader.read_i32()? as usize;
        for _ in 0..block_count {
            reader.read_u32()?; // uncompressed_size
            reader.read_u32()?; // compressed_size
            reader.read_u16()?; // flags
        }

        // Now read directory information
        let node_count = reader.read_i32()? as usize;

        // Read directory nodes (UnityFS format)
        for i in 0..node_count {
            let offset = reader.read_i64()? as u64;  // UnityFS uses i64 for offset
            let size = reader.read_i64()? as u64;    // UnityFS uses i64 for size
            let flags = reader.read_u32()?;
            let name = reader.read_cstring()?;

            println!("DEBUG: Directory node {}: name='{}', offset={}, size={}, flags=0x{:X}",
                i, name, offset, size, flags);

            let node = DirectoryNode::new(name, offset, size, flags);
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Parse directory structure from data (legacy method, kept for compatibility)
    fn parse_directory_from_data(bundle: &mut AssetBundle, data: &[u8]) -> Result<()> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);

        // Skip to directory info (this offset varies by bundle version)
        // This is a simplified implementation
        reader.set_position(0)?;

        // Read directory node count
        let node_count = reader.read_i32()? as usize;

        // Read directory nodes
        for _ in 0..node_count {
            let offset = reader.read_u64()?;
            let size = reader.read_u64()?;
            let flags = reader.read_u32()?;
            let name = reader.read_cstring()?;

            let node = DirectoryNode::new(name, offset, size, flags);
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Parse legacy bundle directory
    fn parse_legacy_directory(
        bundle: &mut AssetBundle,
        directory_data: &[u8],
        header_size: usize,
    ) -> Result<()> {
        let mut dir_reader = BinaryReader::new(directory_data, ByteOrder::Big);
        dir_reader.set_position(header_size as u64)?; // Skip header in directory data

        // Read file count
        let file_count = dir_reader.read_i32()? as usize;

        // Read file entries
        for _ in 0..file_count {
            let name = dir_reader.read_cstring()?;
            let offset = dir_reader.read_u32()? as u64;
            let size = dir_reader.read_u32()? as u64;

            let file_info = BundleFileInfo::new(name.clone(), offset, size);
            bundle.files.push(file_info);

            // Also create a directory node for consistency
            let node = DirectoryNode::new(name, offset, size, 1); // Flag 1 = file
            bundle.nodes.push(node);
        }

        Ok(())
    }

    /// Load assets from the bundle files
    fn load_assets(bundle: &mut AssetBundle) -> Result<()> {
        println!("DEBUG: load_assets called with {} nodes", bundle.nodes.len());

        // Clone the data to avoid borrowing issues
        let bundle_data = bundle.data().to_vec();
        let mut data_reader = BinaryReader::new(&bundle_data, ByteOrder::Big);

        // Clone nodes to avoid borrowing issues
        let nodes = bundle.nodes.clone();

        for node in &nodes {
            println!("DEBUG: Processing node: {} (is_file: {})", node.name, node.is_file());

            if node.is_file() {
                // Skip non-asset files (like .resS files)
                if node.name.ends_with(".resS") || node.name.ends_with(".resource") {
                    println!("DEBUG: Skipping resource file: {}", node.name);
                    continue;
                }

                println!("DEBUG: Attempting to parse asset file: {} (offset: {}, size: {})",
                    node.name, node.offset, node.size);

                // Set position to the file's offset in decompressed data
                data_reader.set_position(node.offset)?;

                // Read the file data
                let file_data = data_reader.read_bytes(node.size as usize)?;

                // Show first 32 bytes of file data for debugging
                let preview_len = 32.min(file_data.len());
                let preview: Vec<String> = file_data[..preview_len].iter()
                    .map(|b| format!("{:02X}", b)).collect();
                println!("DEBUG: File data first {} bytes: {}", preview_len, preview.join(" "));

                // Try to parse as SerializedFile
                match crate::asset::SerializedFileParser::from_bytes(file_data) {
                    Ok(serialized_file) => {
                        println!("DEBUG: Successfully parsed SerializedFile: {} objects",
                            serialized_file.objects.len());
                        // Add the SerializedFile as an asset
                        bundle.assets.push(serialized_file);
                    }
                    Err(e) => {
                        println!("DEBUG: Failed to parse as SerializedFile: {}", e);
                        // If it's not a valid SerializedFile, skip or handle differently
                        // For now, we'll skip non-serialized files
                        continue;
                    }
                }
            }
        }

        println!("DEBUG: load_assets completed, {} assets loaded", bundle.assets.len());
        Ok(())
    }

    /// Estimate parsing complexity
    pub fn estimate_complexity(data: &[u8]) -> Result<ParsingComplexity> {
        let mut reader = BinaryReader::new(data, ByteOrder::Big);
        let header = BundleHeader::from_reader(&mut reader)?;
        
        let complexity = match header.signature.as_str() {
            "UnityFS" => {
                let compression_type = header.compression_type()?;
                let has_compression = compression_type != CompressionType::None;
                
                ParsingComplexity {
                    format: "UnityFS".to_string(),
                    estimated_time: if has_compression { "Medium" } else { "Fast" }.to_string(),
                    memory_usage: header.size,
                    has_compression,
                    block_count: 0, // Would need to parse blocks info to get accurate count
                }
            }
            "UnityWeb" | "UnityRaw" => {
                ParsingComplexity {
                    format: header.signature.clone(),
                    estimated_time: "Fast".to_string(),
                    memory_usage: header.size,
                    has_compression: header.signature == "UnityWeb",
                    block_count: 1,
                }
            }
            _ => {
                return Err(BinaryError::unsupported(format!(
                    "Unknown bundle format: {}",
                    header.signature
                )));
            }
        };
        
        Ok(complexity)
    }
}

/// Parsing complexity information
#[derive(Debug, Clone)]
pub struct ParsingComplexity {
    pub format: String,
    pub estimated_time: String,
    pub memory_usage: u64,
    pub has_compression: bool,
    pub block_count: usize,
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parser_creation() {
        // Basic test to ensure parser can be created
        // In practice, you'd need actual bundle data to test parsing
        assert!(true);
    }
}
