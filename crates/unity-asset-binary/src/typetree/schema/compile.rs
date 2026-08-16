use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use unity_asset_core::AssetLoadBudget;

use super::{
    CompiledNode, CompiledSemantics, IntegerLayout, ManagedPayloadNode, ManagedReferenceCatalog,
    ManagedReferenceEntry, ManagedReferenceKey, NodeId, PPtrNodeLayout, PairNodeLayout,
    PrimitiveKind, ReferencedObjectNodeLayout, SchemaArena, SchemaProgram, SequenceNodeLayout,
    TypeTreeSchema,
};
use crate::asset::SerializedType;
use crate::error::{BinaryError, Result};
use crate::typetree::types::{TypeTree, TypeTreeNode};

const MAX_SCHEMA_NODES: u64 = crate::typetree::parser::MAX_TYPE_TREE_NODES as u64;
const MAX_SCHEMA_EDGES: u64 = crate::typetree::parser::MAX_TYPE_TREE_NODES as u64;
const MAX_MANAGED_REFERENCE_TYPES: u64 = crate::typetree::parser::MAX_TYPE_TREE_NODES as u64;
const ARC_REFERENCE_COUNTERS: u64 = 2;

impl TypeTreeSchema {
    /// Compiles one object TypeTree and its managed-reference type catalog.
    ///
    /// Budget charges happen before arena reservation, string cloning, or recursive compilation.
    pub fn compile(
        raw: &TypeTree,
        ref_types: &[SerializedType],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        Self::compile_with_catalog(raw, budget, |budget| {
            ManagedReferenceCatalog::compile(ref_types, budget).map(Arc::new)
        })
    }

    pub(crate) fn compile_with_catalog(
        raw: &TypeTree,
        budget: &mut AssetLoadBudget,
        load_catalog: impl FnOnce(&mut AssetLoadBudget) -> Result<Arc<ManagedReferenceCatalog>>,
    ) -> Result<Self> {
        require_single_root(raw, "object TypeTree")?;

        let mut preflight = Preflight::default();
        preflight_tree(raw, 0, budget, &mut preflight)?;
        charge_object_storage(&preflight, budget)?;

        let node_capacity = count_to_usize(preflight.raw_nodes, "TypeTree node")?;
        let edge_capacity = count_to_usize(preflight.raw_edges, "TypeTree edge")?;
        let name_capacity = count_to_usize(preflight.max_sibling_names, "TypeTree sibling name")?;
        let mut compiler = Compiler::with_capacity(node_capacity, edge_capacity, 0, name_capacity)?;

        let raw_root = raw
            .nodes
            .first()
            .ok_or_else(|| BinaryError::invalid_data("object TypeTree has no root"))?;
        let root = compiler.compile_node(raw_root, AlignmentPolicy::Preserve)?;
        let managed = if compiler.nodes[root.0].contains_managed_reference {
            Some(load_catalog(budget)?)
        } else {
            None
        };

        Ok(Self {
            program: Arc::new(SchemaProgram {
                arena: SchemaArena {
                    nodes: compiler.nodes,
                    edges: compiler.edges,
                },
                root,
            }),
            managed,
        })
    }
}

impl ManagedReferenceCatalog {
    pub(crate) fn compile(
        ref_types: &[SerializedType],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        let mut preflight = Preflight::default();
        preflight_reference_types(ref_types, budget, &mut preflight)?;
        charge_catalog_storage(&preflight, budget)?;

        let node_capacity = count_to_usize(preflight.raw_nodes, "managed TypeTree node")?;
        let edge_capacity = count_to_usize(preflight.raw_edges, "managed TypeTree edge")?;
        let reference_capacity = count_to_usize(preflight.reference_entries, "ref type")?;
        let name_capacity =
            count_to_usize(preflight.max_sibling_names, "managed TypeTree sibling name")?;
        let mut compiler = Compiler::with_capacity(
            node_capacity,
            edge_capacity,
            reference_capacity,
            name_capacity,
        )?;

        for ref_type in eligible_reference_types(ref_types) {
            let key = ManagedReferenceKey::try_from_serialized_type(ref_type)?;
            let raw_root = ref_type
                .type_tree
                .nodes
                .first()
                .ok_or_else(|| BinaryError::invalid_data("ref TypeTree has no root"))?;
            let root = compiler.compile_node(raw_root, AlignmentPolicy::Preserve)?;
            compiler.push_reference(ManagedReferenceEntry { key, root })?;
        }
        compiler.sort_and_validate_reference_index()?;

        Ok(Self {
            arena: SchemaArena {
                nodes: compiler.nodes,
                edges: compiler.edges,
            },
            reference_index: compiler.reference_index.into_boxed_slice(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PPtrFieldRole {
    FileId,
    PathId,
}

impl PPtrFieldRole {
    const fn label(self) -> &'static str {
        match self {
            Self::FileId => "file ID",
            Self::PathId => "path ID",
        }
    }
}

impl IntegerLayout {
    fn from_node(node: &TypeTreeNode, role: PPtrFieldRole) -> Result<Self> {
        let role_label = role.label();
        let primitive = primitive_kind(node.type_name.as_str()).ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "PPtr {role_label} field '{}' has non-integer type '{}'",
                node.name, node.type_name
            ))
        })?;
        if !primitive.is_integer() {
            return Err(BinaryError::invalid_data(format!(
                "PPtr {role_label} field '{}' has non-integer type '{}'",
                node.name, node.type_name
            )));
        }
        if role == PPtrFieldRole::FileId && primitive.width() > 4 {
            return Err(BinaryError::invalid_data(format!(
                "PPtr file ID field '{}' is wider than 32 bits",
                node.name
            )));
        }
        Ok(Self { primitive })
    }
}

impl ManagedReferenceKey {
    fn try_from_serialized_type(value: &SerializedType) -> Result<Self> {
        Ok(Self::new(
            try_clone_string(&value.class_name, "managed reference class name")?,
            try_clone_string(&value.namespace, "managed reference namespace")?,
            try_clone_string(&value.assembly_name, "managed reference assembly name")?,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionKind {
    Sequence,
    Map,
}

impl CollectionKind {
    const fn semantics(self, layout: SequenceNodeLayout) -> CompiledSemantics {
        match self {
            Self::Sequence => CompiledSemantics::Sequence(layout),
            Self::Map => CompiledSemantics::Map(layout),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    ManagedRegistry,
    Record,
}

impl RecordKind {
    const fn semantics(self) -> CompiledSemantics {
        match self {
            Self::ManagedRegistry => CompiledSemantics::ManagedRegistry,
            Self::Record => CompiledSemantics::Record,
        }
    }
}

struct Compiler {
    nodes: Vec<CompiledNode>,
    edges: Vec<NodeId>,
    reference_index: Vec<ManagedReferenceEntry>,
    name_scratch: Vec<usize>,
    node_limit: usize,
    edge_limit: usize,
    reference_limit: usize,
}

impl Compiler {
    fn with_capacity(
        node_capacity: usize,
        edge_capacity: usize,
        reference_capacity: usize,
        name_capacity: usize,
    ) -> Result<Self> {
        let mut nodes = Vec::new();
        nodes.try_reserve_exact(node_capacity).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {node_capacity} compiled TypeTree nodes: {error}"
            ))
        })?;

        let mut edges = Vec::new();
        edges.try_reserve_exact(edge_capacity).map_err(|error| {
            BinaryError::memory_error(format!(
                "Failed to reserve {edge_capacity} compiled TypeTree edges: {error}"
            ))
        })?;

        let mut reference_index = Vec::new();
        reference_index
            .try_reserve_exact(reference_capacity)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {reference_capacity} managed reference entries: {error}"
                ))
            })?;

        let mut name_scratch = Vec::new();
        name_scratch
            .try_reserve_exact(name_capacity)
            .map_err(|error| {
                BinaryError::memory_error(format!(
                    "Failed to reserve {name_capacity} TypeTree sibling name indexes: {error}"
                ))
            })?;

        Ok(Self {
            nodes,
            edges,
            reference_index,
            name_scratch,
            node_limit: node_capacity,
            edge_limit: edge_capacity,
            reference_limit: reference_capacity,
        })
    }

    fn push_reference(&mut self, entry: ManagedReferenceEntry) -> Result<()> {
        if self.reference_index.len() == self.reference_limit {
            return Err(BinaryError::memory_error(
                "Compiled TypeTree exceeded its preflight reference limit",
            ));
        }
        self.reference_index.push(entry);
        Ok(())
    }

    fn sort_and_validate_reference_index(&mut self) -> Result<()> {
        self.reference_index
            .sort_unstable_by(|left, right| left.key.cmp(&right.key));
        if let Some(duplicate) = self
            .reference_index
            .windows(2)
            .find(|pair| pair[0].key == pair[1].key)
        {
            return Err(duplicate_reference_key(&duplicate[0].key));
        }
        Ok(())
    }

    fn compile_node(&mut self, raw: &TypeTreeNode, alignment: AlignmentPolicy) -> Result<NodeId> {
        if raw.type_name.is_empty() {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree node '{}' has an empty type name",
                raw.name
            )));
        }
        if raw.byte_size < -1 {
            return Err(BinaryError::invalid_data(format!(
                "TypeTree node '{}' ({}) has invalid byte size {}",
                raw.name, raw.type_name, raw.byte_size
            )));
        }
        if raw.type_name == "string" {
            return self.push_node(
                raw,
                self.edges.len()..self.edges.len(),
                true,
                CompiledSemantics::String,
            );
        }
        if raw.type_name == "TypelessData" {
            return self.push_leaf(raw, CompiledSemantics::TypelessData, alignment);
        }
        if let Some(primitive) = primitive_kind(raw.type_name.as_str()) {
            if !raw.children.is_empty() {
                return Err(BinaryError::invalid_data(format!(
                    "Primitive TypeTree node '{}' ({}) has {} children",
                    raw.name,
                    raw.type_name,
                    raw.children.len()
                )));
            }
            return self.push_leaf(raw, CompiledSemantics::Scalar(primitive), alignment);
        }

        if raw.type_name == "pair" {
            if raw.children.len() != 2 {
                return Err(BinaryError::invalid_data(format!(
                    "Pair TypeTree node '{}' must have exactly two children, got {}",
                    raw.name,
                    raw.children.len()
                )));
            }
            return self.compile_pair(raw, alignment);
        }

        if is_pptr_type(raw.type_name.as_str()) {
            return self.compile_pptr(raw, alignment);
        }

        if raw.type_name == "ReferencedObject" {
            return self.compile_referenced_object(raw, alignment);
        }
        if raw.type_name == "ManagedReferencesRegistry" {
            return self.compile_record(raw, RecordKind::ManagedRegistry, alignment);
        }

        let array = collection_array(raw)?;
        if raw.type_name == "map" && array.is_none() {
            return Err(BinaryError::invalid_data(format!(
                "Map TypeTree node '{}' has no Array layout",
                raw.name
            )));
        }
        if let Some(array) = array {
            let kind = if raw.type_name == "map" {
                CollectionKind::Map
            } else {
                CollectionKind::Sequence
            };
            return self.compile_collection(raw, array, kind, alignment);
        }

        if !raw.children.is_empty() {
            return self.compile_record(raw, RecordKind::Record, alignment);
        }

        let byte_size = u64::try_from(raw.byte_size).map_err(|_| {
            BinaryError::invalid_data(format!(
                "Unknown leaf TypeTree node '{}' ({}) has no fixed byte size",
                raw.name, raw.type_name
            ))
        })?;
        self.push_leaf(raw, CompiledSemantics::OpaqueFixed { byte_size }, alignment)
    }

    fn compile_referenced_object(
        &mut self,
        raw: &TypeTreeNode,
        alignment: AlignmentPolicy,
    ) -> Result<NodeId> {
        self.validate_unique_object_child_names(raw, false)?;
        let raw_layout = RawReferencedObjectLayout::parse(raw)?;
        let children = self.reserve_edge_slots(raw.children.len())?;

        for (index, child) in raw.children.iter().enumerate() {
            let id = if index == raw_layout.payload_index && raw_layout.payload_is_dynamic {
                self.compile_dynamic_payload(child)?
            } else {
                self.compile_node(child, AlignmentPolicy::Preserve)?
            };
            self.edges[children.start + index] = id;
        }

        let type_node = self.edges[children.start + raw_layout.type_index];
        let payload_node = self.edges[children.start + raw_layout.payload_index];
        let class_field = self.compiled_child(type_node, raw_layout.class_index)?;
        let namespace_field = self.compiled_child(type_node, raw_layout.namespace_index)?;
        let assembly_field = self.compiled_child(type_node, raw_layout.assembly_index)?;
        let payload = if raw_layout.payload_is_dynamic {
            ManagedPayloadNode::Dynamic(payload_node)
        } else {
            ManagedPayloadNode::Fallback(payload_node)
        };
        self.push_node(
            raw,
            children,
            alignment.apply(raw.is_aligned()),
            CompiledSemantics::ReferencedObject(ReferencedObjectNodeLayout {
                type_node,
                class_field,
                namespace_field,
                assembly_field,
                payload,
            }),
        )
    }

    fn compile_dynamic_payload(&mut self, raw: &TypeTreeNode) -> Result<NodeId> {
        if !raw.children.is_empty() || raw.byte_size != -1 {
            return Err(BinaryError::invalid_data(format!(
                "Dynamic managed payload '{}' must be a childless -1 byte node",
                raw.name
            )));
        }
        let edge_end = self.edges.len();
        self.push_node(
            raw,
            edge_end..edge_end,
            raw.is_aligned(),
            CompiledSemantics::ManagedPayload,
        )
    }

    fn compiled_child(&self, parent: NodeId, index: usize) -> Result<NodeId> {
        let range = self
            .nodes
            .get(parent.0)
            .ok_or_else(|| BinaryError::invalid_data("Compiled TypeTree parent is missing"))?
            .children
            .clone();
        self.edges
            .get(range)
            .and_then(|children| children.get(index))
            .copied()
            .ok_or_else(|| BinaryError::invalid_data("Compiled TypeTree child is missing"))
    }

    fn compile_collection(
        &mut self,
        wrapper: &TypeTreeNode,
        array: &TypeTreeNode,
        kind: CollectionKind,
        alignment: AlignmentPolicy,
    ) -> Result<NodeId> {
        if array.children.len() != 2 {
            return Err(BinaryError::invalid_data(format!(
                "Array layout for '{}' must be [size, data], got {} children",
                wrapper.name,
                array.children.len()
            )));
        }
        let size = &array.children[0];
        if !size.children.is_empty()
            || primitive_kind(size.type_name.as_str()) != Some(PrimitiveKind::I32)
        {
            return Err(BinaryError::invalid_data(format!(
                "Array size node for '{}' must be a scalar i32, got '{}'",
                wrapper.name, size.type_name
            )));
        }

        let element_raw = &array.children[1];
        let bulk_primitive = if element_raw.children.is_empty() {
            primitive_kind(element_raw.type_name.as_str())
        } else {
            None
        };
        let promote_element_alignment = element_raw.is_aligned();
        let element_alignment = if promote_element_alignment {
            AlignmentPolicy::Suppress
        } else {
            AlignmentPolicy::Preserve
        };
        let element = self.compile_node(element_raw, element_alignment)?;

        if kind == CollectionKind::Map
            && !matches!(self.nodes[element.0].semantics, CompiledSemantics::Pair(_))
        {
            return Err(BinaryError::invalid_data(format!(
                "Map TypeTree node '{}' must contain pair elements, got {:?}",
                wrapper.name,
                self.nodes[element.0].semantics.kind()
            )));
        }

        let children = self.append_edges(std::slice::from_ref(&element))?;
        let align_after = alignment.apply(wrapper.is_aligned())
            || array.is_aligned()
            || promote_element_alignment;
        self.push_node(
            wrapper,
            children,
            align_after,
            kind.semantics(SequenceNodeLayout {
                element,
                bulk_primitive,
            }),
        )
    }

    fn compile_pptr(&mut self, raw: &TypeTreeNode, alignment: AlignmentPolicy) -> Result<NodeId> {
        if raw.children.is_empty() {
            return Err(BinaryError::invalid_data(format!(
                "PPtr TypeTree node '{}' has no fields",
                raw.name
            )));
        }

        self.validate_unique_object_child_names(raw, true)?;

        let children = self.compile_children(&raw.children)?;
        let mut file: Option<(usize, IntegerLayout)> = None;
        let mut path: Option<(usize, IntegerLayout)> = None;

        for (index, child) in raw.children.iter().enumerate() {
            if is_file_id_name(&child.name) {
                let layout = IntegerLayout::from_node(child, PPtrFieldRole::FileId)?;
                if file.replace((index, layout)).is_some() {
                    return Err(BinaryError::invalid_data(format!(
                        "PPtr TypeTree node '{}' has duplicate file ID fields",
                        raw.name
                    )));
                }
            } else if is_path_id_name(&child.name) {
                let layout = IntegerLayout::from_node(child, PPtrFieldRole::PathId)?;
                if path.replace((index, layout)).is_some() {
                    return Err(BinaryError::invalid_data(format!(
                        "PPtr TypeTree node '{}' has duplicate path ID fields",
                        raw.name
                    )));
                }
            }
        }

        let (file_index, file_integer) = file.ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "PPtr TypeTree node '{}' has no fileID/m_FileID field",
                raw.name
            ))
        })?;
        let (path_index, path_integer) = path.ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "PPtr TypeTree node '{}' has no pathID/m_PathID field",
                raw.name
            ))
        })?;
        let file_child = self.edges[children.start + file_index];
        let path_child = self.edges[children.start + path_index];

        self.push_node(
            raw,
            children,
            alignment.apply(raw.is_aligned()),
            CompiledSemantics::PPtr(PPtrNodeLayout {
                file_child,
                file_integer,
                path_child,
                path_integer,
            }),
        )
    }

    fn compile_record(
        &mut self,
        raw: &TypeTreeNode,
        kind: RecordKind,
        alignment: AlignmentPolicy,
    ) -> Result<NodeId> {
        self.validate_unique_object_child_names(raw, true)?;
        let children = self.compile_children(&raw.children)?;
        self.push_node(
            raw,
            children,
            alignment.apply(raw.is_aligned()),
            kind.semantics(),
        )
    }

    fn compile_pair(&mut self, raw: &TypeTreeNode, alignment: AlignmentPolicy) -> Result<NodeId> {
        debug_assert_eq!(raw.children.len(), 2);
        let children = self.compile_children(&raw.children)?;
        let first = self.edges[children.start];
        let second = self.edges[children.start + 1];
        self.push_node(
            raw,
            children,
            alignment.apply(raw.is_aligned()),
            CompiledSemantics::Pair(PairNodeLayout { first, second }),
        )
    }

    fn validate_unique_object_child_names(
        &mut self,
        raw: &TypeTreeNode,
        guards_registry: bool,
    ) -> Result<()> {
        self.name_scratch.clear();
        let mut has_managed_registry = false;

        for (index, child) in raw.children.iter().enumerate() {
            if guards_registry && child.type_name == "ManagedReferencesRegistry" {
                if has_managed_registry {
                    continue;
                }
                has_managed_registry = true;
            }
            if !child.name.is_empty() {
                self.name_scratch.push(index);
            }
        }

        self.name_scratch.sort_unstable_by(|left, right| {
            raw.children[*left]
                .name
                .as_str()
                .cmp(raw.children[*right].name.as_str())
        });
        if let Some(duplicate) = self
            .name_scratch
            .windows(2)
            .find(|pair| raw.children[pair[0]].name == raw.children[pair[1]].name)
        {
            let name = &raw.children[duplicate[0]].name;
            return Err(BinaryError::invalid_data(format!(
                "TypeTree object node '{}' ({}) has duplicate non-empty child name '{name}'",
                raw.name, raw.type_name
            )));
        }
        Ok(())
    }

    fn compile_children(&mut self, raw_children: &[TypeTreeNode]) -> Result<Range<usize>> {
        let range = self.reserve_edge_slots(raw_children.len())?;
        for (index, child) in raw_children.iter().enumerate() {
            let id = self.compile_node(child, AlignmentPolicy::Preserve)?;
            self.edges[range.start + index] = id;
        }
        Ok(range)
    }

    fn push_leaf(
        &mut self,
        raw: &TypeTreeNode,
        semantics: CompiledSemantics,
        alignment: AlignmentPolicy,
    ) -> Result<NodeId> {
        let edge_end = self.edges.len();
        self.push_node(
            raw,
            edge_end..edge_end,
            alignment.apply(raw.is_aligned()),
            semantics,
        )
    }

    fn push_node(
        &mut self,
        raw: &TypeTreeNode,
        children: Range<usize>,
        align_after: bool,
        semantics: CompiledSemantics,
    ) -> Result<NodeId> {
        if self.nodes.len() == self.node_limit {
            return Err(BinaryError::memory_error(
                "Compiled TypeTree exceeded its preflight node limit",
            ));
        }
        let contains_managed_reference =
            matches!(semantics, CompiledSemantics::ReferencedObject(_))
                || self.edges[children.clone()]
                    .iter()
                    .any(|child| self.nodes[child.0].contains_managed_reference);
        let id = NodeId(self.nodes.len());
        self.nodes.push(CompiledNode {
            name: try_clone_string(&raw.name, "TypeTree field name")?,
            type_name: try_clone_string(&raw.type_name, "TypeTree type name")?,
            children,
            align_after,
            contains_managed_reference,
            semantics,
        });
        Ok(id)
    }

    fn reserve_edge_slots(&mut self, count: usize) -> Result<Range<usize>> {
        let start = self.edges.len();
        let end = start
            .checked_add(count)
            .ok_or_else(|| BinaryError::invalid_data("Compiled TypeTree edge count overflow"))?;
        if end > self.edge_limit {
            return Err(BinaryError::memory_error(
                "Compiled TypeTree exceeded its preflight edge limit",
            ));
        }
        self.edges.resize(end, NodeId::INVALID);
        Ok(start..end)
    }

    fn append_edges(&mut self, values: &[NodeId]) -> Result<Range<usize>> {
        let range = self.reserve_edge_slots(values.len())?;
        self.edges[range.clone()].copy_from_slice(values);
        Ok(range)
    }
}

#[derive(Debug, Clone, Copy)]
struct RawReferencedObjectLayout {
    type_index: usize,
    class_index: usize,
    namespace_index: usize,
    assembly_index: usize,
    payload_index: usize,
    payload_is_dynamic: bool,
}

impl RawReferencedObjectLayout {
    fn parse(raw: &TypeTreeNode) -> Result<Self> {
        let mut type_index = None;
        let mut payload_index = None;

        for (index, child) in raw.children.iter().enumerate() {
            if child.name.eq_ignore_ascii_case("type") && type_index.replace(index).is_some() {
                return Err(BinaryError::invalid_data(format!(
                    "ReferencedObject '{}' has duplicate type nodes",
                    raw.name
                )));
            }
            if child.type_name == "ReferencedObjectData" && payload_index.replace(index).is_some() {
                return Err(BinaryError::invalid_data(format!(
                    "ReferencedObject '{}' has duplicate payload nodes",
                    raw.name
                )));
            }
        }

        let type_index = type_index.ok_or_else(|| {
            BinaryError::invalid_data(format!("ReferencedObject '{}' has no type node", raw.name))
        })?;
        let payload_index = payload_index.ok_or_else(|| {
            BinaryError::invalid_data(format!(
                "ReferencedObject '{}' has no ReferencedObjectData payload",
                raw.name
            ))
        })?;
        if payload_index <= type_index {
            return Err(BinaryError::invalid_data(format!(
                "ReferencedObject '{}' payload must follow its type key",
                raw.name
            )));
        }
        let type_node = &raw.children[type_index];

        let mut class_index = None;
        let mut namespace_index = None;
        let mut assembly_index = None;
        for (index, field) in type_node.children.iter().enumerate() {
            let (slot, role) = if is_managed_class_name(&field.name) {
                (&mut class_index, "class")
            } else if is_managed_namespace_name(&field.name) {
                (&mut namespace_index, "namespace")
            } else if is_managed_assembly_name(&field.name) {
                (&mut assembly_index, "assembly")
            } else {
                continue;
            };

            if field.type_name != "string" {
                return Err(BinaryError::invalid_data(format!(
                    "ReferencedObject {role} field '{}' must be a string",
                    field.name
                )));
            }
            if slot.replace(index).is_some() {
                return Err(BinaryError::invalid_data(format!(
                    "ReferencedObject '{}' has duplicate {role} fields",
                    raw.name
                )));
            }
        }

        let class_index = required_referenced_type_field(raw, class_index, "class")?;
        let namespace_index = required_referenced_type_field(raw, namespace_index, "namespace")?;
        let assembly_index = required_referenced_type_field(raw, assembly_index, "assembly")?;
        let payload = &raw.children[payload_index];

        Ok(Self {
            type_index,
            class_index,
            namespace_index,
            assembly_index,
            payload_index,
            payload_is_dynamic: payload.children.is_empty() && payload.byte_size == -1,
        })
    }
}

fn required_referenced_type_field(
    raw: &TypeTreeNode,
    index: Option<usize>,
    role: &str,
) -> Result<usize> {
    index.ok_or_else(|| {
        BinaryError::invalid_data(format!(
            "ReferencedObject '{}' has no {role} string field",
            raw.name
        ))
    })
}

#[derive(Debug, Clone, Copy)]
enum AlignmentPolicy {
    Preserve,
    Suppress,
}

impl AlignmentPolicy {
    const fn apply(self, raw_alignment: bool) -> bool {
        match self {
            Self::Preserve => raw_alignment,
            Self::Suppress => false,
        }
    }
}

#[derive(Debug, Default)]
struct Preflight {
    raw_nodes: u64,
    raw_edges: u64,
    reference_entries: u64,
    max_sibling_names: u64,
}

fn preflight_tree(
    tree: &TypeTree,
    root_depth: u32,
    budget: &mut AssetLoadBudget,
    preflight: &mut Preflight,
) -> Result<()> {
    require_single_root(tree, "TypeTree")?;
    let root = tree
        .nodes
        .first()
        .ok_or_else(|| BinaryError::invalid_data("TypeTree has no root"))?;
    preflight_node(root, root_depth, budget, preflight)
}

fn preflight_node(
    node: &TypeTreeNode,
    depth: u32,
    budget: &mut AssetLoadBudget,
    preflight: &mut Preflight,
) -> Result<()> {
    let child_count = u64::try_from(node.children.len())
        .map_err(|_| BinaryError::invalid_data("TypeTree child count does not fit u64"))?;
    increment_logical_count(
        &mut preflight.raw_nodes,
        1,
        MAX_SCHEMA_NODES,
        "compiled TypeTree node",
    )?;
    preflight.max_sibling_names = preflight.max_sibling_names.max(child_count);
    increment_logical_count(
        &mut preflight.raw_edges,
        child_count,
        MAX_SCHEMA_EDGES,
        "compiled TypeTree edge",
    )?;

    budget.observe_depth(depth)?;
    budget.consume_entries(1)?;
    budget.consume_members(child_count)?;

    let string_bytes = node
        .type_name
        .len()
        .checked_add(node.name.len())
        .ok_or_else(|| BinaryError::invalid_data("TypeTree string byte count overflow"))?;
    budget.consume_bytes(usize_to_u64(string_bytes, "TypeTree string byte")?)?;

    let child_depth = depth
        .checked_add(1)
        .ok_or_else(|| BinaryError::invalid_data("TypeTree depth overflow"))?;
    for child in &node.children {
        preflight_node(child, child_depth, budget, preflight)?;
    }
    Ok(())
}

fn preflight_reference_types(
    ref_types: &[SerializedType],
    budget: &mut AssetLoadBudget,
    preflight: &mut Preflight,
) -> Result<()> {
    for (index, ref_type) in ref_types.iter().enumerate() {
        if ref_type.type_tree.is_empty() {
            continue;
        }
        if ref_type.class_name.is_empty() {
            return Err(BinaryError::invalid_data(format!(
                "Managed reference type at index {index} has a TypeTree but no class name"
            )));
        }

        increment_logical_count(
            &mut preflight.reference_entries,
            1,
            MAX_MANAGED_REFERENCE_TYPES,
            "managed reference type",
        )?;
        budget.consume_entries(1)?;
        let key_bytes = ref_type
            .class_name
            .len()
            .checked_add(ref_type.namespace.len())
            .and_then(|size| size.checked_add(ref_type.assembly_name.len()))
            .ok_or_else(|| {
                BinaryError::invalid_data("Managed reference key byte count overflow")
            })?;
        budget.consume_bytes(usize_to_u64(key_bytes, "managed reference key byte")?)?;
        require_single_root(&ref_type.type_tree, "managed reference TypeTree")?;
        preflight_tree(&ref_type.type_tree, 0, budget, preflight)?;
    }
    Ok(())
}

fn increment_logical_count(total: &mut u64, amount: u64, limit: u64, label: &str) -> Result<()> {
    let next = total
        .checked_add(amount)
        .ok_or_else(|| BinaryError::invalid_data(format!("{label} count overflow")))?;
    if next > limit {
        return Err(BinaryError::invalid_data(format!(
            "{label} count {next} exceeds limit {limit}"
        )));
    }
    *total = next;
    Ok(())
}

fn eligible_reference_types(ref_types: &[SerializedType]) -> impl Iterator<Item = &SerializedType> {
    ref_types
        .iter()
        .filter(|ref_type| !ref_type.type_tree.is_empty())
}

fn charge_object_storage(preflight: &Preflight, budget: &mut AssetLoadBudget) -> Result<()> {
    let nodes = checked_storage_bytes::<CompiledNode>(preflight.raw_nodes, "TypeTree node arena")?;
    let edges = checked_storage_bytes::<NodeId>(preflight.raw_edges, "TypeTree edge arena")?;
    let program = usize_to_u64(size_of::<SchemaProgram>(), "TypeTree schema program")?;
    let arc_header = checked_storage_bytes::<usize>(ARC_REFERENCE_COUNTERS, "Arc header")?;
    let name_scratch = checked_storage_bytes::<usize>(
        preflight.max_sibling_names,
        "TypeTree sibling name scratch",
    )?;
    let total = nodes
        .checked_add(edges)
        .and_then(|size| size.checked_add(program))
        .and_then(|size| size.checked_add(arc_header))
        .and_then(|size| size.checked_add(name_scratch))
        .ok_or_else(|| BinaryError::invalid_data("Compiled TypeTree allocation size overflow"))?;
    budget.consume_bytes(total)?;
    Ok(())
}

fn charge_catalog_storage(preflight: &Preflight, budget: &mut AssetLoadBudget) -> Result<()> {
    let nodes =
        checked_storage_bytes::<CompiledNode>(preflight.raw_nodes, "managed TypeTree node arena")?;
    let edges =
        checked_storage_bytes::<NodeId>(preflight.raw_edges, "managed TypeTree edge arena")?;
    let references = checked_storage_bytes::<ManagedReferenceEntry>(
        preflight.reference_entries,
        "managed reference index",
    )?;
    let catalog = usize_to_u64(
        size_of::<ManagedReferenceCatalog>(),
        "managed reference catalog",
    )?;
    let arc_header = checked_storage_bytes::<usize>(ARC_REFERENCE_COUNTERS, "Arc header")?;
    let name_scratch = checked_storage_bytes::<usize>(
        preflight.max_sibling_names,
        "managed TypeTree sibling name scratch",
    )?;
    let total = nodes
        .checked_add(edges)
        .and_then(|size| size.checked_add(references))
        .and_then(|size| size.checked_add(catalog))
        .and_then(|size| size.checked_add(arc_header))
        .and_then(|size| size.checked_add(name_scratch))
        .ok_or_else(|| {
            BinaryError::invalid_data("Managed reference catalog allocation size overflow")
        })?;
    budget.consume_bytes(total)?;
    Ok(())
}

fn checked_storage_bytes<T>(count: u64, label: &str) -> Result<u64> {
    let width = usize_to_u64(size_of::<T>(), label)?;
    count
        .checked_mul(width)
        .ok_or_else(|| BinaryError::invalid_data(format!("{label} allocation size overflow")))
}

fn require_single_root(tree: &TypeTree, label: &str) -> Result<()> {
    if tree.nodes.len() != 1 {
        return Err(BinaryError::invalid_data(format!(
            "{label} must contain exactly one root, got {}",
            tree.nodes.len()
        )));
    }
    Ok(())
}

fn collection_array(node: &TypeTreeNode) -> Result<Option<&TypeTreeNode>> {
    if node.type_name == "Array" {
        return Ok(Some(node));
    }

    let mut arrays = node
        .children
        .iter()
        .filter(|child| child.type_name == "Array");
    let Some(array) = arrays.next() else {
        return Ok(None);
    };
    if arrays.next().is_some() || node.children.len() != 1 {
        return Err(BinaryError::invalid_data(format!(
            "Sequence TypeTree node '{}' must have exactly one Array child",
            node.name
        )));
    }
    Ok(Some(array))
}

/// This is the only TypeTree primitive alias table.
fn primitive_kind(type_name: &str) -> Option<PrimitiveKind> {
    match type_name {
        "bool" => Some(PrimitiveKind::Bool),
        "SInt8" => Some(PrimitiveKind::I8),
        "UInt8" | "char" => Some(PrimitiveKind::U8),
        "SInt16" | "short" => Some(PrimitiveKind::I16),
        "UInt16" | "unsigned short" | "ushort" => Some(PrimitiveKind::U16),
        "SInt32" | "int" | "EntityId" => Some(PrimitiveKind::I32),
        "UInt32" | "unsigned int" | "uint" | "Type*" => Some(PrimitiveKind::U32),
        "SInt64" | "long long" => Some(PrimitiveKind::I64),
        "UInt64" | "unsigned long long" | "FileSize" => Some(PrimitiveKind::U64),
        "float" => Some(PrimitiveKind::F32),
        "double" => Some(PrimitiveKind::F64),
        _ => None,
    }
}

fn is_pptr_type(type_name: &str) -> bool {
    type_name == "PPtr" || type_name.starts_with("PPtr<")
}

fn is_file_id_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("fileID") || name.eq_ignore_ascii_case("m_FileID")
}

fn is_path_id_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("pathID") || name.eq_ignore_ascii_case("m_PathID")
}

fn is_managed_class_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("class") || name.eq_ignore_ascii_case("m_ClassName")
}

fn is_managed_namespace_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("ns") || name.eq_ignore_ascii_case("m_NameSpace")
}

fn is_managed_assembly_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("asm") || name.eq_ignore_ascii_case("m_AssemblyName")
}

fn duplicate_reference_key(key: &ManagedReferenceKey) -> BinaryError {
    BinaryError::invalid_data(format!(
        "Duplicate managed reference type key: class='{}', namespace='{}', assembly='{}'",
        key.class_name, key.namespace, key.assembly_name
    ))
}

fn try_clone_string(value: &str, label: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {} bytes for {label}: {error}",
            value.len()
        ))
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn count_to_usize(count: u64, label: &str) -> Result<usize> {
    usize::try_from(count)
        .map_err(|_| BinaryError::memory_error(format!("{label} count does not fit usize")))
}

fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| BinaryError::invalid_data(format!("{label} count does not fit u64")))
}
