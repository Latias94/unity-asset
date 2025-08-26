//! Unity Asset file parsing (SerializedFile format)

use crate::error::{BinaryError, Result};
use crate::object::{ObjectInfo, UnityObject};
use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::TypeTree;

#[cfg(feature = "async")]
use futures::stream::StreamExt;

/// Header of a Unity SerializedFile
#[derive(Debug, Clone)]
pub struct SerializedFileHeader {
    /// Size of the metadata section
    pub metadata_size: u32,
    /// Total file size
    pub file_size: u32,
    /// File format version
    pub version: u32,
    /// Offset to the data section
    pub data_offset: u32,
    /// Endianness (0 = little, 1 = big)
    pub endian: u8,
    /// Reserved bytes
    pub reserved: [u8; 3],
}

impl SerializedFileHeader {
    /// Parse header from binary data (improved based on unity-rs)
    pub fn from_reader(reader: &mut BinaryReader) -> Result<Self> {
        let mut metadata_size = reader.read_u32()?;
        let mut file_size = reader.read_u32()?;
        let version = reader.read_u32()?;
        let mut data_offset = reader.read_u32()?;

        let endian;
        let mut reserved = [0u8; 3];

        // Handle different Unity versions (based on unity-rs logic)
        if version >= 9 {
            endian = reader.read_u8()?;
            let reserved_bytes = reader.read_bytes(3)?;
            reserved.copy_from_slice(&reserved_bytes);
        } else {
            // For older versions, endian is at the end of metadata
            let current_pos = reader.position();
            reader.set_position((file_size - metadata_size) as u64)?;
            endian = reader.read_u8()?;
            reader.set_position(current_pos)?;
        }

        // Handle version 22+ format changes
        if version >= 22 {
            metadata_size = reader.read_u32()?;
            file_size = reader.read_i64()? as u32;
            data_offset = reader.read_i64()? as u32;
            reader.read_i64()?; // Skip unknown field
        }

        Ok(Self {
            metadata_size,
            file_size,
            version,
            data_offset,
            endian,
            reserved,
        })
    }

    /// Get the byte order from the endian flag
    pub fn byte_order(&self) -> ByteOrder {
        if self.endian == 0 {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
    }

    /// Check if this is a valid Unity file header
    pub fn is_valid(&self) -> bool {
        // Basic sanity checks
        self.version > 0
            && self.version < 100
            && self.data_offset > 0
            && self.file_size > self.data_offset
    }
}

/// Type information for Unity objects
#[derive(Debug, Clone)]
pub struct SerializedType {
    /// Unity class ID
    pub class_id: i32,
    /// Whether this type is stripped
    pub is_stripped_type: bool,
    /// Script type index (for MonoBehaviour)
    pub script_type_index: Option<i16>,
    /// Type tree for this type
    pub type_tree: TypeTree,
    /// Script ID hash
    pub script_id: [u8; 16],
    /// Old type hash
    pub old_type_hash: [u8; 16],
    /// Type dependencies
    pub type_dependencies: Vec<i32>,
    /// Class name
    pub class_name: String,
    /// Namespace
    pub namespace: String,
    /// Assembly name
    pub assembly_name: String,
}

impl SerializedType {
    /// Create a new SerializedType
    pub fn new(class_id: i32) -> Self {
        Self {
            class_id,
            is_stripped_type: false,
            script_type_index: None,
            type_tree: TypeTree::new(),
            script_id: [0; 16],
            old_type_hash: [0; 16],
            type_dependencies: Vec::new(),
            class_name: String::new(),
            namespace: String::new(),
            assembly_name: String::new(),
        }
    }

    /// Parse SerializedType from binary data
    pub fn from_reader(
        reader: &mut BinaryReader,
        version: u32,
        enable_type_tree: bool,
    ) -> Result<Self> {
        let class_id = reader.read_i32()?;
        let mut serialized_type = Self::new(class_id);

        if version >= 16 {
            serialized_type.is_stripped_type = reader.read_bool()?;
        }

        if version >= 17 {
            let script_type_index = reader.read_i16()?;
            serialized_type.script_type_index = Some(script_type_index);
        }

        if version >= 13 {
            // Based on unity-rs logic: check conditions for script_id
            let should_read_script_id = if version < 16 {
                class_id < 0
            } else {
                class_id == 114 // MonoBehaviour
            };

            if should_read_script_id {
                // Read script ID
                let script_id_bytes = reader.read_bytes(16)?;
                serialized_type.script_id.copy_from_slice(&script_id_bytes);
            }

            // Always read old type hash for version >= 13
            let old_type_hash_bytes = reader.read_bytes(16)?;
            serialized_type
                .old_type_hash
                .copy_from_slice(&old_type_hash_bytes);
        }

        if enable_type_tree {
            // Use blob format for version >= 12 or version == 10 (like unity-rs)
            if version >= 12 || version == 10 {
                serialized_type.type_tree = TypeTree::from_reader_blob(reader, version)?;
            } else {
                serialized_type.type_tree = TypeTree::from_reader(reader, version)?;
            }
        }

        Ok(serialized_type)
    }
}

/// External reference to another Unity file
#[derive(Debug, Clone)]
pub struct FileIdentifier {
    /// GUID of the referenced file
    pub guid: [u8; 16],
    /// Type of the reference
    pub type_: i32,
    /// Path to the referenced file
    pub path_name: String,
}

impl FileIdentifier {
    /// Parse FileIdentifier from binary data
    pub fn from_reader(reader: &mut BinaryReader, _version: u32) -> Result<Self> {
        let mut guid = [0u8; 16];
        for i in 0..16 {
            guid[i] = reader.read_u8()?;
        }

        let type_ = reader.read_i32()?;
        let path_name = reader.read_cstring()?;

        Ok(Self {
            guid,
            type_,
            path_name,
        })
    }
}

/// A Unity SerializedFile (Asset file)
#[derive(Debug)]
pub struct SerializedFile {
    /// File header
    pub header: SerializedFileHeader,
    /// Unity version string
    pub unity_version: String,
    /// Target platform
    pub target_platform: i32,
    /// Whether type tree is enabled
    pub enable_type_tree: bool,
    /// Type information
    pub types: Vec<SerializedType>,
    /// Whether big IDs are enabled
    pub big_id_enabled: bool,
    /// Object information
    pub objects: Vec<ObjectInfo>,
    /// Script types
    pub script_types: Vec<SerializedType>,
    /// External file references
    pub externals: Vec<FileIdentifier>,
    /// Reference types
    pub ref_types: Vec<SerializedType>,
    /// User information
    pub user_information: String,
    /// Raw file data
    data: Vec<u8>,
}

impl SerializedFile {
    /// Parse a SerializedFile from binary data
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let data_clone = data.clone();
        let mut reader = BinaryReader::new(&data_clone, ByteOrder::Big);

        // Read header
        let header = SerializedFileHeader::from_reader(&mut reader)?;

        if !header.is_valid() {
            return Err(BinaryError::invalid_format("Invalid SerializedFile header"));
        }

        // Switch to the correct byte order
        reader.set_byte_order(header.byte_order());

        let mut file = Self {
            header,
            unity_version: String::new(),
            target_platform: 0,
            enable_type_tree: false,
            types: Vec::new(),
            big_id_enabled: false,
            objects: Vec::new(),
            script_types: Vec::new(),
            externals: Vec::new(),
            ref_types: Vec::new(),
            user_information: String::new(),
            data: data.clone(), // Clone data for storage
        };

        // Parse metadata
        file.parse_metadata(&mut reader)?;

        Ok(file)
    }

    /// Parse a SerializedFile from binary data asynchronously
    #[cfg(feature = "async")]
    pub async fn from_bytes_async(data: Vec<u8>) -> Result<Self> {
        // For now, use spawn_blocking to run the sync version
        let result = tokio::task::spawn_blocking(move || Self::from_bytes(data))
            .await
            .map_err(|e| BinaryError::format(format!("Task join error: {}", e)))??;

        Ok(result)
    }

    /// Parse the metadata section
    fn parse_metadata(&mut self, reader: &mut BinaryReader) -> Result<()> {
        // Read Unity version (if version >= 7)
        if self.header.version >= 7 {
            self.unity_version = reader.read_cstring()?;
        }

        // Read target platform (if version >= 8)
        if self.header.version >= 8 {
            self.target_platform = reader.read_i32()?;
        }

        // Read enable type tree flag (if version >= 13)
        if self.header.version >= 13 {
            self.enable_type_tree = reader.read_bool()?;
        }

        // Read types
        let type_count = reader.read_u32()? as usize;
        for _ in 0..type_count {
            let serialized_type =
                SerializedType::from_reader(reader, self.header.version, self.enable_type_tree)?;
            self.types.push(serialized_type);
        }

        // Read big ID enabled flag (if version 7-13)
        if self.header.version >= 7 && self.header.version < 14 {
            self.big_id_enabled = reader.read_bool()?;
        }

        // Read objects
        let object_count = reader.read_u32()? as usize;
        for _ in 0..object_count {
            let object_info = self.parse_object_info(reader)?;
            self.objects.push(object_info);
        }

        // Read script types (if version >= 11)
        if self.header.version >= 11 {
            let script_count = reader.read_u32()? as usize;
            for _ in 0..script_count {
                let script_type = SerializedType::from_reader(
                    reader,
                    self.header.version,
                    self.enable_type_tree,
                )?;
                self.script_types.push(script_type);
            }
        }

        // Read externals
        let external_count = reader.read_u32()? as usize;
        for _ in 0..external_count {
            let external = FileIdentifier::from_reader(reader, self.header.version)?;
            self.externals.push(external);
        }

        // Read ref types (if version >= 20)
        if self.header.version >= 20 {
            let ref_type_count = reader.read_u32()? as usize;
            for _ in 0..ref_type_count {
                let ref_type = SerializedType::from_reader(
                    reader,
                    self.header.version,
                    self.enable_type_tree,
                )?;
                self.ref_types.push(ref_type);
            }
        }

        // Read user information (if version >= 5)
        if self.header.version >= 5 {
            self.user_information = reader.read_cstring()?;
        }

        Ok(())
    }

    /// Parse object information
    fn parse_object_info(&self, reader: &mut BinaryReader) -> Result<ObjectInfo> {
        reader.align()?;

        let path_id = if self.big_id_enabled {
            reader.read_i64()?
        } else if self.header.version < 14 {
            reader.read_i32()? as i64
        } else {
            reader.align()?;
            reader.read_i64()?
        };

        let byte_start = if self.header.version >= 22 {
            reader.read_u64()?
        } else {
            reader.read_u32()? as u64
        };

        let byte_size = reader.read_u32()?;
        let type_id = reader.read_i32()?;

        let class_id = if self.header.version < 16 {
            // For version < 16, class_id is read separately
            reader.read_u16()? as i32
        } else {
            // For version >= 16, type_id is an index into the types array
            if type_id >= 0 && (type_id as usize) < self.types.len() {
                self.types[type_id as usize].class_id
            } else {
                0 // Default fallback
            }
        };

        let mut object_info = ObjectInfo::new(path_id, byte_start, byte_size, class_id);
        object_info.type_id = type_id;
        object_info.byte_order = self.header.byte_order();

        Ok(object_info)
    }

    /// Get all Unity objects in this file
    pub fn get_objects(&self) -> Result<Vec<UnityObject>> {
        let mut objects = Vec::new();

        for object_info in &self.objects {
            // Extract object data from the file
            let start = (self.header.data_offset as u64 + object_info.byte_start) as usize;
            let end = start + object_info.byte_size as usize;

            if end > self.data.len() {
                return Err(BinaryError::invalid_data(format!(
                    "Object data out of bounds: {} > {}",
                    end,
                    self.data.len()
                )));
            }

            let mut info = object_info.clone();
            info.data = self.data[start..end].to_vec();

            // Find type information for this object
            // First try to match by type_id, then by class_id
            if let Some(serialized_type) = self
                .types
                .iter()
                .find(|t| t.class_id == object_info.type_id)
            {
                info.type_tree = Some(serialized_type.type_tree.clone());
            } else if let Some(serialized_type) = self
                .types
                .iter()
                .find(|t| t.class_id == object_info.class_id)
            {
                info.type_tree = Some(serialized_type.type_tree.clone());
            }

            let unity_object = UnityObject::new(info)?;
            objects.push(unity_object);
        }

        Ok(objects)
    }

    /// Get all Unity objects in this file asynchronously with concurrent processing
    #[cfg(feature = "async")]
    pub async fn get_objects_async(&self, max_concurrent: usize) -> Result<Vec<UnityObject>> {
        use futures::stream::{self, StreamExt};

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
        let data = &self.data;
        let header = &self.header;
        let types = &self.types;

        let results: Result<Vec<UnityObject>> = stream::iter(self.objects.iter())
            .map(|object_info| {
                let semaphore = semaphore.clone();
                async move {
                    let _permit = semaphore
                        .acquire()
                        .await
                        .map_err(|e| BinaryError::format(format!("Semaphore error: {}", e)))?;

                    // Extract object data from the file
                    let start = (header.data_offset as u64 + object_info.byte_start) as usize;
                    let end = start + object_info.byte_size as usize;

                    if end > data.len() {
                        return Err(BinaryError::invalid_data(format!(
                            "Object data out of bounds: {} > {}",
                            end,
                            data.len()
                        )));
                    }

                    let mut info = object_info.clone();
                    info.data = data[start..end].to_vec();

                    // Find type information for this object
                    if let Some(serialized_type) =
                        types.iter().find(|t| t.class_id == object_info.type_id)
                    {
                        info.type_tree = Some(serialized_type.type_tree.clone());
                    } else if let Some(serialized_type) =
                        types.iter().find(|t| t.class_id == object_info.class_id)
                    {
                        info.type_tree = Some(serialized_type.type_tree.clone());
                    }

                    UnityObject::new(info)
                }
            })
            .buffer_unordered(max_concurrent)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect();

        results
    }

    /// Get the file name/path
    pub fn name(&self) -> &str {
        "SerializedFile"
    }

    /// Get Unity version
    pub fn unity_version(&self) -> &str {
        &self.unity_version
    }

    /// Get target platform
    pub fn target_platform(&self) -> i32 {
        self.target_platform
    }
}

/// Type alias for compatibility
pub type Asset = SerializedFile;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_byte_order() {
        let header = SerializedFileHeader {
            metadata_size: 0,
            file_size: 0,
            version: 1,
            data_offset: 0,
            endian: 0,
            reserved: [0; 3],
        };
        assert_eq!(header.byte_order(), ByteOrder::Little);

        let header = SerializedFileHeader {
            metadata_size: 0,
            file_size: 0,
            version: 1,
            data_offset: 0,
            endian: 1,
            reserved: [0; 3],
        };
        assert_eq!(header.byte_order(), ByteOrder::Big);
    }

    #[test]
    fn test_header_validation() {
        let valid_header = SerializedFileHeader {
            metadata_size: 100,
            file_size: 1000,
            version: 15,
            data_offset: 200,
            endian: 0,
            reserved: [0; 3],
        };
        assert!(valid_header.is_valid());

        let invalid_header = SerializedFileHeader {
            metadata_size: 100,
            file_size: 100, // file_size <= data_offset
            version: 15,
            data_offset: 200,
            endian: 0,
            reserved: [0; 3],
        };
        assert!(!invalid_header.is_valid());
    }
}
