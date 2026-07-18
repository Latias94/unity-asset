//! Canonical, immutable TypeTree schemas used by semantic traversals.

use std::cmp::Ordering;
use std::iter::FusedIterator;
use std::ops::Range;
use std::sync::Arc;

mod compile;

#[cfg(test)]
mod tests;

/// Canonical primitive kinds understood by every TypeTree traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveKind {
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

impl PrimitiveKind {
    /// Returns the encoded width of one value in bytes.
    #[must_use]
    pub const fn width(self) -> u8 {
        match self {
            Self::Bool | Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// Returns integer signedness, or `None` for booleans and floating-point values.
    #[must_use]
    pub const fn signedness(self) -> Option<IntegerSignedness> {
        match self {
            Self::I8 | Self::I16 | Self::I32 | Self::I64 => Some(IntegerSignedness::Signed),
            Self::U8 | Self::U16 | Self::U32 | Self::U64 => Some(IntegerSignedness::Unsigned),
            Self::Bool | Self::F32 | Self::F64 => None,
        }
    }

    #[must_use]
    pub const fn is_integer(self) -> bool {
        self.signedness().is_some()
    }

    #[must_use]
    pub const fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

/// Signedness of an integer primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerSignedness {
    Signed,
    Unsigned,
}

/// Compatibility projection of a TypeTree node's semantic shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticKind {
    Scalar(PrimitiveKind),
    String,
    TypelessData,
    Sequence,
    Map,
    Pair,
    PPtr,
    ReferencedObject,
    ManagedPayload,
    ManagedRegistry,
    Record,
    OpaqueFixed { byte_size: u64 },
}

const fn guards_managed_registry(kind: SemanticKind) -> bool {
    matches!(
        kind,
        SemanticKind::Record | SemanticKind::ManagedRegistry | SemanticKind::PPtr
    )
}

/// Lexically inherited state shared by every adapter traversing a canonical schema.
///
/// Unity applies the managed-registry guard only while iterating generic record-like children.
/// State changes flow to later siblings and their descendants, but a nested record receives a copy
/// and cannot mutate its parent's state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TypeTreeTraversalContext {
    has_managed_registry: bool,
}

impl TypeTreeTraversalContext {
    #[must_use]
    pub const fn root() -> Self {
        Self {
            has_managed_registry: false,
        }
    }

    /// Returns the context for one child, or `None` when a duplicate registry has zero extent.
    ///
    /// Call this on a local copy while iterating siblings. Pair, ReferencedObject, and collection
    /// adapters intentionally do not update the registry guard.
    #[must_use]
    pub fn descend(&mut self, parent: SchemaNode<'_>, child: SchemaNode<'_>) -> Option<Self> {
        debug_assert!(std::ptr::eq(parent.arena, child.arena));
        if guards_managed_registry(parent.kind()) && child.kind() == SemanticKind::ManagedRegistry {
            if self.has_managed_registry {
                return None;
            }
            self.has_managed_registry = true;
        }
        Some(*self)
    }
}

/// An immutable, compiled TypeTree program.
///
/// Cloning this value is cheap. Raw TypeTree nodes are never exposed through this interface.
#[derive(Debug, Clone)]
pub struct TypeTreeSchema {
    program: Arc<SchemaProgram>,
    managed: Option<Arc<ManagedReferenceCatalog>>,
}

impl TypeTreeSchema {
    #[must_use]
    pub fn root(&self) -> SchemaNode<'_> {
        self.node(self.program.root)
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.program.arena.nodes.len()
            + self
                .managed
                .as_deref()
                .map_or(0, |catalog| catalog.arena.nodes.len())
    }

    fn node(&self, id: NodeId) -> SchemaNode<'_> {
        SchemaNode {
            arena: &self.program.arena,
            id,
        }
    }

    pub(super) fn owns_node(&self, node: SchemaNode<'_>) -> bool {
        std::ptr::eq(&self.program.arena, node.arena)
            || self
                .managed
                .as_deref()
                .is_some_and(|catalog| std::ptr::eq(&catalog.arena, node.arena))
    }

    /// Resolves a managed-reference TypeTree without allocating a lookup key.
    ///
    /// An empty class name represents a null managed reference and intentionally resolves to
    /// `None`; traversal adapters must treat that case as a zero-extent payload.
    #[must_use]
    pub fn resolve_managed_root(
        &self,
        class_name: &str,
        namespace: &str,
        assembly_name: &str,
    ) -> Option<SchemaNode<'_>> {
        if class_name.is_empty() {
            return None;
        }

        let catalog = self.managed.as_deref()?;
        catalog
            .reference_index
            .binary_search_by(|entry| entry.compare(class_name, namespace, assembly_name))
            .ok()
            .map(|index| SchemaNode {
                arena: &catalog.arena,
                id: catalog.reference_index[index].root,
            })
    }
}

/// A borrowed, read-only view into a compiled TypeTree schema.
#[derive(Clone, Copy)]
pub struct SchemaNode<'schema> {
    arena: &'schema SchemaArena,
    id: NodeId,
}

impl<'schema> SchemaNode<'schema> {
    #[must_use]
    pub fn name(self) -> &'schema str {
        &self.compiled().name
    }

    #[must_use]
    pub fn type_name(self) -> &'schema str {
        &self.compiled().type_name
    }

    /// Returns a compatibility projection without storing parallel semantic state.
    #[must_use]
    pub fn kind(self) -> SemanticKind {
        self.compiled().semantics.kind()
    }

    /// Returns the canonical semantic shape together with its precompiled typed layout.
    #[must_use]
    pub fn semantic_layout(self) -> SemanticLayout<'schema> {
        match self.compiled().semantics {
            CompiledSemantics::Scalar(kind) => SemanticLayout::Scalar(kind),
            CompiledSemantics::String => SemanticLayout::String,
            CompiledSemantics::TypelessData => SemanticLayout::TypelessData,
            CompiledSemantics::Sequence(layout) => {
                SemanticLayout::Sequence(self.sequence_view(layout))
            }
            CompiledSemantics::Map(layout) => SemanticLayout::Map(self.sequence_view(layout)),
            CompiledSemantics::Pair(layout) => SemanticLayout::Pair(self.pair_view(layout)),
            CompiledSemantics::PPtr(layout) => SemanticLayout::PPtr(self.pptr_view(layout)),
            CompiledSemantics::ReferencedObject(layout) => {
                SemanticLayout::ReferencedObject(self.referenced_object_view(layout))
            }
            CompiledSemantics::ManagedPayload => SemanticLayout::ManagedPayload,
            CompiledSemantics::ManagedRegistry => SemanticLayout::ManagedRegistry,
            CompiledSemantics::Record => SemanticLayout::Record,
            CompiledSemantics::OpaqueFixed { byte_size } => {
                SemanticLayout::OpaqueFixed { byte_size }
            }
        }
    }

    #[must_use]
    pub fn align_after(self) -> bool {
        self.compiled().align_after
    }

    #[must_use]
    pub fn child_count(self) -> usize {
        self.compiled().children.len()
    }

    #[must_use]
    pub fn child(self, index: usize) -> Option<Self> {
        let edge = self
            .arena
            .edges
            .get(self.compiled().children.clone())?
            .get(index)?;
        Some(self.with_id(*edge))
    }

    #[must_use]
    pub fn children(self) -> SchemaChildren<'schema> {
        let edges = self
            .arena
            .edges
            .get(self.compiled().children.clone())
            .unwrap_or(&[]);
        SchemaChildren {
            arena: self.arena,
            edges: edges.iter(),
        }
    }

    /// Returns the precompiled collection layout for a sequence or map node.
    #[must_use]
    pub fn sequence_layout(self) -> Option<SequenceLayout<'schema>> {
        match self.compiled().semantics {
            CompiledSemantics::Sequence(layout) | CompiledSemantics::Map(layout) => {
                Some(self.sequence_view(layout))
            }
            _ => None,
        }
    }

    /// Returns the precompiled children for a pair node.
    #[must_use]
    pub fn pair_layout(self) -> Option<PairLayout<'schema>> {
        let CompiledSemantics::Pair(layout) = self.compiled().semantics else {
            return None;
        };
        Some(self.pair_view(layout))
    }

    /// Returns the precompiled pointer layout for a PPtr node.
    #[must_use]
    pub fn pptr_layout(self) -> Option<PPtrLayout<'schema>> {
        let CompiledSemantics::PPtr(layout) = self.compiled().semantics else {
            return None;
        };
        Some(self.pptr_view(layout))
    }

    /// Returns the precompiled managed-reference layout for a ReferencedObject node.
    #[must_use]
    pub fn referenced_object_layout(self) -> Option<ReferencedObjectLayout<'schema>> {
        let CompiledSemantics::ReferencedObject(layout) = self.compiled().semantics else {
            return None;
        };
        Some(self.referenced_object_view(layout))
    }

    fn compiled(self) -> &'schema CompiledNode {
        // NodeId values are created only after the backing node has been appended.
        &self.arena.nodes[self.id.0]
    }

    fn with_id(self, id: NodeId) -> Self {
        Self {
            arena: self.arena,
            id,
        }
    }

    fn sequence_view(self, layout: SequenceNodeLayout) -> SequenceLayout<'schema> {
        SequenceLayout {
            element: self.with_id(layout.element),
            bulk_primitive: layout.bulk_primitive,
        }
    }

    fn pair_view(self, layout: PairNodeLayout) -> PairLayout<'schema> {
        PairLayout {
            first: self.with_id(layout.first),
            second: self.with_id(layout.second),
        }
    }

    fn pptr_view(self, layout: PPtrNodeLayout) -> PPtrLayout<'schema> {
        PPtrLayout {
            file_child: self.with_id(layout.file_child),
            file_primitive: layout.file_integer.primitive,
            path_child: self.with_id(layout.path_child),
            path_primitive: layout.path_integer.primitive,
        }
    }

    fn referenced_object_view(
        self,
        layout: ReferencedObjectNodeLayout,
    ) -> ReferencedObjectLayout<'schema> {
        let payload = match layout.payload {
            ManagedPayloadNode::Dynamic(node) => ManagedPayload::Dynamic(self.with_id(node)),
            ManagedPayloadNode::Fallback(node) => ManagedPayload::Fallback(self.with_id(node)),
        };
        ReferencedObjectLayout {
            type_node: self.with_id(layout.type_node),
            class_field: self.with_id(layout.class_field),
            namespace_field: self.with_id(layout.namespace_field),
            assembly_field: self.with_id(layout.assembly_field),
            payload,
        }
    }
}

impl PartialEq for SchemaNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.arena, other.arena) && self.id == other.id
    }
}

impl Eq for SchemaNode<'_> {}

impl std::fmt::Debug for SchemaNode<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SchemaNode")
            .field("name", &self.name())
            .field("type_name", &self.type_name())
            .field("kind", &self.kind())
            .field("align_after", &self.align_after())
            .finish()
    }
}

/// Iterator over the direct semantic children of a compiled node.
pub struct SchemaChildren<'schema> {
    arena: &'schema SchemaArena,
    edges: std::slice::Iter<'schema, NodeId>,
}

impl<'schema> Iterator for SchemaChildren<'schema> {
    type Item = SchemaNode<'schema>;

    fn next(&mut self) -> Option<Self::Item> {
        self.edges.next().copied().map(|id| SchemaNode {
            arena: self.arena,
            id,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.edges.size_hint()
    }
}

impl DoubleEndedIterator for SchemaChildren<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.edges.next_back().copied().map(|id| SchemaNode {
            arena: self.arena,
            id,
        })
    }
}

impl ExactSizeIterator for SchemaChildren<'_> {}
impl FusedIterator for SchemaChildren<'_> {}

/// Read-only collection layout compiled from a sequence or map node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceLayout<'schema> {
    element: SchemaNode<'schema>,
    bulk_primitive: Option<PrimitiveKind>,
}

impl<'schema> SequenceLayout<'schema> {
    /// Returns the element schema whose root alignment is represented by the collection.
    #[must_use]
    pub fn element(self) -> SchemaNode<'schema> {
        self.element
    }

    /// Returns the primitive kind when the element payload can use a contiguous bulk codec.
    #[must_use]
    pub fn bulk_primitive(self) -> Option<PrimitiveKind> {
        self.bulk_primitive
    }
}

/// Read-only pair layout whose two children were validated during schema compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairLayout<'schema> {
    first: SchemaNode<'schema>,
    second: SchemaNode<'schema>,
}

impl<'schema> PairLayout<'schema> {
    #[must_use]
    pub fn first(self) -> SchemaNode<'schema> {
        self.first
    }

    #[must_use]
    pub fn second(self) -> SchemaNode<'schema> {
        self.second
    }

    #[must_use]
    pub fn children(self) -> [SchemaNode<'schema>; 2] {
        [self.first, self.second]
    }
}

/// Read-only PPtr layout with prevalidated integer fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PPtrLayout<'schema> {
    file_child: SchemaNode<'schema>,
    file_primitive: PrimitiveKind,
    path_child: SchemaNode<'schema>,
    path_primitive: PrimitiveKind,
}

impl<'schema> PPtrLayout<'schema> {
    #[must_use]
    pub fn file_child(self) -> SchemaNode<'schema> {
        self.file_child
    }

    #[must_use]
    pub fn file_primitive(self) -> PrimitiveKind {
        self.file_primitive
    }

    #[must_use]
    pub fn path_child(self) -> SchemaNode<'schema> {
        self.path_child
    }

    #[must_use]
    pub fn path_primitive(self) -> PrimitiveKind {
        self.path_primitive
    }
}

/// Canonical semantic shape of a borrowed schema node.
///
/// Layout-bearing variants can only be created by successful schema compilation, so adapters do
/// not need to reconstruct or revalidate structural contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticLayout<'schema> {
    Scalar(PrimitiveKind),
    String,
    TypelessData,
    Sequence(SequenceLayout<'schema>),
    Map(SequenceLayout<'schema>),
    Pair(PairLayout<'schema>),
    PPtr(PPtrLayout<'schema>),
    ReferencedObject(ReferencedObjectLayout<'schema>),
    ManagedPayload,
    ManagedRegistry,
    Record,
    OpaqueFixed { byte_size: u64 },
}

impl SemanticLayout<'_> {
    /// Returns the compatibility semantic kind projection.
    #[must_use]
    pub const fn kind(self) -> SemanticKind {
        match self {
            Self::Scalar(kind) => SemanticKind::Scalar(kind),
            Self::String => SemanticKind::String,
            Self::TypelessData => SemanticKind::TypelessData,
            Self::Sequence(_) => SemanticKind::Sequence,
            Self::Map(_) => SemanticKind::Map,
            Self::Pair(_) => SemanticKind::Pair,
            Self::PPtr(_) => SemanticKind::PPtr,
            Self::ReferencedObject(_) => SemanticKind::ReferencedObject,
            Self::ManagedPayload => SemanticKind::ManagedPayload,
            Self::ManagedRegistry => SemanticKind::ManagedRegistry,
            Self::Record => SemanticKind::Record,
            Self::OpaqueFixed { byte_size } => SemanticKind::OpaqueFixed { byte_size },
        }
    }
}

/// Placeholder policy for a managed-reference payload.
///
/// A dynamic placeholder has no statically provable extent. A fallback node can be traversed when
/// a non-empty managed type key does not resolve. Resolved payloads always use the managed root
/// returned by [`TypeTreeSchema::resolve_managed_root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedPayload<'schema> {
    Dynamic(SchemaNode<'schema>),
    Fallback(SchemaNode<'schema>),
}

impl<'schema> ManagedPayload<'schema> {
    /// Returns the ordered ReferencedObjectData child represented by this policy.
    #[must_use]
    pub fn node(self) -> SchemaNode<'schema> {
        match self {
            Self::Dynamic(node) | Self::Fallback(node) => node,
        }
    }

    /// Returns a statically traversable unresolved-type fallback when one exists.
    #[must_use]
    pub fn fallback(self) -> Option<SchemaNode<'schema>> {
        match self {
            Self::Dynamic(_) => None,
            Self::Fallback(node) => Some(node),
        }
    }
}

/// Read-only ReferencedObject layout with its type triplet and payload prevalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferencedObjectLayout<'schema> {
    type_node: SchemaNode<'schema>,
    class_field: SchemaNode<'schema>,
    namespace_field: SchemaNode<'schema>,
    assembly_field: SchemaNode<'schema>,
    payload: ManagedPayload<'schema>,
}

impl<'schema> ReferencedObjectLayout<'schema> {
    #[must_use]
    pub fn type_node(self) -> SchemaNode<'schema> {
        self.type_node
    }

    #[must_use]
    pub fn class_field(self) -> SchemaNode<'schema> {
        self.class_field
    }

    #[must_use]
    pub fn namespace_field(self) -> SchemaNode<'schema> {
        self.namespace_field
    }

    #[must_use]
    pub fn assembly_field(self) -> SchemaNode<'schema> {
        self.assembly_field
    }

    #[must_use]
    pub fn payload(self) -> ManagedPayload<'schema> {
        self.payload
    }

    /// Identifies the ordered child that owns the managed type triplet.
    #[must_use]
    pub fn is_type_node(self, node: SchemaNode<'schema>) -> bool {
        self.type_node == node
    }

    /// Identifies the ordered child whose wire layout is selected dynamically.
    #[must_use]
    pub fn is_payload(self, node: SchemaNode<'schema>) -> bool {
        self.payload.node() == node
    }
}

#[derive(Debug)]
struct SchemaProgram {
    arena: SchemaArena,
    root: NodeId,
}

#[derive(Debug)]
struct SchemaArena {
    nodes: Vec<CompiledNode>,
    edges: Vec<NodeId>,
}

#[derive(Debug)]
pub(crate) struct ManagedReferenceCatalog {
    arena: SchemaArena,
    reference_index: Box<[ManagedReferenceEntry]>,
}

#[derive(Debug)]
struct CompiledNode {
    name: String,
    type_name: String,
    children: Range<usize>,
    align_after: bool,
    contains_managed_reference: bool,
    semantics: CompiledSemantics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompiledSemantics {
    Scalar(PrimitiveKind),
    String,
    TypelessData,
    Sequence(SequenceNodeLayout),
    Map(SequenceNodeLayout),
    Pair(PairNodeLayout),
    PPtr(PPtrNodeLayout),
    ReferencedObject(ReferencedObjectNodeLayout),
    ManagedPayload,
    ManagedRegistry,
    Record,
    OpaqueFixed { byte_size: u64 },
}

impl CompiledSemantics {
    const fn kind(self) -> SemanticKind {
        match self {
            Self::Scalar(kind) => SemanticKind::Scalar(kind),
            Self::String => SemanticKind::String,
            Self::TypelessData => SemanticKind::TypelessData,
            Self::Sequence(_) => SemanticKind::Sequence,
            Self::Map(_) => SemanticKind::Map,
            Self::Pair(_) => SemanticKind::Pair,
            Self::PPtr(_) => SemanticKind::PPtr,
            Self::ReferencedObject(_) => SemanticKind::ReferencedObject,
            Self::ManagedPayload => SemanticKind::ManagedPayload,
            Self::ManagedRegistry => SemanticKind::ManagedRegistry,
            Self::Record => SemanticKind::Record,
            Self::OpaqueFixed { byte_size } => SemanticKind::OpaqueFixed { byte_size },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SequenceNodeLayout {
    element: NodeId,
    bulk_primitive: Option<PrimitiveKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PairNodeLayout {
    first: NodeId,
    second: NodeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PPtrNodeLayout {
    file_child: NodeId,
    file_integer: IntegerLayout,
    path_child: NodeId,
    path_integer: IntegerLayout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReferencedObjectNodeLayout {
    type_node: NodeId,
    class_field: NodeId,
    namespace_field: NodeId,
    assembly_field: NodeId,
    payload: ManagedPayloadNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedPayloadNode {
    Dynamic(NodeId),
    Fallback(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntegerLayout {
    primitive: PrimitiveKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeId(usize);

impl NodeId {
    const INVALID: Self = Self(usize::MAX);
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ManagedReferenceKey {
    class_name: String,
    namespace: String,
    assembly_name: String,
}

impl ManagedReferenceKey {
    const fn new(class_name: String, namespace: String, assembly_name: String) -> Self {
        Self {
            class_name,
            namespace,
            assembly_name,
        }
    }
}

#[derive(Debug)]
struct ManagedReferenceEntry {
    key: ManagedReferenceKey,
    root: NodeId,
}

impl ManagedReferenceEntry {
    fn compare(&self, class_name: &str, namespace: &str, assembly_name: &str) -> Ordering {
        (
            self.key.class_name.as_str(),
            self.key.namespace.as_str(),
            self.key.assembly_name.as_str(),
        )
            .cmp(&(class_name, namespace, assembly_name))
    }
}
