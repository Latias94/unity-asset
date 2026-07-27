//! Byte-preserving rewrites over canonical TypeTree schemas.

use std::mem::size_of;
use std::ops::Range;

use indexmap::IndexMap;
use unity_asset_binary::BinaryError;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    PPtrLayout, PairLayout, SchemaNode, SemanticKind, SemanticLayout, SequenceLayout,
    TypeTreeSchema, TypeTreeTraversalContext, TypeTreeTraversalStats, TypeTreeWriteError,
    TypeTreeWriteResult as Result,
};
use unity_asset_core::{AssetLoadBudget, UnityValue};

use super::output::TypeTreeOutput;
use super::primitives::{checked_i32_length, expect_pair, summarize_value, usize_to_u64};
use super::writer::{
    encode_object, validate_object_shape, validate_pptr_file_id, validate_pptr_path_id, write_value,
};
use crate::binary_writer::Endian;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TemplateRewriteStats {
    pub(crate) input: TypeTreeTraversalStats,
    pub(crate) output: TypeTreeTraversalStats,
    pub(crate) preserved_bytes: u64,
}

pub(crate) fn rewrite_object(
    schema: &TypeTreeSchema,
    properties: &IndexMap<String, UnityValue>,
    original_bytes: &[u8],
    endian: Endian,
    budget: &mut AssetLoadBudget,
) -> Result<(Vec<u8>, TemplateRewriteStats)> {
    if original_bytes.is_empty() {
        let (bytes, output) = encode_object(schema, properties, endian, budget)?;
        return Ok((
            bytes,
            TemplateRewriteStats {
                output,
                ..TemplateRewriteStats::default()
            },
        ));
    }

    let byte_order = match endian {
        Endian::Big => ByteOrder::Big,
        Endian::Little => ByteOrder::Little,
    };
    let mut reader = BinaryReader::new(original_bytes, byte_order);
    let mut planner = RewritePlanner::new(original_bytes);
    planner.plan_record(
        schema,
        schema.root(),
        properties,
        &mut reader,
        budget,
        TypeTreeTraversalContext::root(),
        0,
    )?;

    let root_end = position_to_usize(reader.position(), "TypeTree root extent")?;
    if root_end < original_bytes.len() {
        planner.push(
            RewriteAction::PreserveTail {
                range: root_end..original_bytes.len(),
            },
            budget,
        )?;
    }

    let (actions, input) = planner.finish();
    let original = original_bytes;
    let mut output = TypeTreeOutput::new(endian, budget);
    let mut preserved_bytes = 0_u64;

    for action in actions {
        match action {
            RewriteAction::PreserveOrEncode {
                range,
                node,
                value,
                context,
                depth,
            } => {
                if has_same_alignment_phase(output.position(), range.start) {
                    output.enter_node(depth)?;
                    copy_range(&mut output, original, range, &mut preserved_bytes)?;
                } else {
                    write_value(schema, node, value, endian, &mut output, context, depth)?;
                }
            }
            RewriteAction::PreserveOpaque { range, node, depth } => {
                output.enter_node(depth)?;
                if has_same_alignment_phase(output.position(), range.start)
                    || is_position_independent_opaque_leaf(node)
                {
                    copy_range(&mut output, original, range, &mut preserved_bytes)?;
                } else if matches!(node.kind(), SemanticKind::OpaqueFixed { byte_size: 0 }) {
                    if node.align_after() {
                        output.align_to(4)?;
                    }
                } else {
                    return Err(TypeTreeWriteError::invalid_value(format!(
                        "Cannot relocate byte-preserved unnamed TypeTree field '{}' across an alignment boundary",
                        node.type_name()
                    )));
                }
            }
            RewriteAction::Encode {
                node,
                value,
                context,
                depth,
            } => {
                write_value(schema, node, value, endian, &mut output, context, depth)?;
            }
            RewriteAction::EnterComposite { depth, members } => {
                output.enter_node(depth)?;
                output.consume_members(members)?;
            }
            RewriteAction::SequenceHeader {
                original_range,
                original_length,
                target_length,
                depth,
                members,
            } => {
                output.enter_node(depth)?;
                output.consume_members(members)?;
                if original_length == target_length
                    && has_same_alignment_phase(output.position(), original_range.start)
                {
                    copy_range(&mut output, original, original_range, &mut preserved_bytes)?;
                } else {
                    output.write_i32(target_length)?;
                }
            }
            RewriteAction::Align { original_padding } => {
                if has_same_alignment_phase(output.position(), original_padding.start) {
                    copy_range(
                        &mut output,
                        original,
                        original_padding,
                        &mut preserved_bytes,
                    )?;
                } else {
                    output.align_to(4)?;
                }
            }
            RewriteAction::PreserveTail { range } => {
                let expected = usize_to_u64(range.start, "TypeTree tail offset")?;
                if output.position() != expected {
                    return Err(TypeTreeWriteError::invalid_value(format!(
                        "Cannot relocate {} trailing bytes outside the TypeTree schema",
                        range.len()
                    )));
                }
                copy_range(&mut output, original, range, &mut preserved_bytes)?;
            }
        }
    }

    let (bytes, output_stats) = output.finish();
    Ok((
        bytes,
        TemplateRewriteStats {
            input,
            output: output_stats,
            preserved_bytes,
        },
    ))
}

enum RewriteAction<'value, 'schema> {
    PreserveOrEncode {
        range: Range<usize>,
        node: SchemaNode<'schema>,
        value: &'value UnityValue,
        context: TypeTreeTraversalContext,
        depth: u32,
    },
    PreserveOpaque {
        range: Range<usize>,
        node: SchemaNode<'schema>,
        depth: u32,
    },
    Encode {
        node: SchemaNode<'schema>,
        value: &'value UnityValue,
        context: TypeTreeTraversalContext,
        depth: u32,
    },
    EnterComposite {
        depth: u32,
        members: u64,
    },
    SequenceHeader {
        original_range: Range<usize>,
        original_length: i32,
        target_length: i32,
        depth: u32,
        members: u64,
    },
    Align {
        original_padding: Range<usize>,
    },
    PreserveTail {
        range: Range<usize>,
    },
}

struct RewritePlan<'value, 'schema> {
    actions: Vec<RewriteAction<'value, 'schema>>,
    accounted_capacity: usize,
}

impl<'value, 'schema> RewritePlan<'value, 'schema> {
    fn new() -> Self {
        Self {
            actions: Vec::new(),
            accounted_capacity: 0,
        }
    }

    fn push(
        &mut self,
        action: RewriteAction<'value, 'schema>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        if self.actions.len() == self.accounted_capacity {
            let target_capacity = if self.accounted_capacity == 0 {
                1
            } else {
                self.accounted_capacity.checked_mul(2).ok_or_else(|| {
                    TypeTreeWriteError::invalid_value("TypeTree rewrite plan capacity overflow")
                })?
            };
            let additional_slots = target_capacity - self.accounted_capacity;
            let allocation = additional_slots
                .checked_mul(size_of::<RewriteAction<'value, 'schema>>())
                .ok_or_else(|| {
                    TypeTreeWriteError::invalid_value("TypeTree rewrite plan allocation overflow")
                })?;
            let allocation = usize_to_u64(allocation, "TypeTree rewrite action allocation")?;
            budget.check_bytes(allocation).map_err(|error| {
                TypeTreeWriteError::budget("check TypeTree rewrite plan allocation", error)
            })?;
            let reserve = target_capacity - self.actions.len();
            self.actions.try_reserve_exact(reserve).map_err(|error| {
                TypeTreeWriteError::allocation("reserve TypeTree rewrite actions", error)
            })?;
            budget.consume_bytes(allocation).map_err(|error| {
                TypeTreeWriteError::budget("charge TypeTree rewrite plan allocation", error)
            })?;
            self.accounted_capacity = target_capacity;
        }
        self.actions.push(action);
        Ok(())
    }
}

struct RewritePlanner<'bytes, 'value, 'schema> {
    original: &'bytes [u8],
    plan: RewritePlan<'value, 'schema>,
    input: TypeTreeTraversalStats,
}

struct InputSequenceHeader {
    range: Range<usize>,
    raw_length: i32,
    length: usize,
}

impl<'bytes, 'value, 'schema> RewritePlanner<'bytes, 'value, 'schema> {
    fn new(original: &'bytes [u8]) -> Self {
        Self {
            original,
            plan: RewritePlan::new(),
            input: TypeTreeTraversalStats::default(),
        }
    }

    fn finish(self) -> (Vec<RewriteAction<'value, 'schema>>, TypeTreeTraversalStats) {
        (self.plan.actions, self.input)
    }

    fn push(
        &mut self,
        action: RewriteAction<'value, 'schema>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        self.plan.push(action, budget)
    }

    fn plan_node(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        value: &'value UnityValue,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        match node.semantic_layout() {
            SemanticLayout::Record | SemanticLayout::ManagedRegistry => {
                let object = expect_object(node, value)?;
                self.plan_record(schema, node, object, reader, budget, context, depth)
            }
            SemanticLayout::Pair(layout) => {
                let values = expect_pair(node, value)?;
                self.plan_pair(schema, node, layout, values, reader, budget, context, depth)
            }
            SemanticLayout::PPtr(layout) if matches!(value, UnityValue::Object(_)) => {
                let object = expect_object(node, value)?;
                self.plan_pptr(schema, node, layout, object, reader, budget, context, depth)
            }
            SemanticLayout::Sequence(layout) | SemanticLayout::Map(layout)
                if layout.bulk_primitive().is_none() =>
            {
                let values = expect_sequence(node, value)?;
                self.plan_sequence(schema, node, layout, values, reader, budget, context, depth)
            }
            _ => self.plan_atomic(schema, node, value, reader, budget, context, depth),
        }
    }

    fn plan_record(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        object: &'value IndexMap<String, UnityValue>,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        mut context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        if !matches!(
            node.kind(),
            SemanticKind::Record | SemanticKind::ManagedRegistry
        ) {
            return Err(TypeTreeWriteError::invalid_value(format!(
                "TypeTree template root '{}' is not a record",
                node.name()
            )));
        }

        validate_object_shape(node, object, context)?;

        self.observe_input_composite(node, depth, budget)?;

        self.push(
            RewriteAction::EnterComposite {
                depth,
                members: usize_to_u64(node.child_count(), "TypeTree record child count")?,
            },
            budget,
        )?;
        let child_depth = next_depth(depth)?;

        for child in node.children() {
            let Some(child_context) = context.descend(node, child) else {
                continue;
            };
            if child.name().is_empty() {
                self.plan_opaque(schema, child, reader, budget, child_context, child_depth)?;
                continue;
            }
            let value = object.get(child.name()).ok_or_else(|| {
                TypeTreeWriteError::invalid_value(format!(
                    "Missing required field '{}' while template-rewriting '{}'",
                    child.name(),
                    node.name()
                ))
            })?;
            self.plan_node(
                schema,
                child,
                value,
                reader,
                budget,
                child_context,
                child_depth,
            )?;
        }

        self.plan_alignment(node, reader, budget)?;
        Ok(())
    }

    fn plan_pair(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        layout: PairLayout<'schema>,
        values: &'value [UnityValue],
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        self.observe_input_composite(node, depth, budget)?;

        self.push(RewriteAction::EnterComposite { depth, members: 2 }, budget)?;
        let child_depth = next_depth(depth)?;
        self.plan_node(
            schema,
            layout.first(),
            &values[0],
            reader,
            budget,
            context,
            child_depth,
        )?;
        self.plan_node(
            schema,
            layout.second(),
            &values[1],
            reader,
            budget,
            context,
            child_depth,
        )?;
        self.plan_alignment(node, reader, budget)
    }

    fn plan_pptr(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        layout: PPtrLayout<'schema>,
        object: &'value IndexMap<String, UnityValue>,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        mut context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        validate_object_shape(node, object, context)?;
        let file_id = object.get(layout.file_child().name()).ok_or_else(|| {
            TypeTreeWriteError::invalid_value(format!(
                "Missing required field '{}' while template-rewriting '{}'",
                layout.file_child().name(),
                node.name()
            ))
        })?;
        let path_id = object.get(layout.path_child().name()).ok_or_else(|| {
            TypeTreeWriteError::invalid_value(format!(
                "Missing required field '{}' while template-rewriting '{}'",
                layout.path_child().name(),
                node.name()
            ))
        })?;
        validate_pptr_file_id(node, file_id)?;
        validate_pptr_path_id(node, path_id)?;

        self.observe_input_composite(node, depth, budget)?;
        self.push(
            RewriteAction::EnterComposite {
                depth,
                members: usize_to_u64(node.child_count(), "TypeTree PPtr child count")?,
            },
            budget,
        )?;
        let child_depth = next_depth(depth)?;

        for child in node.children() {
            let Some(child_context) = context.descend(node, child) else {
                continue;
            };
            if child.name().is_empty() {
                self.plan_opaque(schema, child, reader, budget, child_context, child_depth)?;
                continue;
            }
            let value = object.get(child.name()).ok_or_else(|| {
                TypeTreeWriteError::invalid_value(format!(
                    "Missing required field '{}' while template-rewriting '{}'",
                    child.name(),
                    node.name()
                ))
            })?;
            self.plan_node(
                schema,
                child,
                value,
                reader,
                budget,
                child_context,
                child_depth,
            )?;
        }

        self.plan_alignment(node, reader, budget)
    }

    fn plan_sequence(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        layout: SequenceLayout<'schema>,
        values: &'value [UnityValue],
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        debug_assert!(layout.bulk_primitive().is_none());
        let header = self.read_sequence_header(node, reader, budget, depth)?;
        let target_length = checked_i32_length(values.len(), "TypeTree rewrite sequence")?;
        let target_members = usize_to_u64(values.len(), "TypeTree rewrite sequence length")?;
        self.push(
            RewriteAction::SequenceHeader {
                original_range: header.range,
                original_length: header.raw_length,
                target_length,
                depth,
                members: target_members,
            },
            budget,
        )?;

        let child_depth = next_depth(depth)?;
        let shared_length = header.length.min(values.len());
        for value in values.iter().take(shared_length) {
            let element_start = reader.position();
            self.plan_node(
                schema,
                layout.element(),
                value,
                reader,
                budget,
                context,
                child_depth,
            )?;
            checked_range(element_start, reader.position(), self.original.len())?;
        }

        for _ in shared_length..header.length {
            let element_start = reader.position();
            let stats = schema
                .skip_value_with_context(reader, budget, layout.element(), context, child_depth)
                .map_err(|error| {
                    template_input_error(node, "scan removed sequence element", error)
                })?;
            self.observe_input(stats)?;
            checked_range(element_start, reader.position(), self.original.len())?;
        }

        for value in values.iter().skip(header.length) {
            self.push(
                RewriteAction::Encode {
                    node: layout.element(),
                    value,
                    context,
                    depth: child_depth,
                },
                budget,
            )?;
        }

        self.plan_alignment(node, reader, budget)
    }

    fn read_sequence_header(
        &mut self,
        node: SchemaNode<'schema>,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<InputSequenceHeader> {
        const HEADER_BYTES: u64 = 4;

        let start = reader.position();
        let end = start.checked_add(HEADER_BYTES).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite sequence header position overflow")
        })?;
        let range = checked_range(start, end, self.original.len())?;
        let encoded: [u8; 4] = self
            .original
            .get(range.clone())
            .ok_or_else(|| {
                TypeTreeWriteError::invalid_value(
                    "TypeTree rewrite sequence header is outside its input",
                )
            })?
            .try_into()
            .map_err(|_| {
                TypeTreeWriteError::invalid_value(
                    "TypeTree rewrite sequence header has invalid extent",
                )
            })?;
        let raw_length = match reader.byte_order() {
            ByteOrder::Big => i32::from_be_bytes(encoded),
            ByteOrder::Little => i32::from_le_bytes(encoded),
        };
        let node_visits = self.input.node_visits.checked_add(1).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input node counter overflow")
        })?;
        let wire_bytes = self
            .input
            .wire_bytes
            .checked_add(HEADER_BYTES)
            .ok_or_else(|| {
                TypeTreeWriteError::invalid_value(
                    "TypeTree rewrite input wire byte counter overflow",
                )
            })?;

        budget
            .check_depth(depth)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input depth", error))?;
        budget
            .check_entries(1)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input node", error))?;
        budget.check_bytes(HEADER_BYTES).map_err(|error| {
            TypeTreeWriteError::budget("check TypeTree input sequence header", error)
        })?;

        budget
            .consume_entries(1)
            .map_err(|error| TypeTreeWriteError::budget("charge TypeTree input node", error))?;
        budget
            .observe_depth(depth)
            .map_err(|error| TypeTreeWriteError::budget("observe TypeTree input depth", error))?;
        budget.consume_bytes(HEADER_BYTES).map_err(|error| {
            TypeTreeWriteError::budget("charge TypeTree input sequence header", error)
        })?;
        reader.set_position(end).map_err(|error| {
            TypeTreeWriteError::binary("advance TypeTree input sequence header", error)
        })?;

        self.input.node_visits = node_visits;
        self.input.wire_bytes = wire_bytes;
        if raw_length < 0 {
            return Err(template_input_error(
                node,
                "read sequence length",
                BinaryError::invalid_data(format!(
                    "Negative TypeTree sequence length: {raw_length}"
                )),
            ));
        }
        let length = usize::try_from(raw_length).map_err(|_| {
            template_input_error(
                node,
                "read sequence length",
                BinaryError::invalid_data("TypeTree sequence length does not fit usize"),
            )
        })?;
        let members = usize_to_u64(length, "TypeTree input sequence length")?;
        let total_members = self.input.members.checked_add(members).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input member counter overflow")
        })?;
        budget.check_members(members).map_err(|error| {
            TypeTreeWriteError::budget("check TypeTree input sequence members", error)
        })?;
        budget.consume_members(members).map_err(|error| {
            TypeTreeWriteError::budget("charge TypeTree input sequence members", error)
        })?;
        self.input.members = total_members;
        Ok(InputSequenceHeader {
            range,
            raw_length,
            length,
        })
    }

    fn plan_atomic(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        value: &'value UnityValue,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        let start = reader.position();
        let (equal, stats) = schema
            .compare_value_with_context(reader, budget, node, context, depth, value)
            .map_err(|error| template_input_error(node, "compare value", error))?;
        self.observe_input(stats)?;
        let range = checked_range(start, reader.position(), self.original.len())?;
        if equal {
            self.push(
                RewriteAction::PreserveOrEncode {
                    range,
                    node,
                    value,
                    context,
                    depth,
                },
                budget,
            )
        } else {
            self.push(
                RewriteAction::Encode {
                    node,
                    value,
                    context,
                    depth,
                },
                budget,
            )
        }
    }

    fn plan_opaque(
        &mut self,
        schema: &'schema TypeTreeSchema,
        node: SchemaNode<'schema>,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> Result<()> {
        let start = reader.position();
        let stats = schema
            .skip_value_with_context(reader, budget, node, context, depth)
            .map_err(|error| template_input_error(node, "scan unnamed field", error))?;
        self.observe_input(stats)?;
        let range = checked_range(start, reader.position(), self.original.len())?;
        self.push(RewriteAction::PreserveOpaque { range, node, depth }, budget)
    }

    fn plan_alignment(
        &mut self,
        node: SchemaNode<'schema>,
        reader: &mut BinaryReader<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        if !node.align_after() {
            return Ok(());
        }

        let payload_end = reader.position();
        let padding = (4 - payload_end % 4) % 4;
        let aligned_end = payload_end.checked_add(padding).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input alignment overflow")
        })?;
        let original_padding = checked_range(payload_end, aligned_end, self.original.len())?;
        let wire_bytes = self.input.wire_bytes.checked_add(padding).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input wire byte counter overflow")
        })?;
        budget
            .check_bytes(padding)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input padding", error))?;
        reader.set_position(aligned_end).map_err(|error| {
            TypeTreeWriteError::binary("advance TypeTree input alignment", error)
        })?;
        budget
            .consume_bytes(padding)
            .map_err(|error| TypeTreeWriteError::budget("charge TypeTree input padding", error))?;
        self.input.wire_bytes = wire_bytes;
        self.push(RewriteAction::Align { original_padding }, budget)
    }

    fn observe_input_composite(
        &mut self,
        node: SchemaNode<'schema>,
        depth: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let members = usize_to_u64(node.child_count(), "TypeTree composite child count")?;
        let node_visits = self.input.node_visits.checked_add(1).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input node counter overflow")
        })?;
        let total_members = self.input.members.checked_add(members).ok_or_else(|| {
            TypeTreeWriteError::invalid_value("TypeTree rewrite input member counter overflow")
        })?;

        budget
            .check_depth(depth)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input depth", error))?;
        budget
            .check_entries(1)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input node", error))?;
        budget
            .check_members(members)
            .map_err(|error| TypeTreeWriteError::budget("check TypeTree input members", error))?;
        budget
            .consume_entries(1)
            .map_err(|error| TypeTreeWriteError::budget("charge TypeTree input node", error))?;
        budget
            .consume_members(members)
            .map_err(|error| TypeTreeWriteError::budget("charge TypeTree input members", error))?;
        budget
            .observe_depth(depth)
            .map_err(|error| TypeTreeWriteError::budget("observe TypeTree input depth", error))?;

        self.input.node_visits = node_visits;
        self.input.members = total_members;
        Ok(())
    }

    fn observe_input(&mut self, stats: TypeTreeTraversalStats) -> Result<()> {
        self.input = self.input.checked_add(stats).map_err(|error| {
            TypeTreeWriteError::invalid_value(format!(
                "TypeTree rewrite input statistic overflow: {}",
                error.field()
            ))
        })?;
        Ok(())
    }
}

fn copy_range(
    output: &mut TypeTreeOutput<'_>,
    original: &[u8],
    range: Range<usize>,
    preserved_bytes: &mut u64,
) -> Result<()> {
    let bytes = original.get(range.clone()).ok_or_else(|| {
        TypeTreeWriteError::invalid_value(format!(
            "TypeTree rewrite produced an invalid original byte range {}..{}",
            range.start, range.end
        ))
    })?;
    output.write_bytes(bytes)?;
    *preserved_bytes = preserved_bytes
        .checked_add(usize_to_u64(bytes.len(), "preserved TypeTree bytes")?)
        .ok_or_else(|| {
            TypeTreeWriteError::invalid_value("preserved TypeTree byte count overflow")
        })?;
    Ok(())
}

fn expect_object<'value>(
    node: SchemaNode<'_>,
    value: &'value UnityValue,
) -> Result<&'value IndexMap<String, UnityValue>> {
    match value {
        UnityValue::Object(object) => Ok(object),
        _ => Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree record '{}' requires an Object, got {}",
            node.name(),
            summarize_value(value)
        ))),
    }
}

fn expect_sequence<'value>(
    node: SchemaNode<'_>,
    value: &'value UnityValue,
) -> Result<&'value [UnityValue]> {
    match value {
        UnityValue::Array(values) => Ok(values),
        _ => Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree sequence '{}' requires an Array, got {}",
            node.name(),
            summarize_value(value)
        ))),
    }
}

fn template_input_error(
    node: SchemaNode<'_>,
    operation: &'static str,
    error: BinaryError,
) -> TypeTreeWriteError {
    TypeTreeWriteError::malformed_template(
        format!("{} ({})", node.name(), node.type_name()),
        operation,
        error,
    )
}

fn checked_range(start: u64, end: u64, len: usize) -> Result<Range<usize>> {
    let start = position_to_usize(start, "TypeTree range start")?;
    let end = position_to_usize(end, "TypeTree range end")?;
    if start > end || end > len {
        return Err(TypeTreeWriteError::invalid_value(format!(
            "TypeTree rewrite range {start}..{end} exceeds original length {len}"
        )));
    }
    Ok(start..end)
}

fn has_same_alignment_phase(output_position: u64, original_position: usize) -> bool {
    output_position % 4 == (original_position as u64) % 4
}

fn is_position_independent_opaque_leaf(node: SchemaNode<'_>) -> bool {
    matches!(node.kind(), SemanticKind::OpaqueFixed { byte_size } if byte_size != 0)
        && node.child_count() == 0
        && !node.align_after()
}

fn next_depth(depth: u32) -> Result<u32> {
    depth
        .checked_add(1)
        .ok_or_else(|| TypeTreeWriteError::invalid_value("TypeTree rewrite depth overflow"))
}

fn position_to_usize(value: u64, label: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        TypeTreeWriteError::invalid_value(format!("{label} does not fit usize: {value}"))
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{aligned, map, node, pptr, record, sequence};
    use super::*;
    use unity_asset_binary::asset::SerializedType;
    use unity_asset_binary::typetree::{
        TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions,
    };
    use unity_asset_core::{AssetLoadLimits, BudgetError};

    fn registry(name: &str) -> TypeTreeNode {
        let mut registry = node("ManagedReferencesRegistry", name);
        registry.children.push(node("int", "m_Version"));
        registry
    }

    fn schema(root: TypeTreeNode, budget: &mut AssetLoadBudget) -> TypeTreeSchema {
        schema_with_refs(root, &[], budget)
    }

    fn schema_with_refs(
        root: TypeTreeNode,
        ref_types: &[SerializedType],
        budget: &mut AssetLoadBudget,
    ) -> TypeTreeSchema {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        TypeTreeSchema::compile(&tree, ref_types, budget).unwrap()
    }

    #[test]
    fn no_op_rewrite_is_byte_identical_including_nonzero_padding() {
        let mut root = record(vec![node("UInt8", "m_Value")]);
        root.meta_flags = 0x4000;
        let original = [0x7f, 0xaa, 0xbb, 0xcc];
        let mut properties = IndexMap::new();
        properties.insert("m_Value".to_string(), UnityValue::Integer(0x7f));
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(rewritten, original);
        assert_eq!(stats.preserved_bytes, original.len() as u64);
    }

    #[test]
    fn rewrite_promotes_compare_budget_errors() {
        let properties = IndexMap::from([("m_Value".to_owned(), UnityValue::Unsigned(7))]);
        let original = 7_u32.to_le_bytes();
        let mut schema_budget = AssetLoadBudget::default();
        let schema = schema(record(vec![node("UInt32", "m_Value")]), &mut schema_budget);
        let plan_allocation = size_of::<RewriteAction<'static, 'static>>() as u64;
        let mut rewrite_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: plan_allocation + 3,
            ..AssetLoadLimits::default()
        })
        .expect("valid rewrite budget");

        assert!(matches!(
            rewrite_object(
                &schema,
                &properties,
                &original,
                Endian::Little,
                &mut rewrite_budget,
            ),
            Err(TypeTreeWriteError::Budget {
                operation: "compare value",
                source: BudgetError::Exceeded {
                    resource: "bytes",
                    limit,
                    requested,
                },
            }) if limit == plan_allocation + 3 && requested == plan_allocation + 4
        ));
    }

    #[test]
    fn rewrite_action_plan_grows_geometrically_under_the_budget() {
        let mut plan: RewritePlan<'static, 'static> = RewritePlan::new();
        let mut budget = AssetLoadBudget::default();

        for _ in 0..5 {
            plan.push(
                RewriteAction::EnterComposite {
                    depth: 0,
                    members: 0,
                },
                &mut budget,
            )
            .unwrap();
        }

        assert_eq!(plan.accounted_capacity, 8);
        assert!(plan.actions.capacity() >= 8);
        assert_eq!(
            budget.usage().bytes,
            (8 * size_of::<RewriteAction<'static, 'static>>()) as u64
        );
    }

    #[test]
    fn nested_rewrite_planning_scans_each_input_node_once() {
        let leaf = node("int", "m_Value");
        let level_two = record(vec![leaf]);
        let mut level_one = node("Nested", "m_LevelTwo");
        level_one.children = level_two.children;
        let mut outer = node("Nested", "m_LevelOne");
        outer.children.push(level_one);
        let root = record(vec![outer]);
        let properties = IndexMap::from([(
            "m_LevelOne".to_owned(),
            UnityValue::Object(IndexMap::from([(
                "m_LevelTwo".to_owned(),
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(42),
                )])),
            )])),
        )]);
        let original = 42_i32.to_le_bytes();
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(rewritten, original);
        assert_eq!(stats.input.node_visits, 4);
        assert_eq!(stats.input.wire_bytes, 4);
        assert_eq!(stats.output.node_visits, 4);
    }

    #[test]
    fn nested_template_rewrite_rejects_unrepresentable_extra_fields() {
        let mut nested = node("Nested", "m_Nested");
        nested.children.push(node("int", "m_Value"));
        let root = record(vec![nested]);
        let properties = IndexMap::from([(
            "m_Nested".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_Value".to_owned(), UnityValue::Integer(42)),
                ("m_Extra".to_owned(), UnityValue::Integer(7)),
            ])),
        )]);
        let original = 42_i32.to_le_bytes();
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        assert!(matches!(
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget,),
            Err(TypeTreeWriteError::Shape {
                expected_fields: 1,
                actual_fields: 2,
            })
        ));
    }

    #[test]
    fn managed_registry_context_is_shared_across_all_adapters() {
        let mut nested = node("Nested", "m_Nested");
        nested.children.push(registry("m_NestedRegistry"));
        nested.children.push(node("int", "m_Marker"));
        let mut pair = node("pair", "m_Pair");
        pair.children.push(registry("first"));
        pair.children.push(node("int", "second"));
        let mut pointer = pptr("m_Pointer");
        pointer.children.push(registry("m_PointerRegistry"));
        let registries = sequence("m_Registries", registry("data"));
        let mut root_registry = registry("m_RootRegistry");
        root_registry.children.push(registry("m_NestedRegistry"));
        let root = record(vec![root_registry, nested, pair, pointer, registries]);
        let properties = IndexMap::from([
            (
                "m_RootRegistry".to_owned(),
                UnityValue::Object(IndexMap::from([(
                    "m_Version".to_owned(),
                    UnityValue::Integer(7),
                )])),
            ),
            (
                "m_Nested".to_owned(),
                UnityValue::Object(IndexMap::from([(
                    "m_Marker".to_owned(),
                    UnityValue::Integer(0x1122_3344),
                )])),
            ),
            (
                "m_Pair".to_owned(),
                UnityValue::Array(vec![
                    UnityValue::Object(IndexMap::from([(
                        "m_Version".to_owned(),
                        UnityValue::Integer(9),
                    )])),
                    UnityValue::Integer(0x5566_7788),
                ]),
            ),
            (
                "m_Pointer".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("m_FileID".to_owned(), UnityValue::Integer(0)),
                    ("m_PathID".to_owned(), UnityValue::Integer(77)),
                ])),
            ),
            (
                "m_Registries".to_owned(),
                UnityValue::Array(vec![UnityValue::Object(IndexMap::from([(
                    "m_Version".to_owned(),
                    UnityValue::Integer(11),
                )]))]),
            ),
        ]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);
        let (encoded, _) =
            encode_object(&schema, &properties, Endian::Little, &mut budget).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&7_i32.to_le_bytes());
        expected.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
        expected.extend_from_slice(&9_i32.to_le_bytes());
        expected.extend_from_slice(&0x5566_7788_i32.to_le_bytes());
        expected.extend_from_slice(&0_i32.to_le_bytes());
        expected.extend_from_slice(&77_i64.to_le_bytes());
        expected.extend_from_slice(&1_i32.to_le_bytes());
        expected.extend_from_slice(&11_i32.to_le_bytes());
        assert_eq!(encoded, expected);

        let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
        let read = schema
            .read_object(
                &mut reader,
                &mut budget,
                unity_asset_binary::typetree::TypeTreeParseOptions {
                    mode: unity_asset_binary::typetree::TypeTreeParseMode::Strict,
                },
            )
            .unwrap();
        assert_eq!(read.properties, properties);
        assert_eq!(reader.position(), encoded.len() as u64);

        let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
        schema
            .skip_value(&mut reader, &mut budget, schema.root())
            .unwrap();
        assert_eq!(reader.position(), encoded.len() as u64);

        let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
        let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();
        assert_eq!(scan.internal, vec![77]);
        assert!(scan.external.is_empty());
        assert_eq!(reader.position(), encoded.len() as u64);

        let (rewritten, _) =
            rewrite_object(&schema, &properties, &encoded, Endian::Little, &mut budget).unwrap();
        assert_eq!(rewritten, encoded);
    }

    #[test]
    fn referenced_object_registry_context_distinguishes_direct_and_resolved_children() {
        fn pointer_value(path_id: i64) -> UnityValue {
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(0)),
                ("m_PathID".to_owned(), UnityValue::Integer(path_id)),
            ]))
        }

        fn managed_reference_value(include_resolved_registry: bool) -> UnityValue {
            let mut data = IndexMap::new();
            if include_resolved_registry {
                data.insert(
                    "m_ManagedRegistry".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_RegistryPtr".to_owned(),
                        pointer_value(202),
                    )])),
                );
            }
            data.insert("m_ManagedPtr".to_owned(), pointer_value(303));

            UnityValue::Object(IndexMap::from([
                (
                    "type".to_owned(),
                    UnityValue::Object(IndexMap::from([
                        ("class".to_owned(), UnityValue::String("Managed".to_owned())),
                        ("ns".to_owned(), UnityValue::String("Tests".to_owned())),
                        ("asm".to_owned(), UnityValue::String("Tests".to_owned())),
                    ])),
                ),
                (
                    "m_DirectRegistry".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_DirectPtr".to_owned(),
                        pointer_value(101),
                    )])),
                ),
                ("data".to_owned(), UnityValue::Object(data)),
            ]))
        }

        fn fixture(outer_registry_first: bool) -> (TypeTreeNode, SerializedType) {
            let mut direct_registry = node("ManagedReferencesRegistry", "m_DirectRegistry");
            direct_registry.children.push(pptr("m_DirectPtr"));

            let mut type_node = node("ReferencedObjectType", "type");
            type_node.children = vec![
                node("string", "class"),
                node("string", "ns"),
                node("string", "asm"),
            ];
            let mut referenced = node("ReferencedObject", "m_Reference");
            referenced.children = vec![
                type_node,
                direct_registry,
                node("ReferencedObjectData", "data"),
            ];

            let mut root_children = Vec::new();
            if outer_registry_first {
                root_children.push(registry("m_OuterRegistry"));
            }
            root_children.push(referenced);
            root_children.push(registry("m_TrailingRegistry"));
            root_children.push(node("int", "m_Marker"));

            let mut managed_root = node("Managed", "Managed");
            let mut managed_registry = node("ManagedReferencesRegistry", "m_ManagedRegistry");
            managed_registry.children.push(pptr("m_RegistryPtr"));
            managed_root.children = vec![managed_registry, pptr("m_ManagedPtr")];
            let mut managed_tree = TypeTree::new();
            managed_tree.add_node(managed_root);
            let mut managed_type = SerializedType::new(114);
            managed_type.class_name = "Managed".to_owned();
            managed_type.namespace = "Tests".to_owned();
            managed_type.assembly_name = "Tests".to_owned();
            managed_type.type_tree = managed_tree;

            (record(root_children), managed_type)
        }

        for outer_registry_first in [false, true] {
            let (root, managed_type) = fixture(outer_registry_first);
            let mut properties = IndexMap::new();
            if outer_registry_first {
                properties.insert(
                    "m_OuterRegistry".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_Version".to_owned(),
                        UnityValue::Integer(7),
                    )])),
                );
            }
            properties.insert(
                "m_Reference".to_owned(),
                managed_reference_value(!outer_registry_first),
            );
            if !outer_registry_first {
                properties.insert(
                    "m_TrailingRegistry".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_Version".to_owned(),
                        UnityValue::Integer(9),
                    )])),
                );
            }
            properties.insert("m_Marker".to_owned(), UnityValue::Integer(11));

            let mut budget = AssetLoadBudget::default();
            let schema = schema_with_refs(root, &[managed_type], &mut budget);
            let (encoded, _) =
                encode_object(&schema, &properties, Endian::Little, &mut budget).unwrap();

            let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
            let read = schema
                .read_object(
                    &mut reader,
                    &mut budget,
                    unity_asset_binary::typetree::TypeTreeParseOptions {
                        mode: unity_asset_binary::typetree::TypeTreeParseMode::Strict,
                    },
                )
                .unwrap();
            assert_eq!(read.properties, properties);
            assert_eq!(reader.position(), encoded.len() as u64);

            let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
            schema
                .skip_value(&mut reader, &mut budget, schema.root())
                .unwrap();
            assert_eq!(reader.position(), encoded.len() as u64);

            let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
            let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();
            let expected = if outer_registry_first {
                vec![101, 303]
            } else {
                vec![101, 202, 303]
            };
            assert_eq!(scan.internal, expected);
            assert!(scan.external.is_empty());
            assert_eq!(reader.position(), encoded.len() as u64);

            let (preserved, _) =
                rewrite_object(&schema, &properties, &encoded, Endian::Little, &mut budget)
                    .unwrap();
            assert_eq!(preserved, encoded);

            let reference = properties
                .get_mut("m_Reference")
                .and_then(UnityValue::as_object_mut)
                .unwrap();
            reference
                .get_mut("data")
                .and_then(UnityValue::as_object_mut)
                .unwrap()
                .insert("m_ManagedPtr".to_owned(), pointer_value(404));
            let (rewritten, _) =
                rewrite_object(&schema, &properties, &encoded, Endian::Little, &mut budget)
                    .unwrap();
            let mut reader = BinaryReader::new(&rewritten, ByteOrder::Little);
            assert_eq!(
                schema
                    .scan_pptrs(&mut reader, &mut budget)
                    .unwrap()
                    .internal,
                if outer_registry_first {
                    vec![101, 404]
                } else {
                    vec![101, 202, 404]
                }
            );
            assert_eq!(reader.position(), rewritten.len() as u64);
        }
    }

    #[test]
    fn changed_sibling_keeps_unchanged_subtree_bytes() {
        let root = record(vec![
            node("int", "m_Changed"),
            aligned(node("UInt8", "m_Kept")),
        ]);
        let mut original = 1_i32.to_le_bytes().to_vec();
        original.extend_from_slice(&[0x7f, 0xa1, 0xb2, 0xc3]);
        let mut properties = IndexMap::new();
        properties.insert("m_Changed".to_string(), UnityValue::Integer(2));
        properties.insert("m_Kept".to_string(), UnityValue::Integer(0x7f));
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, _) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        let mut expected = 2_i32.to_le_bytes().to_vec();
        expected.extend_from_slice(&[0x7f, 0xa1, 0xb2, 0xc3]);
        assert_eq!(rewritten, expected);
    }

    #[test]
    fn changed_sequence_element_preserves_each_unnamed_field_from_the_template() {
        let mut unnamed = node("Opaque16", "");
        unnamed.byte_size = 2;
        let mut element = node("Entry", "data");
        element.children = vec![unnamed, node("UInt16", "m_Value")];
        let root = record(vec![sequence("m_Entries", element)]);
        let properties = IndexMap::from([(
            "m_Entries".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x1122),
                )])),
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x5566),
                )])),
            ]),
        )]);
        let original = [
            2, 0, 0, 0, // sequence length
            0xaa, 0xbb, 0x22, 0x11, // element 0
            0xcc, 0xdd, 0x44, 0x33, // element 1
        ];
        let expected = [
            2, 0, 0, 0, // sequence length
            0xaa, 0xbb, 0x22, 0x11, // unchanged element 0
            0xcc, 0xdd, 0x66, 0x55, // edited element 1
        ];
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut rewrite_budget = AssetLoadBudget::default();

        let (rewritten, stats) = rewrite_object(
            &schema,
            &properties,
            &original,
            Endian::Little,
            &mut rewrite_budget,
        )
        .expect("existing sequence elements must retain unnamed template fields");

        assert_eq!(rewritten, expected);
        assert_eq!(stats.input.owned_bytes, 0);
        assert_eq!(stats.input.unity_values_materialized, 0);
    }

    #[test]
    fn shortened_sequence_drops_removed_elements_after_budgeted_input_scan() {
        let mut unnamed = node("Opaque16", "");
        unnamed.byte_size = 2;
        let mut element = node("Entry", "data");
        element.children = vec![unnamed, node("UInt16", "m_Value")];
        let root = record(vec![sequence("m_Entries", element)]);
        let properties = IndexMap::from([(
            "m_Entries".to_owned(),
            UnityValue::Array(vec![UnityValue::Object(IndexMap::from([(
                "m_Value".to_owned(),
                UnityValue::Integer(0x1122),
            )]))]),
        )]);
        let original = [
            2, 0, 0, 0, // sequence length
            0xaa, 0xbb, 0x22, 0x11, // retained element
            0xcc, 0xdd, 0x44, 0x33, // removed element
        ];
        let expected = [
            1, 0, 0, 0, // sequence length
            0xaa, 0xbb, 0x22, 0x11, // retained element
        ];
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut rewrite_budget = AssetLoadBudget::default();

        let (rewritten, stats) = rewrite_object(
            &schema,
            &properties,
            &original,
            Endian::Little,
            &mut rewrite_budget,
        )
        .unwrap();

        assert_eq!(rewritten, expected);
        assert_eq!(stats.input.wire_bytes, original.len() as u64);
        assert_eq!(stats.input.owned_bytes, 0);
        assert_eq!(stats.input.unity_values_materialized, 0);
    }

    #[test]
    fn appended_sequence_elements_require_complete_fresh_encoding() {
        let mut unnamed = node("Opaque16", "");
        unnamed.byte_size = 2;
        let mut element = node("Entry", "data");
        element.children = vec![unnamed, node("UInt16", "m_Value")];
        let root = record(vec![sequence("m_Entries", element)]);
        let properties = IndexMap::from([(
            "m_Entries".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x1122),
                )])),
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x3344),
                )])),
            ]),
        )]);
        let original = [
            1, 0, 0, 0, // sequence length
            0xaa, 0xbb, 0x22, 0x11, // existing element
        ];
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut rewrite_budget = AssetLoadBudget::default();

        let error = rewrite_object(
            &schema,
            &properties,
            &original,
            Endian::Little,
            &mut rewrite_budget,
        )
        .expect_err("a new element cannot borrow an unnamed field from an existing element");

        assert!(
            error
                .to_string()
                .contains("Fresh TypeTree encoding cannot represent an unnamed child")
        );
    }

    #[test]
    fn appended_representable_sequence_element_is_fresh_encoded() {
        let mut element = node("Entry", "data");
        element.children = vec![node("UInt16", "m_Value")];
        let root = record(vec![sequence("m_Entries", element)]);
        let properties = IndexMap::from([(
            "m_Entries".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x1122),
                )])),
                UnityValue::Object(IndexMap::from([(
                    "m_Value".to_owned(),
                    UnityValue::Integer(0x3344),
                )])),
            ]),
        )]);
        let original = [
            1, 0, 0, 0, // sequence length
            0x22, 0x11, // existing element
        ];
        let expected = [
            2, 0, 0, 0, // sequence length
            0x22, 0x11, // existing element
            0x44, 0x33, // appended element
        ];
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut rewrite_budget = AssetLoadBudget::default();

        let (rewritten, stats) = rewrite_object(
            &schema,
            &properties,
            &original,
            Endian::Little,
            &mut rewrite_budget,
        )
        .unwrap();

        assert_eq!(rewritten, expected);
        assert_eq!(stats.input.wire_bytes, original.len() as u64);
        assert_eq!(stats.input.unity_values_materialized, 0);
    }

    #[test]
    fn shifted_subtree_is_reencoded_at_its_new_alignment_phase() {
        let root = record(vec![
            sequence("m_Bytes", node("UInt8", "data")),
            aligned(node("UInt8", "m_Tail")),
        ]);
        let mut original = 1_i32.to_le_bytes().to_vec();
        original.extend_from_slice(&[0x11, 0x7f, 0xa1, 0xb2]);
        let mut properties = IndexMap::new();
        properties.insert("m_Bytes".to_string(), UnityValue::Bytes(vec![1, 2, 3, 4]));
        properties.insert("m_Tail".to_string(), UnityValue::Integer(0x7f));
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, _) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        let mut expected = 4_i32.to_le_bytes().to_vec();
        expected.extend_from_slice(&[1, 2, 3, 4, 0x7f, 0, 0, 0]);
        assert_eq!(rewritten, expected);
    }

    #[test]
    fn shifted_zero_extent_opaque_field_preserves_following_alignment() {
        let mut opaque = aligned(node("Unknown", ""));
        opaque.byte_size = 0;
        let root = record(vec![
            sequence("m_Bytes", node("UInt8", "data")),
            opaque,
            node("UInt8", "m_Tail"),
        ]);
        let original = [0_u8, 0, 0, 0, 0x7f];
        let properties = IndexMap::from([
            ("m_Bytes".to_owned(), UnityValue::Bytes(vec![0x11])),
            ("m_Tail".to_owned(), UnityValue::Integer(0x7f)),
        ]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, _) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(rewritten, [1, 0, 0, 0, 0x11, 0, 0, 0, 0x7f]);
    }

    #[test]
    fn shifted_position_independent_opaque_leaf_is_preserved_across_alignment_phase() {
        let mut opaque = node("Unknown", "");
        opaque.byte_size = 3;
        let root = record(vec![
            sequence("m_Bytes", node("UInt8", "data")),
            opaque,
            node("int", "m_Tail"),
        ]);
        let mut original = 0_i32.to_le_bytes().to_vec();
        original.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        original.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
        let properties = IndexMap::from([
            ("m_Bytes".to_owned(), UnityValue::Bytes(vec![0x11])),
            ("m_Tail".to_owned(), UnityValue::Integer(0x1122_3344)),
        ]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut budget = AssetLoadBudget::default();

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        let mut expected = 1_i32.to_le_bytes().to_vec();
        expected.push(0x11);
        expected.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        expected.extend_from_slice(&0x1122_3344_i32.to_le_bytes());
        assert_eq!(rewritten, expected);
        assert!(stats.preserved_bytes >= 3);
    }

    #[test]
    fn nested_unchanged_atomic_rewrite_observes_its_semantic_depth() {
        let mut nested = node("Nested", "m_Nested");
        nested.children.push(node("int", "m_Value"));
        let root = record(vec![nested]);
        let properties = IndexMap::from([(
            "m_Nested".to_owned(),
            UnityValue::Object(IndexMap::from([(
                "m_Value".to_owned(),
                UnityValue::Integer(42),
            )])),
        )]);
        let original = 42_i32.to_le_bytes();
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let scalar = schema.root().child(0).unwrap().child(0).unwrap();
        let depth_limits = AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        };

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let mut reader = BinaryReader::new(&original, ByteOrder::Little);
        let error = schema
            .read_value_with_context(
                &mut reader,
                &mut budget,
                scalar,
                TypeTreeTraversalContext::root(),
                2,
            )
            .unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(reader.position(), 0);

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let mut reader = BinaryReader::new(&original, ByteOrder::Little);
        let error = schema
            .skip_value_with_context(
                &mut reader,
                &mut budget,
                scalar,
                TypeTreeTraversalContext::root(),
                2,
            )
            .unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(reader.position(), 0);

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let mut reader = BinaryReader::new(&original, ByteOrder::Little);
        let error = schema
            .read_object(&mut reader, &mut budget, TypeTreeParseOptions::default())
            .unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(reader.position(), 0);

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let mut reader = BinaryReader::new(&original, ByteOrder::Little);
        let error = schema
            .skip_value(&mut reader, &mut budget, schema.root())
            .unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(reader.position(), 0);

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let mut reader = BinaryReader::new(&original, ByteOrder::Little);
        let error = schema.scan_pptrs(&mut reader, &mut budget).unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(reader.position(), 0);

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let error = encode_object(&schema, &properties, Endian::Little, &mut budget).unwrap_err();
        assert!(error.to_string().contains("depth"));

        let mut budget = AssetLoadBudget::new(depth_limits).unwrap();
        let error = rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget)
            .expect_err("depth-two atomic field must exceed max_depth one");

        assert!(error.to_string().contains("depth"));
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn unchanged_bulk_numeric_sequences_compare_without_unity_value_rematerialization() {
        let root = record(vec![
            sequence("m_Bytes", node("UInt8", "data")),
            sequence("m_SignedBytes", node("SInt8", "data")),
            sequence("m_Wide", node("int", "data")),
        ]);
        let properties = IndexMap::from([
            (
                "m_Bytes".to_owned(),
                UnityValue::Bytes((0_u8..=255).cycle().take(64 * 1024).collect()),
            ),
            (
                "m_SignedBytes".to_owned(),
                UnityValue::Bytes((0_u8..=255).rev().cycle().take(16 * 1024).collect()),
            ),
            (
                "m_Wide".to_owned(),
                UnityValue::Array(
                    (0..4096)
                        .map(|value| UnityValue::Integer(i64::from(value) - 2048))
                        .collect(),
                ),
            ),
        ]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let mut encode_budget = AssetLoadBudget::default();
        let (original, _) =
            encode_object(&schema, &properties, Endian::Little, &mut encode_budget).unwrap();
        let mut rewrite_budget = AssetLoadBudget::default();

        let (rewritten, stats) = rewrite_object(
            &schema,
            &properties,
            &original,
            Endian::Little,
            &mut rewrite_budget,
        )
        .unwrap();

        assert_eq!(rewritten, original);
        assert_eq!(stats.input.unity_values_materialized, 0);
        assert_eq!(stats.input.owned_bytes, 0);
        assert_eq!(stats.input.bulk_runs, 3);
        assert_eq!(
            stats.input.bulk_bytes,
            (64 * 1024 + 16 * 1024 + 4096 * 4) as u64
        );
        assert_eq!(stats.preserved_bytes, original.len() as u64);

        let mut changed = properties.clone();
        changed
            .get_mut("m_Wide")
            .and_then(|value| match value {
                UnityValue::Array(values) => values.get_mut(2048),
                _ => None,
            })
            .map(|value| *value = UnityValue::Integer(99_999))
            .unwrap();
        let mut changed_budget = AssetLoadBudget::default();
        let (changed_bytes, changed_stats) = rewrite_object(
            &schema,
            &changed,
            &original,
            Endian::Little,
            &mut changed_budget,
        )
        .unwrap();
        assert_ne!(changed_bytes, original);
        assert_eq!(changed_stats.input.unity_values_materialized, 0);
        assert_eq!(changed_stats.input.owned_bytes, 0);
        let mut read_budget = AssetLoadBudget::default();
        let mut reader = BinaryReader::new(&changed_bytes, ByteOrder::Little);
        let decoded = schema
            .read_object(
                &mut reader,
                &mut read_budget,
                TypeTreeParseOptions {
                    mode: TypeTreeParseMode::Strict,
                },
            )
            .unwrap();
        assert_eq!(decoded.properties, changed);
        assert_eq!(reader.position(), changed_bytes.len() as u64);
    }

    #[test]
    fn borrowed_string_comparison_preserves_the_decode_error_boundary() {
        let root = record(vec![aligned(node("string", "m_Text"))]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let field = schema.root().child(0).unwrap();
        let bytes = [1, 0, 0, 0, 0xff, 0xaa, 0xbb, 0xcc];
        let expected = UnityValue::String("x".to_owned());

        let mut budget = AssetLoadBudget::default();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let error = schema
            .compare_value(&mut reader, &mut budget, field, &expected)
            .unwrap_err();

        assert!(error.to_string().contains("Invalid UTF-8 string"));
        assert_eq!(reader.position(), 5);
        assert_eq!(budget.usage().bytes, 5);
    }

    #[test]
    fn malformed_sequence_adapter_matrix_preserves_error_class_and_boundary() {
        let root = record(vec![sequence("m_Values", node("int", "data"))]);
        let properties = IndexMap::from([("m_Values".to_owned(), UnityValue::Array(Vec::new()))]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);

        let negative = (-1_i32).to_le_bytes();
        for operation in ["read", "skip", "scan"] {
            let mut budget = AssetLoadBudget::default();
            let mut reader = BinaryReader::new(&negative, ByteOrder::Little);
            let error = match operation {
                "read" => schema
                    .read_object(
                        &mut reader,
                        &mut budget,
                        TypeTreeParseOptions {
                            mode: TypeTreeParseMode::Strict,
                        },
                    )
                    .unwrap_err(),
                "skip" => schema
                    .skip_value(&mut reader, &mut budget, schema.root())
                    .unwrap_err(),
                "scan" => schema.scan_pptrs(&mut reader, &mut budget).unwrap_err(),
                _ => unreachable!(),
            };
            assert!(
                error
                    .to_string()
                    .contains("Negative TypeTree sequence length")
            );
            assert_eq!(reader.position(), 4);
        }
        let mut budget = AssetLoadBudget::default();
        let error = rewrite_object(&schema, &properties, &negative, Endian::Little, &mut budget)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Negative TypeTree sequence length")
        );

        let huge = i32::MAX.to_le_bytes();
        let member_limits = AssetLoadLimits {
            max_members: 8,
            ..AssetLoadLimits::default()
        };
        for operation in ["read", "skip", "scan"] {
            let mut budget = AssetLoadBudget::new(member_limits).unwrap();
            let mut reader = BinaryReader::new(&huge, ByteOrder::Little);
            let error = match operation {
                "read" => schema
                    .read_object(
                        &mut reader,
                        &mut budget,
                        TypeTreeParseOptions {
                            mode: TypeTreeParseMode::Strict,
                        },
                    )
                    .unwrap_err(),
                "skip" => schema
                    .skip_value(&mut reader, &mut budget, schema.root())
                    .unwrap_err(),
                "scan" => schema.scan_pptrs(&mut reader, &mut budget).unwrap_err(),
                _ => unreachable!(),
            };
            assert!(error.to_string().contains("members"));
            assert_eq!(reader.position(), 4);
        }
        let mut budget = AssetLoadBudget::new(member_limits).unwrap();
        let error =
            rewrite_object(&schema, &properties, &huge, Endian::Little, &mut budget).unwrap_err();
        assert!(error.to_string().contains("members"));

        let oversized_properties = IndexMap::from([(
            "m_Values".to_owned(),
            UnityValue::Array(vec![UnityValue::Integer(0); 9]),
        )]);
        let mut budget = AssetLoadBudget::new(member_limits).unwrap();
        let error =
            encode_object(&schema, &oversized_properties, Endian::Little, &mut budget).unwrap_err();
        assert!(error.to_string().contains("members"));
    }

    #[test]
    fn bulk_sequence_depth_overflow_matches_read_skip_and_compare_boundaries() {
        let root = record(vec![sequence("m_Values", node("int", "data"))]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema(root, &mut compile_budget);
        let sequence = schema.root().child(0).unwrap();
        let bytes = 0_i32.to_le_bytes();
        let limits = AssetLoadLimits {
            max_depth: u32::MAX,
            ..AssetLoadLimits::default()
        };

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let read_error = schema
            .read_value_with_context(
                &mut reader,
                &mut budget,
                sequence,
                TypeTreeTraversalContext::root(),
                u32::MAX,
            )
            .unwrap_err();
        assert!(read_error.to_string().contains("depth overflow"));
        assert_eq!(reader.position(), 4);

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let skip_error = schema
            .skip_value_with_context(
                &mut reader,
                &mut budget,
                sequence,
                TypeTreeTraversalContext::root(),
                u32::MAX,
            )
            .unwrap_err();
        assert!(skip_error.to_string().contains("depth overflow"));
        assert_eq!(reader.position(), 4);

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
        let compare_error = schema
            .compare_value_with_context(
                &mut reader,
                &mut budget,
                sequence,
                TypeTreeTraversalContext::root(),
                u32::MAX,
                &UnityValue::Array(Vec::new()),
            )
            .unwrap_err();
        assert!(compare_error.to_string().contains("depth overflow"));
        assert_eq!(reader.position(), 4);

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut output = TypeTreeOutput::new(Endian::Little, &mut budget);
        let write_error = write_value(
            &schema,
            sequence,
            &UnityValue::Array(Vec::new()),
            Endian::Little,
            &mut output,
            TypeTreeTraversalContext::root(),
            u32::MAX,
        )
        .unwrap_err();
        assert!(write_error.to_string().contains("depth overflow"));
        assert_eq!(output.position(), 0);
    }

    #[test]
    fn managed_payload_expansion_observes_the_same_depth_across_adapters() {
        let mut type_node = node("ReferencedObjectType", "type");
        type_node.children = vec![
            node("string", "class"),
            node("string", "ns"),
            node("string", "asm"),
        ];
        let mut referenced = node("ReferencedObject", "m_Reference");
        referenced.children = vec![type_node, node("ReferencedObjectData", "data")];
        let root = record(vec![referenced]);

        let mut managed_nested = node("Nested", "m_Nested");
        managed_nested.children.push(node("int", "m_Value"));
        let mut managed_root = node("Managed", "Managed");
        managed_root.children.push(managed_nested);
        let mut managed_tree = TypeTree::new();
        managed_tree.add_node(managed_root);
        let mut managed_type = SerializedType::new(114);
        managed_type.class_name = "Managed".to_owned();
        managed_type.namespace = "Tests".to_owned();
        managed_type.assembly_name = "Tests".to_owned();
        managed_type.type_tree = managed_tree;

        let properties = IndexMap::from([(
            "m_Reference".to_owned(),
            UnityValue::Object(IndexMap::from([
                (
                    "type".to_owned(),
                    UnityValue::Object(IndexMap::from([
                        ("class".to_owned(), UnityValue::String("Managed".to_owned())),
                        ("ns".to_owned(), UnityValue::String("Tests".to_owned())),
                        ("asm".to_owned(), UnityValue::String("Tests".to_owned())),
                    ])),
                ),
                (
                    "data".to_owned(),
                    UnityValue::Object(IndexMap::from([(
                        "m_Nested".to_owned(),
                        UnityValue::Object(IndexMap::from([(
                            "m_Value".to_owned(),
                            UnityValue::Integer(42),
                        )])),
                    )])),
                ),
            ])),
        )]);
        let mut compile_budget = AssetLoadBudget::default();
        let schema = schema_with_refs(root, &[managed_type], &mut compile_budget);
        let mut encode_budget = AssetLoadBudget::default();
        let (encoded, _) =
            encode_object(&schema, &properties, Endian::Little, &mut encode_budget).unwrap();
        let limits = AssetLoadLimits {
            max_depth: 3,
            ..AssetLoadLimits::default()
        };

        let mut failure_boundary = None;
        for operation in ["read", "skip", "scan"] {
            let mut budget = AssetLoadBudget::new(limits).unwrap();
            let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
            let error = match operation {
                "read" => schema
                    .read_object(
                        &mut reader,
                        &mut budget,
                        TypeTreeParseOptions {
                            mode: TypeTreeParseMode::Strict,
                        },
                    )
                    .unwrap_err(),
                "skip" => schema
                    .skip_value(&mut reader, &mut budget, schema.root())
                    .unwrap_err(),
                "scan" => schema.scan_pptrs(&mut reader, &mut budget).unwrap_err(),
                _ => unreachable!(),
            };
            assert!(error.to_string().contains("depth"));
            assert_eq!(budget.usage().max_observed_depth, 3);
            if let Some(expected) = failure_boundary {
                assert_eq!(reader.position(), expected);
            } else {
                failure_boundary = Some(reader.position());
            }
        }
        assert_eq!(failure_boundary, Some(36));

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let error = encode_object(&schema, &properties, Endian::Little, &mut budget).unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(budget.usage().max_observed_depth, 3);

        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let error = rewrite_object(&schema, &properties, &encoded, Endian::Little, &mut budget)
            .unwrap_err();
        assert!(error.to_string().contains("depth"));
        assert_eq!(budget.usage().max_observed_depth, 3);
    }

    #[test]
    fn no_op_nan_rewrite_preserves_wire_bits_and_padding() {
        let root = record(vec![
            node("UInt8", "m_Prefix"),
            aligned(node("float", "m_NaN")),
        ]);
        let nan_bits = 0x7fa1_2345_u32;
        let mut original = vec![0x11];
        original.extend_from_slice(&nan_bits.to_le_bytes());
        original.extend_from_slice(&[0xa1, 0xb2, 0xc3]);
        let properties = IndexMap::from([
            ("m_Prefix".to_owned(), UnityValue::Integer(0x11)),
            (
                "m_NaN".to_owned(),
                UnityValue::Float(f32::from_bits(nan_bits) as f64),
            ),
        ]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(root, &mut budget);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(rewritten, original);
        assert_eq!(stats.preserved_bytes, original.len() as u64);
    }

    #[test]
    fn every_adapter_consumes_the_same_extent_in_both_endian_modes() {
        let mut root = record(vec![
            node("UInt64", "m_Id"),
            pptr("m_Target"),
            aligned(node("TypelessData", "m_Data")),
            sequence("m_Numbers", node("int", "data")),
            map("m_Map", node("string", "first"), node("UInt16", "second")),
        ]);
        root.meta_flags = 0x4000;

        let mut target = IndexMap::new();
        target.insert("m_FileID".to_string(), UnityValue::Integer(2));
        target.insert("m_PathID".to_string(), UnityValue::Integer(77));
        let mut properties = IndexMap::new();
        properties.insert("m_Id".to_string(), UnityValue::Unsigned(u64::MAX));
        properties.insert("m_Target".to_string(), UnityValue::Object(target));
        properties.insert("m_Data".to_string(), UnityValue::Bytes(vec![9, 8, 7]));
        properties.insert(
            "m_Numbers".to_string(),
            UnityValue::Array(vec![UnityValue::Integer(10), UnityValue::Integer(-20)]),
        );
        properties.insert(
            "m_Map".to_string(),
            UnityValue::Array(vec![UnityValue::Array(vec![
                UnityValue::String("answer".to_string()),
                UnityValue::Integer(42),
            ])]),
        );

        for (endian, byte_order) in [
            (Endian::Little, ByteOrder::Little),
            (Endian::Big, ByteOrder::Big),
        ] {
            let mut compile_budget = AssetLoadBudget::default();
            let schema = schema(root.clone(), &mut compile_budget);
            let mut write_budget = AssetLoadBudget::default();
            let (encoded, write_stats) =
                encode_object(&schema, &properties, endian, &mut write_budget).unwrap();
            assert!(write_stats.bulk_runs > 0);

            let mut read_budget = AssetLoadBudget::default();
            let mut reader = BinaryReader::new(&encoded, byte_order);
            let read = schema
                .read_object(
                    &mut reader,
                    &mut read_budget,
                    unity_asset_binary::typetree::TypeTreeParseOptions {
                        mode: unity_asset_binary::typetree::TypeTreeParseMode::Strict,
                    },
                )
                .unwrap();
            assert_eq!(read.properties, properties);
            assert_eq!(reader.position(), encoded.len() as u64);

            let mut skip_budget = AssetLoadBudget::default();
            let mut reader = BinaryReader::new(&encoded, byte_order);
            schema
                .skip_value(&mut reader, &mut skip_budget, schema.root())
                .unwrap();
            assert_eq!(reader.position(), encoded.len() as u64);

            let mut scan_budget = AssetLoadBudget::default();
            let mut reader = BinaryReader::new(&encoded, byte_order);
            let scan = schema.scan_pptrs(&mut reader, &mut scan_budget).unwrap();
            assert_eq!(scan.external, vec![(2, 77)]);
            assert!(scan.internal.is_empty());
            assert_eq!(scan.stats.unity_values_materialized, 0);
            assert_eq!(reader.position(), encoded.len() as u64);

            let mut rewrite_budget = AssetLoadBudget::default();
            let (rewritten, _) =
                rewrite_object(&schema, &properties, &encoded, endian, &mut rewrite_budget)
                    .unwrap();
            assert_eq!(rewritten, encoded);
        }
    }

    #[test]
    fn pptr_extension_fields_remain_scannable_and_template_preserved() {
        let mut pointer = node("PPtr<Object>", "m_Target");
        pointer.children = vec![
            node("int", "m_FileID"),
            node("UInt8", "m_Tag"),
            node("long long", "m_PathID"),
        ];
        let properties = IndexMap::from([(
            "m_Target".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(2)),
                ("m_Tag".to_owned(), UnityValue::Integer(0xaa)),
                ("m_PathID".to_owned(), UnityValue::Integer(77)),
            ])),
        )]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(record(vec![pointer]), &mut budget);
        let (encoded, _) =
            encode_object(&schema, &properties, Endian::Little, &mut budget).unwrap();

        let mut reader = BinaryReader::new(&encoded, ByteOrder::Little);
        let scan = schema.scan_pptrs(&mut reader, &mut budget).unwrap();
        assert_eq!(scan.external, vec![(2, 77)]);
        assert_eq!(scan.stats.unity_values_materialized, 0);
        assert_eq!(reader.position(), encoded.len() as u64);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &encoded, Endian::Little, &mut budget).unwrap();
        assert_eq!(rewritten, encoded);
        assert_eq!(stats.preserved_bytes, encoded.len() as u64);
        assert_eq!(stats.input.unity_values_materialized, 0);
    }

    #[test]
    fn pptr_role_edit_preserves_named_extension_wire_bytes() {
        let mut pointer = node("PPtr<Object>", "m_Target");
        pointer.children = vec![
            node("int", "m_FileID"),
            node("float", "m_NaN"),
            aligned(node("UInt8", "m_Tag")),
            node("long long", "m_PathID"),
        ];
        let nan_bits = 0x7fa1_2345_u32;
        let extension_bytes =
            [nan_bits.to_le_bytes().as_slice(), &[0xaa, 0xb1, 0xc2, 0xd3]].concat();
        let mut original = 2_i32.to_le_bytes().to_vec();
        original.extend_from_slice(&extension_bytes);
        original.extend_from_slice(&77_i64.to_le_bytes());
        let properties = IndexMap::from([(
            "m_Target".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(2)),
                (
                    "m_NaN".to_owned(),
                    UnityValue::Float(f32::from_bits(nan_bits) as f64),
                ),
                ("m_Tag".to_owned(), UnityValue::Integer(0xaa)),
                ("m_PathID".to_owned(), UnityValue::Integer(99)),
            ])),
        )]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(record(vec![pointer]), &mut budget);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(&rewritten[..4], &2_i32.to_le_bytes());
        assert_eq!(&rewritten[4..12], extension_bytes);
        assert_eq!(&rewritten[12..], &99_i64.to_le_bytes());
        assert_eq!(stats.preserved_bytes, 12);
    }

    #[test]
    fn pptr_role_edit_preserves_unnamed_extension_wire_bytes() {
        let mut pointer = node("PPtr<Object>", "m_Target");
        pointer.children = vec![
            node("int", "m_FileID"),
            aligned(node("UInt8", "")),
            node("long long", "m_PathID"),
        ];
        let opaque = [0x5a, 0xa1, 0xb2, 0xc3];
        let mut original = 3_i32.to_le_bytes().to_vec();
        original.extend_from_slice(&opaque);
        original.extend_from_slice(&41_i64.to_le_bytes());
        let properties = IndexMap::from([(
            "m_Target".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(3)),
                ("m_PathID".to_owned(), UnityValue::Integer(42)),
            ])),
        )]);
        let mut budget = AssetLoadBudget::default();
        let schema = schema(record(vec![pointer]), &mut budget);

        let (rewritten, stats) =
            rewrite_object(&schema, &properties, &original, Endian::Little, &mut budget).unwrap();

        assert_eq!(&rewritten[..4], &3_i32.to_le_bytes());
        assert_eq!(&rewritten[4..8], &opaque);
        assert_eq!(&rewritten[8..], &42_i64.to_le_bytes());
        assert_eq!(stats.preserved_bytes, 8);
    }
}
