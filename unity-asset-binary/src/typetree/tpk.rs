//! TPK (Type Package) support for external TypeTree registries.
//!
//! UnityPy ships a `uncompressed.tpk` registry which maps `(class_id, unity_version)` to a
//! release TypeTree root node. This module implements a compatible reader so we can provide a
//! UnityPy-like fallback when SerializedFile TypeTrees are stripped.

use crate::compression::{self, CompressionType};
use crate::error::{BinaryError, Result};
use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry};
use crate::unity_version::{UnityVersion, UnityVersionType};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
enum TpkCompressionType {
    None = 0,
    Lz4 = 1,
    Lzma = 2,
    Brotli = 3,
}

impl TryFrom<i8> for TpkCompressionType {
    type Error = BinaryError;

    fn try_from(value: i8) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Lz4),
            2 => Ok(Self::Lzma),
            3 => Ok(Self::Brotli),
            other => Err(BinaryError::invalid_data(format!(
                "Invalid TPK compression type: {}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
enum TpkDataType {
    TypeTreeInformation = 0,
    Collection = 1,
    FileSystem = 2,
    Json = 3,
    ReferenceAssemblies = 4,
    EngineAssets = 5,
}

impl TryFrom<i8> for TpkDataType {
    type Error = BinaryError;

    fn try_from(value: i8) -> Result<Self> {
        match value {
            0 => Ok(Self::TypeTreeInformation),
            1 => Ok(Self::Collection),
            2 => Ok(Self::FileSystem),
            3 => Ok(Self::Json),
            4 => Ok(Self::ReferenceAssemblies),
            5 => Ok(Self::EngineAssets),
            other => Err(BinaryError::invalid_data(format!(
                "Invalid TPK data type: {}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TpkUnityClassFlags {
    HasEditorRootNode = 64,
    HasReleaseRootNode = 128,
}

#[derive(Debug, Clone)]
struct TpkFileHeader {
    compression: TpkCompressionType,
    data_type: TpkDataType,
    compressed_size: u32,
    uncompressed_size: u32,
}

#[derive(Debug, Clone)]
struct TpkUnityClass {
    #[allow(dead_code)]
    name: u16,
    #[allow(dead_code)]
    base: u16,
    #[allow(dead_code)]
    flags: u8,
    #[allow(dead_code)]
    editor_root_node: Option<u16>,
    release_root_node: Option<u16>,
}

#[derive(Debug, Clone)]
struct TpkClassInformation {
    #[allow(dead_code)]
    id: i32,
    classes: Vec<(u64, Option<TpkUnityClass>)>,
}

#[derive(Debug, Clone)]
struct TpkUnityNode {
    type_name: u16,
    name: u16,
    byte_size: i32,
    version: i16,
    type_flags: i8,
    meta_flag: u32,
    sub_nodes: Vec<u16>,
}

#[derive(Debug, Clone)]
struct TpkTypeTreeBlob {
    #[allow(dead_code)]
    creation_time: i64,
    #[allow(dead_code)]
    versions: Vec<u64>,
    class_information: HashMap<i32, TpkClassInformation>,
    nodes: Vec<TpkUnityNode>,
    strings: Vec<String>,
}

#[derive(Debug)]
struct TpkReader<'a> {
    cur: Cursor<&'a [u8]>,
}

impl<'a> TpkReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            cur: Cursor::new(data),
        }
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut buf = [0u8; N];
        self.cur
            .read_exact(&mut buf)
            .map_err(|e| BinaryError::generic(format!("TPK read failed: {}", e)))?;
        Ok(buf)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact::<1>()?[0])
    }

    fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_exact::<2>()?))
    }

    fn read_i16_le(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.read_exact::<2>()?))
    }

    fn read_u32_le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_exact::<4>()?))
    }

    fn read_i32_le(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.read_exact::<4>()?))
    }

    fn read_i64_le(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.read_exact::<8>()?))
    }

    fn read_u64_le(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.read_exact::<8>()?))
    }

    fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.cur
            .read_exact(&mut buf)
            .map_err(|e| BinaryError::generic(format!("TPK read failed: {}", e)))?;
        Ok(buf)
    }

    fn read_varint_len(&mut self) -> Result<usize> {
        let mut shift = 0u32;
        let mut len: u64 = 0;
        loop {
            let b = self.read_u8()?;
            len |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                break;
            }
            shift = shift.saturating_add(7);
            if shift > 63 {
                return Err(BinaryError::invalid_data(
                    "TPK varint too large".to_string(),
                ));
            }
        }
        Ok(len as usize)
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_varint_len()?;
        let bytes = self.read_bytes(len)?;
        String::from_utf8(bytes)
            .map_err(|e| BinaryError::invalid_data(format!("TPK invalid utf8: {}", e)))
    }
}

fn unity_version_to_u64(v: &UnityVersion) -> u64 {
    let type_byte: u8 = match v.version_type {
        UnityVersionType::A => 0,
        UnityVersionType::B => 1,
        UnityVersionType::C => 2,
        UnityVersionType::F => 3,
        UnityVersionType::P => 4,
        UnityVersionType::X => 5,
        UnityVersionType::U => 255,
    };
    ((v.major as u64) << 48)
        | ((v.minor as u64) << 32)
        | ((v.build as u64) << 16)
        | ((type_byte as u64) << 8)
        | (v.type_number as u64)
}

fn select_versioned_class(
    version: u64,
    classes: &[(u64, Option<TpkUnityClass>)],
) -> Option<&TpkUnityClass> {
    let mut ret: Option<&TpkUnityClass> = None;
    for (v, item) in classes {
        if version >= *v {
            if let Some(c) = item.as_ref() {
                ret = Some(c);
            }
        } else {
            break;
        }
    }
    ret
}

fn build_tree_from_blob(blob: &TpkTypeTreeBlob, class: &TpkUnityClass) -> Result<TypeTree> {
    let root_id = class
        .release_root_node
        .ok_or_else(|| BinaryError::invalid_data("TPK class has no ReleaseRootNode".to_string()))?
        as usize;

    fn build_node(
        blob: &TpkTypeTreeBlob,
        node_id: usize,
        level: i32,
        next_index: &mut i32,
    ) -> Result<TypeTreeNode> {
        let node = blob.nodes.get(node_id).ok_or_else(|| {
            BinaryError::invalid_data(format!("TPK node out of range: {}", node_id))
        })?;
        let type_name = blob
            .strings
            .get(node.type_name as usize)
            .ok_or_else(|| {
                BinaryError::invalid_data("TPK type string index out of range".to_string())
            })?
            .clone();
        let name = blob
            .strings
            .get(node.name as usize)
            .ok_or_else(|| {
                BinaryError::invalid_data("TPK name string index out of range".to_string())
            })?
            .clone();

        let mut out = TypeTreeNode::new();
        out.type_name = type_name;
        out.name = name;
        out.byte_size = node.byte_size;
        out.index = *next_index;
        out.version = node.version as i32;
        out.type_flags = node.type_flags as i32;
        out.meta_flags = node.meta_flag as i32;
        out.level = level;

        *next_index = next_index.saturating_add(1);
        out.children = node
            .sub_nodes
            .iter()
            .map(|id| build_node(blob, *id as usize, level + 1, next_index))
            .collect::<Result<Vec<_>>>()?;
        Ok(out)
    }

    let mut next_index: i32 = 0;
    let root = build_node(blob, root_id, 0, &mut next_index)?;
    let mut tree = TypeTree::new();
    tree.add_node(root);
    Ok(tree)
}

fn parse_tpk_header(reader: &mut TpkReader<'_>) -> Result<TpkFileHeader> {
    let magic = reader.read_u32_le()?;
    const TPK_MAGIC: u32 = 0x2A4B5054;
    if magic != TPK_MAGIC {
        return Err(BinaryError::invalid_data(
            "Invalid TPK magic bytes".to_string(),
        ));
    }

    let version_number = reader.read_i8()?;
    if version_number != 1 {
        return Err(BinaryError::invalid_data(format!(
            "Invalid TPK version number: {}",
            version_number
        )));
    }

    let compression = TpkCompressionType::try_from(reader.read_i8()?)?;
    let data_type = TpkDataType::try_from(reader.read_i8()?)?;
    let _unused_b = reader.read_i8()?;
    let _unused_u32 = reader.read_u32_le()?;
    let compressed_size = reader.read_u32_le()?;
    let uncompressed_size = reader.read_u32_le()?;

    Ok(TpkFileHeader {
        compression,
        data_type,
        compressed_size,
        uncompressed_size,
    })
}

fn decompress_tpk_payload(header: &TpkFileHeader, compressed: &[u8]) -> Result<Vec<u8>> {
    let (ctype, expected) = match header.compression {
        TpkCompressionType::None => (CompressionType::None, compressed.len()),
        TpkCompressionType::Lz4 => (CompressionType::Lz4, header.uncompressed_size as usize),
        TpkCompressionType::Lzma => (CompressionType::Lzma, header.uncompressed_size as usize),
        TpkCompressionType::Brotli => (CompressionType::Brotli, header.uncompressed_size as usize),
    };
    if ctype == CompressionType::None {
        return Ok(compressed.to_vec());
    }
    compression::decompress(compressed, ctype, expected)
}

fn parse_tpk_typetree_blob(data: &[u8]) -> Result<TpkTypeTreeBlob> {
    let mut r = TpkReader::new(data);
    let creation_time = r.read_i64_le()?;
    let version_count = r.read_i32_le()?;
    if version_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK version count".to_string(),
        ));
    }
    let mut versions: Vec<u64> = Vec::with_capacity(version_count as usize);
    for _ in 0..version_count {
        versions.push(r.read_u64_le()?);
    }

    let class_count = r.read_i32_le()?;
    if class_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK class count".to_string(),
        ));
    }
    let mut class_information: HashMap<i32, TpkClassInformation> = HashMap::new();
    for _ in 0..class_count {
        let id = r.read_i32_le()?;
        let count = r.read_i32_le()?;
        if count < 0 {
            return Err(BinaryError::invalid_data(
                "Negative TPK class version count".to_string(),
            ));
        }
        let mut classes: Vec<(u64, Option<TpkUnityClass>)> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let version = r.read_u64_le()?;
            let present = r.read_u8()?;
            let class = if present != 0 {
                let name = r.read_u16_le()?;
                let base = r.read_u16_le()?;
                let flags = r.read_u8()?;
                let mut editor_root_node: Option<u16> = None;
                let mut release_root_node: Option<u16> = None;
                if (flags & TpkUnityClassFlags::HasEditorRootNode as u8) != 0 {
                    editor_root_node = Some(r.read_u16_le()?);
                }
                if (flags & TpkUnityClassFlags::HasReleaseRootNode as u8) != 0 {
                    release_root_node = Some(r.read_u16_le()?);
                }
                Some(TpkUnityClass {
                    name,
                    base,
                    flags,
                    editor_root_node,
                    release_root_node,
                })
            } else {
                None
            };
            classes.push((version, class));
        }
        class_information.insert(id, TpkClassInformation { id, classes });
    }

    // CommonString (we don't need the data for tree construction, but we must consume it)
    let common_version_count = r.read_i32_le()?;
    if common_version_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK common string version count".to_string(),
        ));
    }
    for _ in 0..common_version_count {
        let _ver = r.read_u64_le()?;
        let _count = r.read_u8()?;
    }
    let indices_count = r.read_i32_le()?;
    if indices_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK common string indices count".to_string(),
        ));
    }
    for _ in 0..indices_count {
        let _idx = r.read_u16_le()?;
    }

    // NodeBuffer
    let node_count = r.read_i32_le()?;
    if node_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK node count".to_string(),
        ));
    }
    let mut nodes: Vec<TpkUnityNode> = Vec::with_capacity(node_count as usize);
    for _ in 0..node_count {
        let type_name = r.read_u16_le()?;
        let name = r.read_u16_le()?;
        let byte_size = r.read_i32_le()?;
        let version = r.read_i16_le()?;
        let type_flags = r.read_i8()?;
        let meta_flag = r.read_u32_le()?;
        let count = r.read_u16_le()? as usize;
        let mut sub_nodes: Vec<u16> = Vec::with_capacity(count);
        for _ in 0..count {
            sub_nodes.push(r.read_u16_le()?);
        }
        nodes.push(TpkUnityNode {
            type_name,
            name,
            byte_size,
            version,
            type_flags,
            meta_flag,
            sub_nodes,
        });
    }

    // StringBuffer
    let string_count = r.read_i32_le()?;
    if string_count < 0 {
        return Err(BinaryError::invalid_data(
            "Negative TPK string count".to_string(),
        ));
    }
    let mut strings: Vec<String> = Vec::with_capacity(string_count as usize);
    for _ in 0..string_count {
        strings.push(r.read_string()?);
    }

    Ok(TpkTypeTreeBlob {
        creation_time,
        versions,
        class_information,
        nodes,
        strings,
    })
}

/// A UnityPy-compatible TPK TypeTree registry.
#[derive(Debug, Clone)]
pub struct TpkTypeTreeRegistry {
    blob: Arc<TpkTypeTreeBlob>,
    cache: Arc<RwLock<HashMap<(i32, u64), Arc<TypeTree>>>>,
}

impl TpkTypeTreeRegistry {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let mut r = TpkReader::new(data);
        let header = parse_tpk_header(&mut r)?;
        if header.data_type != TpkDataType::TypeTreeInformation {
            return Err(BinaryError::unsupported(format!(
                "Unsupported TPK data type: {:?}",
                header.data_type
            )));
        }
        let compressed = r.read_bytes(header.compressed_size as usize)?;
        if compressed.len() != header.compressed_size as usize {
            return Err(BinaryError::invalid_data(
                "Invalid TPK compressed size".to_string(),
            ));
        }
        let decompressed = decompress_tpk_payload(&header, &compressed)?;
        let blob = parse_tpk_typetree_blob(&decompressed)?;
        Ok(Self {
            blob: Arc::new(blob),
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path.as_ref()).map_err(|e| {
            BinaryError::generic(format!(
                "Failed to read TPK file {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;
        Self::from_bytes(&data)
    }
}

impl TypeTreeRegistry for TpkTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        let Ok(v) = UnityVersion::parse_version(unity_version) else {
            return None;
        };
        let encoded = unity_version_to_u64(&v);

        if let Ok(cache) = self.cache.read() {
            if let Some(found) = cache.get(&(class_id, encoded)) {
                return Some(found.clone());
            }
        }

        let ci = self.blob.class_information.get(&class_id)?;
        let class = select_versioned_class(encoded, &ci.classes)?;
        let built = build_tree_from_blob(&self.blob, class).ok()?;
        let built = Arc::new(built);

        match self.cache.write() {
            Ok(mut cache) => {
                cache.insert((class_id, encoded), built.clone());
            }
            Err(e) => {
                let mut cache = e.into_inner();
                cache.insert((class_id, encoded), built.clone());
            }
        }

        Some(built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{BinaryReader, ByteOrder};
    use crate::typetree::{TypeTreeParseOptions, TypeTreeSerializer};
    use unity_asset_core::UnityValue;

    fn write_varint(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let mut b = (n & 0x7F) as u8;
            n >>= 7;
            if n != 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                break;
            }
        }
    }

    fn write_tpk_string(s: &str, out: &mut Vec<u8>) {
        write_varint(s.len(), out);
        out.extend_from_slice(s.as_bytes());
    }

    fn build_minimal_tpk() -> Vec<u8> {
        // Build a minimal, uncompressed TPK TypeTreeInformation blob with one class (28) and a root->m_Name string node.
        let mut blob: Vec<u8> = Vec::new();
        blob.extend_from_slice(&0i64.to_le_bytes()); // creation_time
        blob.extend_from_slice(&1i32.to_le_bytes()); // versionCount

        let v = UnityVersion::parse_version("2020.3.0f1").unwrap();
        let v_u64 = unity_version_to_u64(&v);
        blob.extend_from_slice(&v_u64.to_le_bytes()); // versions[0]

        blob.extend_from_slice(&1i32.to_le_bytes()); // classCount
        blob.extend_from_slice(&(28i32).to_le_bytes()); // class id
        blob.extend_from_slice(&1i32.to_le_bytes()); // classes count
        blob.extend_from_slice(&v_u64.to_le_bytes()); // class version
        blob.push(1u8); // present

        // TpkUnityClass: name/base/flags + release root
        blob.extend_from_slice(&(0u16).to_le_bytes()); // name
        blob.extend_from_slice(&(0u16).to_le_bytes()); // base
        blob.push(TpkUnityClassFlags::HasReleaseRootNode as u8); // flags
        blob.extend_from_slice(&(0u16).to_le_bytes()); // ReleaseRootNode = node 0

        // CommonString: versionCount=0, indicesCount=0
        blob.extend_from_slice(&0i32.to_le_bytes());
        blob.extend_from_slice(&0i32.to_le_bytes());

        // NodeBuffer: count=2
        blob.extend_from_slice(&2i32.to_le_bytes());
        // Node0: RootType/Base, subnodes=[1]
        blob.extend_from_slice(&(0u16).to_le_bytes()); // TypeName idx
        blob.extend_from_slice(&(1u16).to_le_bytes()); // Name idx
        blob.extend_from_slice(&(-1i32).to_le_bytes()); // ByteSize
        blob.extend_from_slice(&(1i16).to_le_bytes()); // Version
        blob.push(0i8 as u8); // TypeFlags
        blob.extend_from_slice(&(0u32).to_le_bytes()); // MetaFlag
        blob.extend_from_slice(&(1u16).to_le_bytes()); // SubNode count
        blob.extend_from_slice(&(1u16).to_le_bytes()); // SubNode id 1
        // Node1: string/m_Name, subnodes=[]
        blob.extend_from_slice(&(2u16).to_le_bytes()); // TypeName idx
        blob.extend_from_slice(&(3u16).to_le_bytes()); // Name idx
        blob.extend_from_slice(&(-1i32).to_le_bytes()); // ByteSize
        blob.extend_from_slice(&(1i16).to_le_bytes()); // Version
        blob.push(0i8 as u8); // TypeFlags
        blob.extend_from_slice(&(0u32).to_le_bytes()); // MetaFlag
        blob.extend_from_slice(&(0u16).to_le_bytes()); // SubNode count

        // StringBuffer
        blob.extend_from_slice(&4i32.to_le_bytes());
        write_tpk_string("RootType", &mut blob); // 0
        write_tpk_string("Base", &mut blob); // 1
        write_tpk_string("string", &mut blob); // 2
        write_tpk_string("m_Name", &mut blob); // 3

        let mut out: Vec<u8> = Vec::new();
        // TpkFile header: <IbbbbIII
        out.extend_from_slice(&0x2A4B5054u32.to_le_bytes()); // magic
        out.push(1u8); // versionNumber (i8)
        out.push(TpkCompressionType::None as i8 as u8); // compressionType
        out.push(TpkDataType::TypeTreeInformation as i8 as u8); // dataType
        out.push(0u8); // unused b
        out.extend_from_slice(&0u32.to_le_bytes()); // unused u32
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes()); // compressedSize
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes()); // uncompressedSize
        out.extend_from_slice(&blob);
        out
    }

    #[test]
    fn tpk_registry_resolves_typetree_and_parses_name() {
        let tpk = build_minimal_tpk();
        let registry = TpkTypeTreeRegistry::from_bytes(&tpk).unwrap();
        let tree = registry.resolve("2020.3.0f1", 28).unwrap();

        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&(3i32).to_le_bytes());
        bytes.extend_from_slice(b"foo");
        bytes.push(0); // align to 4

        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(tree.as_ref());
        let out = serializer
            .parse_object_prefix_detailed(&mut reader, TypeTreeParseOptions::default(), 1)
            .unwrap();
        assert_eq!(
            out.properties.get("m_Name").and_then(|v| v.as_str()),
            Some("foo")
        );
        assert_eq!(reader.remaining(), 0);
        assert_eq!(out.warnings.len(), 0);
        assert_eq!(out.properties.len(), 1);
        assert!(matches!(
            out.properties.get("m_Name"),
            Some(UnityValue::String(_))
        ));
    }
}
