//! Budgeted semantic traversal adapters for compiled TypeTree schemas.

use indexmap::IndexMap;
use unity_asset_core::{AssetLoadBudget, FieldPath, FieldPathSegment, UnityValue};

use super::parser::MAX_TYPE_TREE_DEPTH;
use super::schema::{
    ManagedPayload, PPtrLayout, PairLayout, PrimitiveKind, ReferencedObjectLayout, SchemaNode,
    SemanticKind, SemanticLayout, SequenceLayout, TypeTreeSchema, TypeTreeTraversalContext,
};
use super::traversal::{
    TraversalCheckpoint, TraversalCursor, TraversalMap, TraversalVec, TypeTreeTraversalStats,
};
use crate::error::{BinaryError, Result};
use crate::reader::{BinaryReader, ByteOrder};
use crate::reference::{BinaryReferenceDiagnostic, BinaryReferenceOccurrence, BinaryReferenceScan};

/// PPtr references found by a semantic TypeTree scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PPtrScanResult {
    pub internal: Vec<i64>,
    pub external: Vec<(i32, i64)>,
    pub stats: TypeTreeTraversalStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TypeTreeParseMode {
    Strict,
    #[default]
    Lenient,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TypeTreeParseOptions {
    pub mode: TypeTreeParseMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeTreeParseWarning {
    pub field: String,
    pub error: String,
}

/// Result of reading the fields selected by an object or prefix request.
#[derive(Debug)]
pub struct TypeTreeParseOutput {
    pub properties: IndexMap<String, UnityValue>,
    pub warnings: Vec<TypeTreeParseWarning>,
    /// Whether the requested prefix was completely traversed.
    ///
    /// A successful prefix remains complete even when it intentionally omits later root fields.
    pub complete: bool,
    pub stats: TypeTreeTraversalStats,
}

impl Default for TypeTreeParseOutput {
    fn default() -> Self {
        Self {
            properties: IndexMap::new(),
            warnings: Vec::new(),
            complete: true,
            stats: TypeTreeTraversalStats::default(),
        }
    }
}

/// One strictly decoded schema value and its traversal metrics.
#[derive(Debug)]
pub struct TypeTreeValueRead {
    pub value: UnityValue,
    pub stats: TypeTreeTraversalStats,
}

impl TypeTreeSchema {
    /// Reads every direct child of the compiled object root.
    pub fn read_object(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<TypeTreeParseOutput> {
        self.read_object_prefix(reader, budget, options, usize::MAX)
    }

    /// Reads at most `root_children` direct fields without aligning an incomplete root prefix.
    pub fn read_object_prefix(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
        root_children: usize,
    ) -> Result<TypeTreeParseOutput> {
        let root = self.root();
        if !matches!(
            root.kind(),
            SemanticKind::Record | SemanticKind::ManagedRegistry
        ) {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree object root '{}' has non-record semantic kind {:?}",
                root.name(),
                root.kind()
            )));
        }

        let selected = root_children.min(root.child_count());
        let full_root = selected == root.child_count();
        let mut cursor = TraversalCursor::new(reader, budget)?;
        cursor.enter_node(0)?;
        cursor.consume_members(usize_to_u64(selected, "root child count")?)?;
        let mut adapter = ReadAdapter::new(&mut cursor, options.mode)?;
        let mut properties = adapter.begin_record(
            &mut cursor,
            SemanticKind::Record,
            selected,
            "TypeTree object properties",
        )?;

        let mut complete = true;
        let mut context = TypeTreeTraversalContext::root();
        for child in root.children().take(selected) {
            let Some(child_context) = context.descend(root, child) else {
                continue;
            };
            let checkpoint = cursor.checkpoint();
            let attempt = traverse_value(self, &mut cursor, &mut adapter, child, child_context, 1);
            match recover_record_child(
                self,
                &mut cursor,
                &mut adapter,
                child,
                child_context,
                1,
                checkpoint,
                attempt,
            )? {
                ChildResult::Value(value) => {
                    adapter.push_record(&mut cursor, &mut properties, child, value)?;
                }
                ChildResult::Skipped => {
                    adapter.recovered_record_field(&mut cursor, &mut properties, child)?;
                }
                ChildResult::Terminal => {
                    complete = false;
                    break;
                }
            }
        }

        if complete && full_root && root.align_after() {
            cursor.align()?;
        }

        let stats = cursor.stats();
        let properties = properties.into_object()?;
        let warnings = adapter.into_warnings();
        Ok(TypeTreeParseOutput {
            properties,
            warnings,
            complete,
            stats,
        })
    }

    /// Strictly reads one node belonging to this schema.
    pub fn read_value(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
    ) -> Result<TypeTreeValueRead> {
        self.read_value_with_context(reader, budget, node, TypeTreeTraversalContext::root(), 0)
    }

    /// Strictly reads one node under an inherited traversal context.
    pub fn read_value_with_context(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<TypeTreeValueRead> {
        if !self.owns_node(node) {
            return Err(BinaryError::invalid_data(
                "TypeTree value node belongs to a different schema",
            ));
        }
        let mut cursor = TraversalCursor::new(reader, budget)?;
        let mut adapter = ReadAdapter::new(&mut cursor, TypeTreeParseMode::Strict)?;
        let value = match traverse_value(self, &mut cursor, &mut adapter, node, context, depth)? {
            TraverseOutcome::Complete(value) => value,
            TraverseOutcome::Terminal => {
                return Err(BinaryError::invalid_data(
                    "Strict TypeTree traversal terminated without an error",
                ));
            }
        };
        Ok(TypeTreeValueRead {
            value,
            stats: cursor.stats(),
        })
    }

    /// Strictly consumes one node without constructing a UnityValue tree.
    pub fn skip_value(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
    ) -> Result<TypeTreeTraversalStats> {
        self.skip_value_with_context(reader, budget, node, TypeTreeTraversalContext::root(), 0)
    }

    /// Strictly consumes one node under an inherited traversal context.
    pub fn skip_value_with_context(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<TypeTreeTraversalStats> {
        if !self.owns_node(node) {
            return Err(BinaryError::invalid_data(
                "TypeTree value node belongs to a different schema",
            ));
        }
        let mut cursor = TraversalCursor::new(reader, budget)?;
        let mut adapter = UnitAdapter::strict(IgnorePPtrs);
        match traverse_value(self, &mut cursor, &mut adapter, node, context, depth)? {
            TraverseOutcome::Complete(()) => Ok(cursor.into_stats()),
            TraverseOutcome::Terminal => Err(BinaryError::invalid_data(
                "Strict TypeTree skip terminated without an error",
            )),
        }
    }

    /// Compares one canonical node with its wire representation without materializing values.
    pub fn compare_value(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
        expected: &UnityValue,
    ) -> Result<(bool, TypeTreeTraversalStats)> {
        self.compare_value_with_context(
            reader,
            budget,
            node,
            TypeTreeTraversalContext::root(),
            0,
            expected,
        )
    }

    /// Compares one canonical node under an inherited traversal context.
    ///
    /// The complete input extent is consumed and budgeted even after a mismatch. Scalar values are
    /// compared directly and byte payloads are inspected through borrowed slices.
    pub fn compare_value_with_context(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        node: SchemaNode<'_>,
        context: TypeTreeTraversalContext,
        depth: u32,
        expected: &UnityValue,
    ) -> Result<(bool, TypeTreeTraversalStats)> {
        if !self.owns_node(node) {
            return Err(BinaryError::invalid_data(
                "TypeTree value node belongs to a different schema",
            ));
        }
        let mut cursor = TraversalCursor::new(reader, budget)?;
        let equal = compare_wire_value(
            self,
            &mut cursor,
            node,
            context,
            depth,
            ExpectedWireValue::Borrowed(expected),
        )?;
        Ok((equal, cursor.into_stats()))
    }

    /// Scans the complete root for PPtr references without materializing UnityValue instances.
    pub fn scan_pptrs(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<PPtrScanResult> {
        self.scan_pptrs_with_options(
            reader,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Scans the complete root for PPtr references under the requested recovery policy.
    ///
    /// Lenient scans discard format warnings and resume only when the canonical traversal proves
    /// the failed field's wire extent. Resource errors are always returned to the caller.
    pub fn scan_pptrs_with_options(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<PPtrScanResult> {
        let mut cursor = TraversalCursor::new(reader, budget)?;
        let mut adapter = UnitAdapter::with_mode(CollectPPtrs::new(&mut cursor)?, options.mode);
        match traverse_value(
            self,
            &mut cursor,
            &mut adapter,
            self.root(),
            TypeTreeTraversalContext::root(),
            0,
        )? {
            TraverseOutcome::Complete(()) => {
                let stats = cursor.stats();
                let (internal, external) = adapter.into_sink().into_parts();
                Ok(PPtrScanResult {
                    internal,
                    external,
                    stats,
                })
            }
            TraverseOutcome::Terminal => {
                let message = match options.mode {
                    TypeTreeParseMode::Strict => {
                        "Strict TypeTree PPtr scan terminated without an error"
                    }
                    TypeTreeParseMode::Lenient => {
                        "Lenient TypeTree PPtr scan could not prove the remaining wire extent"
                    }
                };
                Err(BinaryError::invalid_data(message))
            }
        }
    }

    /// Scans the complete root for depth-first, completion-ordered binary references.
    ///
    /// The traversal retains null pointers and negative file IDs exactly as encoded. It does not
    /// construct any [`UnityValue`] instances.
    pub fn scan_reference_occurrences(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryReferenceScan> {
        self.scan_reference_occurrences_with_options(
            reader,
            budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
    }

    /// Scans ordered, path-aware references under the requested recovery policy.
    ///
    /// Lenient scans return a diagnostic for each field whose exact wire extent was proven and
    /// skipped. Resource errors are never converted into diagnostics.
    pub fn scan_reference_occurrences_with_options(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<BinaryReferenceScan> {
        self.scan_reference_occurrences_internal(reader, budget, options)
    }

    fn scan_reference_occurrences_internal(
        &self,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        options: TypeTreeParseOptions,
    ) -> Result<BinaryReferenceScan> {
        let mut cursor = TraversalCursor::new(reader, budget)?;
        let mut adapter =
            UnitAdapter::with_mode(CollectReferences::new(&mut cursor)?, options.mode);
        match traverse_value(
            self,
            &mut cursor,
            &mut adapter,
            self.root(),
            TypeTreeTraversalContext::root(),
            0,
        )? {
            TraverseOutcome::Complete(()) => {
                let stats = cursor.stats();
                let (occurrences, diagnostics) = adapter.into_sink().into_parts();
                Ok(BinaryReferenceScan {
                    occurrences,
                    diagnostics,
                    stats,
                })
            }
            TraverseOutcome::Terminal => {
                let message = match options.mode {
                    TypeTreeParseMode::Strict => {
                        "Strict TypeTree reference scan terminated without an error"
                    }
                    TypeTreeParseMode::Lenient => {
                        "Lenient TypeTree reference scan could not prove the remaining wire extent"
                    }
                };
                Err(BinaryError::invalid_data(message))
            }
        }
    }
}

enum TraverseOutcome<T> {
    Complete(T),
    Terminal,
}

enum ChildResult<T> {
    Value(T),
    Skipped,
    Terminal,
}

#[derive(Debug, Clone, Copy)]
enum WirePrimitive {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

#[derive(Clone, Copy)]
enum ExpectedWireValue<'value> {
    Borrowed(&'value UnityValue),
    IntegerZero,
    Missing,
}

impl<'value> ExpectedWireValue<'value> {
    fn borrowed(self) -> Option<&'value UnityValue> {
        match self {
            Self::Borrowed(value) => Some(value),
            Self::IntegerZero | Self::Missing => None,
        }
    }

    fn object(self) -> Option<&'value IndexMap<String, UnityValue>> {
        match self.borrowed() {
            Some(UnityValue::Object(value)) => Some(value),
            _ => None,
        }
    }

    fn array(self) -> Option<&'value [UnityValue]> {
        match self.borrowed() {
            Some(UnityValue::Array(value)) => Some(value),
            _ => None,
        }
    }

    fn property(self, name: &str) -> Self {
        self.object()
            .and_then(|object| object.get(name))
            .map_or(Self::Missing, Self::Borrowed)
    }
}

fn compare_wire_value(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    cursor.enter_node(depth)?;
    let equal = match node.semantic_layout() {
        SemanticLayout::Scalar(primitive) => {
            wire_matches_expected(read_wire_primitive(cursor, primitive)?, expected)
        }
        SemanticLayout::String => compare_string(cursor, expected)?,
        SemanticLayout::TypelessData => compare_sized_bytes(cursor, "TypelessData", expected)?,
        SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => {
            compare_sequence(schema, cursor, layout, context, depth, expected)?
        }
        SemanticLayout::Record | SemanticLayout::ManagedRegistry => {
            compare_record(schema, cursor, node, context, depth, expected)?
        }
        SemanticLayout::Pair(layout) => {
            compare_pair(schema, cursor, layout, context, depth, expected)?
        }
        SemanticLayout::PPtr(layout) => {
            compare_pptr(schema, cursor, node, layout, context, depth, expected)?
        }
        SemanticLayout::ReferencedObject(layout) => {
            compare_referenced_object(schema, cursor, node, layout, context, depth, expected)?
        }
        SemanticLayout::ManagedPayload => {
            return Err(BinaryError::invalid_data(format!(
                "Managed payload '{}' has no statically provable extent",
                node.name()
            )));
        }
        SemanticLayout::OpaqueFixed { byte_size } => {
            let length = usize::try_from(byte_size).map_err(|_| {
                BinaryError::invalid_data("Opaque TypeTree extent does not fit usize")
            })?;
            compare_borrowed_bytes(cursor, length, expected)?
        }
    };
    align_completed_node(cursor, node)?;
    Ok(equal)
}

fn compare_string(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    let length = read_length(cursor, "string", BinaryReader::DEFAULT_MAX_STRING_LEN)?;
    let expected = match expected.borrowed() {
        Some(UnityValue::String(value)) if value.len() == length => Some(value.as_bytes()),
        _ => None,
    };
    cursor.with_borrowed_slice(length, |bytes| {
        std::str::from_utf8(bytes)?;
        Ok(expected == Some(bytes))
    })
}

fn compare_sized_bytes(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    label: &str,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    let length = read_length(cursor, label, usize::MAX)?;
    compare_borrowed_bytes(cursor, length, expected)
}

fn compare_borrowed_bytes(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    length: usize,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    let expected = match expected.borrowed() {
        Some(UnityValue::Bytes(value)) if value.len() == length => Some(value.as_slice()),
        _ => None,
    };
    cursor.with_borrowed_slice(length, |bytes| Ok(expected == Some(bytes)))
}

fn compare_sequence(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    layout: SequenceLayout<'_>,
    context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    let raw_length = cursor.read_i32()?;
    if raw_length < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Negative TypeTree sequence length: {raw_length}"
        )));
    }
    let length = usize::try_from(raw_length)
        .map_err(|_| BinaryError::invalid_data("TypeTree sequence length does not fit usize"))?;
    cursor.consume_members(usize_to_u64(length, "sequence length")?)?;
    let child_depth = next_depth(depth)?;

    if let Some(primitive) = layout.bulk_primitive() {
        if length != 0 {
            cursor.enter_nodes(
                child_depth,
                usize_to_u64(length, "bulk sequence element count")?,
            )?;
        }
        return compare_bulk_sequence(cursor, primitive, length, expected);
    }

    let expected_values = expected.array();
    let mut equal = expected_values.is_some_and(|values| values.len() == length);
    for index in 0..length {
        let element_expected = expected_values
            .and_then(|values| values.get(index))
            .map_or(ExpectedWireValue::Missing, ExpectedWireValue::Borrowed);
        let element_equal = compare_wire_value(
            schema,
            cursor,
            layout.element(),
            context,
            child_depth,
            element_expected,
        )?;
        equal &= element_equal;
    }
    Ok(equal)
}

fn compare_bulk_sequence(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    primitive: PrimitiveKind,
    length: usize,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    let expected_bytes = match expected.borrowed() {
        Some(UnityValue::Bytes(value))
            if matches!(primitive, PrimitiveKind::I8 | PrimitiveKind::U8)
                && value.len() == length =>
        {
            Some(value.as_slice())
        }
        _ => None,
    };
    let expected_values = expected.array().filter(|values| values.len() == length);
    let byte_length = bulk_byte_length(primitive, length)?;
    if expected_bytes.is_none() && expected_values.is_none() {
        cursor.with_wire_slice(byte_length, |_, _| Ok(()))?;
        cursor.record_scalar_elements(usize_to_u64(length, "bulk comparison element count")?)?;
        return Ok(false);
    }
    let equal = cursor.with_wire_slice(byte_length, |bytes, byte_order| {
        if let Some(expected_bytes) = expected_bytes {
            return Ok(bytes == expected_bytes);
        }

        let mut equal = expected_values.is_some();
        for (index, chunk) in bytes
            .chunks_exact(usize::from(primitive.width()))
            .enumerate()
        {
            let wire = decode_wire_primitive(primitive, chunk, byte_order)?;
            let element_equal = expected_values
                .and_then(|values| values.get(index))
                .is_some_and(|value| wire_matches_unity_value(wire, value));
            equal &= element_equal;
        }
        Ok(equal)
    })?;
    if expected_bytes.is_none() {
        cursor.record_scalar_elements(usize_to_u64(length, "bulk comparison element count")?)?;
    }
    Ok(equal)
}

fn compare_record(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    mut context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    cursor.consume_members(usize_to_u64(node.child_count(), "record child count")?)?;
    let child_depth = next_depth(depth)?;
    let expected_object = expected.object();
    let mut equal = expected_object.is_some();
    let mut named_fields = 0_usize;

    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let child_expected = if child.name().is_empty() {
            ExpectedWireValue::Missing
        } else {
            named_fields = named_fields.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("TypeTree compared field count overflow")
            })?;
            expected.property(child.name())
        };
        let child_equal = compare_wire_value(
            schema,
            cursor,
            child,
            child_context,
            child_depth,
            child_expected,
        )?;
        if !child.name().is_empty() {
            equal &= child_equal;
        }
    }

    if let Some(object) = expected_object {
        equal &= object.len() == named_fields;
    }
    Ok(equal)
}

fn compare_pair(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    layout: PairLayout<'_>,
    context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    cursor.consume_members(2)?;
    let child_depth = next_depth(depth)?;
    let expected_values = expected.array();
    let mut equal = expected_values.is_some_and(|values| values.len() == 2);

    for (index, child) in layout.children().into_iter().enumerate() {
        let child_expected = expected_values
            .and_then(|values| values.get(index))
            .map_or(ExpectedWireValue::Missing, ExpectedWireValue::Borrowed);
        equal &= compare_wire_value(schema, cursor, child, context, child_depth, child_expected)?;
    }
    Ok(equal)
}

fn compare_pptr(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    layout: PPtrLayout<'_>,
    mut context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    cursor.consume_members(usize_to_u64(node.child_count(), "PPtr child count")?)?;
    let child_depth = next_depth(depth)?;
    let expected_object = expected.object();
    let expected_null = matches!(expected.borrowed(), Some(UnityValue::Null));
    let mut equal = expected_object.is_some() || expected_null;
    let mut named_fields = 0_usize;

    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let is_file = child == layout.file_child();
        let is_path = child == layout.path_child();
        let child_expected = if expected_null && (is_file || is_path) {
            ExpectedWireValue::IntegerZero
        } else if child.name().is_empty() {
            ExpectedWireValue::Missing
        } else {
            named_fields = named_fields.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("TypeTree compared PPtr field count overflow")
            })?;
            expected.property(child.name())
        };

        let child_equal = if is_file || is_path {
            let primitive = if is_file {
                layout.file_primitive()
            } else {
                layout.path_primitive()
            };
            let (equal, wire) =
                compare_integer_node(cursor, child, primitive, child_depth, child_expected)?;
            if is_file {
                wire.into_file_id()?;
            } else {
                wire.into_path_id()?;
            }
            equal
        } else {
            compare_wire_value(
                schema,
                cursor,
                child,
                child_context,
                child_depth,
                child_expected,
            )?
        };
        if !child.name().is_empty() {
            equal &= child_equal;
        }
    }

    if let Some(object) = expected_object {
        equal &= object.len() == named_fields;
    }
    Ok(equal)
}

fn compare_integer_node(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    primitive: PrimitiveKind,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<(bool, WirePrimitive)> {
    cursor.enter_node(depth)?;
    let wire = read_wire_primitive(cursor, primitive)?;
    let equal = wire_matches_expected(wire, expected);
    align_completed_node(cursor, node)?;
    Ok((equal, wire))
}

fn compare_referenced_object(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    layout: ReferencedObjectLayout<'_>,
    context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    cursor.consume_members(usize_to_u64(
        node.child_count(),
        "ReferencedObject child count",
    )?)?;
    let child_depth = next_depth(depth)?;
    let expected_object = expected.object();
    let mut equal = expected_object.is_some();
    let mut named_fields = 0_usize;
    let mut key = ManagedTypeKey::default();

    for child in node.children() {
        let child_expected = if child.name().is_empty() {
            ExpectedWireValue::Missing
        } else {
            named_fields = named_fields.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("TypeTree compared referenced field count overflow")
            })?;
            expected.property(child.name())
        };
        let child_equal = if layout.is_type_node(child) {
            let (equal, parsed_key) = compare_managed_type(
                schema,
                cursor,
                child,
                layout,
                context,
                child_depth,
                child_expected,
            )?;
            key = parsed_key;
            equal
        } else if layout.is_payload(child) {
            compare_managed_payload(
                schema,
                cursor,
                layout.payload(),
                &key,
                context,
                child_depth,
                child_expected,
            )?
        } else {
            compare_wire_value(schema, cursor, child, context, child_depth, child_expected)?
        };
        if !child.name().is_empty() {
            equal &= child_equal;
        }
    }

    if let Some(object) = expected_object {
        equal &= object.len() == named_fields;
    }
    Ok(equal)
}

fn compare_managed_type(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    layout: super::schema::ReferencedObjectLayout<'_>,
    mut context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<(bool, ManagedTypeKey)> {
    cursor.enter_node(depth)?;
    cursor.consume_members(usize_to_u64(
        node.child_count(),
        "managed type child count",
    )?)?;
    let child_depth = next_depth(depth)?;
    let expected_object = expected.object();
    let mut equal = expected_object.is_some();
    let mut named_fields = 0_usize;
    let mut key = ManagedTypeKey::default();

    for child in node.children() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let child_expected = if child.name().is_empty() {
            ExpectedWireValue::Missing
        } else {
            named_fields = named_fields.checked_add(1).ok_or_else(|| {
                BinaryError::invalid_data("TypeTree compared managed type field count overflow")
            })?;
            expected.property(child.name())
        };
        let is_key_field = child == layout.class_field()
            || child == layout.namespace_field()
            || child == layout.assembly_field();
        let child_equal = if is_key_field {
            let (equal, text) =
                compare_captured_string(cursor, child, child_depth, child_expected)?;
            if child == layout.class_field() {
                key.class_name = Some(text);
            } else if child == layout.namespace_field() {
                key.namespace = Some(text);
            } else {
                key.assembly_name = Some(text);
            }
            equal
        } else {
            compare_wire_value(
                schema,
                cursor,
                child,
                child_context,
                child_depth,
                child_expected,
            )?
        };
        if !child.name().is_empty() {
            equal &= child_equal;
        }
    }

    if let Some(object) = expected_object {
        equal &= object.len() == named_fields;
    }
    align_completed_node(cursor, node)?;
    Ok((equal, key))
}

fn compare_captured_string(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<(bool, String)> {
    cursor.enter_node(depth)?;
    let length = read_length(
        cursor,
        "managed type string",
        BinaryReader::DEFAULT_MAX_STRING_LEN,
    )?;
    let text = String::from_utf8(cursor.read_bytes(length)?)?;
    let equal = matches!(expected.borrowed(), Some(UnityValue::String(value)) if value == &text);
    align_completed_node(cursor, node)?;
    Ok((equal, text))
}

fn compare_managed_payload(
    schema: &TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    payload: ManagedPayload<'_>,
    key: &ManagedTypeKey,
    context: TypeTreeTraversalContext,
    depth: u32,
    expected: ExpectedWireValue<'_>,
) -> Result<bool> {
    if key.class_name.as_deref() == Some("") {
        cursor.enter_node(depth)?;
        return Ok(matches!(
            expected,
            ExpectedWireValue::Borrowed(UnityValue::Null) | ExpectedWireValue::Missing
        ));
    }

    if let (Some(class_name), Some(namespace), Some(assembly_name)) = (
        key.class_name.as_deref(),
        key.namespace.as_deref(),
        key.assembly_name.as_deref(),
    ) && let Some(root) = schema.resolve_managed_root(class_name, namespace, assembly_name)
    {
        return compare_wire_value(schema, cursor, root, context, depth, expected);
    }

    if let Some(fallback) = payload.fallback() {
        return compare_wire_value(schema, cursor, fallback, context, depth, expected);
    }

    cursor.enter_node(depth)?;
    let key_description = match (
        key.class_name.as_deref(),
        key.namespace.as_deref(),
        key.assembly_name.as_deref(),
    ) {
        (Some(class_name), Some(namespace), Some(assembly_name)) => {
            format!("{class_name}|{namespace}|{assembly_name}")
        }
        _ => "<incomplete managed type key>".to_string(),
    };
    Err(BinaryError::invalid_data(format!(
        "Managed type '{key_description}' is unresolved and its payload extent is unknown"
    )))
}

fn wire_matches_expected(wire: WirePrimitive, expected: ExpectedWireValue<'_>) -> bool {
    match expected {
        ExpectedWireValue::Borrowed(expected) => wire_matches_unity_value(wire, expected),
        ExpectedWireValue::IntegerZero => {
            matches!(wire, WirePrimitive::Signed(0) | WirePrimitive::Unsigned(0))
        }
        ExpectedWireValue::Missing => false,
    }
}

fn wire_matches_unity_value(wire: WirePrimitive, expected: &UnityValue) -> bool {
    match (wire, expected) {
        (WirePrimitive::Bool(left), UnityValue::Bool(right)) => left == *right,
        (WirePrimitive::Signed(left), UnityValue::Integer(right)) => left == *right,
        (WirePrimitive::Unsigned(left), UnityValue::Integer(right)) => {
            i64::try_from(left).is_ok_and(|left| left == *right)
        }
        (WirePrimitive::Unsigned(left), UnityValue::Unsigned(right)) => {
            left > i64::MAX as u64 && left == *right
        }
        (WirePrimitive::Float(left), UnityValue::Float(right)) => left.to_bits() == right.to_bits(),
        _ => false,
    }
}

impl WirePrimitive {
    fn into_unity_value(self) -> UnityValue {
        match self {
            Self::Bool(value) => UnityValue::Bool(value),
            Self::Signed(value) => UnityValue::Integer(value),
            Self::Unsigned(value) => UnityValue::from(value),
            Self::Float(value) => UnityValue::Float(value),
        }
    }

    fn into_file_id(self) -> Result<i32> {
        match self {
            Self::Signed(value) => i32::try_from(value),
            Self::Unsigned(value) => i32::try_from(value),
            Self::Bool(_) | Self::Float(_) => {
                return Err(BinaryError::invalid_data("PPtr file ID is not an integer"));
            }
        }
        .map_err(|_| BinaryError::invalid_data("PPtr file ID does not fit in i32"))
    }

    fn into_path_id(self) -> Result<i64> {
        match self {
            Self::Signed(value) => Ok(value),
            Self::Unsigned(value) => i64::try_from(value)
                .map_err(|_| BinaryError::invalid_data("PPtr path ID does not fit in i64")),
            Self::Bool(_) | Self::Float(_) => {
                Err(BinaryError::invalid_data("PPtr path ID is not an integer"))
            }
        }
    }
}

trait TraversalAdapter<'schema> {
    type Value;
    type Sequence;
    type Record;

    fn parse_mode(&self) -> TypeTreeParseMode;

    fn scalar(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        value: WirePrimitive,
    ) -> Result<Self::Value>;

    fn string_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value>;

    fn captured_string(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        value: &str,
    ) -> Result<Self::Value>;

    fn bytes_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value>;

    fn bulk_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        primitive: PrimitiveKind,
        length: usize,
    ) -> Result<Self::Value>;

    fn begin_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Sequence>;

    fn push_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        sequence: &mut Self::Sequence,
        value: Self::Value,
    ) -> Result<()>;

    fn finish_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        sequence: Self::Sequence,
    ) -> Result<Self::Value>;

    fn begin_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        kind: SemanticKind,
        child_count: usize,
        label: &'static str,
    ) -> Result<Self::Record>;

    fn push_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: &mut Self::Record,
        child: SchemaNode<'schema>,
        value: Self::Value,
    ) -> Result<()>;

    fn recovered_record_field(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: &mut Self::Record,
        child: SchemaNode<'schema>,
    ) -> Result<()>;

    fn finish_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: Self::Record,
    ) -> Result<Self::Value>;

    fn null(&mut self, cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<Self::Value>;

    fn emit_pptr(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<()>;

    fn enter_record_child(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        kind: SemanticKind,
        index: usize,
        child: SchemaNode<'schema>,
    ) -> Result<()>;

    fn enter_sequence_element(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        index: usize,
    ) -> Result<()>;

    fn path_checkpoint(&self) -> usize;

    fn restore_path(&mut self, checkpoint: usize);

    fn warning(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        child: SchemaNode<'schema>,
        error: &BinaryError,
    ) -> Result<()>;
}

struct ReadAdapter {
    mode: TypeTreeParseMode,
    warnings: TraversalVec<TypeTreeParseWarning>,
}

enum ReadRecord {
    Object(TraversalMap<String, UnityValue>),
    Pair(TraversalVec<UnityValue>),
}

impl ReadRecord {
    fn into_object(self) -> Result<IndexMap<String, UnityValue>> {
        match self {
            Self::Object(values) => Ok(values.into_map()),
            Self::Pair(_) => Err(BinaryError::invalid_data(
                "TypeTree object root unexpectedly used a pair aggregate",
            )),
        }
    }
}

impl ReadAdapter {
    fn new(cursor: &mut TraversalCursor<'_, '_, '_>, mode: TypeTreeParseMode) -> Result<Self> {
        Ok(Self {
            mode,
            warnings: cursor.vector(0, "TypeTree parse warnings")?,
        })
    }

    fn into_warnings(self) -> Vec<TypeTreeParseWarning> {
        self.warnings.into_vec()
    }
}

impl<'schema> TraversalAdapter<'schema> for ReadAdapter {
    type Value = UnityValue;
    type Sequence = TraversalVec<UnityValue>;
    type Record = ReadRecord;

    fn parse_mode(&self) -> TypeTreeParseMode {
        self.mode
    }

    fn scalar(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        value: WirePrimitive,
    ) -> Result<Self::Value> {
        cursor.record_materialized(1)?;
        Ok(value.into_unity_value())
    }

    fn string_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value> {
        let value = String::from_utf8(cursor.read_bytes(length)?)?;
        cursor.record_materialized(1)?;
        Ok(UnityValue::String(value))
    }

    fn captured_string(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        value: &str,
    ) -> Result<Self::Value> {
        let value = cursor.clone_string(value, "managed type output string")?;
        cursor.record_materialized(1)?;
        Ok(UnityValue::String(value))
    }

    fn bytes_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value> {
        let value = cursor.read_bytes(length)?;
        cursor.record_materialized(1)?;
        Ok(UnityValue::Bytes(value))
    }

    fn bulk_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        primitive: PrimitiveKind,
        length: usize,
    ) -> Result<Self::Value> {
        let byte_length = bulk_byte_length(primitive, length)?;
        if matches!(primitive, PrimitiveKind::I8 | PrimitiveKind::U8) {
            let mut values = cursor
                .vector::<u8>(length, "TypeTree byte sequence")?
                .into_vec();
            cursor.with_wire_slice(byte_length, |bytes, _| {
                values.extend_from_slice(bytes);
                Ok(())
            })?;
            cursor.record_materialized(1)?;
            return Ok(UnityValue::Bytes(values));
        }

        let mut values = cursor
            .vector::<UnityValue>(length, "TypeTree primitive sequence")?
            .into_vec();
        cursor.with_wire_slice(byte_length, |bytes, byte_order| {
            for chunk in bytes.chunks_exact(usize::from(primitive.width())) {
                values
                    .push(decode_wire_primitive(primitive, chunk, byte_order)?.into_unity_value());
            }
            Ok(())
        })?;
        cursor.record_scalar_elements(usize_to_u64(length, "primitive sequence length")?)?;
        let materialized = usize_to_u64(length, "primitive sequence length")?
            .checked_add(1)
            .ok_or_else(|| BinaryError::invalid_data("materialized value count overflow"))?;
        cursor.record_materialized(materialized)?;
        Ok(UnityValue::Array(values))
    }

    fn begin_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Sequence> {
        cursor.vector(length, "TypeTree sequence values")
    }

    fn push_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        sequence: &mut Self::Sequence,
        value: Self::Value,
    ) -> Result<()> {
        sequence.push(cursor, value)
    }

    fn finish_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        sequence: Self::Sequence,
    ) -> Result<Self::Value> {
        cursor.record_materialized(1)?;
        Ok(UnityValue::Array(sequence.into_vec()))
    }

    fn begin_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        kind: SemanticKind,
        child_count: usize,
        label: &'static str,
    ) -> Result<Self::Record> {
        if kind == SemanticKind::Pair {
            Ok(ReadRecord::Pair(
                cursor.vector(child_count, "TypeTree pair values")?,
            ))
        } else {
            Ok(ReadRecord::Object(cursor.map(child_count, label)?))
        }
    }

    fn push_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: &mut Self::Record,
        child: SchemaNode<'schema>,
        value: Self::Value,
    ) -> Result<()> {
        match record {
            ReadRecord::Pair(values) => values.push(cursor, value),
            ReadRecord::Object(values) => {
                if child.name().is_empty() {
                    return Ok(());
                }
                let key = cursor.clone_string(child.name(), "TypeTree output property name")?;
                values.insert(cursor, key, value)?;
                Ok(())
            }
        }
    }

    fn recovered_record_field(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: &mut Self::Record,
        _child: SchemaNode<'schema>,
    ) -> Result<()> {
        if let ReadRecord::Pair(values) = record {
            cursor.record_materialized(1)?;
            values.push(cursor, UnityValue::Null)?;
        }
        Ok(())
    }

    fn finish_record(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        record: Self::Record,
    ) -> Result<Self::Value> {
        cursor.record_materialized(1)?;
        Ok(match record {
            ReadRecord::Object(values) => UnityValue::Object(values.into_map()),
            ReadRecord::Pair(values) => UnityValue::Array(values.into_vec()),
        })
    }

    fn null(&mut self, cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<Self::Value> {
        cursor.record_materialized(1)?;
        Ok(UnityValue::Null)
    }

    fn emit_pptr(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _file_id: i32,
        _path_id: i64,
    ) -> Result<()> {
        Ok(())
    }

    fn enter_record_child(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _kind: SemanticKind,
        _index: usize,
        _child: SchemaNode<'schema>,
    ) -> Result<()> {
        Ok(())
    }

    fn enter_sequence_element(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _index: usize,
    ) -> Result<()> {
        Ok(())
    }

    fn path_checkpoint(&self) -> usize {
        0
    }

    fn restore_path(&mut self, _checkpoint: usize) {}

    fn warning(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        child: SchemaNode<'schema>,
        error: &BinaryError,
    ) -> Result<()> {
        cursor.consume_members(1)?;
        let field = cursor.clone_string(child.name(), "TypeTree warning field")?;
        let error = cursor.display_string(error, "TypeTree warning message")?;
        self.warnings
            .push(cursor, TypeTreeParseWarning { field, error })
    }
}

struct UnitAdapter<S> {
    mode: TypeTreeParseMode,
    sink: S,
}

impl<S> UnitAdapter<S> {
    fn strict(sink: S) -> Self {
        Self::with_mode(sink, TypeTreeParseMode::Strict)
    }

    fn with_mode(sink: S, mode: TypeTreeParseMode) -> Self {
        Self { mode, sink }
    }

    fn into_sink(self) -> S {
        self.sink
    }
}

trait ReferenceSink<'schema> {
    fn emit(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<()>;

    fn enter_record_child(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _kind: SemanticKind,
        _index: usize,
        _child: SchemaNode<'schema>,
    ) -> Result<()> {
        Ok(())
    }

    fn enter_sequence_element(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _index: usize,
    ) -> Result<()> {
        Ok(())
    }

    fn path_checkpoint(&self) -> usize {
        0
    }

    fn restore_path(&mut self, _checkpoint: usize) {}

    fn warning(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _error: &BinaryError,
    ) -> Result<()> {
        Ok(())
    }
}

struct IgnorePPtrs;

impl<'schema> ReferenceSink<'schema> for IgnorePPtrs {
    fn emit(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _file_id: i32,
        _path_id: i64,
    ) -> Result<()> {
        Ok(())
    }
}

struct CollectPPtrs {
    internal: TraversalVec<i64>,
    external: TraversalVec<(i32, i64)>,
}

impl CollectPPtrs {
    fn new(cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<Self> {
        Ok(Self {
            internal: cursor.vector(0, "internal PPtr scan results")?,
            external: cursor.vector(0, "external PPtr scan results")?,
        })
    }

    fn into_parts(self) -> (Vec<i64>, Vec<(i32, i64)>) {
        (self.internal.into_vec(), self.external.into_vec())
    }
}

impl<'schema> ReferenceSink<'schema> for CollectPPtrs {
    fn emit(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        if path_id == 0 {
            return Ok(());
        }
        cursor.consume_members(1)?;
        if file_id == 0 {
            self.internal.push(cursor, path_id)?;
        } else {
            self.external.push(cursor, (file_id, path_id))?;
        }
        cursor.record_pptrs(1)
    }
}

#[derive(Clone, Copy)]
enum BorrowedPathSegment<'schema> {
    Field(&'schema str),
    Index(u32),
}

struct CollectReferences<'schema> {
    occurrences: TraversalVec<BinaryReferenceOccurrence>,
    diagnostics: TraversalVec<BinaryReferenceDiagnostic>,
    path: [BorrowedPathSegment<'schema>; MAX_TYPE_TREE_DEPTH],
    path_len: usize,
}

impl<'schema> CollectReferences<'schema> {
    fn new(cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<Self> {
        Ok(Self {
            occurrences: cursor.vector(0, "binary reference occurrences")?,
            diagnostics: cursor.vector(0, "binary reference diagnostics")?,
            path: [BorrowedPathSegment::Index(0); MAX_TYPE_TREE_DEPTH],
            path_len: 0,
        })
    }

    fn into_parts(
        self,
    ) -> (
        Vec<BinaryReferenceOccurrence>,
        Vec<BinaryReferenceDiagnostic>,
    ) {
        (self.occurrences.into_vec(), self.diagnostics.into_vec())
    }

    fn push_path(&mut self, segment: BorrowedPathSegment<'schema>) -> Result<()> {
        let Some(slot) = self.path.get_mut(self.path_len) else {
            return Err(BinaryError::invalid_data(format!(
                "binary reference field path exceeds {MAX_TYPE_TREE_DEPTH} segments"
            )));
        };
        *slot = segment;
        self.path_len += 1;
        Ok(())
    }

    fn snapshot_path(&self, cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<FieldPath> {
        cursor.consume_members(usize_to_u64(
            self.path_len,
            "binary reference field path length",
        )?)?;
        let mut segments = cursor.vector(self.path_len, "binary reference field path")?;
        for segment in &self.path[..self.path_len] {
            let segment = match segment {
                BorrowedPathSegment::Field(name) => {
                    let name = cursor.clone_string(name, "binary reference field name")?;
                    FieldPathSegment::field(name).map_err(|error| {
                        BinaryError::invalid_data(format!(
                            "invalid binary reference field path: {error}"
                        ))
                    })?
                }
                BorrowedPathSegment::Index(index) => FieldPathSegment::Index(*index),
            };
            segments.push(cursor, segment)?;
        }
        FieldPath::from_segments(segments.into_vec()).map_err(|error| {
            BinaryError::invalid_data(format!("invalid binary reference field path: {error}"))
        })
    }
}

impl<'schema> ReferenceSink<'schema> for CollectReferences<'schema> {
    fn emit(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        cursor.consume_members(1)?;
        let field_path = self.snapshot_path(cursor)?;
        self.occurrences.push(
            cursor,
            BinaryReferenceOccurrence {
                field_path,
                file_id,
                path_id,
            },
        )?;
        if path_id != 0 {
            cursor.record_pptrs(1)?;
        }
        Ok(())
    }

    fn enter_record_child(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        kind: SemanticKind,
        index: usize,
        child: SchemaNode<'schema>,
    ) -> Result<()> {
        if kind == SemanticKind::Pair {
            let index = u32::try_from(index)
                .map_err(|_| BinaryError::invalid_data("pair index does not fit in u32"))?;
            return self.push_path(BorrowedPathSegment::Index(index));
        }
        if child.name().is_empty() {
            let index = u32::try_from(index).map_err(|_| {
                BinaryError::invalid_data("unnamed record child index does not fit in u32")
            })?;
            return self.push_path(BorrowedPathSegment::Index(index));
        }
        self.push_path(BorrowedPathSegment::Field(child.name()))
    }

    fn enter_sequence_element(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        index: usize,
    ) -> Result<()> {
        let index = u32::try_from(index)
            .map_err(|_| BinaryError::invalid_data("sequence index does not fit in u32"))?;
        self.push_path(BorrowedPathSegment::Index(index))
    }

    fn path_checkpoint(&self) -> usize {
        self.path_len
    }

    fn restore_path(&mut self, checkpoint: usize) {
        debug_assert!(checkpoint <= self.path_len);
        self.path_len = checkpoint;
    }

    fn warning(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        error: &BinaryError,
    ) -> Result<()> {
        cursor.consume_members(1)?;
        let field_path = self.snapshot_path(cursor)?;
        let message = cursor.display_string(error, "binary reference diagnostic")?;
        self.diagnostics.push(
            cursor,
            BinaryReferenceDiagnostic {
                field_path,
                message,
            },
        )
    }
}

impl<'schema, S: ReferenceSink<'schema>> TraversalAdapter<'schema> for UnitAdapter<S> {
    type Value = ();
    type Sequence = ();
    type Record = ();

    fn parse_mode(&self) -> TypeTreeParseMode {
        self.mode
    }

    fn scalar(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _value: WirePrimitive,
    ) -> Result<Self::Value> {
        Ok(())
    }

    fn string_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value> {
        cursor.skip_bytes(length)
    }

    fn captured_string(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _value: &str,
    ) -> Result<Self::Value> {
        Ok(())
    }

    fn bytes_payload(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        length: usize,
    ) -> Result<Self::Value> {
        cursor.skip_bytes(length)
    }

    fn bulk_sequence(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        primitive: PrimitiveKind,
        length: usize,
    ) -> Result<Self::Value> {
        cursor.with_wire_slice(bulk_byte_length(primitive, length)?, |_, _| Ok(()))
    }

    fn begin_sequence(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _length: usize,
    ) -> Result<Self::Sequence> {
        Ok(())
    }

    fn push_sequence(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _sequence: &mut Self::Sequence,
        _value: Self::Value,
    ) -> Result<()> {
        Ok(())
    }

    fn finish_sequence(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _sequence: Self::Sequence,
    ) -> Result<Self::Value> {
        Ok(())
    }

    fn begin_record(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _kind: SemanticKind,
        _child_count: usize,
        _label: &'static str,
    ) -> Result<Self::Record> {
        Ok(())
    }

    fn push_record(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _record: &mut Self::Record,
        _child: SchemaNode<'schema>,
        _value: Self::Value,
    ) -> Result<()> {
        Ok(())
    }

    fn recovered_record_field(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _record: &mut Self::Record,
        _child: SchemaNode<'schema>,
    ) -> Result<()> {
        Ok(())
    }

    fn finish_record(
        &mut self,
        _cursor: &mut TraversalCursor<'_, '_, '_>,
        _record: Self::Record,
    ) -> Result<Self::Value> {
        Ok(())
    }

    fn null(&mut self, _cursor: &mut TraversalCursor<'_, '_, '_>) -> Result<Self::Value> {
        Ok(())
    }

    fn emit_pptr(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        file_id: i32,
        path_id: i64,
    ) -> Result<()> {
        self.sink.emit(cursor, file_id, path_id)
    }

    fn enter_record_child(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        kind: SemanticKind,
        index: usize,
        child: SchemaNode<'schema>,
    ) -> Result<()> {
        self.sink.enter_record_child(cursor, kind, index, child)
    }

    fn enter_sequence_element(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        index: usize,
    ) -> Result<()> {
        self.sink.enter_sequence_element(cursor, index)
    }

    fn path_checkpoint(&self) -> usize {
        self.sink.path_checkpoint()
    }

    fn restore_path(&mut self, checkpoint: usize) {
        self.sink.restore_path(checkpoint);
    }

    fn warning(
        &mut self,
        cursor: &mut TraversalCursor<'_, '_, '_>,
        _child: SchemaNode<'schema>,
        error: &BinaryError,
    ) -> Result<()> {
        self.sink.warning(cursor, error)
    }
}

fn traverse_value<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    cursor.enter_node(depth)?;
    match node.semantic_layout() {
        SemanticLayout::Scalar(primitive) => {
            let wire = read_wire_primitive(cursor, primitive)?;
            let value = adapter.scalar(cursor, wire)?;
            align_completed_node(cursor, node)?;
            Ok(TraverseOutcome::Complete(value))
        }
        SemanticLayout::String => {
            let length = read_length(cursor, "string", BinaryReader::DEFAULT_MAX_STRING_LEN)?;
            let value = adapter.string_payload(cursor, length)?;
            align_completed_node(cursor, node)?;
            Ok(TraverseOutcome::Complete(value))
        }
        SemanticLayout::TypelessData => {
            let length = read_length(cursor, "TypelessData", usize::MAX)?;
            let value = adapter.bytes_payload(cursor, length)?;
            align_completed_node(cursor, node)?;
            Ok(TraverseOutcome::Complete(value))
        }
        SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout) => {
            traverse_sequence(schema, cursor, adapter, node, layout, context, depth)
        }
        SemanticLayout::Record | SemanticLayout::ManagedRegistry => {
            traverse_record(schema, cursor, adapter, node, context, depth)
        }
        SemanticLayout::Pair(layout) => {
            traverse_pair(schema, cursor, adapter, node, layout, context, depth)
        }
        SemanticLayout::PPtr(layout) => {
            traverse_pptr(schema, cursor, adapter, node, layout, context, depth)
        }
        SemanticLayout::ReferencedObject(layout) => {
            traverse_referenced_object(schema, cursor, adapter, node, layout, context, depth)
        }
        SemanticLayout::ManagedPayload => Err(BinaryError::invalid_data(format!(
            "Managed payload '{}' has no statically provable extent",
            node.name()
        ))),
        SemanticLayout::OpaqueFixed { byte_size } => {
            let length = usize::try_from(byte_size).map_err(|_| {
                BinaryError::invalid_data("Opaque TypeTree extent does not fit usize")
            })?;
            let value = adapter.bytes_payload(cursor, length)?;
            align_completed_node(cursor, node)?;
            Ok(TraverseOutcome::Complete(value))
        }
    }
}

fn traverse_sequence<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    layout: SequenceLayout<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    let raw_length = cursor.read_i32()?;
    if raw_length < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Negative TypeTree sequence length: {raw_length}"
        )));
    }
    let length = usize::try_from(raw_length)
        .map_err(|_| BinaryError::invalid_data("TypeTree sequence length does not fit usize"))?;
    cursor.consume_members(usize_to_u64(length, "sequence length")?)?;
    let child_depth = next_depth(depth)?;

    if let Some(primitive) = layout.bulk_primitive() {
        if length != 0 {
            cursor.enter_nodes(
                child_depth,
                usize_to_u64(length, "bulk sequence element count")?,
            )?;
        }
        let value = adapter.bulk_sequence(cursor, primitive, length)?;
        align_completed_node(cursor, node)?;
        return Ok(TraverseOutcome::Complete(value));
    }

    let mut sequence = adapter.begin_sequence(cursor, length)?;
    for index in 0..length {
        let path_checkpoint = adapter.path_checkpoint();
        adapter.enter_sequence_element(cursor, index)?;
        let outcome = traverse_value(
            schema,
            cursor,
            adapter,
            layout.element(),
            context,
            child_depth,
        )?;
        adapter.restore_path(path_checkpoint);
        match outcome {
            TraverseOutcome::Complete(value) => {
                adapter.push_sequence(cursor, &mut sequence, value)?;
            }
            TraverseOutcome::Terminal => return Ok(TraverseOutcome::Terminal),
        }
    }
    let value = adapter.finish_sequence(cursor, sequence)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete(value))
}

fn traverse_record<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    traverse_record_children(
        schema,
        cursor,
        adapter,
        node,
        node.kind(),
        node.children(),
        node.child_count(),
        context,
        depth,
    )
}

fn traverse_pair<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    layout: PairLayout<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    traverse_record_children(
        schema,
        cursor,
        adapter,
        node,
        SemanticKind::Pair,
        layout.children(),
        2,
        context,
        depth,
    )
}

#[allow(clippy::too_many_arguments)]
fn traverse_record_children<'schema, A, I>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    kind: SemanticKind,
    children: I,
    child_count: usize,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>>
where
    A: TraversalAdapter<'schema>,
    I: IntoIterator<Item = SchemaNode<'schema>>,
{
    cursor.consume_members(usize_to_u64(child_count, "record child count")?)?;
    let mut record =
        adapter.begin_record(cursor, kind, child_count, "TypeTree record properties")?;
    let child_depth = next_depth(depth)?;

    for (index, child) in children.into_iter().enumerate() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let path_checkpoint = adapter.path_checkpoint();
        adapter.enter_record_child(cursor, kind, index, child)?;
        let checkpoint = cursor.checkpoint();
        let attempt = traverse_value(schema, cursor, adapter, child, child_context, child_depth);
        let child_result = recover_record_child(
            schema,
            cursor,
            adapter,
            child,
            child_context,
            child_depth,
            checkpoint,
            attempt,
        )?;
        adapter.restore_path(path_checkpoint);
        match child_result {
            ChildResult::Value(value) => {
                adapter.push_record(cursor, &mut record, child, value)?;
            }
            ChildResult::Skipped => {
                adapter.recovered_record_field(cursor, &mut record, child)?;
            }
            ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
        }
    }

    let value = adapter.finish_record(cursor, record)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete(value))
}

fn traverse_pptr<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    layout: PPtrLayout<'schema>,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    cursor.consume_members(usize_to_u64(node.child_count(), "PPtr child count")?)?;
    let mut record = adapter.begin_record(
        cursor,
        SemanticKind::PPtr,
        node.child_count(),
        "TypeTree PPtr properties",
    )?;
    let child_depth = next_depth(depth)?;
    let mut file_id = None;
    let mut path_id = None;

    for (index, child) in node.children().enumerate() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let path_checkpoint = adapter.path_checkpoint();
        adapter.enter_record_child(cursor, SemanticKind::PPtr, index, child)?;
        let checkpoint = cursor.checkpoint();
        if child == layout.file_child() || child == layout.path_child() {
            let primitive = if child == layout.file_child() {
                layout.file_primitive()
            } else {
                layout.path_primitive()
            };
            let attempt = traverse_integer_node(cursor, adapter, child, primitive, child_depth)
                .and_then(|outcome| match outcome {
                    TraverseOutcome::Complete((value, wire)) => {
                        let converted = if child == layout.file_child() {
                            PPtrInteger::File(wire.into_file_id()?)
                        } else {
                            PPtrInteger::Path(wire.into_path_id()?)
                        };
                        Ok(TraverseOutcome::Complete((value, converted)))
                    }
                    TraverseOutcome::Terminal => Ok(TraverseOutcome::Terminal),
                });
            let child_result = recover_record_child(
                schema,
                cursor,
                adapter,
                child,
                child_context,
                child_depth,
                checkpoint,
                attempt,
            )?;
            adapter.restore_path(path_checkpoint);
            match child_result {
                ChildResult::Value((value, converted)) => {
                    match converted {
                        PPtrInteger::File(value) => file_id = Some(value),
                        PPtrInteger::Path(value) => path_id = Some(value),
                    }
                    adapter.push_record(cursor, &mut record, child, value)?;
                }
                ChildResult::Skipped => {
                    adapter.recovered_record_field(cursor, &mut record, child)?;
                }
                ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
            }
            continue;
        }

        let attempt = traverse_value(schema, cursor, adapter, child, child_context, child_depth);
        let child_result = recover_record_child(
            schema,
            cursor,
            adapter,
            child,
            child_context,
            child_depth,
            checkpoint,
            attempt,
        )?;
        adapter.restore_path(path_checkpoint);
        match child_result {
            ChildResult::Value(value) => {
                adapter.push_record(cursor, &mut record, child, value)?;
            }
            ChildResult::Skipped => {
                adapter.recovered_record_field(cursor, &mut record, child)?;
            }
            ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
        }
    }

    if let (Some(file_id), Some(path_id)) = (file_id, path_id) {
        adapter.emit_pptr(cursor, file_id, path_id)?;
    }
    let value = adapter.finish_record(cursor, record)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete(value))
}

enum PPtrInteger {
    File(i32),
    Path(i64),
}

fn traverse_integer_node<'schema, A: TraversalAdapter<'schema>>(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    expected: PrimitiveKind,
    depth: u32,
) -> Result<TraverseOutcome<(A::Value, WirePrimitive)>> {
    cursor.enter_node(depth)?;
    let wire = read_wire_primitive(cursor, expected)?;
    let value = adapter.scalar(cursor, wire)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete((value, wire)))
}

#[derive(Default)]
struct ManagedTypeKey {
    class_name: Option<String>,
    namespace: Option<String>,
    assembly_name: Option<String>,
}

fn traverse_referenced_object<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    layout: ReferencedObjectLayout<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    cursor.consume_members(usize_to_u64(
        node.child_count(),
        "ReferencedObject child count",
    )?)?;
    let mut record = adapter.begin_record(
        cursor,
        SemanticKind::ReferencedObject,
        node.child_count(),
        "TypeTree referenced object properties",
    )?;
    let child_depth = next_depth(depth)?;
    let mut key = ManagedTypeKey::default();

    for (index, child) in node.children().enumerate() {
        let path_checkpoint = adapter.path_checkpoint();
        adapter.enter_record_child(cursor, SemanticKind::ReferencedObject, index, child)?;
        let checkpoint = cursor.checkpoint();
        if layout.is_type_node(child) {
            let attempt =
                traverse_managed_type(schema, cursor, adapter, child, layout, context, child_depth);
            let child_result = recover_record_child(
                schema,
                cursor,
                adapter,
                child,
                context,
                child_depth,
                checkpoint,
                attempt,
            )?;
            adapter.restore_path(path_checkpoint);
            match child_result {
                ChildResult::Value((value, parsed_key)) => {
                    key = parsed_key;
                    adapter.push_record(cursor, &mut record, child, value)?;
                }
                ChildResult::Skipped => {
                    adapter.recovered_record_field(cursor, &mut record, child)?;
                }
                ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
            }
            continue;
        }

        let attempt = if layout.is_payload(child) {
            traverse_managed_payload(
                schema,
                cursor,
                adapter,
                layout.payload(),
                &key,
                context,
                child_depth,
            )
        } else {
            traverse_value(schema, cursor, adapter, child, context, child_depth)
        };
        let child_result = recover_record_child(
            schema,
            cursor,
            adapter,
            child,
            context,
            child_depth,
            checkpoint,
            attempt,
        )?;
        adapter.restore_path(path_checkpoint);
        match child_result {
            ChildResult::Value(value) => {
                adapter.push_record(cursor, &mut record, child, value)?;
            }
            ChildResult::Skipped => {
                adapter.recovered_record_field(cursor, &mut record, child)?;
            }
            ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
        }
    }

    let value = adapter.finish_record(cursor, record)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete(value))
}

fn traverse_managed_type<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    layout: super::schema::ReferencedObjectLayout<'schema>,
    mut context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<(A::Value, ManagedTypeKey)>> {
    cursor.enter_node(depth)?;
    cursor.consume_members(usize_to_u64(
        node.child_count(),
        "managed type child count",
    )?)?;
    let mut record = adapter.begin_record(
        cursor,
        node.kind(),
        node.child_count(),
        "TypeTree managed type properties",
    )?;
    let child_depth = next_depth(depth)?;
    let mut key = ManagedTypeKey::default();

    for (index, child) in node.children().enumerate() {
        let Some(child_context) = context.descend(node, child) else {
            continue;
        };
        let path_checkpoint = adapter.path_checkpoint();
        adapter.enter_record_child(cursor, node.kind(), index, child)?;
        let checkpoint = cursor.checkpoint();
        let is_key_field = child == layout.class_field()
            || child == layout.namespace_field()
            || child == layout.assembly_field();
        if is_key_field {
            let attempt = traverse_captured_string(cursor, adapter, child, child_depth);
            let child_result = recover_record_child(
                schema,
                cursor,
                adapter,
                child,
                child_context,
                child_depth,
                checkpoint,
                attempt,
            )?;
            adapter.restore_path(path_checkpoint);
            match child_result {
                ChildResult::Value((value, text)) => {
                    if child == layout.class_field() {
                        key.class_name = Some(text);
                    } else if child == layout.namespace_field() {
                        key.namespace = Some(text);
                    } else {
                        key.assembly_name = Some(text);
                    }
                    adapter.push_record(cursor, &mut record, child, value)?;
                }
                ChildResult::Skipped => {
                    adapter.recovered_record_field(cursor, &mut record, child)?;
                }
                ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
            }
            continue;
        }

        let attempt = traverse_value(schema, cursor, adapter, child, child_context, child_depth);
        let child_result = recover_record_child(
            schema,
            cursor,
            adapter,
            child,
            child_context,
            child_depth,
            checkpoint,
            attempt,
        )?;
        adapter.restore_path(path_checkpoint);
        match child_result {
            ChildResult::Value(value) => {
                adapter.push_record(cursor, &mut record, child, value)?;
            }
            ChildResult::Skipped => {
                adapter.recovered_record_field(cursor, &mut record, child)?;
            }
            ChildResult::Terminal => return Ok(TraverseOutcome::Terminal),
        }
    }

    let value = adapter.finish_record(cursor, record)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete((value, key)))
}

fn traverse_captured_string<'schema, A: TraversalAdapter<'schema>>(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    node: SchemaNode<'schema>,
    depth: u32,
) -> Result<TraverseOutcome<(A::Value, String)>> {
    cursor.enter_node(depth)?;
    if node.kind() != SemanticKind::String {
        return Err(BinaryError::invalid_data(
            "Managed type key field is not a canonical string",
        ));
    }
    let length = read_length(
        cursor,
        "managed type string",
        BinaryReader::DEFAULT_MAX_STRING_LEN,
    )?;
    let text = String::from_utf8(cursor.read_bytes(length)?)?;
    let value = adapter.captured_string(cursor, &text)?;
    align_completed_node(cursor, node)?;
    Ok(TraverseOutcome::Complete((value, text)))
}

fn traverse_managed_payload<'schema, A: TraversalAdapter<'schema>>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    payload: ManagedPayload<'schema>,
    key: &ManagedTypeKey,
    context: TypeTreeTraversalContext,
    depth: u32,
) -> Result<TraverseOutcome<A::Value>> {
    if key.class_name.as_deref() == Some("") {
        cursor.enter_node(depth)?;
        return Ok(TraverseOutcome::Complete(adapter.null(cursor)?));
    }

    if let (Some(class_name), Some(namespace), Some(assembly_name)) = (
        key.class_name.as_deref(),
        key.namespace.as_deref(),
        key.assembly_name.as_deref(),
    ) && let Some(root) = schema.resolve_managed_root(class_name, namespace, assembly_name)
    {
        return traverse_value(schema, cursor, adapter, root, context, depth);
    }

    if let Some(fallback) = payload.fallback() {
        return traverse_value(schema, cursor, adapter, fallback, context, depth);
    }

    cursor.enter_node(depth)?;
    let key_description = match (
        key.class_name.as_deref(),
        key.namespace.as_deref(),
        key.assembly_name.as_deref(),
    ) {
        (Some(class_name), Some(namespace), Some(assembly_name)) => {
            format!("{class_name}|{namespace}|{assembly_name}")
        }
        _ => "<incomplete managed type key>".to_string(),
    };
    Err(BinaryError::invalid_data(format!(
        "Managed type '{key_description}' is unresolved and its payload extent is unknown"
    )))
}

fn recover_record_child<'schema, A: TraversalAdapter<'schema>, T>(
    schema: &'schema TypeTreeSchema,
    cursor: &mut TraversalCursor<'_, '_, '_>,
    adapter: &mut A,
    child: SchemaNode<'schema>,
    context: TypeTreeTraversalContext,
    depth: u32,
    checkpoint: TraversalCheckpoint,
    attempt: Result<TraverseOutcome<T>>,
) -> Result<ChildResult<T>> {
    match attempt {
        Ok(TraverseOutcome::Complete(value)) => Ok(ChildResult::Value(value)),
        Ok(TraverseOutcome::Terminal) => Ok(ChildResult::Terminal),
        Err(error) if error.is_resource_error() => Err(error),
        Err(error) if adapter.parse_mode() == TypeTreeParseMode::Strict => Err(error),
        Err(error) => {
            cursor.restore(checkpoint)?;
            let mut skipper = UnitAdapter::strict(IgnorePPtrs);
            let skipped = traverse_value(schema, cursor, &mut skipper, child, context, depth);
            match skipped {
                Ok(TraverseOutcome::Complete(())) => {
                    adapter.warning(cursor, child, &error)?;
                    Ok(ChildResult::Skipped)
                }
                Ok(TraverseOutcome::Terminal) => Err(BinaryError::invalid_data(
                    "Strict TypeTree recovery skip terminated without an error",
                )),
                Err(skip_error) if skip_error.is_resource_error() => Err(skip_error),
                Err(_) => {
                    adapter.warning(cursor, child, &error)?;
                    Ok(ChildResult::Terminal)
                }
            }
        }
    }
}

fn read_wire_primitive(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    primitive: PrimitiveKind,
) -> Result<WirePrimitive> {
    let value = match primitive {
        PrimitiveKind::Bool => WirePrimitive::Bool(cursor.read_bool()?),
        PrimitiveKind::I8 => WirePrimitive::Signed(i64::from(cursor.read_i8()?)),
        PrimitiveKind::U8 => WirePrimitive::Unsigned(u64::from(cursor.read_u8()?)),
        PrimitiveKind::I16 => WirePrimitive::Signed(i64::from(cursor.read_i16()?)),
        PrimitiveKind::U16 => WirePrimitive::Unsigned(u64::from(cursor.read_u16()?)),
        PrimitiveKind::I32 => WirePrimitive::Signed(i64::from(cursor.read_i32()?)),
        PrimitiveKind::U32 => WirePrimitive::Unsigned(u64::from(cursor.read_u32()?)),
        PrimitiveKind::I64 => WirePrimitive::Signed(cursor.read_i64()?),
        PrimitiveKind::U64 => WirePrimitive::Unsigned(cursor.read_u64()?),
        PrimitiveKind::F32 => WirePrimitive::Float(f64::from(cursor.read_f32()?)),
        PrimitiveKind::F64 => WirePrimitive::Float(cursor.read_f64()?),
    };
    cursor.record_scalar_elements(1)?;
    Ok(value)
}

fn decode_wire_primitive(
    primitive: PrimitiveKind,
    bytes: &[u8],
    byte_order: ByteOrder,
) -> Result<WirePrimitive> {
    if bytes.len() != usize::from(primitive.width()) {
        return Err(BinaryError::invalid_data(
            "Primitive bulk decoder received an invalid element width",
        ));
    }
    macro_rules! ordered {
        ($type:ty, $from_le:ident, $from_be:ident) => {{
            let array: [u8; std::mem::size_of::<$type>()] = bytes.try_into().map_err(|_| {
                BinaryError::invalid_data("Primitive bulk element has an invalid width")
            })?;
            match byte_order {
                ByteOrder::Little => <$type>::$from_le(array),
                ByteOrder::Big => <$type>::$from_be(array),
            }
        }};
    }

    Ok(match primitive {
        PrimitiveKind::Bool => WirePrimitive::Bool(bytes[0] != 0),
        PrimitiveKind::I8 => WirePrimitive::Signed(i64::from(bytes[0] as i8)),
        PrimitiveKind::U8 => WirePrimitive::Unsigned(u64::from(bytes[0])),
        PrimitiveKind::I16 => {
            WirePrimitive::Signed(i64::from(ordered!(i16, from_le_bytes, from_be_bytes)))
        }
        PrimitiveKind::U16 => {
            WirePrimitive::Unsigned(u64::from(ordered!(u16, from_le_bytes, from_be_bytes)))
        }
        PrimitiveKind::I32 => {
            WirePrimitive::Signed(i64::from(ordered!(i32, from_le_bytes, from_be_bytes)))
        }
        PrimitiveKind::U32 => {
            WirePrimitive::Unsigned(u64::from(ordered!(u32, from_le_bytes, from_be_bytes)))
        }
        PrimitiveKind::I64 => WirePrimitive::Signed(ordered!(i64, from_le_bytes, from_be_bytes)),
        PrimitiveKind::U64 => WirePrimitive::Unsigned(ordered!(u64, from_le_bytes, from_be_bytes)),
        PrimitiveKind::F32 => {
            WirePrimitive::Float(f64::from(ordered!(f32, from_le_bytes, from_be_bytes)))
        }
        PrimitiveKind::F64 => WirePrimitive::Float(ordered!(f64, from_le_bytes, from_be_bytes)),
    })
}

fn read_length(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    label: &str,
    maximum: usize,
) -> Result<usize> {
    let raw = cursor.read_i32()?;
    if raw < 0 {
        return Err(BinaryError::invalid_data(format!(
            "Negative {label} length: {raw}"
        )));
    }
    let length = usize::try_from(raw)
        .map_err(|_| BinaryError::invalid_data(format!("{label} length does not fit usize")))?;
    if length > maximum {
        return Err(BinaryError::invalid_data(format!(
            "{label} length {length} exceeds limit {maximum}"
        )));
    }
    Ok(length)
}

fn bulk_byte_length(primitive: PrimitiveKind, length: usize) -> Result<usize> {
    length
        .checked_mul(usize::from(primitive.width()))
        .ok_or_else(|| BinaryError::invalid_data("Primitive sequence byte length overflow"))
}

fn align_completed_node(
    cursor: &mut TraversalCursor<'_, '_, '_>,
    node: SchemaNode<'_>,
) -> Result<()> {
    if node.align_after() {
        cursor.align()?;
    }
    Ok(())
}

fn next_depth(depth: u32) -> Result<u32> {
    depth
        .checked_add(1)
        .ok_or_else(|| BinaryError::invalid_data("TypeTree traversal depth overflow"))
}

fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BinaryError::invalid_data(format!("{label} does not fit in u64")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pptr_wire_roles_preserve_ranges_and_errors() {
        assert_eq!(
            WirePrimitive::Signed(i64::from(i32::MIN))
                .into_file_id()
                .unwrap(),
            i32::MIN
        );
        assert_eq!(
            WirePrimitive::Unsigned(i32::MAX as u64)
                .into_file_id()
                .unwrap(),
            i32::MAX
        );
        assert_eq!(
            WirePrimitive::Unsigned(i64::MAX as u64)
                .into_path_id()
                .unwrap(),
            i64::MAX
        );

        assert!(matches!(
            WirePrimitive::Unsigned(i32::MAX as u64 + 1).into_file_id(),
            Err(BinaryError::InvalidData(message))
                if message == "PPtr file ID does not fit in i32"
        ));
        assert!(matches!(
            WirePrimitive::Unsigned(i64::MAX as u64 + 1).into_path_id(),
            Err(BinaryError::InvalidData(message))
                if message == "PPtr path ID does not fit in i64"
        ));
        assert!(matches!(
            WirePrimitive::Float(0.0).into_file_id(),
            Err(BinaryError::InvalidData(message))
                if message == "PPtr file ID is not an integer"
        ));
        assert!(matches!(
            WirePrimitive::Bool(false).into_path_id(),
            Err(BinaryError::InvalidData(message))
                if message == "PPtr path ID is not an integer"
        ));
    }
}
