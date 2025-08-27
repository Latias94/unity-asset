//! Unity WebFile parsing
//!
//! WebFiles are Unity's web-optimized format that can contain other files
//! and may be compressed with gzip or brotli.

use crate::bundle::{AssetBundle, BundleFileInfo};
use crate::compression::{decompress_brotli, decompress_gzip};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};

/// Magic bytes for different compression formats
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const BROTLI_MAGIC: &[u8] = &[0xce, 0xb2, 0xcf, 0x81, 0x13, 0x00];

/// Compression type used in WebFile
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFileCompression {
    None,
    Gzip,
    Brotli,
}

/// A Unity WebFile that can contain other files
#[derive(Debug)]
pub struct WebFile {
    /// Signature (e.g., "UnityWebData1.0")
    pub signature: String,
    /// Compression type used
    pub compression: WebFileCompression,
    /// Files contained in this WebFile
    pub files: Vec<BundleFileInfo>,
    /// Raw decompressed data
    data: Vec<u8>,
}

impl WebFile {
    /// Parse a WebFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let mut reader = BinaryReader::new(&data, ByteOrder::Little);

        // Detect compression type
        let compression = Self::detect_compression(&mut reader)?;

        // Decompress if necessary
        let decompressed_data = match compression {
            WebFileCompression::None => data,
            WebFileCompression::Gzip => decompress_gzip(&data)?,
            WebFileCompression::Brotli => decompress_brotli(&data)?,
        };

        // Create reader for decompressed data
        let mut reader = BinaryReader::new(&decompressed_data, ByteOrder::Little);

        // Read signature
        let signature = reader.read_cstring()?;
        if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
            return Err(BinaryError::invalid_signature(
                "UnityWebData or TuanjieWebData",
                &signature,
            ));
        }

        // Read header length
        let head_length = reader.read_i32()? as usize;

        // Read file entries
        let mut files = Vec::new();
        while reader.position() < head_length as u64 {
            let offset = reader.read_i32()? as u64;
            let length = reader.read_i32()? as u64;
            let path_length = reader.read_i32()? as usize;
            let name_bytes = reader.read_bytes(path_length)?;
            let name = String::from_utf8(name_bytes).map_err(|e| {
                BinaryError::invalid_data(format!("Invalid UTF-8 in file name: {}", e))
            })?;

            files.push(BundleFileInfo {
                name,
                offset,
                size: length,
            });
        }

        Ok(WebFile {
            signature,
            compression,
            files,
            data: decompressed_data,
        })
    }

    /// Detect compression type from file header
    fn detect_compression(reader: &mut BinaryReader) -> Result<WebFileCompression> {
        // Check for GZIP magic
        let magic = reader.read_bytes(2)?;
        reader.set_position(0)?; // Reset position

        if magic == GZIP_MAGIC {
            return Ok(WebFileCompression::Gzip);
        }

        // Check for Brotli magic at offset 0x20
        reader.set_position(0x20)?;
        let magic = reader.read_bytes(6)?;
        reader.set_position(0)?; // Reset position

        if magic == BROTLI_MAGIC {
            return Ok(WebFileCompression::Brotli);
        }

        Ok(WebFileCompression::None)
    }

    /// Get the files contained in this WebFile
    pub fn files(&self) -> &[BundleFileInfo] {
        &self.files
    }

    /// Extract a specific file by name
    pub fn extract_file(&self, name: &str) -> Result<Vec<u8>> {
        let file_info = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| BinaryError::invalid_data(format!("File not found: {}", name)))?;

        let start = file_info.offset as usize;
        let end = start + file_info.size as usize;

        if end > self.data.len() {
            return Err(BinaryError::invalid_data(format!(
                "File {} extends beyond data bounds: {} > {}",
                name,
                end,
                self.data.len()
            )));
        }

        Ok(self.data[start..end].to_vec())
    }

    /// Try to parse contained files as AssetBundles
    pub fn parse_bundles(&self) -> Result<Vec<AssetBundle>> {
        let mut bundles = Vec::new();

        for file_info in &self.files {
            // Extract file data
            let file_data = self.extract_file(&file_info.name)?;

            // Try to parse as AssetBundle
            match crate::bundle::load_bundle_from_memory(file_data) {
                Ok(bundle) => bundles.push(bundle),
                Err(_) => {
                    // Not an AssetBundle, skip
                    continue;
                }
            }
        }

        Ok(bundles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_detection() {
        // Test GZIP magic detection
        let gzip_data = [0x1f, 0x8b, 0x08, 0x00];
        let mut reader = BinaryReader::new(&gzip_data, ByteOrder::Little);
        let compression = WebFile::detect_compression(&mut reader).unwrap();
        assert_eq!(compression, WebFileCompression::Gzip);
    }

    #[test]
    fn test_webfile_creation() {
        // Test basic WebFile structure creation
        let webfile = WebFile {
            signature: "UnityWebData1.0".to_string(),
            compression: WebFileCompression::None,
            files: Vec::new(),
            data: Vec::new(),
        };

        assert_eq!(webfile.signature, "UnityWebData1.0");
        assert_eq!(webfile.compression, WebFileCompression::None);
        assert!(webfile.files().is_empty());
    }
}
