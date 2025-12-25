//! TypeTree serialization and deserialization
//!
//! This module provides functionality for serializing and deserializing
//! Unity objects using TypeTree information.

use super::types::{TypeTree, TypeTreeNode};
use crate::asset::SerializedType;
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use indexmap::IndexMap;
use unity_asset_core::UnityValue;

/// TypeTree serializer
///
/// This struct provides methods for serializing and deserializing Unity objects
/// using TypeTree structure information.
pub struct TypeTreeSerializer<'a> {
    tree: &'a TypeTree,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PPtrScanResult {
    pub internal: Vec<i64>,
    pub external: Vec<(i32, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTreeParseMode {
    Strict,
    Lenient,
}

impl Default for TypeTreeParseMode {
    fn default() -> Self {
        Self::Lenient
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TypeTreeParseOptions {
    pub mode: TypeTreeParseMode,
}

#[derive(Debug, Clone)]
pub struct TypeTreeParseWarning {
    pub field: String,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct TypeTreeParseOutput {
    pub properties: IndexMap<String, UnityValue>,
    pub warnings: Vec<TypeTreeParseWarning>,
}

#[derive(Debug)]
struct TypeTreeParseContext<'a> {
    options: TypeTreeParseOptions,
    ref_types: Option<&'a [SerializedType]>,
    has_managed_registry: bool,
}

#[derive(Debug)]
struct TypeTreeScanContext<'a> {
    ref_types: Option<&'a [SerializedType]>,
    has_managed_registry: bool,
}

impl<'a> TypeTreeSerializer<'a> {
    const MAX_ARRAY_LEN: usize = 1_000_000;
    const MAX_TYPELESSDATA_LEN: usize = Self::MAX_ARRAY_LEN;

    /// Create a new serializer with a TypeTree
    pub fn new(tree: &'a TypeTree) -> Self {
        Self { tree }
    }

    /// Parse object data using the TypeTree structure
    pub fn parse_object(&self, reader: &mut BinaryReader) -> Result<IndexMap<String, UnityValue>> {
        Ok(self
            .parse_object_detailed(reader, TypeTreeParseOptions::default())?
            .properties)
    }

    pub fn parse_object_with_ref_types(
        &self,
        reader: &mut BinaryReader,
        ref_types: &'a [SerializedType],
    ) -> Result<IndexMap<String, UnityValue>> {
        Ok(self
            .parse_object_detailed_with_ref_types(
                reader,
                TypeTreeParseOptions::default(),
                ref_types,
            )?
            .properties)
    }

    pub fn parse_object_detailed(
        &self,
        reader: &mut BinaryReader,
        options: TypeTreeParseOptions,
    ) -> Result<TypeTreeParseOutput> {
        self.parse_object_prefix_detailed(reader, options, usize::MAX)
    }

    pub fn parse_object_detailed_with_ref_types(
        &self,
        reader: &mut BinaryReader,
        options: TypeTreeParseOptions,
        ref_types: &'a [SerializedType],
    ) -> Result<TypeTreeParseOutput> {
        self.parse_object_prefix_detailed_with_ref_types(reader, options, usize::MAX, ref_types)
    }

    /// Parse only the first `root_children` fields of the root node.
    ///
    /// This enables UnityPy-like fast paths such as `peek_name()` where we only need a small prefix
    /// of the TypeTree to reach `m_Name`.
    pub fn parse_object_prefix_detailed(
        &self,
        reader: &mut BinaryReader,
        options: TypeTreeParseOptions,
        root_children: usize,
    ) -> Result<TypeTreeParseOutput> {
        self.parse_object_prefix_ctx(
            reader,
            TypeTreeParseContext {
                options,
                ref_types: None,
                has_managed_registry: false,
            },
            root_children,
        )
    }

    pub fn parse_object_prefix_detailed_with_ref_types(
        &self,
        reader: &mut BinaryReader,
        options: TypeTreeParseOptions,
        root_children: usize,
        ref_types: &'a [SerializedType],
    ) -> Result<TypeTreeParseOutput> {
        self.parse_object_prefix_ctx(
            reader,
            TypeTreeParseContext {
                options,
                ref_types: Some(ref_types),
                has_managed_registry: false,
            },
            root_children,
        )
    }

    fn parse_object_prefix_ctx(
        &self,
        reader: &mut BinaryReader,
        mut ctx: TypeTreeParseContext<'a>,
        root_children: usize,
    ) -> Result<TypeTreeParseOutput> {
        let mut out = TypeTreeParseOutput::default();

        if let Some(root) = self.tree.nodes.first() {
            for child in root.children.iter().take(root_children) {
                if child.name.is_empty() {
                    continue;
                }
                match self.parse_value_by_type_ctx(reader, child, &mut ctx) {
                    Ok(value) => {
                        out.properties.insert(child.name.clone(), value);
                    }
                    Err(e) => {
                        if reader.remaining() == 0 {
                            break;
                        }
                        match ctx.options.mode {
                            TypeTreeParseMode::Strict => return Err(e),
                            TypeTreeParseMode::Lenient => out.warnings.push(TypeTreeParseWarning {
                                field: child.name.clone(),
                                error: e.to_string(),
                            }),
                        }
                    }
                }
            }
        }

        Ok(out)
    }

    /// Scan TypeTree-based object bytes and collect any encountered `PPtr` references without
    /// allocating a full `UnityValue` tree.
    pub fn scan_pptrs(&self, reader: &mut BinaryReader) -> Result<PPtrScanResult> {
        self.scan_pptrs_with_ref_types(reader, None)
    }

    fn scan_value(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        out: &mut PPtrScanResult,
    ) -> Result<()> {
        let mut ctx = TypeTreeScanContext {
            ref_types: None,
            has_managed_registry: false,
        };
        self.scan_value_ctx(reader, node, out, &mut ctx)
    }

    /// Scan TypeTree-based object bytes and collect any encountered `PPtr` references, using
    /// file-level `ref_types` to best-effort traverse managed reference payloads.
    pub fn scan_pptrs_with_ref_types(
        &self,
        reader: &mut BinaryReader,
        ref_types: Option<&[SerializedType]>,
    ) -> Result<PPtrScanResult> {
        let mut out = PPtrScanResult::default();
        let mut ctx = TypeTreeScanContext {
            ref_types,
            has_managed_registry: false,
        };
        if let Some(root) = self.tree.nodes.first() {
            for child in &root.children {
                self.scan_value_ctx(reader, child, &mut out, &mut ctx)?;
            }
        }
        Ok(out)
    }

    fn scan_value_ctx(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        out: &mut PPtrScanResult,
        ctx: &mut TypeTreeScanContext<'_>,
    ) -> Result<()> {
        // Array types
        if !node.children.is_empty() && node.children.iter().any(|c| c.type_name == "Array") {
            self.scan_array(reader, node, out, ctx)?;
            if node.is_aligned() {
                reader.align_to(4)?;
            }
            return Ok(());
        }

        // Managed reference payload (`SerializeReference`): best-effort typed scanning via `ref_types`.
        if node.type_name == "ReferencedObject" && !node.children.is_empty() {
            let mut class: Option<String> = None;
            let mut ns: Option<String> = None;
            let mut asm: Option<String> = None;

            for child in &node.children {
                if child.type_name == "ManagedReferencesRegistry" {
                    if ctx.has_managed_registry {
                        self.scan_value_ctx(reader, child, out, ctx)?;
                    } else {
                        ctx.has_managed_registry = true;
                        self.scan_value_ctx(reader, child, out, ctx)?;
                    }
                    continue;
                }

                if child.name == "type" && !child.children.is_empty() {
                    for field in &child.children {
                        if field.type_name == "string"
                            && (field.name == "class" || field.name == "m_ClassName")
                        {
                            class = Some(reader.read_aligned_string()?);
                            continue;
                        }
                        if field.type_name == "string"
                            && (field.name == "ns" || field.name == "m_NameSpace")
                        {
                            ns = Some(reader.read_aligned_string()?);
                            continue;
                        }
                        if field.type_name == "string"
                            && (field.name == "asm" || field.name == "m_AssemblyName")
                        {
                            asm = Some(reader.read_aligned_string()?);
                            continue;
                        }
                        self.scan_value_ctx(reader, field, out, ctx)?;
                    }
                    if child.is_aligned() {
                        reader.align_to(4)?;
                    }
                    continue;
                }

                if child.type_name == "ReferencedObjectData" {
                    if let (Some(class), Some(ns), Some(asm), Some(ref_types)) =
                        (class.as_ref(), ns.as_ref(), asm.as_ref(), ctx.ref_types)
                    {
                        if let Some(tree) = resolve_ref_type_tree_triplet(class, ns, asm, ref_types)
                        {
                            if let Some(root) = tree.nodes.first() {
                                for field in &root.children {
                                    self.scan_value_ctx(reader, field, out, ctx)?;
                                }
                                if child.is_aligned() {
                                    reader.align_to(4)?;
                                }
                                continue;
                            }
                        }
                    }

                    // Fallback: consume according to placeholder node (if any).
                    self.scan_value_ctx(reader, child, out, ctx)?;
                    continue;
                }

                self.scan_value_ctx(reader, child, out, ctx)?;
            }

            if node.is_aligned() {
                reader.align_to(4)?;
            }
            return Ok(());
        }

        // `PPtr<T>` types (best-effort): parse `fileID` + `pathID` while still consuming all children.
        let is_pptr = node.type_name == "PPtr" || node.type_name.starts_with("PPtr<");
        if is_pptr && !node.children.is_empty() {
            let mut file_id: Option<i32> = None;
            let mut path_id: Option<i64> = None;

            for child in &node.children {
                if child.name.eq_ignore_ascii_case("fileID")
                    || child.name.eq_ignore_ascii_case("m_FileID")
                {
                    // Unity encodes fileID as int.
                    let v = self.scan_read_i32_like(reader, child)?;
                    file_id = Some(v);
                } else if child.name.eq_ignore_ascii_case("pathID")
                    || child.name.eq_ignore_ascii_case("m_PathID")
                {
                    // Unity encodes pathID as long (may be 32-bit in older versions, TypeTree guides us).
                    let v = self.scan_read_i64_like(reader, child)?;
                    path_id = Some(v);
                } else {
                    self.scan_value_ctx(reader, child, out, ctx)?;
                }
            }

            if let (Some(file_id), Some(path_id)) = (file_id, path_id) {
                if path_id != 0 {
                    if file_id == 0 {
                        out.internal.push(path_id);
                    } else {
                        out.external.push((file_id, path_id));
                    }
                }
            }

            if node.is_aligned() {
                reader.align_to(4)?;
            }
            return Ok(());
        }

        match node.type_name.as_str() {
            "SInt8" | "char" | "UInt8" => {
                let _ = reader.read_u8()?;
            }
            "bool" => {
                let _ = reader.read_u8()?;
            }
            "SInt16" | "short" => {
                let _ = reader.read_i16()?;
            }
            "UInt16" | "unsigned short" => {
                let _ = reader.read_u16()?;
            }
            "SInt32" | "int" => {
                let _ = reader.read_i32()?;
            }
            "UInt32" | "unsigned int" | "Type*" => {
                let _ = reader.read_u32()?;
            }
            "SInt64" | "long long" => {
                let _ = reader.read_i64()?;
            }
            "UInt64" | "unsigned long long" | "FileSize" => {
                let _ = reader.read_u64()?;
            }
            "float" => {
                let _ = reader.read_f32()?;
            }
            "double" => {
                let _ = reader.read_f64()?;
            }
            "string" => {
                let len = reader.read_i32()?;
                if len < 0 {
                    return Err(BinaryError::invalid_data(format!(
                        "Negative string length: {}",
                        len
                    )));
                }
                let len: usize = len as usize;
                if len > BinaryReader::DEFAULT_MAX_STRING_LEN {
                    return Err(BinaryError::invalid_data(format!(
                        "String length {} exceeds limit {}",
                        len,
                        BinaryReader::DEFAULT_MAX_STRING_LEN
                    )));
                }
                reader.skip_bytes(len)?;
                reader.align_to(4)?;
            }
            "TypelessData" => {
                let length = reader.read_i32()?;
                if length < 0 {
                    return Err(BinaryError::invalid_data(format!(
                        "Negative TypelessData length: {}",
                        length
                    )));
                }
                let length: usize = length as usize;
                if length > Self::MAX_TYPELESSDATA_LEN {
                    return Err(BinaryError::invalid_data(format!(
                        "TypelessData length {} exceeds limit {}",
                        length,
                        Self::MAX_TYPELESSDATA_LEN
                    )));
                }
                reader.skip_bytes(length)?;
            }
            _ => {
                if !node.children.is_empty() {
                    for child in &node.children {
                        self.scan_value_ctx(reader, child, out, ctx)?;
                    }
                } else if node.byte_size > 0 {
                    reader.skip_bytes(node.byte_size as usize)?;
                }
            }
        }

        if node.is_aligned() {
            reader.align_to(4)?;
        }
        Ok(())
    }

    fn scan_read_i32_like(&self, reader: &mut BinaryReader, node: &TypeTreeNode) -> Result<i32> {
        let v = match node.type_name.as_str() {
            "SInt32" | "int" => reader.read_i32()?,
            "UInt32" | "unsigned int" | "Type*" => reader.read_u32()? as i32,
            "SInt16" | "short" => reader.read_i16()? as i32,
            "UInt16" | "unsigned short" => reader.read_u16()? as i32,
            "SInt8" => reader.read_i8()? as i32,
            "char" => reader.read_u8()? as i32,
            "UInt8" => reader.read_u8()? as i32,
            other => {
                return Err(BinaryError::invalid_data(format!(
                    "Unsupported fileID type: {}",
                    other
                )));
            }
        };
        if node.is_aligned() {
            reader.align_to(4)?;
        }
        Ok(v)
    }

    fn scan_read_i64_like(&self, reader: &mut BinaryReader, node: &TypeTreeNode) -> Result<i64> {
        let v = match node.type_name.as_str() {
            "SInt64" | "long long" => reader.read_i64()?,
            "UInt64" | "unsigned long long" | "FileSize" => reader.read_u64()? as i64,
            "SInt32" | "int" => reader.read_i32()? as i64,
            "UInt32" | "unsigned int" | "Type*" => reader.read_u32()? as i64,
            other => {
                return Err(BinaryError::invalid_data(format!(
                    "Unsupported pathID type: {}",
                    other
                )));
            }
        };
        if node.is_aligned() {
            reader.align_to(4)?;
        }
        Ok(v)
    }

    fn scan_array(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        out: &mut PPtrScanResult,
        ctx: &mut TypeTreeScanContext<'_>,
    ) -> Result<()> {
        let array_node = node
            .children
            .iter()
            .find(|child| child.type_name == "Array")
            .ok_or_else(|| BinaryError::invalid_data("Array node not found in array type"))?;

        let size_i32 = reader.read_i32()?;
        if size_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative array size: {}",
                size_i32
            )));
        }
        if let Some(size_node) = array_node.children.first() {
            if size_node.is_aligned() {
                reader.align_to(4)?;
            }
        }
        let size = size_i32 as usize;
        if size > Self::MAX_ARRAY_LEN {
            return Err(BinaryError::invalid_data(format!(
                "Array size too large: {}",
                size
            )));
        }

        let element_node = array_node
            .children
            .get(1)
            .ok_or_else(|| BinaryError::invalid_data("Array element type not found"))?;

        // Mirror the deserializer fast paths, but skipping bytes instead of allocating.
        if element_node.children.is_empty() {
            match element_node.type_name.as_str() {
                "UInt8" | "char" | "SInt8" | "bool" => {
                    reader.skip_bytes(size)?;
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(());
                }
                "SInt16" | "short" | "UInt16" | "unsigned short" => {
                    reader.skip_bytes(size.checked_mul(2).ok_or_else(|| {
                        BinaryError::invalid_data("Array byte length overflow")
                    })?)?;
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(());
                }
                "SInt32" | "int" | "UInt32" | "unsigned int" | "Type*" | "float" => {
                    reader.skip_bytes(size.checked_mul(4).ok_or_else(|| {
                        BinaryError::invalid_data("Array byte length overflow")
                    })?)?;
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(());
                }
                "SInt64" | "long long" | "UInt64" | "unsigned long long" | "FileSize"
                | "double" => {
                    reader.skip_bytes(size.checked_mul(8).ok_or_else(|| {
                        BinaryError::invalid_data("Array byte length overflow")
                    })?)?;
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }

        for _ in 0..size {
            self.scan_value_ctx(reader, element_node, out, ctx)?;
        }
        if array_node.is_aligned() {
            reader.align_to(4)?;
        }
        Ok(())
    }

    /// Parse value based on TypeTree node type
    fn parse_value_by_type_ctx(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        ctx: &mut TypeTreeParseContext<'a>,
    ) -> Result<UnityValue> {
        let value = match node.type_name.as_str() {
            // Signed integers
            "SInt8" => {
                let val = reader.read_i8()?;
                UnityValue::Integer(val as i64)
            }
            "char" => {
                let val = reader.read_u8()?;
                UnityValue::Integer(val as i64)
            }
            "SInt16" | "short" => {
                let val = reader.read_i16()?;
                UnityValue::Integer(val as i64)
            }
            "SInt32" | "int" => {
                let val = reader.read_i32()?;
                UnityValue::Integer(val as i64)
            }
            "SInt64" | "long long" => {
                let val = reader.read_i64()?;
                UnityValue::Integer(val)
            }

            // Unsigned integers
            "UInt8" => {
                let val = reader.read_u8()?;
                UnityValue::Integer(val as i64)
            }
            "UInt16" | "unsigned short" => {
                let val = reader.read_u16()?;
                UnityValue::Integer(val as i64)
            }
            "UInt32" | "unsigned int" | "Type*" => {
                let val = reader.read_u32()?;
                UnityValue::Integer(val as i64)
            }
            "UInt64" | "unsigned long long" | "FileSize" => {
                let val = reader.read_u64()?;
                UnityValue::Integer(val as i64)
            }

            // Floating point
            "float" => {
                let val = reader.read_f32()?;
                UnityValue::Float(val as f64)
            }
            "double" => {
                let val = reader.read_f64()?;
                UnityValue::Float(val)
            }

            // Boolean
            "bool" => {
                let val = reader.read_u8()? != 0;
                UnityValue::Bool(val)
            }

            // String
            "string" => UnityValue::String(reader.read_aligned_string()?),

            // Typeless raw bytes (UnityPy: read_byte_array)
            "TypelessData" => {
                let length = reader.read_i32()?;
                if length < 0 {
                    return Err(BinaryError::invalid_data(format!(
                        "Negative TypelessData length: {}",
                        length
                    )));
                }
                let length: usize = length as usize;
                if length > Self::MAX_TYPELESSDATA_LEN {
                    return Err(BinaryError::invalid_data(format!(
                        "TypelessData length {} exceeds limit {}",
                        length,
                        Self::MAX_TYPELESSDATA_LEN
                    )));
                }
                let bytes = reader.read_bytes(length)?;
                UnityValue::Bytes(bytes)
            }

            // Array types
            _ if !node.children.is_empty()
                && node.children.iter().any(|c| c.type_name == "Array") =>
            {
                self.parse_array(reader, node, ctx)?
            }

            // Pair type
            "pair" if node.children.len() == 2 => {
                let first = self.parse_value_by_type_ctx(reader, &node.children[0], ctx)?;
                let second = self.parse_value_by_type_ctx(reader, &node.children[1], ctx)?;
                UnityValue::Array(vec![first, second])
            }

            // Managed reference payload (`SerializeReference`): best-effort typed parsing via `ref_types`.
            "ReferencedObject" => {
                let mut nested = IndexMap::new();
                for child in &node.children {
                    if child.type_name == "ManagedReferencesRegistry" {
                        // Consume bytes without allocating: we only need the file-level `ref_types`
                        // list for resolving `ReferencedObjectData`.
                        ctx.has_managed_registry = true;
                        let mut dummy = PPtrScanResult::default();
                        self.scan_value(reader, child, &mut dummy)?;
                        continue;
                    }

                    if child.type_name == "ReferencedObjectData" {
                        let resolved = ctx
                            .ref_types
                            .and_then(|r| resolve_ref_type_tree(&nested, r));
                        if let Some(tree) = resolved {
                            if let Some(root) = tree.nodes.first() {
                                let mut props = IndexMap::new();
                                for field in &root.children {
                                    if field.name.is_empty() {
                                        continue;
                                    }
                                    let v = self.parse_value_by_type_ctx(reader, field, ctx)?;
                                    props.insert(field.name.clone(), v);
                                }
                                if !child.name.is_empty() {
                                    nested.insert(child.name.clone(), UnityValue::Object(props));
                                }
                                continue;
                            }
                        }

                        // Explainable fallback: mark unresolved referenced type so callers can
                        // distinguish “parsed placeholder layout” vs “typed payload”.
                        if let Some((class, ns, asm)) = referenced_type_triplet(&nested) {
                            nested.insert(
                                "_referenced_type_unresolved".to_string(),
                                UnityValue::Bool(true),
                            );
                            nested.insert(
                                "_referenced_type_key".to_string(),
                                UnityValue::String(format!("{}|{}|{}", class, ns, asm)),
                            );
                        }

                        // Fallback: parse according to the placeholder node.
                        let v = self.parse_value_by_type_ctx(reader, child, ctx)?;
                        if !child.name.is_empty() {
                            nested.insert(child.name.clone(), v);
                        }
                        continue;
                    }

                    if !child.name.is_empty() {
                        let v = self.parse_value_by_type_ctx(reader, child, ctx)?;
                        nested.insert(child.name.clone(), v);
                    } else {
                        let _ = self.parse_value_by_type_ctx(reader, child, ctx)?;
                    }
                }
                UnityValue::Object(nested)
            }

            // Complex object types
            "ManagedReferencesRegistry" => {
                // `ManagedReferencesRegistry` can be extremely large. We keep parsing byte-accurate
                // by consuming the bytes according to the TypeTree, but we do not allocate/return a
                // full parsed value.
                ctx.has_managed_registry = true;
                let mut dummy = PPtrScanResult::default();
                self.scan_value(reader, node, &mut dummy)?;
                UnityValue::Null
            }
            _ => {
                if !node.children.is_empty() {
                    let mut nested_props = IndexMap::new();
                    for child in &node.children {
                        if child.type_name == "ManagedReferencesRegistry" {
                            // UnityPy uses a `has_registry` guard to avoid parsing multiple registries.
                            // We skip registry nodes to keep parsing fast while remaining byte-accurate.
                            if ctx.has_managed_registry {
                                let mut dummy = PPtrScanResult::default();
                                self.scan_value(reader, child, &mut dummy)?;
                                continue;
                            }
                            ctx.has_managed_registry = true;
                            let mut dummy = PPtrScanResult::default();
                            self.scan_value(reader, child, &mut dummy)?;
                            continue;
                        }
                        if !child.name.is_empty() {
                            let child_value = self.parse_value_by_type_ctx(reader, child, ctx)?;
                            nested_props.insert(child.name.clone(), child_value);
                        } else {
                            let _ = self.parse_value_by_type_ctx(reader, child, ctx)?;
                        }
                    }
                    UnityValue::Object(nested_props)
                } else {
                    // Unknown type with no children, skip bytes if size is known
                    if node.byte_size > 0 {
                        let _data = reader.read_bytes(node.byte_size as usize)?;
                        UnityValue::Null
                    } else {
                        UnityValue::Null
                    }
                }
            }
        };

        // Unity aligns the stream after reading certain fields (meta flag 0x4000).
        // This is essential for correctly parsing TypeTree-based objects with packed booleans and
        // nested structs (e.g. StreamedResource / StreamingInfo).
        if node.is_aligned() {
            reader.align_to(4)?;
        }

        Ok(value)
    }

    /// Parse array from TypeTree node
    fn parse_array(
        &self,
        reader: &mut BinaryReader,
        node: &TypeTreeNode,
        ctx: &mut TypeTreeParseContext<'a>,
    ) -> Result<UnityValue> {
        // Find the Array child node
        let array_node = node
            .children
            .iter()
            .find(|child| child.type_name == "Array")
            .ok_or_else(|| BinaryError::invalid_data("Array node not found in array type"))?;

        // Read array size (first child is size)
        let size_i32 = reader.read_i32()?;
        if size_i32 < 0 {
            return Err(BinaryError::invalid_data(format!(
                "Negative array size: {}",
                size_i32
            )));
        }
        if let Some(size_node) = array_node.children.first() {
            if size_node.is_aligned() {
                reader.align_to(4)?;
            }
        }
        let size = size_i32 as usize;
        if size > Self::MAX_ARRAY_LEN {
            return Err(BinaryError::invalid_data(format!(
                "Array size too large: {}",
                size
            )));
        }

        let mut elements = Vec::with_capacity(size);

        // Find the element type (usually the second child of Array node)
        let element_node = array_node
            .children
            .get(1)
            .ok_or_else(|| BinaryError::invalid_data("Array element type not found"))?;

        // Fast-path: byte/bool arrays are extremely common and are a hot path for large objects.
        if element_node.children.is_empty() {
            let byte_order = reader.byte_order();
            match element_node.type_name.as_str() {
                "UInt8" | "char" => {
                    let bytes = reader.read_bytes(size)?;
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Bytes(bytes));
                }
                "SInt8" => {
                    let bytes = reader.read_bytes(size)?;
                    // Preserve signedness as encoded bytes; callers that care can reinterpret.
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Bytes(bytes));
                }
                "bool" => {
                    let bytes = reader.read_bytes(size)?;
                    let out = UnityValue::Array(
                        bytes
                            .into_iter()
                            .map(|b| UnityValue::Bool(b != 0))
                            .collect(),
                    );
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(out);
                }
                "SInt16" | "short" => {
                    let byte_len = size
                        .checked_mul(2)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(2) {
                        let raw: [u8; 2] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => i16::from_be_bytes(raw),
                            ByteOrder::Little => i16::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v as i64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "UInt16" | "unsigned short" => {
                    let byte_len = size
                        .checked_mul(2)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(2) {
                        let raw: [u8; 2] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => u16::from_be_bytes(raw),
                            ByteOrder::Little => u16::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v as i64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "SInt32" | "int" => {
                    let byte_len = size
                        .checked_mul(4)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(4) {
                        let raw: [u8; 4] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => i32::from_be_bytes(raw),
                            ByteOrder::Little => i32::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v as i64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "UInt32" | "unsigned int" | "Type*" => {
                    let byte_len = size
                        .checked_mul(4)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(4) {
                        let raw: [u8; 4] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => u32::from_be_bytes(raw),
                            ByteOrder::Little => u32::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v as i64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "SInt64" | "long long" => {
                    let byte_len = size
                        .checked_mul(8)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(8) {
                        let raw: [u8; 8] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => i64::from_be_bytes(raw),
                            ByteOrder::Little => i64::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "UInt64" | "unsigned long long" | "FileSize" => {
                    let byte_len = size
                        .checked_mul(8)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(8) {
                        let raw: [u8; 8] = chunk.try_into().expect("chunks_exact size");
                        let v = match byte_order {
                            ByteOrder::Big => u64::from_be_bytes(raw),
                            ByteOrder::Little => u64::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Integer(v as i64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "float" => {
                    let byte_len = size
                        .checked_mul(4)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(4) {
                        let raw: [u8; 4] = chunk.try_into().expect("chunks_exact size");
                        let bits = match byte_order {
                            ByteOrder::Big => u32::from_be_bytes(raw),
                            ByteOrder::Little => u32::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Float(f32::from_bits(bits) as f64));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                "double" => {
                    let byte_len = size
                        .checked_mul(8)
                        .ok_or_else(|| BinaryError::invalid_data("Array byte length overflow"))?;
                    let bytes = reader.read_bytes(byte_len)?;
                    let mut out = Vec::with_capacity(size);
                    for chunk in bytes.chunks_exact(8) {
                        let raw: [u8; 8] = chunk.try_into().expect("chunks_exact size");
                        let bits = match byte_order {
                            ByteOrder::Big => u64::from_be_bytes(raw),
                            ByteOrder::Little => u64::from_le_bytes(raw),
                        };
                        out.push(UnityValue::Float(f64::from_bits(bits)));
                    }
                    if array_node.is_aligned() {
                        reader.align_to(4)?;
                    }
                    return Ok(UnityValue::Array(out));
                }
                _ => {}
            }
        }

        for _ in 0..size {
            let element = self.parse_value_by_type_ctx(reader, element_node, ctx)?;
            elements.push(element);
        }
        if array_node.is_aligned() {
            reader.align_to(4)?;
        }

        Ok(UnityValue::Array(elements))
    }

    /// Serialize object data using the TypeTree structure
    pub fn serialize_object(&self, data: &IndexMap<String, UnityValue>) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();

        if let Some(root) = self.tree.nodes.first() {
            for child in &root.children {
                if !child.name.is_empty()
                    && let Some(value) = data.get(&child.name)
                {
                    self.serialize_value(&mut buffer, value, child)?;
                }
            }
        }

        Ok(buffer)
    }

    /// Serialize a single value based on TypeTree node type
    fn serialize_value(
        &self,
        buffer: &mut Vec<u8>,
        value: &UnityValue,
        node: &TypeTreeNode,
    ) -> Result<()> {
        match node.type_name.as_str() {
            "SInt8" | "char" => {
                if let UnityValue::Integer(val) = value {
                    buffer.push(*val as u8);
                    self.align_buffer(buffer, 4);
                }
            }
            "SInt16" | "short" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as i16).to_le_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            "SInt32" | "int" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as i32).to_le_bytes());
                }
            }
            "SInt64" | "long long" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            "UInt8" => {
                if let UnityValue::Integer(val) = value {
                    buffer.push(*val as u8);
                    self.align_buffer(buffer, 4);
                }
            }
            "UInt16" | "unsigned short" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u16).to_le_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            "UInt32" | "unsigned int" | "Type*" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u32).to_le_bytes());
                }
            }
            "UInt64" | "unsigned long long" | "FileSize" => {
                if let UnityValue::Integer(val) = value {
                    buffer.extend_from_slice(&(*val as u64).to_le_bytes());
                }
            }
            "float" => {
                if let UnityValue::Float(val) = value {
                    buffer.extend_from_slice(&(*val as f32).to_le_bytes());
                }
            }
            "double" => {
                if let UnityValue::Float(val) = value {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            "bool" => {
                if let UnityValue::Bool(val) = value {
                    buffer.push(if *val { 1 } else { 0 });
                    self.align_buffer(buffer, 4);
                }
            }
            "string" => {
                if let UnityValue::String(val) = value {
                    // Write string length
                    buffer.extend_from_slice(&(val.len() as u32).to_le_bytes());
                    // Write string data
                    buffer.extend_from_slice(val.as_bytes());
                    self.align_buffer(buffer, 4);
                }
            }
            _ if node.is_array() => {
                if let UnityValue::Array(elements) = value {
                    // Write array size
                    buffer.extend_from_slice(&(elements.len() as i32).to_le_bytes());

                    // Find element type
                    if let Some(array_node) = node.children.iter().find(|c| c.type_name == "Array")
                        && let Some(element_node) = array_node.children.get(1)
                    {
                        for element in elements {
                            self.serialize_value(buffer, element, element_node)?;
                        }
                    }
                }
            }
            _ => {
                // Complex object
                if let UnityValue::Object(obj) = value {
                    for child in &node.children {
                        if !child.name.is_empty()
                            && let Some(child_value) = obj.get(&child.name)
                        {
                            self.serialize_value(buffer, child_value, child)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Align buffer to specified boundary
    fn align_buffer(&self, buffer: &mut Vec<u8>, alignment: usize) {
        let remainder = buffer.len() % alignment;
        if remainder != 0 {
            let padding = alignment - remainder;
            buffer.resize(buffer.len() + padding, 0);
        }
    }

    /// Get the TypeTree being used
    pub fn tree(&self) -> &TypeTree {
        self.tree
    }

    /// Estimate serialized size
    pub fn estimate_size(&self, data: &IndexMap<String, UnityValue>) -> usize {
        let mut size = 0;

        if let Some(root) = self.tree.nodes.first() {
            for child in &root.children {
                if !child.name.is_empty()
                    && let Some(value) = data.get(&child.name)
                {
                    size += Self::estimate_value_size(value, child);
                }
            }
        }

        size
    }

    /// Estimate size of a single value
    fn estimate_value_size(value: &UnityValue, node: &TypeTreeNode) -> usize {
        match node.type_name.as_str() {
            "SInt8" | "UInt8" | "char" | "bool" => 4, // Including alignment
            "SInt16" | "UInt16" | "short" | "unsigned short" => 4, // Including alignment
            "SInt32" | "UInt32" | "int" | "unsigned int" | "float" | "Type*" => 4,
            "SInt64" | "UInt64" | "long long" | "unsigned long long" | "double" | "FileSize" => 8,
            "string" => {
                if let UnityValue::String(s) = value {
                    4 + s.len() + (4 - (s.len() % 4)) % 4 // Length + data + alignment
                } else {
                    4
                }
            }
            _ if node.is_array() => {
                if let UnityValue::Array(elements) = value {
                    let mut size = 4; // Array size
                    if let Some(array_node) = node.children.iter().find(|c| c.type_name == "Array")
                        && let Some(element_node) = array_node.children.get(1)
                    {
                        for element in elements {
                            size += Self::estimate_value_size(element, element_node);
                        }
                    }
                    size
                } else {
                    4
                }
            }
            _ => {
                // Complex object
                if let UnityValue::Object(obj) = value {
                    let mut size = 0;
                    for child in &node.children {
                        if !child.name.is_empty()
                            && let Some(child_value) = obj.get(&child.name)
                        {
                            size += Self::estimate_value_size(child_value, child);
                        }
                    }
                    size
                } else {
                    node.byte_size.max(0) as usize
                }
            }
        }
    }
}

fn resolve_ref_type_tree<'a>(
    referenced_object: &IndexMap<String, UnityValue>,
    ref_types: &'a [SerializedType],
) -> Option<&'a TypeTree> {
    let type_v = referenced_object.get("type")?;
    let type_obj = type_v.as_object()?;

    fn get_str_ci(map: &IndexMap<String, UnityValue>, keys: &[&str]) -> Option<String> {
        for key in keys {
            for (k, v) in map.iter() {
                if k.eq_ignore_ascii_case(key) {
                    if let UnityValue::String(s) = v {
                        return Some(s.clone());
                    }
                }
            }
        }
        None
    }

    let class = get_str_ci(type_obj, &["class", "m_ClassName"])?;
    if class.is_empty() {
        return None;
    }
    let ns = get_str_ci(type_obj, &["ns", "m_NameSpace"]).unwrap_or_default();
    let asm = get_str_ci(type_obj, &["asm", "m_AssemblyName"]).unwrap_or_default();

    ref_types.iter().find_map(|t| {
        if !t.class_name.is_empty()
            && t.class_name == class
            && t.namespace == ns
            && t.assembly_name == asm
            && !t.type_tree.is_empty()
        {
            Some(&t.type_tree)
        } else {
            None
        }
    })
}

fn resolve_ref_type_tree_triplet<'a>(
    class: &str,
    ns: &str,
    asm: &str,
    ref_types: &'a [SerializedType],
) -> Option<&'a TypeTree> {
    if class.is_empty() {
        return None;
    }
    ref_types.iter().find_map(|t| {
        if !t.class_name.is_empty()
            && t.class_name == class
            && t.namespace == ns
            && t.assembly_name == asm
            && !t.type_tree.is_empty()
        {
            Some(&t.type_tree)
        } else {
            None
        }
    })
}

fn referenced_type_triplet(
    referenced_object: &IndexMap<String, UnityValue>,
) -> Option<(String, String, String)> {
    let type_v = referenced_object.get("type")?;
    let type_obj = type_v.as_object()?;
    let class = type_obj.get("class")?.as_str()?.to_string();
    let ns = type_obj
        .get("ns")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let asm = type_obj
        .get("asm")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some((class, ns, asm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(type_name: &str, name: &str) -> TypeTreeNode {
        let mut n = TypeTreeNode::new();
        n.type_name = type_name.to_string();
        n.name = name.to_string();
        n
    }

    #[test]
    fn test_serializer_creation() {
        let tree = TypeTree::new();
        let serializer = TypeTreeSerializer::new(&tree);
        assert!(serializer.tree().is_empty());
    }

    #[test]
    fn test_buffer_alignment() {
        let tree = TypeTree::new();
        let serializer = TypeTreeSerializer::new(&tree);

        let mut buffer = vec![1, 2, 3]; // 3 bytes
        serializer.align_buffer(&mut buffer, 4);
        assert_eq!(buffer.len(), 4); // Should be padded to 4 bytes
        assert_eq!(buffer[3], 0); // Padding should be zero
    }

    #[test]
    fn typetree_array_alignment_honors_array_node_flag() {
        let mut tree = TypeTree::new();

        let mut root = node("TestObject", "TestObject");

        let mut vec_node = node("vector", "m_Data");
        let mut array_node = node("Array", "Array");
        array_node.meta_flags = 0x4000; // kAlignBytes
        array_node.children = vec![node("int", "size"), node("UInt8", "data")];
        vec_node.children = vec![array_node];

        let next_node = node("int", "m_Next");

        root.children = vec![vec_node, next_node];
        tree.nodes.push(root);

        let serializer = TypeTreeSerializer::new(&tree);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_le_bytes()); // array size
        bytes.push(0xAA); // one byte element
        bytes.extend_from_slice(&[0u8; 3]); // align to 4
        bytes.extend_from_slice(&0x11223344i32.to_le_bytes()); // next int

        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let props = serializer.parse_object(&mut reader).unwrap();
        assert_eq!(
            props.get("m_Next").and_then(|v| v.as_i64()),
            Some(0x11223344)
        );
    }

    #[test]
    fn typetree_scan_pptrs_honors_array_node_alignment() {
        let mut tree = TypeTree::new();

        let mut root = node("TestObject", "TestObject");

        let mut vec_node = node("vector", "m_Data");
        let mut array_node = node("Array", "Array");
        array_node.meta_flags = 0x4000; // kAlignBytes
        array_node.children = vec![node("int", "size"), node("UInt8", "data")];
        vec_node.children = vec![array_node];

        let next_node = node("int", "m_Next");
        root.children = vec![vec_node, next_node];
        tree.nodes.push(root);

        let serializer = TypeTreeSerializer::new(&tree);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.push(0xAA);
        bytes.extend_from_slice(&[0u8; 3]);
        bytes.extend_from_slice(&0x11223344i32.to_le_bytes());

        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let _ = serializer.scan_pptrs(&mut reader).unwrap();
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn typetree_char_reads_as_unsigned_byte() {
        let mut tree = TypeTree::new();
        let mut root = node("TestObject", "TestObject");

        let mut char_node = node("char", "m_Char");
        char_node.meta_flags = 0x4000;
        let next_node = node("int", "m_Next");

        root.children = vec![char_node, next_node];
        tree.nodes.push(root);

        let serializer = TypeTreeSerializer::new(&tree);

        let mut bytes = Vec::new();
        bytes.push(0xFF);
        bytes.extend_from_slice(&[0u8; 3]);
        bytes.extend_from_slice(&0x11223344i32.to_le_bytes());

        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let props = serializer.parse_object(&mut reader).unwrap();
        assert_eq!(props.get("m_Char").and_then(|v| v.as_i64()), Some(255));
        assert_eq!(
            props.get("m_Next").and_then(|v| v.as_i64()),
            Some(0x11223344)
        );
    }

    #[test]
    fn typetree_byte_arrays_are_emitted_as_bytes() {
        let mut tree = TypeTree::new();
        let mut root = node("TestObject", "TestObject");

        let mut vec_u8 = node("vector", "m_UInt8");
        let mut arr_u8 = node("Array", "Array");
        arr_u8.children = vec![node("int", "size"), node("UInt8", "data")];
        vec_u8.children = vec![arr_u8];

        let mut vec_char = node("vector", "m_Char");
        let mut arr_char = node("Array", "Array");
        arr_char.children = vec![node("int", "size"), node("char", "data")];
        vec_char.children = vec![arr_char];

        root.children = vec![vec_u8, vec_char];
        tree.nodes.push(root);

        let serializer = TypeTreeSerializer::new(&tree);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2i32.to_le_bytes());
        bytes.extend_from_slice(&[0xAB, 0xCD]);
        bytes.extend_from_slice(&3i32.to_le_bytes());
        bytes.extend_from_slice(&[0x41, 0x80, 0xFF]);

        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let props = serializer.parse_object(&mut reader).unwrap();

        assert_eq!(
            props.get("m_UInt8").and_then(|v| v.as_bytes()),
            Some(&[0xAB, 0xCD][..])
        );
        assert_eq!(
            props.get("m_Char").and_then(|v| v.as_bytes()),
            Some(&[0x41, 0x80, 0xFF][..])
        );
    }
}
