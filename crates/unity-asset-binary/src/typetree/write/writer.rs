use indexmap::IndexMap;
use unity_asset_core::{AssetLoadBudget, UnityValue};

use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::{
    PPtrLayout, PairLayout, PrimitiveKind, ReferencedObjectLayout, SchemaNode, SemanticKind,
    SemanticLayout, SequenceLayout, TypeTreeSchema, TypeTreeTraversalContext,
    TypeTreeTraversalStats, TypeTreeWriteError, TypeTreeWriteResult as Result,
};

use super::output::{TypeTreeOutput, TypeTreeSink, TypeTreeValidation};
use super::primitives::{
    checked_i32_length, expect_pair, summarize_value, usize_to_u64, write_primitive,
    write_primitive_run,
};

pub(crate) fn encode_object(
    schema: &TypeTreeSchema,
    properties: &IndexMap<String, UnityValue>,
    byte_order: ByteOrder,
    budget: &mut AssetLoadBudget,
) -> Result<(Vec<u8>, TypeTreeTraversalStats)> {
    let mut output = TypeTreeOutput::new(byte_order, budget);
    write_object_node(
        schema,
        schema.root(),
        properties,
        byte_order,
        &mut output,
        TypeTreeTraversalContext::root(),
        0,
    )?;
    Ok(output.finish())
}

/// Validates a canonical root object without allocating an output buffer.
pub(crate) fn validate_object(
    schema: &TypeTreeSchema,
    properties: &IndexMap<String, UnityValue>,
    byte_order: ByteOrder,
    budget: &mut AssetLoadBudget,
) -> Result<TypeTreeTraversalStats> {
    let mut validation = TypeTreeValidation::for_encoding(budget);
    write_object_node(
        schema,
        schema.root(),
        properties,
        byte_order,
        &mut validation,
        TypeTreeTraversalContext::root(),
        0,
    )?;
    Ok(validation.finish())
}

/// Validates one canonical value through the same traversal used by the materializing writer.
pub(crate) fn validate_value(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    budget: &mut AssetLoadBudget,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TypeTreeTraversalStats> {
    validate_value_with_sink(
        schema,
        node,
        value,
        byte_order,
        TypeTreeValidation::for_encoding(budget),
        context,
        depth,
    )
}

/// Validates one rewrite value while allowing unnamed fields to remain in the template.
pub(crate) fn validate_rewrite_value(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    budget: &mut AssetLoadBudget,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TypeTreeTraversalStats> {
    validate_value_with_sink(
        schema,
        node,
        value,
        byte_order,
        TypeTreeValidation::for_rewrite(budget),
        context,
        depth,
    )
}

fn validate_value_with_sink(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    mut validation: TypeTreeValidation<'_>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TypeTreeTraversalStats> {
    write_value(
        schema,
        node,
        value,
        byte_order,
        &mut validation,
        context,
        depth,
    )?;
    Ok(validation.finish())
}

/// Writes one canonical schema node into a caller-owned, budgeted output.
///
/// Template rewriting uses this adapter for every value that does not need byte preservation.
/// `byte_order` must match the byte order used to construct `output`.
pub(crate) fn write_value<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    output.enter_node(depth)?;
    write_value_body(schema, node, value, byte_order, output, context, depth)?;
    align_after_node(node, output)
}

fn write_value_body<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    match node.semantic_layout() {
        SemanticLayout::Scalar(kind) => write_primitive(output, kind, value, byte_order),
        SemanticLayout::String => write_string(output, value),
        SemanticLayout::TypelessData => write_sized_bytes(output, node, value),
        SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => write_sequence(
            schema, node, layout, value, byte_order, output, context, depth,
        ),
        SemanticLayout::Pair(layout) => write_pair(
            schema, node, layout, value, byte_order, output, context, depth,
        ),
        SemanticLayout::PPtr(layout) => write_pptr(
            schema, node, layout, value, byte_order, output, context, depth,
        ),
        SemanticLayout::ReferencedObject(layout) => write_referenced_object(
            schema, node, layout, value, byte_order, output, context, depth,
        ),
        SemanticLayout::ManagedPayload => Err(TypeTreeWriteError::invalid_value(format!(
            "Dynamic managed payload '{}' requires a ReferencedObject type dispatch",
            node.name()
        ))),
        SemanticLayout::ManagedRegistry | SemanticLayout::Record => {
            let object = expect_object(node, value)?;
            write_record_body(schema, node, object, byte_order, output, context, depth)
        }
        SemanticLayout::OpaqueFixed { byte_size } => {
            write_fixed_bytes(output, node, value, byte_size)
        }
    }
}

fn write_object_node<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    properties: &IndexMap<String, UnityValue>,
    byte_order: ByteOrder,
    output: &mut S,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    if !matches!(
        node.kind(),
        SemanticKind::Record | SemanticKind::ManagedRegistry
    ) {
        return Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree object root must be a record, got {:?}",
            node.kind()
        )));
    }

    output.enter_node(depth)?;
    write_record_body(schema, node, properties, byte_order, output, context, depth)?;
    align_after_node(node, output)
}

fn write_record_body<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    object: &IndexMap<String, UnityValue>,
    byte_order: ByteOrder,
    output: &mut S,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    validate_object_shape(node, object, context)?;
    output.consume_members(usize_to_u64(
        node.child_count(),
        "TypeTree record child count",
    )?)?;
    let child_depth = child_depth(depth)?;
    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        if child.name().is_empty() && output.skips_template_preserved_unnamed_fields() {
            continue;
        }

        let value = required_property(object, child, node)?;
        write_value(
            schema,
            child,
            value,
            byte_order,
            output,
            child_context,
            child_depth,
        )?;
    }
    Ok(())
}

fn write_sequence<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    layout: SequenceLayout<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    let element = layout.element();
    let child_depth = child_depth(depth)?;

    if let UnityValue::Bytes(bytes) = value {
        if !matches!(
            layout.bulk_primitive(),
            Some(PrimitiveKind::I8 | PrimitiveKind::U8)
        ) {
            return Err(TypeTreeWriteError::invalid_value(format!(
                "TypeTree Bytes value is only valid for I8/U8 sequences, '{}' contains {:?}",
                node.name(),
                element.kind()
            )));
        }

        let length = checked_i32_length(bytes.len(), "TypeTree byte sequence")?;
        let members = usize_to_u64(bytes.len(), "TypeTree byte sequence length")?;
        output.consume_members(members)?;
        output.write_i32(length)?;
        if members != 0 {
            output.enter_nodes(child_depth, members)?;
            output.write_bulk_bytes(bytes)?;
        }
        return Ok(());
    }

    let values = match value {
        UnityValue::Array(values) => values,
        _ => {
            return Err(TypeTreeWriteError::invalid_value(format!(
                "TypeTree write expected an array for sequence '{}', got {}",
                node.name(),
                summarize_value(value)
            )));
        }
    };
    let length = checked_i32_length(values.len(), "TypeTree sequence")?;
    let members = usize_to_u64(values.len(), "TypeTree sequence length")?;
    output.consume_members(members)?;
    output.write_i32(length)?;

    if let Some(kind) = layout.bulk_primitive() {
        if members != 0 {
            output.enter_nodes(child_depth, members)?;
            write_primitive_run(output, kind, values, byte_order)?;
        }
        return Ok(());
    }

    for value in values {
        write_value(
            schema,
            element,
            value,
            byte_order,
            output,
            context,
            child_depth,
        )?;
    }
    Ok(())
}

fn write_pair<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    layout: PairLayout<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    let values = expect_pair(node, value)?;
    output.consume_members(2)?;
    let child_depth = child_depth(depth)?;
    write_value(
        schema,
        layout.first(),
        &values[0],
        byte_order,
        output,
        context,
        child_depth,
    )?;
    write_value(
        schema,
        layout.second(),
        &values[1],
        byte_order,
        output,
        context,
        child_depth,
    )
}

fn write_pptr<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    layout: PPtrLayout<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    let object = match value {
        UnityValue::Null => None,
        UnityValue::Object(object) => {
            validate_object_shape(node, object, context)?;
            Some(object)
        }
        _ => {
            return Err(TypeTreeWriteError::invalid_value(format!(
                "TypeTree PPtr '{}' requires an Object or Null, got {}",
                node.name(),
                summarize_value(value)
            )));
        }
    };
    output.consume_members(usize_to_u64(
        node.child_count(),
        "TypeTree PPtr child count",
    )?)?;
    let child_depth = child_depth(depth)?;
    let zero = UnityValue::Integer(0);

    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let field = match object {
            Some(object) => required_property(object, child, node)?,
            None if child == layout.file_child() || child == layout.path_child() => &zero,
            None => {
                return Err(TypeTreeWriteError::invalid_value(format!(
                    "Null PPtr '{}' cannot synthesize extra field '{}'",
                    node.name(),
                    child.name()
                )));
            }
        };

        if child == layout.file_child() {
            validate_pptr_file_id(node, field)?;
        } else if child == layout.path_child() {
            validate_pptr_path_id(node, field)?;
        }
        write_value(
            schema,
            child,
            field,
            byte_order,
            output,
            child_context,
            child_depth,
        )?;
    }

    Ok(())
}

pub(super) fn validate_pptr_file_id(node: SchemaNode<'_>, value: &UnityValue) -> Result<()> {
    let valid = value
        .as_i64()
        .is_some_and(|value| i32::try_from(value).is_ok());
    if valid {
        return Ok(());
    }
    Err(TypeTreeWriteError::invalid_value(format!(
        "PPtr '{}' file ID must fit in i32, got {}",
        node.name(),
        summarize_value(value)
    )))
}

pub(super) fn validate_pptr_path_id(node: SchemaNode<'_>, value: &UnityValue) -> Result<()> {
    let valid = value.as_i64().is_some();
    if valid {
        return Ok(());
    }
    Err(TypeTreeWriteError::invalid_value(format!(
        "PPtr '{}' path ID must fit in i64, got {}",
        node.name(),
        summarize_value(value)
    )))
}

fn write_referenced_object<S: TypeTreeSink + ?Sized>(
    schema: &TypeTreeSchema,
    node: SchemaNode<'_>,
    layout: ReferencedObjectLayout<'_>,
    value: &UnityValue,
    byte_order: ByteOrder,
    output: &mut S,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<()> {
    let object = expect_object(node, value)?;
    let type_value = required_property(object, layout.type_node(), node)?;
    let type_object = expect_object(layout.type_node(), type_value)?;
    let class_name = required_string(type_object, layout.class_field(), layout.type_node())?;
    let namespace = required_string(type_object, layout.namespace_field(), layout.type_node())?;
    let assembly_name = required_string(type_object, layout.assembly_field(), layout.type_node())?;
    let omitted_payload = class_name.is_empty().then(|| layout.payload().node());
    validate_object_shape_except(node, object, context, omitted_payload)?;
    output.consume_members(usize_to_u64(
        node.child_count(),
        "ReferencedObject child count",
    )?)?;
    let child_depth = child_depth(depth)?;

    for child in node.children() {
        let child_context = context.descend(node, child).ok_or_else(|| {
            TypeTreeWriteError::invalid_value(format!(
                "ReferencedObject '{}' unexpectedly suppressed child '{}'",
                node.name(),
                child.name()
            ))
        })?;
        if layout.is_type_node(child) {
            write_value(
                schema,
                child,
                type_value,
                byte_order,
                output,
                child_context,
                child_depth,
            )?;
            continue;
        }
        if layout.is_payload(child) {
            if class_name.is_empty() {
                output.enter_node(child_depth)?;
                continue;
            }

            let target = schema
                .resolve_managed_root(class_name, namespace, assembly_name)
                .or_else(|| layout.payload().fallback())
                .ok_or_else(|| {
                    TypeTreeWriteError::invalid_value(format!(
                        "Managed type discriminator has no schema or writable fallback (class_bytes={}, namespace_bytes={}, assembly_bytes={})",
                        class_name.len(),
                        namespace.len(),
                        assembly_name.len()
                    ))
                })?;
            let payload = required_property(object, child, node)?;
            write_value(
                schema,
                target,
                payload,
                byte_order,
                output,
                child_context,
                child_depth,
            )?;
            continue;
        }

        let child_value = required_property(object, child, node)?;
        write_value(
            schema,
            child,
            child_value,
            byte_order,
            output,
            child_context,
            child_depth,
        )?;
    }
    Ok(())
}

pub(super) fn validate_object_shape(
    node: SchemaNode<'_>,
    object: &IndexMap<String, UnityValue>,
    context: TypeTreeTraversalContext,
) -> Result<()> {
    validate_object_shape_except(node, object, context, None)
}

fn validate_object_shape_except(
    node: SchemaNode<'_>,
    object: &IndexMap<String, UnityValue>,
    mut context: TypeTreeTraversalContext,
    omitted_child: Option<SchemaNode<'_>>,
) -> Result<()> {
    let mut expected_fields = 0_usize;
    let mut missing = false;
    for child in node.children() {
        if Some(child) == omitted_child
            || context.descend(node, child).is_none()
            || child.name().is_empty()
        {
            continue;
        }
        expected_fields = expected_fields.checked_add(1).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree writable field count overflow")
        })?;
        missing |= !object.contains_key(child.name());
    }
    if missing || object.len() != expected_fields {
        return Err(TypeTreeWriteError::Shape {
            expected_fields,
            actual_fields: object.len(),
        });
    }
    Ok(())
}

fn write_string<S: TypeTreeSink + ?Sized>(output: &mut S, value: &UnityValue) -> Result<()> {
    let value = match value {
        UnityValue::String(value) => value,
        _ => {
            return Err(TypeTreeWriteError::invalid_value(format!(
                "TypeTree string requires a String value, got {}",
                summarize_value(value)
            )));
        }
    };
    if value.len() > BinaryReader::DEFAULT_MAX_STRING_LEN {
        return Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree string length {} exceeds reader limit {}",
            value.len(),
            BinaryReader::DEFAULT_MAX_STRING_LEN
        )));
    }
    let length = checked_i32_length(value.len(), "TypeTree string")?;
    output.write_i32(length)?;
    output.write_bytes(value.as_bytes())
}

fn write_sized_bytes<S: TypeTreeSink + ?Sized>(
    output: &mut S,
    node: SchemaNode<'_>,
    value: &UnityValue,
) -> Result<()> {
    let bytes = expect_bytes(node, value)?;
    let length = checked_i32_length(bytes.len(), "TypeTree byte payload")?;
    output.write_i32(length)?;
    output.write_bytes(bytes)
}

fn write_fixed_bytes<S: TypeTreeSink + ?Sized>(
    output: &mut S,
    node: SchemaNode<'_>,
    value: &UnityValue,
    byte_size: u64,
) -> Result<()> {
    let bytes = expect_bytes(node, value)?;
    let expected = usize::try_from(byte_size).map_err(|_| {
        TypeTreeWriteError::invalid_value(format!(
            "Fixed TypeTree extent for '{}' does not fit usize: {byte_size}",
            node.name()
        ))
    })?;
    if bytes.len() != expected {
        return Err(TypeTreeWriteError::invalid_value(format!(
            "Fixed TypeTree node '{}' requires {expected} bytes, got {}",
            node.name(),
            bytes.len()
        )));
    }
    output.write_bytes(bytes)
}

fn required_property<'value>(
    object: &'value IndexMap<String, UnityValue>,
    child: SchemaNode<'_>,
    parent: SchemaNode<'_>,
) -> Result<&'value UnityValue> {
    if child.name().is_empty() {
        return Err(TypeTreeWriteError::invalid_value(format!(
            "Fresh TypeTree encoding cannot represent an unnamed child of '{}'",
            parent.name()
        )));
    }
    object.get(child.name()).ok_or_else(|| {
        TypeTreeWriteError::invalid_value(format!(
            "Missing required field '{}' while encoding '{}'",
            child.name(),
            parent.name()
        ))
    })
}

fn required_string<'value>(
    object: &'value IndexMap<String, UnityValue>,
    field: SchemaNode<'_>,
    parent: SchemaNode<'_>,
) -> Result<&'value str> {
    let value = required_property(object, field, parent)?;
    match value {
        UnityValue::String(value) => Ok(value),
        _ => Err(TypeTreeWriteError::invalid_value(format!(
            "Managed type field '{}' requires a String, got {}",
            field.name(),
            summarize_value(value)
        ))),
    }
}

fn expect_object<'value>(
    node: SchemaNode<'_>,
    value: &'value UnityValue,
) -> Result<&'value IndexMap<String, UnityValue>> {
    match value {
        UnityValue::Object(value) => Ok(value),
        _ => Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree node '{}' ({:?}) requires an Object, got {}",
            node.name(),
            node.kind(),
            summarize_value(value)
        ))),
    }
}

fn expect_bytes<'value>(node: SchemaNode<'_>, value: &'value UnityValue) -> Result<&'value [u8]> {
    match value {
        UnityValue::Bytes(value) => Ok(value),
        _ => Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree node '{}' ({:?}) requires Bytes, got {}",
            node.name(),
            node.kind(),
            summarize_value(value)
        ))),
    }
}

fn align_after_node<S: TypeTreeSink + ?Sized>(node: SchemaNode<'_>, output: &mut S) -> Result<()> {
    if node.align_after() {
        output.align_to(4)?;
    }
    Ok(())
}

fn child_depth(depth: u32) -> Result<u32> {
    depth
        .checked_add(1)
        .ok_or_else(|| TypeTreeWriteError::invalid_value("TypeTree write depth overflow"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{node, sequence};
    use super::*;
    use crate::asset::SerializedType;
    use crate::typetree::{TypeTree, TypeTreeNode};

    const ALIGN_BYTES: i32 = 0x4000;

    fn aligned(mut node: TypeTreeNode) -> TypeTreeNode {
        node.meta_flags |= ALIGN_BYTES;
        node
    }

    fn record(name: &str, children: Vec<TypeTreeNode>) -> TypeTreeNode {
        let mut root = node("TestObject", name);
        root.children = children;
        root
    }

    fn compile(root: TypeTreeNode, ref_types: &[SerializedType]) -> TypeTreeSchema {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        TypeTreeSchema::compile(&tree, ref_types, &mut AssetLoadBudget::default()).unwrap()
    }

    fn encode(
        schema: &TypeTreeSchema,
        properties: IndexMap<String, UnityValue>,
        byte_order: ByteOrder,
    ) -> Result<Vec<u8>> {
        let mut budget = AssetLoadBudget::default();
        encode_object(schema, &properties, byte_order, &mut budget).map(|(bytes, _)| bytes)
    }

    #[test]
    fn primitive_endian_and_root_alignment_are_schema_driven() {
        let schema = compile(
            aligned(record("Base", vec![node("UInt16", "m_Value")])),
            &[],
        );
        let properties = IndexMap::from([("m_Value".to_owned(), UnityValue::Integer(0x0102))]);

        assert_eq!(
            encode(&schema, properties.clone(), ByteOrder::Little).unwrap(),
            [0x02, 0x01, 0, 0]
        );
        assert_eq!(
            encode(&schema, properties, ByteOrder::Big).unwrap(),
            [0x01, 0x02, 0, 0]
        );
    }

    #[test]
    fn fresh_record_encoding_rejects_unrepresentable_extra_fields() {
        let schema = compile(record("Base", vec![node("int", "m_Value")]), &[]);
        let properties = IndexMap::from([
            ("m_Value".to_owned(), UnityValue::Integer(1)),
            ("m_Extra".to_owned(), UnityValue::Integer(2)),
        ]);

        assert!(matches!(
            encode(&schema, properties, ByteOrder::Little),
            Err(TypeTreeWriteError::Shape {
                expected_fields: 1,
                actual_fields: 2,
            })
        ));

        let wrong_field = IndexMap::from([("m_Other".to_owned(), UnityValue::Integer(1))]);
        assert!(matches!(
            encode(&schema, wrong_field, ByteOrder::Little),
            Err(TypeTreeWriteError::Shape {
                expected_fields: 1,
                actual_fields: 1,
            })
        ));
    }

    #[test]
    fn validation_reuses_writer_rules_without_allocating_wire_bytes() {
        let schema = compile(record("Base", vec![node("string", "m_Name")]), &[]);
        let value = UnityValue::Object(IndexMap::from([(
            "m_Name".to_owned(),
            UnityValue::String("validated".to_owned()),
        )]));
        let mut budget = AssetLoadBudget::default();

        let stats = validate_value(
            &schema,
            schema.root(),
            &value,
            ByteOrder::Little,
            &mut budget,
            TypeTreeTraversalContext::root(),
            0,
        )
        .unwrap();

        assert_eq!(stats.owned_bytes, 0);
        assert!(stats.wire_bytes > 0);
        assert_eq!(budget.usage().bytes, 0);
        assert!(budget.usage().entries > 0);
        assert!(budget.usage().members > 0);
    }

    #[test]
    fn rejected_values_use_bounded_shape_diagnostics() {
        let schema = compile(record("Base", vec![node("string", "m_Name")]), &[]);
        let value = UnityValue::Object(IndexMap::from([(
            "m_Name".to_owned(),
            UnityValue::Bytes(vec![0; 64 * 1024]),
        )]));
        let mut budget = AssetLoadBudget::default();

        let error = validate_value(
            &schema,
            schema.root(),
            &value,
            ByteOrder::Little,
            &mut budget,
            TypeTreeTraversalContext::root(),
            0,
        )
        .expect_err("bytes are not a writable TypeTree string");
        let diagnostic = error.to_string();

        assert!(diagnostic.contains("Bytes(len=65536)"));
        assert!(diagnostic.len() < 256, "unbounded diagnostic: {diagnostic}");
    }

    #[test]
    fn rewrite_validation_allows_template_preserved_unnamed_record_fields() {
        let schema = compile(
            record("Base", vec![node("int", ""), node("int", "m_Named")]),
            &[],
        );
        let value = UnityValue::Object(IndexMap::from([(
            "m_Named".to_owned(),
            UnityValue::Integer(7),
        )]));
        let mut budget = AssetLoadBudget::default();

        validate_rewrite_value(
            &schema,
            schema.root(),
            &value,
            ByteOrder::Little,
            &mut budget,
            TypeTreeTraversalContext::root(),
            0,
        )
        .unwrap();
        assert!(
            encode(
                &schema,
                match value {
                    UnityValue::Object(properties) => properties,
                    _ => unreachable!("test constructs an object"),
                },
                ByteOrder::Little,
            )
            .is_err()
        );
    }

    #[test]
    fn unsigned_u64_is_lossless_and_narrow_integers_are_checked() {
        let wide = compile(record("Base", vec![node("UInt64", "m_Value")]), &[]);
        let properties = IndexMap::from([("m_Value".to_owned(), UnityValue::Unsigned(u64::MAX))]);
        assert_eq!(
            encode(&wide, properties, ByteOrder::Little).unwrap(),
            u64::MAX.to_le_bytes()
        );

        let narrow = compile(record("Base", vec![node("UInt8", "m_Value")]), &[]);
        for value in [UnityValue::Integer(-1), UnityValue::Integer(256)] {
            let properties = IndexMap::from([("m_Value".to_owned(), value)]);
            assert!(encode(&narrow, properties, ByteOrder::Little).is_err());
        }
    }

    #[test]
    fn string_rejects_payload_above_reader_limit() {
        let schema = compile(record("Base", vec![node("string", "m_Value")]), &[]);
        let value = "x".repeat(BinaryReader::DEFAULT_MAX_STRING_LEN + 1);
        let properties = IndexMap::from([("m_Value".to_owned(), UnityValue::String(value))]);

        assert!(encode(&schema, properties, ByteOrder::Little).is_err());
    }

    #[test]
    fn pair_requires_a_two_element_array_and_honors_alignment() {
        let mut pair = aligned(node("pair", "m_Pair"));
        pair.children = vec![node("UInt8", "first"), node("UInt16", "second")];
        let schema = compile(record("Base", vec![pair]), &[]);
        let properties = IndexMap::from([(
            "m_Pair".to_owned(),
            UnityValue::Array(vec![UnityValue::Integer(1), UnityValue::Integer(0x0203)]),
        )]);
        assert_eq!(
            encode(&schema, properties, ByteOrder::Little).unwrap(),
            [1, 3, 2, 0]
        );

        let invalid = IndexMap::from([(
            "m_Pair".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("first".to_owned(), UnityValue::Integer(1)),
                ("second".to_owned(), UnityValue::Integer(2)),
            ])),
        )]);
        assert!(encode(&schema, invalid, ByteOrder::Little).is_err());
    }

    #[test]
    fn map_writes_pair_entries_in_input_order() {
        let mut pair = node("pair", "data");
        pair.children = vec![node("UInt8", "first"), node("UInt8", "second")];
        let mut array = node("Array", "Array");
        array.children = vec![node("int", "size"), pair];
        let mut map = node("map", "m_Map");
        map.children.push(array);
        let schema = compile(record("Base", vec![map]), &[]);
        let properties = IndexMap::from([(
            "m_Map".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Array(vec![UnityValue::Integer(1), UnityValue::Integer(10)]),
                UnityValue::Array(vec![UnityValue::Integer(2), UnityValue::Integer(20)]),
            ]),
        )]);

        assert_eq!(
            encode(&schema, properties, ByteOrder::Little).unwrap(),
            [2, 0, 0, 0, 1, 10, 2, 20]
        );
    }

    #[test]
    fn pptr_uses_exact_schema_fields_and_null_writes_zero_ids() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![node("int", "m_FileID"), node("long long", "m_PathID")];
        let schema = compile(record("Base", vec![pointer]), &[]);

        let null = IndexMap::from([("m_Texture".to_owned(), UnityValue::Null)]);
        assert_eq!(
            encode(&schema, null, ByteOrder::Little).unwrap(),
            [0_u8; 12]
        );

        let pointer = UnityValue::Object(IndexMap::from([
            ("m_FileID".to_owned(), UnityValue::Integer(1)),
            (
                "m_PathID".to_owned(),
                UnityValue::Integer(0x0102_0304_0506_0708),
            ),
        ]));
        let properties = IndexMap::from([("m_Texture".to_owned(), pointer)]);
        assert_eq!(
            encode(&schema, properties, ByteOrder::Little).unwrap(),
            [1, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]
        );

        let aliases = UnityValue::Object(IndexMap::from([
            ("fileID".to_owned(), UnityValue::Integer(1)),
            ("pathID".to_owned(), UnityValue::Integer(2)),
        ]));
        let properties = IndexMap::from([("m_Texture".to_owned(), aliases)]);
        assert!(encode(&schema, properties, ByteOrder::Little).is_err());
    }

    #[test]
    fn pptr_writes_every_child_in_schema_order() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![
            node("long long", "m_PathID"),
            node("UInt8", "m_Tag"),
            node("int", "m_FileID"),
        ];
        let schema = compile(record("Base", vec![pointer]), &[]);
        let pointer = UnityValue::Object(IndexMap::from([
            (
                "m_PathID".to_owned(),
                UnityValue::Integer(0x0102_0304_0506_0708),
            ),
            ("m_Tag".to_owned(), UnityValue::Integer(0xAA)),
            ("m_FileID".to_owned(), UnityValue::Integer(1)),
        ]));

        assert_eq!(
            encode(
                &schema,
                IndexMap::from([("m_Texture".to_owned(), pointer)]),
                ByteOrder::Little,
            )
            .unwrap(),
            [8, 7, 6, 5, 4, 3, 2, 1, 0xAA, 1, 0, 0, 0]
        );
    }

    #[test]
    fn pptr_null_rejects_unspecified_extra_children() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![
            node("int", "m_FileID"),
            node("UInt8", "m_Tag"),
            node("long long", "m_PathID"),
        ];
        let schema = compile(record("Base", vec![pointer]), &[]);

        assert!(
            encode(
                &schema,
                IndexMap::from([("m_Texture".to_owned(), UnityValue::Null)]),
                ByteOrder::Little,
            )
            .is_err()
        );
    }

    #[test]
    fn pptr_roles_reject_ids_outside_reader_ranges() {
        let mut pointer = node("PPtr<Texture2D>", "m_Texture");
        pointer.children = vec![node("UInt32", "m_FileID"), node("UInt64", "m_PathID")];
        let schema = compile(record("Base", vec![pointer]), &[]);

        for pointer in [
            UnityValue::Object(IndexMap::from([
                (
                    "m_FileID".to_owned(),
                    UnityValue::Unsigned(i32::MAX as u64 + 1),
                ),
                ("m_PathID".to_owned(), UnityValue::Unsigned(0)),
            ])),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Unsigned(0)),
                ("m_PathID".to_owned(), UnityValue::Unsigned(u64::MAX)),
            ])),
        ] {
            assert!(
                encode(
                    &schema,
                    IndexMap::from([("m_Texture".to_owned(), pointer)]),
                    ByteOrder::Little,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn managed_payload_uses_schema_resolution_and_empty_class_is_zero_extent() {
        let mut managed_type = SerializedType::new(114);
        managed_type.class_name = "C".to_owned();
        managed_type.namespace = "N".to_owned();
        managed_type.assembly_name = "A".to_owned();
        managed_type.type_tree.add_node(aligned(record(
            "ManagedBase",
            vec![node("UInt8", "m_Value")],
        )));

        let mut type_node = node("ReferencedObjectType", "type");
        type_node.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Ref");
        referenced.children = vec![type_node, node("ReferencedObjectData", "data")];
        let schema = compile(record("Base", vec![referenced]), &[managed_type]);

        let resolved = UnityValue::Object(IndexMap::from([
            (
                "type".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("class".to_owned(), UnityValue::String("C".to_owned())),
                    ("ns".to_owned(), UnityValue::String("N".to_owned())),
                    ("asm".to_owned(), UnityValue::String("A".to_owned())),
                ])),
            ),
            (
                "data".to_owned(),
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(7),
                )])),
            ),
        ]));
        let bytes = encode(
            &schema,
            IndexMap::from([("m_Ref".to_owned(), resolved)]),
            ByteOrder::Little,
        )
        .unwrap();
        assert_eq!(bytes.len(), 28);
        assert_eq!(&bytes[24..], &[7, 0, 0, 0]);

        let empty = UnityValue::Object(IndexMap::from([(
            "type".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("class".to_owned(), UnityValue::String(String::new())),
                ("ns".to_owned(), UnityValue::String(String::new())),
                ("asm".to_owned(), UnityValue::String(String::new())),
            ])),
        )]));
        assert_eq!(
            encode(
                &schema,
                IndexMap::from([("m_Ref".to_owned(), empty)]),
                ByteOrder::Little,
            )
            .unwrap(),
            [0_u8; 12]
        );

        let empty_with_payload = UnityValue::Object(IndexMap::from([
            (
                "type".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("class".to_owned(), UnityValue::String(String::new())),
                    ("ns".to_owned(), UnityValue::String(String::new())),
                    ("asm".to_owned(), UnityValue::String(String::new())),
                ])),
            ),
            ("data".to_owned(), UnityValue::Object(IndexMap::new())),
        ]));
        assert!(matches!(
            encode(
                &schema,
                IndexMap::from([("m_Ref".to_owned(), empty_with_payload)]),
                ByteOrder::Little,
            ),
            Err(TypeTreeWriteError::Shape {
                expected_fields: 1,
                actual_fields: 2,
            })
        ));

        let unresolved = UnityValue::Object(IndexMap::from([(
            "type".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("class".to_owned(), UnityValue::String("Missing".to_owned())),
                ("ns".to_owned(), UnityValue::String("N".to_owned())),
                ("asm".to_owned(), UnityValue::String("A".to_owned())),
            ])),
        )]));
        assert!(
            encode(
                &schema,
                IndexMap::from([("m_Ref".to_owned(), unresolved)]),
                ByteOrder::Little,
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_managed_registry_requires_no_property_and_writes_no_bytes() {
        let mut first = node("ManagedReferencesRegistry", "first");
        first.children.push(node("UInt8", "value"));
        let mut duplicate = node("ManagedReferencesRegistry", "duplicate");
        duplicate.children.push(node("UInt8", "ignored"));
        let schema = compile(record("Base", vec![first, duplicate]), &[]);
        let properties = IndexMap::from([(
            "first".to_owned(),
            UnityValue::Object(IndexMap::from([(
                "value".to_owned(),
                UnityValue::Integer(9),
            )])),
        )]);

        assert_eq!(encode(&schema, properties, ByteOrder::Little).unwrap(), [9]);
    }

    #[test]
    fn bytes_are_only_accepted_for_i8_and_u8_sequences() {
        let bytes_schema = compile(
            record("Base", vec![sequence("m_Data", node("UInt8", "data"))]),
            &[],
        );
        let properties = IndexMap::from([("m_Data".to_owned(), UnityValue::Bytes(vec![1, 2, 3]))]);
        assert_eq!(
            encode(&bytes_schema, properties, ByteOrder::Little).unwrap(),
            [3, 0, 0, 0, 1, 2, 3]
        );

        let words_schema = compile(
            record("Base", vec![sequence("m_Data", node("UInt16", "data"))]),
            &[],
        );
        let properties = IndexMap::from([("m_Data".to_owned(), UnityValue::Bytes(vec![1, 2, 3]))]);
        assert!(encode(&words_schema, properties, ByteOrder::Little).is_err());
    }
}
