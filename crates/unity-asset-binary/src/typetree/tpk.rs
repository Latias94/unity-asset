//! TPK (Type Package) support for external TypeTree registries.
//!
//! UnityPy ships a `uncompressed.tpk` registry which maps `(class_id, unity_version)` to editor
//! and release TypeTree root nodes. This module implements a compatible reader so we can provide
//! a UnityPy-like fallback when SerializedFile TypeTrees are stripped.

use crate::compression::{self, CompressionType};
use crate::error::{BinaryError, Result};
use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeRegistry, TypeTreeSerializationMode};
use std::io::Read;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;
use unity_asset_core::AssetLoadBudget;

type ResolvedClassMap = Vec<(i32, Vec<(u64, VersionedTrees)>)>;

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
    class_information: Vec<TpkClassInformation>,
    nodes: Vec<TpkUnityNode>,
    strings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum CountKind {
    Entries,
    Members,
}

#[derive(Debug)]
struct TpkReader<'data, 'budget> {
    data: &'data [u8],
    position: usize,
    budget: &'budget mut AssetLoadBudget,
}

impl<'data, 'budget> TpkReader<'data, 'budget> {
    fn new(data: &'data [u8], budget: &'budget mut AssetLoadBudget) -> Self {
        Self {
            data,
            position: 0,
            budget,
        }
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.read_slice(N, true)?
            .try_into()
            .map_err(|_| BinaryError::invalid_data("TPK primitive width mismatch"))
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

    fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    fn into_budget(self) -> &'budget mut AssetLoadBudget {
        self.budget
    }

    fn read_slice(&mut self, count: usize, charge_wire_bytes: bool) -> Result<&'data [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| BinaryError::invalid_data("TPK read offset overflow"))?;
        let bytes = self.data.get(self.position..end).ok_or_else(|| {
            BinaryError::not_enough_data(count, self.data.len().saturating_sub(self.position))
        })?;
        if charge_wire_bytes {
            self.budget
                .consume_bytes(usize_to_u64(count, "TPK read length")?)?;
        }
        self.position = end;
        Ok(bytes)
    }

    fn read_count(
        &mut self,
        label: &str,
        minimum_wire_bytes: u64,
        kind: CountKind,
    ) -> Result<usize> {
        let raw_count = self.read_i32_le()?;
        let count = u64::try_from(raw_count)
            .map_err(|_| BinaryError::invalid_data(format!("Negative {label} count")))?;
        self.ensure_count_fits_remaining(count, minimum_wire_bytes, label)?;
        match kind {
            CountKind::Entries => self.budget.consume_entries(count)?,
            CountKind::Members => self.budget.consume_members(count)?,
        }
        usize::try_from(count)
            .map_err(|_| BinaryError::memory_error(format!("{label} count does not fit usize")))
    }

    fn read_u16_member_count(&mut self, label: &str, minimum_wire_bytes: u64) -> Result<usize> {
        let count = u64::from(self.read_u16_le()?);
        self.ensure_count_fits_remaining(count, minimum_wire_bytes, label)?;
        self.budget.consume_members(count)?;
        usize::try_from(count)
            .map_err(|_| BinaryError::memory_error(format!("{label} count does not fit usize")))
    }

    fn ensure_count_fits_remaining(
        &self,
        count: u64,
        minimum_wire_bytes: u64,
        label: &str,
    ) -> Result<()> {
        let required = count.checked_mul(minimum_wire_bytes).ok_or_else(|| {
            BinaryError::invalid_data(format!("{label} minimum wire size overflow"))
        })?;
        let remaining = usize_to_u64(self.remaining(), "TPK remaining length")?;
        if required > remaining {
            return Err(BinaryError::invalid_data(format!(
                "{label} count {count} requires at least {required} bytes, only {remaining} remain"
            )));
        }
        Ok(())
    }

    fn read_varint_len(&mut self) -> Result<u64> {
        let mut len = 0_u64;
        for index in 0..10_u32 {
            let b = self.read_u8()?;
            let payload = b & 0x7f;
            if index == 9 && payload > 1 {
                return Err(BinaryError::invalid_data("TPK varint too large"));
            }
            len |= u64::from(payload) << (index * 7);
            if (b & 0x80) == 0 {
                return Ok(len);
            }
        }
        Err(BinaryError::invalid_data("TPK varint too large"))
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_varint_len()?;
        let remaining = usize_to_u64(self.remaining(), "TPK remaining string bytes")?;
        if len > remaining {
            return Err(BinaryError::invalid_data(format!(
                "TPK string length {len} exceeds remaining payload {remaining}"
            )));
        }
        let total_charge = len
            .checked_mul(2)
            .ok_or_else(|| BinaryError::invalid_data("TPK string byte budget overflow"))?;
        self.budget.check_bytes(total_charge)?;
        let len = usize::try_from(len)
            .map_err(|_| BinaryError::memory_error("TPK string length does not fit usize"))?;
        let bytes = self.read_slice(len, true)?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(len).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {len} bytes for a TPK string: {error}"
            ))
        })?;
        self.budget
            .consume_bytes(usize_to_u64(len, "TPK owned string length")?)?;
        owned.extend_from_slice(bytes);
        String::from_utf8(owned)
            .map_err(|e| BinaryError::invalid_data(format!("TPK invalid utf8: {}", e)))
    }
}

fn encode_unity_version(major: u16, minor: u16, build: u16, type_byte: u8, type_number: u8) -> u64 {
    ((major as u64) << 48)
        | ((minor as u64) << 32)
        | ((build as u64) << 16)
        | ((type_byte as u64) << 8)
        | (type_number as u64)
}

fn parse_unity_version_key(version: &str) -> Option<u64> {
    let raw = version.trim();
    if raw.is_empty() {
        return None;
    }
    let raw = raw.split_whitespace().next().unwrap_or(raw);
    let mut parts = raw.splitn(3, '.');
    let major = parts.next()?.parse::<u16>().ok()?;
    let minor = parts.next()?.parse::<u16>().ok()?;
    let tail = parts.next()?;
    let build_end = tail
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map(|(index, _)| index)
        .unwrap_or(tail.len());
    let (build_digits, suffix) = tail.split_at(build_end);
    if build_digits.is_empty() {
        return None;
    }
    let build = build_digits.parse::<u16>().ok()?;
    if suffix.is_empty() {
        return Some(encode_unity_version(major, minor, build, 3, 0));
    }

    let number_start = suffix
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_ascii_digit())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    let (type_string, number) = suffix.split_at(number_start);
    let type_number = if number.is_empty() {
        0
    } else {
        number.parse::<u8>().ok()?
    };
    let type_byte = if type_string.eq_ignore_ascii_case("a") {
        0
    } else if type_string.eq_ignore_ascii_case("b") {
        1
    } else if type_string.eq_ignore_ascii_case("c") {
        2
    } else if type_string.eq_ignore_ascii_case("f") {
        3
    } else if type_string.eq_ignore_ascii_case("p") {
        4
    } else if type_string.eq_ignore_ascii_case("x") {
        5
    } else {
        255
    };
    Some(encode_unity_version(
        major,
        minor,
        build,
        type_byte,
        type_number,
    ))
}

#[derive(Debug, Clone)]
enum VersionedTree {
    /// The class is absent for this version or has no TypeTree for this serialization mode.
    Unavailable,
    Available(Arc<TypeTree>),
}

#[derive(Debug, Clone)]
struct VersionedTrees {
    release: VersionedTree,
    editor: VersionedTree,
}

impl VersionedTrees {
    fn unavailable() -> Self {
        Self {
            release: VersionedTree::Unavailable,
            editor: VersionedTree::Unavailable,
        }
    }

    fn for_mode(&self, mode: TypeTreeSerializationMode) -> &VersionedTree {
        match mode {
            TypeTreeSerializationMode::Release => &self.release,
            TypeTreeSerializationMode::Editor => &self.editor,
        }
    }
}

fn select_versioned_tree(
    version: u64,
    classes: &[(u64, VersionedTrees)],
    mode: TypeTreeSerializationMode,
) -> Option<Arc<TypeTree>> {
    let upper_bound = classes.partition_point(|(candidate, _)| *candidate <= version);
    let (_, selected) = classes.get(upper_bound.checked_sub(1)?)?;
    match selected.for_mode(mode) {
        VersionedTree::Unavailable => None,
        VersionedTree::Available(tree) => Some(tree.clone()),
    }
}

fn build_tree_from_blob(
    blob: &TpkTypeTreeBlob,
    root_id: usize,
    budget: &mut AssetLoadBudget,
) -> Result<TypeTree> {
    fn build_node(
        blob: &TpkTypeTreeBlob,
        node_id: usize,
        level: i32,
        next_index: &mut i32,
        budget: &mut AssetLoadBudget,
    ) -> Result<TypeTreeNode> {
        let node = blob.nodes.get(node_id).ok_or_else(|| {
            BinaryError::invalid_data(format!("TPK node out of range: {}", node_id))
        })?;
        let type_name = blob.strings.get(node.type_name as usize).ok_or_else(|| {
            BinaryError::invalid_data("TPK type string index out of range".to_string())
        })?;
        let name = blob.strings.get(node.name as usize).ok_or_else(|| {
            BinaryError::invalid_data("TPK name string index out of range".to_string())
        })?;

        let depth = u32::try_from(level)
            .map_err(|_| BinaryError::invalid_data("Negative TPK TypeTree level"))?;
        budget.observe_depth(depth)?;
        budget.consume_entries(1)?;
        budget.consume_members(usize_to_u64(
            node.sub_nodes.len(),
            "TPK TypeTree child count",
        )?)?;

        let mut out = TypeTreeNode::new();
        out.type_name = clone_budgeted_string(type_name, "TPK TypeTree type name", budget)?;
        out.name = clone_budgeted_string(name, "TPK TypeTree field name", budget)?;
        out.byte_size = node.byte_size;
        out.index = *next_index;
        out.version = node.version as i32;
        out.type_flags = node.type_flags as i32;
        out.meta_flags = node.meta_flag as i32;
        out.level = level;

        *next_index = next_index
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("TPK TypeTree node index overflow"))?;
        reserve_vec_exact(
            &mut out.children,
            node.sub_nodes.len(),
            "TPK TypeTree child nodes",
            budget,
        )?;
        let child_level = level
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("TPK TypeTree level overflow"))?;
        for id in &node.sub_nodes {
            out.children.push(build_node(
                blob,
                usize::from(*id),
                child_level,
                next_index,
                budget,
            )?);
        }
        Ok(out)
    }

    let mut next_index: i32 = 0;
    let root = build_node(blob, root_id, 0, &mut next_index, budget)?;
    let mut tree = TypeTree::new();
    reserve_vec_exact(&mut tree.nodes, 1, "TPK TypeTree roots", budget)?;
    tree.nodes.push(root);
    Ok(tree)
}

fn clone_budgeted_string(value: &str, label: &str, budget: &mut AssetLoadBudget) -> Result<String> {
    let bytes = usize_to_u64(value.len(), label)?;
    budget.check_bytes(bytes)?;
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} bytes for {label}: {error}",
            value.len()
        ))
    })?;
    budget.consume_bytes(bytes)?;
    owned.push_str(value);
    Ok(owned)
}

fn parse_tpk_header(reader: &mut TpkReader<'_, '_>) -> Result<TpkFileHeader> {
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

enum TpkPayload<'data> {
    Borrowed(&'data [u8]),
    Owned(Vec<u8>),
}

impl TpkPayload<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

fn decompress_tpk_payload<'data>(
    header: &TpkFileHeader,
    compressed: &'data [u8],
    budget: &mut AssetLoadBudget,
) -> Result<TpkPayload<'data>> {
    let compressed_size = usize_to_u64(compressed.len(), "TPK compressed payload length")?;
    let uncompressed_size = u64::from(header.uncompressed_size);
    budget.check_decompression(compressed_size, uncompressed_size)?;

    if header.compression == TpkCompressionType::None {
        if compressed_size != uncompressed_size {
            return Err(BinaryError::invalid_data(format!(
                "Uncompressed TPK payload declares {uncompressed_size} bytes but contains {compressed_size}"
            )));
        }
        let mut stream = budget.begin_decompression();
        stream.consume(compressed_size, uncompressed_size)?;
        return Ok(TpkPayload::Borrowed(compressed));
    }

    let compression = match header.compression {
        TpkCompressionType::None => unreachable!("uncompressed TPK returned above"),
        TpkCompressionType::Lz4 => CompressionType::Lz4,
        TpkCompressionType::Lzma => CompressionType::Lzma,
        TpkCompressionType::Brotli => CompressionType::Brotli,
    };
    let expected = usize::try_from(header.uncompressed_size).map_err(|_| {
        BinaryError::memory_error("TPK uncompressed payload length does not fit usize")
    })?;

    // The decoder returns one owned output backing. Charge it separately from parsing that
    // backing, while the borrowed encoded payload remains allocation-free.
    budget.consume_bytes(uncompressed_size)?;
    let decompressed =
        compression::decompress_with_budget(compressed, compression, expected, budget)?;
    Ok(TpkPayload::Owned(decompressed))
}

fn parse_tpk_typetree_blob(data: &[u8], budget: &mut AssetLoadBudget) -> Result<TpkTypeTreeBlob> {
    let mut r = TpkReader::new(data, budget);
    let creation_time = r.read_i64_le()?;
    let version_count = r.read_count("TPK version", 8, CountKind::Members)?;
    let mut versions = Vec::new();
    reserve_vec_exact(&mut versions, version_count, "TPK versions", r.budget)?;
    for _ in 0..version_count {
        versions.push(r.read_u64_le()?);
    }

    let class_count = r.read_count("TPK class", 8, CountKind::Entries)?;
    let mut class_information = Vec::new();
    reserve_vec_exact(&mut class_information, class_count, "TPK classes", r.budget)?;
    for _ in 0..class_count {
        let id = r.read_i32_le()?;
        let count = r.read_count("TPK class version", 9, CountKind::Members)?;
        let mut classes = Vec::new();
        reserve_vec_exact(&mut classes, count, "TPK class versions", r.budget)?;
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
        class_information.push(TpkClassInformation { id, classes });
    }
    class_information.sort_unstable_by_key(|information| information.id);
    for duplicate in class_information.windows(2) {
        if duplicate[0].id == duplicate[1].id {
            let id = duplicate[0].id;
            return Err(BinaryError::invalid_data(format!(
                "Duplicate TPK class id {id}"
            )));
        }
    }

    // CommonString (we don't need the data for tree construction, but we must consume it)
    let common_version_count = r.read_count("TPK common string version", 9, CountKind::Members)?;
    for _ in 0..common_version_count {
        let _ver = r.read_u64_le()?;
        let _count = r.read_u8()?;
    }
    let indices_count = r.read_count("TPK common string index", 2, CountKind::Members)?;
    for _ in 0..indices_count {
        let _idx = r.read_u16_le()?;
    }

    // NodeBuffer
    let node_count = r.read_count("TPK node", 17, CountKind::Entries)?;
    let mut nodes = Vec::new();
    reserve_vec_exact(&mut nodes, node_count, "TPK nodes", r.budget)?;
    for _ in 0..node_count {
        let type_name = r.read_u16_le()?;
        let name = r.read_u16_le()?;
        let byte_size = r.read_i32_le()?;
        let version = r.read_i16_le()?;
        let type_flags = r.read_i8()?;
        let meta_flag = r.read_u32_le()?;
        let count = r.read_u16_member_count("TPK sub-node", 2)?;
        let mut sub_nodes = Vec::new();
        reserve_vec_exact(&mut sub_nodes, count, "TPK sub-nodes", r.budget)?;
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
    let string_count = r.read_count("TPK string", 1, CountKind::Entries)?;
    let mut strings = Vec::new();
    reserve_vec_exact(&mut strings, string_count, "TPK strings", r.budget)?;
    for _ in 0..string_count {
        strings.push(r.read_string()?);
    }

    if r.remaining() != 0 {
        return Err(BinaryError::invalid_data(format!(
            "TPK TypeTree blob has {} trailing bytes",
            r.remaining()
        )));
    }

    let blob = TpkTypeTreeBlob {
        creation_time,
        versions,
        class_information,
        nodes,
        strings,
    };
    let budget = r.into_budget();
    validate_tpk_blob(&blob, budget)?;
    Ok(blob)
}

fn reserve_vec_exact<T>(
    values: &mut Vec<T>,
    count: usize,
    label: &str,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let storage = checked_storage_bytes::<T>(count, label)?;
    budget.check_bytes(storage)?;
    values.try_reserve_exact(count).map_err(|error| {
        BinaryError::memory_error(format!("Failed to reserve {count} {label}: {error}"))
    })?;
    budget.consume_bytes(storage)?;
    Ok(())
}

fn checked_storage_bytes<T>(count: usize, label: &str) -> Result<u64> {
    let storage = count
        .checked_mul(size_of::<T>())
        .ok_or_else(|| BinaryError::memory_error(format!("{label} allocation size overflow")))?;
    usize_to_u64(storage, label)
}

fn charge_arc_storage<T>(label: &str, budget: &mut AssetLoadBudget) -> Result<()> {
    let storage = size_of::<T>()
        .checked_add(size_of::<usize>() * 2)
        .ok_or_else(|| BinaryError::memory_error(format!("{label} allocation size overflow")))?;
    budget.consume_bytes(usize_to_u64(storage, label)?)?;
    Ok(())
}

fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| BinaryError::memory_error(format!("{label} does not fit u64")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeVisitState {
    Unvisited,
    Visiting,
    Complete,
}

const MAX_TPK_GRAPH_DEPTH: u32 = 512;

fn validate_tpk_blob(blob: &TpkTypeTreeBlob, budget: &mut AssetLoadBudget) -> Result<()> {
    for (node_id, node) in blob.nodes.iter().enumerate() {
        if blob.strings.get(usize::from(node.type_name)).is_none() {
            return Err(BinaryError::invalid_data(format!(
                "TPK node {node_id} type string index {} is out of range",
                node.type_name
            )));
        }
        if blob.strings.get(usize::from(node.name)).is_none() {
            return Err(BinaryError::invalid_data(format!(
                "TPK node {node_id} name string index {} is out of range",
                node.name
            )));
        }
        for child in &node.sub_nodes {
            if blob.nodes.get(usize::from(*child)).is_none() {
                return Err(BinaryError::invalid_data(format!(
                    "TPK node {node_id} child index {child} is out of range"
                )));
            }
        }
    }

    for class in blob
        .class_information
        .iter()
        .flat_map(|info| info.classes.iter())
        .filter_map(|(_, class)| class.as_ref())
    {
        for root in [class.editor_root_node, class.release_root_node]
            .into_iter()
            .flatten()
        {
            if blob.nodes.get(usize::from(root)).is_none() {
                return Err(BinaryError::invalid_data(format!(
                    "TPK class root node {root} is out of range"
                )));
            }
        }
    }

    let mut states = Vec::new();
    reserve_vec_exact(
        &mut states,
        blob.nodes.len(),
        "TPK graph visit states",
        budget,
    )?;
    states.resize(blob.nodes.len(), NodeVisitState::Unvisited);
    let mut heights = Vec::new();
    reserve_vec_exact(&mut heights, blob.nodes.len(), "TPK graph heights", budget)?;
    heights.resize(blob.nodes.len(), 0_u32);

    for node_id in 0..blob.nodes.len() {
        validate_tpk_node(node_id, 0, blob, &mut states, &mut heights, budget)?;
    }
    Ok(())
}

fn validate_tpk_node(
    node_id: usize,
    traversal_depth: u32,
    blob: &TpkTypeTreeBlob,
    states: &mut [NodeVisitState],
    heights: &mut [u32],
    budget: &mut AssetLoadBudget,
) -> Result<u32> {
    budget.observe_depth(traversal_depth)?;
    if traversal_depth > MAX_TPK_GRAPH_DEPTH {
        return Err(BinaryError::ResourceLimitExceeded(format!(
            "TPK node depth {traversal_depth} exceeds format limit {MAX_TPK_GRAPH_DEPTH}"
        )));
    }
    match states[node_id] {
        NodeVisitState::Complete => return Ok(heights[node_id]),
        NodeVisitState::Visiting => {
            return Err(BinaryError::invalid_data(format!(
                "TPK node graph contains a cycle at node {node_id}"
            )));
        }
        NodeVisitState::Unvisited => {}
    }

    states[node_id] = NodeVisitState::Visiting;
    let mut maximum_child_height = 0_u32;
    for child in &blob.nodes[node_id].sub_nodes {
        let child_depth = traversal_depth
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("TPK node traversal depth overflow"))?;
        let height = validate_tpk_node(
            usize::from(*child),
            child_depth,
            blob,
            states,
            heights,
            budget,
        )?;
        maximum_child_height = maximum_child_height.max(height);
    }
    let height = maximum_child_height
        .checked_add(1)
        .ok_or_else(|| BinaryError::invalid_data("TPK node graph height overflow"))?;
    let semantic_depth = height - 1;
    budget.observe_depth(semantic_depth)?;
    if semantic_depth > MAX_TPK_GRAPH_DEPTH {
        return Err(BinaryError::ResourceLimitExceeded(format!(
            "TPK node depth {semantic_depth} exceeds format limit {MAX_TPK_GRAPH_DEPTH}"
        )));
    }
    states[node_id] = NodeVisitState::Complete;
    heights[node_id] = height;
    Ok(height)
}

fn prebuild_registry(
    blob: &TpkTypeTreeBlob,
    budget: &mut AssetLoadBudget,
) -> Result<ResolvedClassMap> {
    fn prebuild_root(
        blob: &TpkTypeTreeBlob,
        root_id: Option<u16>,
        trees_by_root: &mut [Option<Arc<TypeTree>>],
        budget: &mut AssetLoadBudget,
    ) -> Result<VersionedTree> {
        let Some(root_id) = root_id.map(usize::from) else {
            return Ok(VersionedTree::Unavailable);
        };
        if let Some(tree) = &trees_by_root[root_id] {
            return Ok(VersionedTree::Available(tree.clone()));
        }

        let tree = build_tree_from_blob(blob, root_id, budget)?;
        charge_arc_storage::<TypeTree>("TPK prebuilt TypeTree Arc", budget)?;
        let tree = Arc::new(tree);
        trees_by_root[root_id] = Some(tree.clone());
        Ok(VersionedTree::Available(tree))
    }

    let mut trees_by_root = Vec::new();
    reserve_vec_exact(
        &mut trees_by_root,
        blob.nodes.len(),
        "TPK prebuilt root index",
        budget,
    )?;
    trees_by_root.resize_with(blob.nodes.len(), || None::<Arc<TypeTree>>);

    let mut resolved = Vec::new();
    reserve_vec_exact(
        &mut resolved,
        blob.class_information.len(),
        "TPK resolved classes",
        budget,
    )?;
    for information in &blob.class_information {
        let class_id = information.id;
        let mut versions = Vec::new();
        reserve_vec_exact(
            &mut versions,
            information.classes.len(),
            "TPK resolved class versions",
            budget,
        )?;
        let mut previous_version = None;
        for (version, class) in &information.classes {
            if previous_version.is_some_and(|previous| *version < previous) {
                return Err(BinaryError::invalid_data(format!(
                    "TPK class {class_id} version records are not sorted"
                )));
            }
            previous_version = Some(*version);
            let trees = match class {
                None => VersionedTrees::unavailable(),
                Some(class) => VersionedTrees {
                    release: prebuild_root(
                        blob,
                        class.release_root_node,
                        &mut trees_by_root,
                        budget,
                    )?,
                    editor: prebuild_root(
                        blob,
                        class.editor_root_node,
                        &mut trees_by_root,
                        budget,
                    )?,
                },
            };
            versions.push((*version, trees));
        }
        resolved.push((class_id, versions));
    }
    Ok(resolved)
}

/// A UnityPy-compatible TPK TypeTree registry.
#[derive(Debug, Clone)]
pub struct TpkTypeTreeRegistry {
    classes: Arc<ResolvedClassMap>,
}

impl TpkTypeTreeRegistry {
    pub fn from_bytes(data: &[u8], budget: &mut AssetLoadBudget) -> Result<Self> {
        let mut r = TpkReader::new(data, budget);
        let header = parse_tpk_header(&mut r)?;
        if header.data_type != TpkDataType::TypeTreeInformation {
            return Err(BinaryError::unsupported(format!(
                "Unsupported TPK data type: {:?}",
                header.data_type
            )));
        }
        let compressed_size = usize::try_from(header.compressed_size).map_err(|_| {
            BinaryError::memory_error("TPK compressed payload length does not fit usize")
        })?;
        let compressed = r.read_slice(
            compressed_size,
            header.compression != TpkCompressionType::None,
        )?;
        if r.remaining() != 0 {
            return Err(BinaryError::invalid_data(format!(
                "TPK file has {} trailing bytes after its declared payload",
                r.remaining()
            )));
        }
        let budget = r.into_budget();

        let payload = decompress_tpk_payload(&header, compressed, budget)?;
        let blob = parse_tpk_typetree_blob(payload.as_slice(), budget)?;
        let classes = prebuild_registry(&blob, budget)?;
        charge_arc_storage::<ResolvedClassMap>("TPK resolved class map Arc", budget)?;
        Ok(Self {
            classes: Arc::new(classes),
        })
    }

    pub fn from_path(path: impl AsRef<Path>, budget: &mut AssetLoadBudget) -> Result<Self> {
        let path = path.as_ref();
        let mut file = std::fs::File::open(path).map_err(|error| {
            BinaryError::generic(format!("Failed to open TPK file {path:?}: {error}"))
        })?;
        let declared_len = file
            .metadata()
            .map_err(|error| {
                BinaryError::generic(format!("Failed to inspect TPK file {path:?}: {error}"))
            })?
            .len();
        budget.check_bytes(declared_len)?;
        let capacity = usize::try_from(declared_len).map_err(|_| {
            BinaryError::memory_error(format!(
                "TPK file {path:?} length {declared_len} does not fit usize"
            ))
        })?;
        let mut data = Vec::new();
        data.try_reserve_exact(capacity).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {capacity} bytes for TPK file {path:?}: {error}"
            ))
        })?;
        budget.consume_bytes(declared_len)?;
        data.resize(capacity, 0);
        file.read_exact(&mut data).map_err(|error| {
            BinaryError::generic(format!("Failed to read TPK file {path:?}: {error}"))
        })?;
        let mut extra = [0_u8; 1];
        if file.read(&mut extra).map_err(|error| {
            BinaryError::generic(format!(
                "Failed to verify TPK file length {path:?}: {error}"
            ))
        })? != 0
        {
            return Err(BinaryError::invalid_data(format!(
                "TPK file {path:?} grew while it was being read"
            )));
        }
        Self::from_bytes(&data, budget)
    }
}

impl TypeTreeRegistry for TpkTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.resolve_with_mode(unity_version, class_id, TypeTreeSerializationMode::Release)
    }

    fn resolve_with_mode(
        &self,
        unity_version: &str,
        class_id: i32,
        mode: TypeTreeSerializationMode,
    ) -> Option<Arc<TypeTree>> {
        let encoded = parse_unity_version_key(unity_version)?;
        let index = self
            .classes
            .binary_search_by_key(&class_id, |(existing, _)| *existing)
            .ok()?;
        select_versioned_tree(encoded, &self.classes[index].1, mode)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::reader::{BinaryReader, ByteOrder};
    use crate::typetree::{TypeTreeParseOptions, TypeTreeSchema};
    use crate::unity_version::{UnityVersion, UnityVersionType};
    use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError, UnityValue};

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

    #[derive(Debug, Clone, Copy)]
    enum TestClassRecord {
        Present {
            version: u64,
            editor_root: Option<u16>,
            release_root: Option<u16>,
        },
        Missing {
            version: u64,
        },
    }

    fn build_typetree_blob_with_graph(
        versions: &[u64],
        class_records: &[TestClassRecord],
        sub_nodes: &[Vec<u16>],
    ) -> Vec<u8> {
        let mut blob: Vec<u8> = Vec::new();
        blob.extend_from_slice(&0i64.to_le_bytes()); // creation_time
        blob.extend_from_slice(&i32::try_from(versions.len()).unwrap().to_le_bytes());
        for version in versions {
            blob.extend_from_slice(&version.to_le_bytes());
        }

        blob.extend_from_slice(&1i32.to_le_bytes()); // classCount
        blob.extend_from_slice(&(28i32).to_le_bytes()); // class id
        blob.extend_from_slice(&i32::try_from(class_records.len()).unwrap().to_le_bytes());
        for record in class_records {
            match record {
                TestClassRecord::Present {
                    version,
                    editor_root,
                    release_root,
                } => {
                    blob.extend_from_slice(&version.to_le_bytes());
                    blob.push(1u8);
                    blob.extend_from_slice(&(0u16).to_le_bytes()); // name
                    blob.extend_from_slice(&(0u16).to_le_bytes()); // base
                    let mut flags = 0_u8;
                    if editor_root.is_some() {
                        flags |= TpkUnityClassFlags::HasEditorRootNode as u8;
                    }
                    if release_root.is_some() {
                        flags |= TpkUnityClassFlags::HasReleaseRootNode as u8;
                    }
                    blob.push(flags);
                    if let Some(editor_root) = editor_root {
                        blob.extend_from_slice(&editor_root.to_le_bytes());
                    }
                    if let Some(release_root) = release_root {
                        blob.extend_from_slice(&release_root.to_le_bytes());
                    }
                }
                TestClassRecord::Missing { version } => {
                    blob.extend_from_slice(&version.to_le_bytes());
                    blob.push(0u8);
                }
            }
        }

        // CommonString: versionCount=0, indicesCount=0
        blob.extend_from_slice(&0i32.to_le_bytes());
        blob.extend_from_slice(&0i32.to_le_bytes());

        // NodeBuffer
        let node_count = i32::try_from(sub_nodes.len()).unwrap();
        blob.extend_from_slice(&node_count.to_le_bytes());
        for (node_id, children) in sub_nodes.iter().enumerate() {
            if node_id == 0 {
                blob.extend_from_slice(&(0u16).to_le_bytes()); // TypeName idx
                blob.extend_from_slice(&(1u16).to_le_bytes()); // Name idx
            } else {
                blob.extend_from_slice(&(2u16).to_le_bytes()); // TypeName idx
                blob.extend_from_slice(&(3u16).to_le_bytes()); // Name idx
            }
            blob.extend_from_slice(&(-1i32).to_le_bytes()); // ByteSize
            blob.extend_from_slice(&(1i16).to_le_bytes()); // Version
            blob.push(0i8 as u8); // TypeFlags
            blob.extend_from_slice(&(0u32).to_le_bytes()); // MetaFlag
            blob.extend_from_slice(&u16::try_from(children.len()).unwrap().to_le_bytes());
            for child in children {
                blob.extend_from_slice(&child.to_le_bytes());
            }
        }

        // StringBuffer
        blob.extend_from_slice(&4i32.to_le_bytes());
        write_tpk_string("RootType", &mut blob); // 0
        write_tpk_string("Base", &mut blob); // 1
        write_tpk_string("string", &mut blob); // 2
        write_tpk_string("m_Name", &mut blob); // 3

        blob
    }

    fn build_typetree_blob(include_grandchild: bool) -> Vec<u8> {
        let version = parse_unity_version_key("2020.3.0f1").unwrap();
        let sub_nodes = if include_grandchild {
            vec![vec![1], vec![2], Vec::new()]
        } else {
            vec![vec![1], Vec::new()]
        };
        build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: None,
                release_root: Some(0),
            }],
            &sub_nodes,
        )
    }

    fn build_linear_typetree_blob(depth: u32) -> Vec<u8> {
        let node_count = usize::try_from(depth).unwrap() + 1;
        let mut sub_nodes = Vec::with_capacity(node_count);
        for node_id in 0..node_count {
            if node_id + 1 < node_count {
                sub_nodes.push(vec![u16::try_from(node_id + 1).unwrap()]);
            } else {
                sub_nodes.push(Vec::new());
            }
        }
        let version = parse_unity_version_key("2020.3.0f1").unwrap();
        build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: None,
                release_root: Some(0),
            }],
            &sub_nodes,
        )
    }

    fn wrap_tpk_blob(blob: &[u8], compression: TpkCompressionType) -> Vec<u8> {
        let compressed = match compression {
            TpkCompressionType::None => blob.to_vec(),
            TpkCompressionType::Lz4 => lz4_flex::block::compress(blob),
            other => panic!("test fixture does not support {other:?}"),
        };
        let compressed_size = u32::try_from(compressed.len()).unwrap();
        let uncompressed_size = u32::try_from(blob.len()).unwrap();
        let mut out: Vec<u8> = Vec::new();
        // TpkFile header: <IbbbbIII
        out.extend_from_slice(&0x2A4B5054u32.to_le_bytes()); // magic
        out.push(1u8); // versionNumber (i8)
        out.push(compression as i8 as u8); // compressionType
        out.push(TpkDataType::TypeTreeInformation as i8 as u8); // dataType
        out.push(0u8); // unused b
        out.extend_from_slice(&0u32.to_le_bytes()); // unused u32
        out.extend_from_slice(&compressed_size.to_le_bytes()); // compressedSize
        out.extend_from_slice(&uncompressed_size.to_le_bytes()); // uncompressedSize
        out.extend_from_slice(&compressed);
        out
    }

    pub(crate) fn build_minimal_tpk() -> Vec<u8> {
        wrap_tpk_blob(&build_typetree_blob(false), TpkCompressionType::None)
    }

    fn assert_budget_resource(error: BinaryError, resource: &'static str) {
        assert!(
            matches!(
                &error,
                BinaryError::Budget(BudgetError::Exceeded {
                    resource: observed,
                    ..
                }) if *observed == resource
            ),
            "expected {resource} budget error, got {error}"
        );
    }

    fn canonical_version_key(version: &str) -> Option<u64> {
        UnityVersion::parse_version(version).ok().map(|version| {
            let type_byte = match version.version_type {
                UnityVersionType::A => 0,
                UnityVersionType::B => 1,
                UnityVersionType::C => 2,
                UnityVersionType::F => 3,
                UnityVersionType::P => 4,
                UnityVersionType::X => 5,
                UnityVersionType::U => 255,
            };
            encode_unity_version(
                version.major,
                version.minor,
                version.build,
                type_byte,
                version.type_number,
            )
        })
    }

    #[test]
    fn allocation_free_version_key_matches_canonical_parser() {
        for version in [
            "",
            "2020.3.0f1",
            "2019.4.2b7",
            "2021.2.3p4",
            "5.6.0",
            "2020.3.0f1 (8c4f651ec7e6)",
            "2022.3.1t2",
            "2022.3.1f1c2",
            "2020.3.0f999",
        ] {
            assert_eq!(
                parse_unity_version_key(version),
                canonical_version_key(version),
                "version key drift for {version:?}"
            );
        }
        assert_eq!(parse_unity_version_key(""), None);
    }

    #[test]
    fn tpk_registry_resolves_typetree_and_parses_name() {
        let tpk = build_minimal_tpk();
        let mut budget = AssetLoadBudget::default();
        let registry = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap();
        let usage_after_ingestion = budget.usage();
        let tree = registry.resolve("2020.3.0f1", 28).unwrap();
        let resolved_again = registry.resolve("2020.3.0f1", 28).unwrap();
        assert!(Arc::ptr_eq(&tree, &resolved_again));
        assert_eq!(budget.usage(), usage_after_ingestion);

        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&(3i32).to_le_bytes());
        bytes.extend_from_slice(b"foo");
        bytes.push(0); // align to 4

        let schema = TypeTreeSchema::compile(tree.as_ref(), &[], &mut budget).unwrap();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let out = schema
            .read_object_prefix(&mut reader, &mut budget, TypeTreeParseOptions::default(), 1)
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

    #[test]
    fn tpk_registry_resolves_distinct_editor_and_release_roots() {
        let version = parse_unity_version_key("2020.3.0f1").unwrap();
        let blob = build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: Some(1),
                release_root: Some(0),
            }],
            &[Vec::new(), Vec::new()],
        );
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let mut budget = AssetLoadBudget::default();
        let registry = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap();
        let usage_after_ingestion = budget.usage();

        let default_tree = registry.resolve("2020.3.0f1", 28).unwrap();
        let release_tree = registry
            .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Release)
            .unwrap();
        let editor_tree = registry
            .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Editor)
            .unwrap();

        assert!(Arc::ptr_eq(&default_tree, &release_tree));
        assert!(!Arc::ptr_eq(&release_tree, &editor_tree));
        assert_eq!(release_tree.nodes.first().unwrap().name, "Base");
        assert_eq!(editor_tree.nodes.first().unwrap().name, "m_Name");
        assert_eq!(budget.usage(), usage_after_ingestion);
    }

    #[test]
    fn tpk_registry_does_not_fallback_between_serialization_modes() {
        let editor_only = parse_unity_version_key("2020.3.0f1").unwrap();
        let release_only = parse_unity_version_key("2020.3.1f1").unwrap();
        let blob = build_typetree_blob_with_graph(
            &[editor_only, release_only],
            &[
                TestClassRecord::Present {
                    version: editor_only,
                    editor_root: Some(0),
                    release_root: None,
                },
                TestClassRecord::Present {
                    version: release_only,
                    editor_root: None,
                    release_root: Some(1),
                },
            ],
            &[Vec::new(), Vec::new()],
        );
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let mut budget = AssetLoadBudget::default();
        let registry = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap();

        assert!(registry.resolve("2020.3.0f1", 28).is_none());
        assert!(
            registry
                .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Release,)
                .is_none()
        );
        assert!(
            registry
                .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Editor)
                .is_some()
        );
        assert!(registry.resolve("2020.3.1f1", 28).is_some());
        assert!(
            registry
                .resolve_with_mode("2020.3.1f1", 28, TypeTreeSerializationMode::Editor)
                .is_none()
        );
    }

    #[test]
    fn tpk_registry_deduplicates_a_root_shared_by_both_modes() {
        let version = parse_unity_version_key("2020.3.0f1").unwrap();
        let release_only_blob = build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: None,
                release_root: Some(0),
            }],
            &[Vec::new()],
        );
        let shared_blob = build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: Some(0),
                release_root: Some(0),
            }],
            &[Vec::new()],
        );
        let release_only_tpk = wrap_tpk_blob(&release_only_blob, TpkCompressionType::None);
        let shared_tpk = wrap_tpk_blob(&shared_blob, TpkCompressionType::None);

        let mut release_only_budget = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&release_only_tpk, &mut release_only_budget).unwrap();
        let mut shared_budget = AssetLoadBudget::default();
        let registry = TpkTypeTreeRegistry::from_bytes(&shared_tpk, &mut shared_budget).unwrap();

        let release_tree = registry
            .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Release)
            .unwrap();
        let editor_tree = registry
            .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert!(Arc::ptr_eq(&release_tree, &editor_tree));

        let release_only_usage = release_only_budget.usage();
        let shared_usage = shared_budget.usage();
        assert_eq!(shared_usage.entries, release_only_usage.entries);
        assert_eq!(shared_usage.members, release_only_usage.members);
        assert_eq!(
            shared_usage.max_observed_depth,
            release_only_usage.max_observed_depth
        );
        assert_eq!(shared_usage.bytes, release_only_usage.bytes + 2);
    }

    #[test]
    fn tpk_registry_treats_missing_class_records_as_tombstones() {
        let introduced = parse_unity_version_key("2020.3.0f1").unwrap();
        let removed = parse_unity_version_key("2020.3.1f1").unwrap();
        let restored = parse_unity_version_key("2020.3.2f1").unwrap();
        let blob = build_typetree_blob_with_graph(
            &[introduced, removed, restored],
            &[
                TestClassRecord::Present {
                    version: introduced,
                    editor_root: Some(0),
                    release_root: Some(0),
                },
                TestClassRecord::Missing { version: removed },
                TestClassRecord::Present {
                    version: restored,
                    editor_root: Some(0),
                    release_root: Some(0),
                },
            ],
            &[Vec::new()],
        );
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let mut budget = AssetLoadBudget::default();
        let registry = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap();

        let initial_release = registry.resolve("2020.3.0f1", 28).unwrap();
        let initial_editor = registry
            .resolve_with_mode("2020.3.0f1", 28, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert!(Arc::ptr_eq(&initial_release, &initial_editor));
        assert!(registry.resolve("2020.3.1f1", 28).is_none());
        assert!(registry.resolve("2020.3.1p1", 28).is_none());
        assert!(
            registry
                .resolve_with_mode("2020.3.1f1", 28, TypeTreeSerializationMode::Editor)
                .is_none()
        );
        assert!(
            registry
                .resolve_with_mode("2020.3.1p1", 28, TypeTreeSerializationMode::Editor)
                .is_none()
        );
        let reintroduced_release = registry.resolve("2020.3.2f1", 28).unwrap();
        let reintroduced_editor = registry
            .resolve_with_mode("2020.3.2f1", 28, TypeTreeSerializationMode::Editor)
            .unwrap();
        assert!(Arc::ptr_eq(&initial_release, &reintroduced_release));
        assert!(Arc::ptr_eq(&initial_editor, &reintroduced_editor));
    }

    #[test]
    fn tpk_registry_enforces_exact_byte_budget() {
        let tpk = build_minimal_tpk();
        let mut probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut probe).unwrap();
        let required = probe.usage().bytes;
        assert!(required > 1);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, required);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut short).unwrap_err();
        assert_budget_resource(error, "bytes");
    }

    #[test]
    fn tpk_registry_enforces_exact_member_budget() {
        let tpk = build_minimal_tpk();
        let mut probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut probe).unwrap();
        let required = probe.usage().members;
        assert!(required > 1);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_members: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().members, required);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_members: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut short).unwrap_err();
        assert_budget_resource(error, "members");
    }

    #[test]
    fn tpk_registry_enforces_exact_entry_budget() {
        let tpk = build_minimal_tpk();
        let mut probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut probe).unwrap();
        let required = probe.usage().entries;
        assert!(required > 1);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().entries, required);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut short).unwrap_err();
        assert_budget_resource(error, "entries");
    }

    #[test]
    fn tpk_registry_enforces_exact_node_depth() {
        let tpk = wrap_tpk_blob(&build_typetree_blob(true), TpkCompressionType::None);
        let mut probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut probe).unwrap();
        let required = probe.usage().max_observed_depth;
        assert_eq!(required, 2);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().max_observed_depth, required);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut short).unwrap_err();
        assert_budget_resource(error, "depth");
    }

    #[test]
    fn tpk_registry_depth_boundary_prefers_the_caller_budget_error() {
        let exact_tpk = wrap_tpk_blob(
            &build_linear_typetree_blob(MAX_TPK_GRAPH_DEPTH),
            TpkCompressionType::None,
        );
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: MAX_TPK_GRAPH_DEPTH,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&exact_tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().max_observed_depth, MAX_TPK_GRAPH_DEPTH);

        let one_over = MAX_TPK_GRAPH_DEPTH + 1;
        let one_over_tpk = wrap_tpk_blob(
            &build_linear_typetree_blob(one_over),
            TpkCompressionType::None,
        );
        let mut bounded = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: MAX_TPK_GRAPH_DEPTH,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&one_over_tpk, &mut bounded).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit,
                requested,
            }) if limit == u64::from(MAX_TPK_GRAPH_DEPTH) && requested == u64::from(one_over)
        ));

        let mut widened = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: one_over,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&one_over_tpk, &mut widened).unwrap_err();
        assert!(matches!(error, BinaryError::ResourceLimitExceeded(_)));
    }

    #[test]
    fn tpk_registry_rejects_node_cycles() {
        let version = parse_unity_version_key("2020.3.0f1").unwrap();
        let blob = build_typetree_blob_with_graph(
            &[version],
            &[TestClassRecord::Present {
                version,
                editor_root: None,
                release_root: Some(0),
            }],
            &[vec![1], vec![0]],
        );
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let mut budget = AssetLoadBudget::default();

        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::InvalidData(message) if message.contains("cycle")
        ));
    }

    #[test]
    fn tpk_registry_enforces_exact_decompression_budgets() {
        let blob = build_typetree_blob(false);
        let uncompressed_tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::Lz4);
        let mut uncompressed_probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&uncompressed_tpk, &mut uncompressed_probe).unwrap();
        let mut probe = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut probe).unwrap();
        let usage = probe.usage();
        assert!(usage.compressed_bytes > 1);
        assert!(usage.decompressed_bytes > 1);
        assert_eq!(
            usage.bytes,
            uncompressed_probe.usage().bytes + (tpk.len() - 20 + blob.len()) as u64
        );
        assert_eq!(usage.compressed_bytes, (tpk.len() - 20) as u64);
        assert_eq!(usage.decompressed_bytes, blob.len() as u64);
        assert_eq!(
            uncompressed_probe.usage().compressed_bytes,
            blob.len() as u64
        );
        assert_eq!(
            uncompressed_probe.usage().decompressed_bytes,
            blob.len() as u64
        );

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_compressed_bytes: usage.compressed_bytes,
            max_decompressed_bytes: usage.decompressed_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut exact).unwrap();
        assert_eq!(exact.usage().compressed_bytes, usage.compressed_bytes);
        assert_eq!(exact.usage().decompressed_bytes, usage.decompressed_bytes);

        let mut compressed_short = AssetLoadBudget::new(AssetLoadLimits {
            max_compressed_bytes: usage.compressed_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut compressed_short).unwrap_err();
        assert_budget_resource(error, "compressed_bytes");

        let mut decompressed_short = AssetLoadBudget::new(AssetLoadLimits {
            max_decompressed_bytes: usage.decompressed_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut decompressed_short).unwrap_err();
        assert_budget_resource(error, "decompressed_bytes");
    }

    #[test]
    fn tpk_registry_rejects_huge_count_before_allocation() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&0_i64.to_le_bytes());
        blob.extend_from_slice(&i32::MAX.to_le_bytes());
        let tpk = wrap_tpk_blob(&blob, TpkCompressionType::None);
        let mut budget = AssetLoadBudget::default();

        let error = TpkTypeTreeRegistry::from_bytes(&tpk, &mut budget).unwrap_err();

        assert!(!matches!(error, BinaryError::MemoryError(_)));
        assert_eq!(budget.usage().entries, 0);
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn tpk_path_charges_one_owned_backing_beyond_borrowed_parsing() {
        use std::io::Write as _;

        let tpk = build_minimal_tpk();
        let mut borrowed_budget = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_bytes(&tpk, &mut borrowed_budget).unwrap();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&tpk).unwrap();
        file.flush().unwrap();
        let mut path_budget = AssetLoadBudget::default();
        TpkTypeTreeRegistry::from_path(file.path(), &mut path_budget).unwrap();

        let borrowed = borrowed_budget.usage();
        let path = path_budget.usage();
        assert_eq!(path.entries, borrowed.entries);
        assert_eq!(path.members, borrowed.members);
        assert_eq!(path.max_observed_depth, borrowed.max_observed_depth);
        assert_eq!(path.compressed_bytes, borrowed.compressed_bytes);
        assert_eq!(path.decompressed_bytes, borrowed.decompressed_bytes);
        assert_eq!(path.bytes, borrowed.bytes + tpk.len() as u64);

        let mut backing_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: tpk.len() as u64 - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = TpkTypeTreeRegistry::from_path(file.path(), &mut backing_short).unwrap_err();
        assert_budget_resource(error, "bytes");
        assert_eq!(backing_short.usage(), Default::default());
    }
}
