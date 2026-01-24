use crate::Result;
use crate::binary_writer::BinaryWriter;
use crate::typetree::context::TypeTreeWriteContext;
use crate::typetree::primitives::write_primitive;
use crate::typetree::referenced_object::write_referenced_object;
use indexmap::IndexMap;
use unity_asset_binary::asset::SerializedType;
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};
use unity_asset_core::{UnityAssetError, UnityValue};

#[derive(Debug, Clone, Copy)]
pub struct TypeTreeWriteOptions {
    pub allow_missing_fields: bool,
}

impl Default for TypeTreeWriteOptions {
    fn default() -> Self {
        Self {
            allow_missing_fields: false,
        }
    }
}

/// A TypeTree-driven writer, targeting UnityPy's `TypeTreeHelper.write_value` behavior.
pub struct TypeTreeWriter<'a> {
    tree: &'a TypeTree,
    ref_types: Option<&'a [SerializedType]>,
}

impl<'a> TypeTreeWriter<'a> {
    pub fn new(tree: &'a TypeTree) -> Self {
        Self {
            tree,
            ref_types: None,
        }
    }

    pub fn with_ref_types(tree: &'a TypeTree, ref_types: &'a [SerializedType]) -> Self {
        Self {
            tree,
            ref_types: Some(ref_types),
        }
    }

    pub fn tree(&self) -> &'a TypeTree {
        self.tree
    }

    /// Encode an object as a byte blob using the root node's children as the field list.
    pub fn write_object(
        &self,
        writer: &mut BinaryWriter,
        properties: &IndexMap<String, UnityValue>,
        options: TypeTreeWriteOptions,
    ) -> Result<()> {
        let root = self.tree.nodes.first().ok_or_else(|| {
            UnityAssetError::format("TypeTreeWriter requires a non-empty TypeTree")
        })?;

        let mut ctx = TypeTreeWriteContext {
            ref_types: self.ref_types,
            ..Default::default()
        };
        for child in &root.children {
            if child.name.is_empty() {
                return Err(UnityAssetError::format(
                    "TypeTree write encountered an unnamed child node; use write_object_with_original_bytes(...) to preserve template bytes",
                ));
            }
            let Some(v) = properties.get(&child.name) else {
                if options.allow_missing_fields {
                    continue;
                }
                return Err(UnityAssetError::format(format!(
                    "Missing field '{}' for TypeTree write",
                    child.name
                )));
            };
            write_value(writer, child, v, &mut ctx, options)?;
        }
        Ok(())
    }

    /// Encode an object as a byte blob, preserving any unknown/unnamed fields by copying their
    /// original byte slices from `original_bytes`.
    ///
    /// This is required for rare TypeTrees that contain unnamed child nodes (`m_Name == ""`).
    pub fn write_object_with_original_bytes(
        &self,
        writer: &mut BinaryWriter,
        properties: &IndexMap<String, UnityValue>,
        original_bytes: &[u8],
        options: TypeTreeWriteOptions,
    ) -> Result<()> {
        super::template::write_object_with_original_bytes(
            writer,
            self.tree,
            self.ref_types,
            properties,
            original_bytes,
            options,
        )
    }
}

pub(crate) fn write_value(
    writer: &mut BinaryWriter,
    node: &TypeTreeNode,
    value: &UnityValue,
    ctx: &mut TypeTreeWriteContext<'_>,
    options: TypeTreeWriteOptions,
) -> Result<()> {
    // UnityPy alignment: node meta flag, plus array child meta flag.
    let mut align = node.is_aligned();
    if node
        .children
        .iter()
        .any(|c| c.type_name == "Array" && c.is_aligned())
    {
        align = true;
    }

    if write_primitive(writer, node.type_name.as_str(), value)? {
        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    // Array-like layout: any child "Array" node means the field is a vector/array container.
    if node.children.iter().any(|c| c.type_name == "Array") {
        write_array(writer, node, value, ctx, options)?;
        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    if node.type_name == "pair" && node.children.len() == 2 {
        match value {
            UnityValue::Array(v) if v.len() == 2 => {
                write_value(writer, &node.children[0], &v[0], ctx, options)?;
                write_value(writer, &node.children[1], &v[1], ctx, options)?;
            }
            UnityValue::Object(map) => {
                let k0 = if node.children[0].name.is_empty() {
                    "first"
                } else {
                    node.children[0].name.as_str()
                };
                let k1 = if node.children[1].name.is_empty() {
                    "second"
                } else {
                    node.children[1].name.as_str()
                };

                let Some(v0) = map.get(k0) else {
                    if options.allow_missing_fields {
                        return Ok(());
                    }
                    return Err(UnityAssetError::format(format!(
                        "Missing pair field '{}' for TypeTree write",
                        k0
                    )));
                };
                let Some(v1) = map.get(k1) else {
                    if options.allow_missing_fields {
                        return Ok(());
                    }
                    return Err(UnityAssetError::format(format!(
                        "Missing pair field '{}' for TypeTree write",
                        k1
                    )));
                };

                write_value(writer, &node.children[0], v0, ctx, options)?;
                write_value(writer, &node.children[1], v1, ctx, options)?;
            }
            _ => {
                return Err(UnityAssetError::format(format!(
                    "TypeTree write type mismatch: expected pair as Array(len=2) or Object(first/second), got {:?}",
                    value
                )));
            }
        }
        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    // Unity `PPtr<T>`: allow a small amount of normalization to match UnityPy ergonomics.
    //
    // UnityPy writes PPtr values either from dicts with `m_FileID/m_PathID`, or from `PPtr` objects
    // whose attributes follow the same naming scheme. In this project, YAML uses `fileID/pathID`,
    // so we accept both spellings when writing binary TypeTrees.
    let is_pptr = node.type_name == "PPtr" || node.type_name.starts_with("PPtr<");
    if is_pptr {
        // Null PPtr shorthand.
        if matches!(value, UnityValue::Null) {
            for child in &node.children {
                if child.name.is_empty() {
                    return Err(UnityAssetError::format(
                        "TypeTree write encountered an unnamed child node; use write_object_with_original_bytes(...) to preserve template bytes",
                    ));
                }
                write_value(writer, child, &UnityValue::Integer(0), ctx, options)?;
            }
            if align {
                writer.align_stream(4);
            }
            return Ok(());
        }

        let obj = value.as_object().ok_or_else(|| {
            UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected object/null for PPtr, got {:?}",
                value
            ))
        })?;

        for child in &node.children {
            if child.name.is_empty() {
                return Err(UnityAssetError::format(
                    "TypeTree write encountered an unnamed child node; use write_object_with_original_bytes(...) to preserve template bytes",
                ));
            }

            let v = if is_pptr_file_id_field(&child.name) {
                pptr_get_field(obj, &child.name, &["m_FileID", "fileID"])
            } else if is_pptr_path_id_field(&child.name) {
                pptr_get_field(obj, &child.name, &["m_PathID", "pathID"])
            } else {
                obj.get(&child.name)
            };

            let Some(v) = v else {
                if options.allow_missing_fields {
                    continue;
                }
                return Err(UnityAssetError::format(format!(
                    "Missing field '{}' for TypeTree write (parent type '{}')",
                    child.name, node.type_name
                )));
            };
            write_value(writer, child, v, ctx, options)?;
        }

        if align {
            writer.align_stream(4);
        }
        return Ok(());
    }

    if node.type_name == "ReferencedObject" {
        let referenced = value.as_object().ok_or_else(|| {
            UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected object for ReferencedObject, got {:?}",
                value
            ))
        })?;
        write_referenced_object(referenced, writer, node, ctx, options)?;
        return Ok(());
    }

    // UnityPy behavior: skip extra ManagedReferencesRegistry nodes after the first.
    if node.type_name == "ManagedReferencesRegistry" {
        if ctx.has_managed_registry {
            return Ok(());
        }
        ctx.has_managed_registry = true;
    }

    // Complex object: write children in declared order.
    let obj = match value {
        UnityValue::Object(m) => m,
        _ => {
            return Err(UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected object for type '{}', got {:?}",
                node.type_name, value
            )));
        }
    };

    for child in &node.children {
        if child.name.is_empty() {
            return Err(UnityAssetError::format(
                "TypeTree write encountered an unnamed child node; use write_object_with_original_bytes(...) to preserve template bytes",
            ));
        }
        let Some(v) = obj.get(&child.name) else {
            if options.allow_missing_fields {
                continue;
            }
            return Err(UnityAssetError::format(format!(
                "Missing field '{}' for TypeTree write (parent type '{}')",
                child.name, node.type_name
            )));
        };
        write_value(writer, child, v, ctx, options)?;
    }

    if align {
        writer.align_stream(4);
    }
    Ok(())
}

fn write_array(
    writer: &mut BinaryWriter,
    node: &TypeTreeNode,
    value: &UnityValue,
    ctx: &mut TypeTreeWriteContext<'_>,
    options: TypeTreeWriteOptions,
) -> Result<()> {
    let array_node = node
        .children
        .iter()
        .find(|c| c.type_name == "Array")
        .ok_or_else(|| UnityAssetError::format("TypeTree array node missing 'Array' child"))?;

    // Unity TypeTree convention: Array children are [size, data].
    let elem_node = array_node.children.get(1).ok_or_else(|| {
        UnityAssetError::format("TypeTree array node missing element child at index 1")
    })?;

    // UnityPy-like optimization: treat byte-like arrays as a single bytes payload.
    //
    // In this project, `vector<UInt8/SInt8/char>` is parsed into `UnityValue::Bytes` for
    // performance. Accept both `Bytes` and `Array<Integer>` so callers can round-trip without
    // manual conversions.
    if matches!(elem_node.type_name.as_str(), "UInt8" | "SInt8" | "char") {
        match value {
            UnityValue::Bytes(bytes) => {
                let len_i32: i32 = bytes.len().try_into().map_err(|_| {
                    UnityAssetError::format(format!(
                        "Array too large for i32 length: {}",
                        bytes.len()
                    ))
                })?;
                writer.write_i32(len_i32);
                writer.write(bytes.as_slice());
                return Ok(());
            }
            UnityValue::Array(elements) => {
                let len_i32: i32 = elements.len().try_into().map_err(|_| {
                    UnityAssetError::format(format!(
                        "Array too large for i32 length: {}",
                        elements.len()
                    ))
                })?;
                writer.write_i32(len_i32);
                for e in elements {
                    write_value(writer, elem_node, e, ctx, options)?;
                }
                return Ok(());
            }
            _ => {
                return Err(UnityAssetError::format(format!(
                    "TypeTree write type mismatch: expected bytes/array for byte-like '{}', got {:?}",
                    node.name, value
                )));
            }
        }
    }

    let elements = match value {
        UnityValue::Array(v) => v,
        _ => {
            return Err(UnityAssetError::format(format!(
                "TypeTree write type mismatch: expected array for '{}', got {:?}",
                node.name, value
            )));
        }
    };

    let len_i32: i32 = elements.len().try_into().map_err(|_| {
        UnityAssetError::format(format!(
            "Array too large for i32 length: {}",
            elements.len()
        ))
    })?;
    writer.write_i32(len_i32);

    for e in elements {
        write_value(writer, elem_node, e, ctx, options)?;
    }

    // UnityPy aligns for arrays based on node/alignment meta flags; higher-level `write_value`
    // handles the final alignment. Keep element-level alignment inside `write_value`.
    Ok(())
}

fn is_pptr_file_id_field(name: &str) -> bool {
    name.eq_ignore_ascii_case("fileID") || name.eq_ignore_ascii_case("m_FileID")
}

fn is_pptr_path_id_field(name: &str) -> bool {
    name.eq_ignore_ascii_case("pathID") || name.eq_ignore_ascii_case("m_PathID")
}

fn pptr_get_field<'a>(
    obj: &'a IndexMap<String, UnityValue>,
    primary: &str,
    aliases: &[&str],
) -> Option<&'a UnityValue> {
    if let Some(v) = obj.get(primary) {
        return Some(v);
    }
    for alias in aliases {
        if let Some(v) = obj.get(*alias) {
            return Some(v);
        }
        if let Some((_, v)) = obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(alias)) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary_writer::{BinaryWriter, Endian};
    use unity_asset_binary::asset::SerializedType;
    use unity_asset_binary::reader::{BinaryReader, ByteOrder};
    use unity_asset_binary::typetree::TypeTreeSerializer;

    fn node(type_name: &str, name: &str) -> TypeTreeNode {
        TypeTreeNode {
            type_name: type_name.to_string(),
            name: name.to_string(),
            byte_size: -1,
            variable_count: 0,
            index: 0,
            type_flags: 0,
            version: 0,
            meta_flags: 0,
            level: 0,
            type_str_offset: 0,
            name_str_offset: 0,
            ref_type_hash: 0,
            children: Vec::new(),
        }
    }

    #[test]
    fn roundtrip_primitives_and_string() {
        let mut root = node("TestObject", "Base");
        root.children.push(node("int", "m_Int"));
        root.children.push(node("bool", "m_Bool"));
        root.children.push(node("string", "m_Name"));

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut props = IndexMap::new();
        props.insert("m_Int".to_string(), UnityValue::Integer(123));
        props.insert("m_Bool".to_string(), UnityValue::Bool(true));
        props.insert("m_Name".to_string(), UnityValue::String("abc".to_string()));

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        assert_eq!(parsed.get("m_Int"), Some(&UnityValue::Integer(123)));
        assert_eq!(parsed.get("m_Bool"), Some(&UnityValue::Bool(true)));
        assert_eq!(
            parsed.get("m_Name"),
            Some(&UnityValue::String("abc".to_string()))
        );
    }

    #[test]
    fn roundtrip_array_of_u8_as_unityvalue_array() {
        // Layout similar to `vector<UInt8>` / `Array` node conventions.
        let mut root = node("TestObject", "Base");

        let mut field = node("vector", "m_Data");
        let mut array = node("Array", "Array");
        array.children.push(node("int", "size"));
        array.children.push(node("UInt8", "data"));
        field.children.push(array);
        root.children.push(field);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut props = IndexMap::new();
        props.insert(
            "m_Data".to_string(),
            UnityValue::Array(vec![
                UnityValue::Integer(1),
                UnityValue::Integer(2),
                UnityValue::Integer(3),
            ]),
        );

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();

        assert_eq!(
            parsed.get("m_Data"),
            Some(&UnityValue::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn roundtrip_array_of_u8_as_unityvalue_bytes() {
        let mut root = node("TestObject", "Base");

        let mut field = node("vector", "m_Data");
        let mut array = node("Array", "Array");
        array.children.push(node("int", "size"));
        array.children.push(node("UInt8", "data"));
        field.children.push(array);
        root.children.push(field);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut props = IndexMap::new();
        props.insert("m_Data".to_string(), UnityValue::Bytes(vec![9, 8, 7]));

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        assert_eq!(
            parsed.get("m_Data"),
            Some(&UnityValue::Bytes(vec![9, 8, 7]))
        );
    }

    #[test]
    fn roundtrip_referenced_object_uses_ref_types_layout_when_available() {
        // Outer TypeTree with a ReferencedObject field.
        let mut outer_root = node("Outer", "Base");
        let mut ro = node("ReferencedObject", "m_Ref");

        let mut ro_type = node("ReferencedObjectType", "type");
        ro_type.children.push(node("string", "class"));
        ro_type.children.push(node("string", "ns"));
        ro_type.children.push(node("string", "asm"));

        let ro_data = node("ReferencedObjectData", "data");
        ro.children.push(ro_type);
        ro.children.push(ro_data);
        outer_root.children.push(ro);

        let mut outer_tree = TypeTree::new();
        outer_tree.add_node(outer_root);

        // Referenced payload TypeTree registered in ref_types.
        let mut payload_root = node("MyRefType", "Base");
        payload_root.children.push(node("int", "x"));
        let mut payload_tree = TypeTree::new();
        payload_tree.add_node(payload_root);

        let mut ref_type = SerializedType::new(0);
        ref_type.class_name = "C".to_string();
        ref_type.namespace = "N".to_string();
        ref_type.assembly_name = "A".to_string();
        ref_type.type_tree = payload_tree;

        let ref_types = vec![ref_type];

        let mut props = IndexMap::new();
        let mut type_obj = IndexMap::new();
        type_obj.insert("class".to_string(), UnityValue::String("C".to_string()));
        type_obj.insert("ns".to_string(), UnityValue::String("N".to_string()));
        type_obj.insert("asm".to_string(), UnityValue::String("A".to_string()));

        let mut data_obj = IndexMap::new();
        data_obj.insert("x".to_string(), UnityValue::Integer(7));

        let mut ref_obj = IndexMap::new();
        ref_obj.insert("type".to_string(), UnityValue::Object(type_obj));
        ref_obj.insert("data".to_string(), UnityValue::Object(data_obj));
        props.insert("m_Ref".to_string(), UnityValue::Object(ref_obj));

        let writer_impl = TypeTreeWriter::with_ref_types(&outer_tree, &ref_types);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&outer_tree);
        let parsed = serializer
            .parse_object_with_ref_types(&mut reader, &ref_types)
            .unwrap();

        let m_ref = parsed.get("m_Ref").and_then(|v| v.as_object()).unwrap();
        let data = m_ref.get("data").and_then(|v| v.as_object()).unwrap();
        assert_eq!(data.get("x"), Some(&UnityValue::Integer(7)));
    }

    #[test]
    fn roundtrip_pair_accepts_object_shape() {
        let mut root = node("TestObject", "Base");

        let mut pair = node("pair", "m_Pair");
        pair.children.push(node("string", "first"));
        pair.children.push(node("int", "second"));
        root.children.push(pair);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut pair_obj = IndexMap::new();
        pair_obj.insert("first".to_string(), UnityValue::String("hello".to_string()));
        pair_obj.insert("second".to_string(), UnityValue::Integer(123));

        let mut props = IndexMap::new();
        props.insert("m_Pair".to_string(), UnityValue::Object(pair_obj));

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();

        let pair_val = parsed.get("m_Pair").unwrap();
        let UnityValue::Array(arr) = pair_val else {
            panic!("expected m_Pair to parse as array, got {:?}", pair_val);
        };
        assert_eq!(arr.get(0).and_then(|v| v.as_str()), Some("hello"));
        assert_eq!(arr.get(1).and_then(|v| v.as_i64()), Some(123));
    }

    #[test]
    fn template_write_preserves_unnamed_root_children() {
        let mut root = node("TestObject", "Base");
        root.children.push(node("int", "m_A"));
        root.children.push(node("int", ""));
        root.children.push(node("int", "m_B"));

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut original_w = BinaryWriter::new(Endian::Little);
        original_w.write_i32(1);
        original_w.write_i32(0x11223344);
        original_w.write_i32(2);
        let original_bytes = original_w.into_bytes();

        let mut props = IndexMap::new();
        props.insert("m_A".to_string(), UnityValue::Integer(10));
        props.insert("m_B".to_string(), UnityValue::Integer(20));

        let writer_impl = TypeTreeWriter::new(&tree);

        // Non-template mode must fail instead of silently dropping bytes.
        let mut out = BinaryWriter::new(Endian::Little);
        assert!(
            writer_impl
                .write_object(&mut out, &props, TypeTreeWriteOptions::default())
                .is_err()
        );

        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object_with_original_bytes(
                &mut out,
                &props,
                original_bytes.as_slice(),
                TypeTreeWriteOptions::default(),
            )
            .unwrap();

        assert_eq!(&out.bytes()[4..8], &original_bytes[4..8]);

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        assert_eq!(parsed.get("m_A"), Some(&UnityValue::Integer(10)));
        assert_eq!(parsed.get("m_B"), Some(&UnityValue::Integer(20)));
    }

    #[test]
    fn template_write_preserves_unnamed_nested_children() {
        let mut root = node("TestObject", "Base");

        let mut foo = node("Foo", "m_Foo");
        foo.children.push(node("int", "x"));
        foo.children.push(node("int", ""));
        foo.children.push(node("int", "y"));
        root.children.push(foo);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut original_w = BinaryWriter::new(Endian::Little);
        original_w.write_i32(1);
        original_w.write_i32(77);
        original_w.write_i32(2);
        let original_bytes = original_w.into_bytes();

        let mut foo_obj = IndexMap::new();
        foo_obj.insert("x".to_string(), UnityValue::Integer(10));
        foo_obj.insert("y".to_string(), UnityValue::Integer(20));
        let mut props = IndexMap::new();
        props.insert("m_Foo".to_string(), UnityValue::Object(foo_obj));

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object_with_original_bytes(
                &mut out,
                &props,
                original_bytes.as_slice(),
                TypeTreeWriteOptions::default(),
            )
            .unwrap();

        assert_eq!(&out.bytes()[4..8], &original_bytes[4..8]);

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        let foo_parsed = parsed.get("m_Foo").and_then(|v| v.as_object()).unwrap();
        assert_eq!(foo_parsed.get("x"), Some(&UnityValue::Integer(10)));
        assert_eq!(foo_parsed.get("y"), Some(&UnityValue::Integer(20)));
    }

    #[test]
    fn write_pptr_accepts_fileid_pathid_aliases() {
        let mut root = node("TestObject", "Base");

        let mut tex = node("PPtr<Texture2D>", "m_Tex");
        tex.children.push(node("int", "m_FileID"));
        tex.children.push(node("long long", "m_PathID"));
        root.children.push(tex);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut pptr = IndexMap::new();
        pptr.insert("fileID".to_string(), UnityValue::Integer(0));
        pptr.insert("pathID".to_string(), UnityValue::Integer(1234));

        let mut props = IndexMap::new();
        props.insert("m_Tex".to_string(), UnityValue::Object(pptr));

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        let tex = parsed.get("m_Tex").and_then(|v| v.as_object()).unwrap();
        assert_eq!(tex.get("m_FileID"), Some(&UnityValue::Integer(0)));
        assert_eq!(tex.get("m_PathID"), Some(&UnityValue::Integer(1234)));
    }

    #[test]
    fn write_pptr_accepts_null_as_zero_ptr() {
        let mut root = node("TestObject", "Base");

        let mut tex = node("PPtr<Texture2D>", "m_Tex");
        tex.children.push(node("int", "m_FileID"));
        tex.children.push(node("long long", "m_PathID"));
        root.children.push(tex);

        let mut tree = TypeTree::new();
        tree.add_node(root);

        let mut props = IndexMap::new();
        props.insert("m_Tex".to_string(), UnityValue::Null);

        let writer_impl = TypeTreeWriter::new(&tree);
        let mut out = BinaryWriter::new(Endian::Little);
        writer_impl
            .write_object(&mut out, &props, TypeTreeWriteOptions::default())
            .unwrap();

        let mut reader = BinaryReader::new(out.bytes(), ByteOrder::Little);
        let serializer = TypeTreeSerializer::new(&tree);
        let parsed = serializer.parse_object(&mut reader).unwrap();
        let tex = parsed.get("m_Tex").and_then(|v| v.as_object()).unwrap();
        assert_eq!(tex.get("m_FileID"), Some(&UnityValue::Integer(0)));
        assert_eq!(tex.get("m_PathID"), Some(&UnityValue::Integer(0)));
    }
}
