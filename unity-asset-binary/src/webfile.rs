//! Unity WebFile parsing
//!
//! WebFiles are Unity's web-optimized format that can contain other files
//! and may be compressed with gzip or brotli.

use crate::bundle::{AssetBundle, BundleFileInfo};
use crate::compression::{decompress_brotli, decompress_gzip};
use crate::data_view::DataView;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use std::ops::Range;
use std::sync::Arc;

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
    data: DataView,
}

impl WebFile {
    /// Parse a WebFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let data: Arc<[u8]> = data.into();
        let len = data.len();
        Self::from_shared_range(data, 0..len)
    }

    pub fn from_shared_range(data: Arc<[u8]>, range: Range<usize>) -> Result<Self> {
        let view = DataView::from_range(data, range)?;
        Self::from_view(view)
    }

    fn from_view(view: DataView) -> Result<Self> {
        let mut reader = BinaryReader::new(view.as_bytes(), ByteOrder::Little);

        // Detect compression type
        let compression = Self::detect_compression(&mut reader)?;

        // Decompress if necessary
        let decompressed_data: DataView = match compression {
            WebFileCompression::None => view,
            WebFileCompression::Gzip => DataView::from_arc(decompress_gzip(view.as_bytes())?.into()),
            WebFileCompression::Brotli => {
                DataView::from_arc(decompress_brotli(view.as_bytes())?.into())
            }
        };

        // Create reader for decompressed data
        let mut reader = BinaryReader::new(decompressed_data.as_bytes(), ByteOrder::Little);

        // Read signature
        let signature = reader.read_cstring()?;
        if !signature.starts_with("UnityWebData") && !signature.starts_with("TuanjieWebData") {
            return Err(BinaryError::invalid_signature(
                "UnityWebData or TuanjieWebData",
                &signature,
            ));
        }

        // Read header length
        let head_length_i32 = reader.read_i32()?;
        if head_length_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative WebFile head_length: {}",
                head_length_i32
            )));
        }
        let head_length = head_length_i32 as usize;
        let total_len = decompressed_data.len();
        if head_length > total_len {
            return Err(BinaryError::invalid_data(format!(
                "WebFile head_length {} exceeds data len {}",
                head_length, total_len
            )));
        }
        if head_length < reader.position() as usize {
            return Err(BinaryError::invalid_data(format!(
                "WebFile head_length {} precedes current position {}",
                head_length,
                reader.position()
            )));
        }

        // Read file entries
        let mut files = Vec::new();
        while reader.position() < head_length as u64 {
            let offset_i32 = reader.read_i32()?;
            let length_i32 = reader.read_i32()?;
            let path_len_i32 = reader.read_i32()?;

            if offset_i32 < 0 || length_i32 < 0 || path_len_i32 < 0 {
                return Err(BinaryError::invalid_data(format!(
                    "Negative WebFile entry values: offset={} length={} path_len={}",
                    offset_i32, length_i32, path_len_i32
                )));
            }

            let offset = offset_i32 as u64;
            let length = length_i32 as u64;
            let path_length = path_len_i32 as usize;
            if path_length > 16 * 1024 {
                return Err(BinaryError::ResourceLimitExceeded(format!(
                    "WebFile entry name too large: {}",
                    path_length
                )));
            }
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

    pub fn data_arc(&self) -> Arc<[u8]> {
        self.data.backing_arc()
    }

    /// Extract a specific file by name
    pub fn extract_file(&self, name: &str) -> Result<Vec<u8>> {
        Ok(self.extract_file_slice(name)?.to_vec())
    }

    pub fn extract_file_slice(&self, name: &str) -> Result<&[u8]> {
        let file_info = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| BinaryError::invalid_data(format!("File not found: {}", name)))?;

        let start = file_info.offset as usize;
        let end = start + file_info.size as usize;

        let bytes = self.data.as_bytes();
        if end > bytes.len() {
            return Err(BinaryError::invalid_data(format!(
                "File {} extends beyond data bounds: {} > {}",
                name,
                end,
                bytes.len()
            )));
        }

        Ok(&bytes[start..end])
    }

    pub fn extract_file_view(&self, name: &str) -> Result<DataView> {
        let file_info = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| BinaryError::invalid_data(format!("File not found: {}", name)))?;

        let start = file_info.offset as usize;
        let end = start + file_info.size as usize;
        let base = self.data.base_offset();
        DataView::from_range(self.data.backing_arc(), (base + start)..(base + end))
    }

    /// Try to parse contained files as AssetBundles
    pub fn parse_bundles(&self) -> Result<Vec<AssetBundle>> {
        let mut bundles = Vec::new();

        for file_info in &self.files {
            if let Ok(view) = self.extract_file_view(&file_info.name) {
                let bytes = view.as_bytes();
                if let Ok(bundle) = crate::bundle::BundleParser::from_slice(bytes) {
                    bundles.push(bundle);
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
        let data = DataView::from_arc(Arc::<[u8]>::from(Vec::<u8>::new()));
        let webfile = WebFile {
            signature: "UnityWebData1.0".to_string(),
            compression: WebFileCompression::None,
            files: Vec::new(),
            data,
        };

        assert_eq!(webfile.signature, "UnityWebData1.0");
        assert_eq!(webfile.compression, WebFileCompression::None);
        assert!(webfile.files().is_empty());
    }
}
