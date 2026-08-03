use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::mem::size_of;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, ObjectAddress, SourceId, WorkspaceId, WorkspaceRevision,
};

use crate::workspace::{
    GenericMutation, MutationValue, ReferenceTarget, SequenceMutation, SourceExpectation,
    SourceObjectDescriptor, WorkspaceView,
};

use super::{
    is_transform_or_rect_transform_class, is_transform_or_rect_transform_class_id,
    validate_transform_or_rect_transform_class,
};
use crate::schema::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, decode_local_reference, value_kind,
};

/// Final position of a child inside one parent's ordered Transform list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HierarchyPlacementV1 {
    First,
    Last,
    Index { index: u32 },
}

/// Destination of a version-one hierarchy reparent operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HierarchyDestinationV1 {
    Root,
    Parent {
        parent: ObjectAddress,
        placement: HierarchyPlacementV1,
    },
}

impl HierarchyDestinationV1 {
    #[must_use]
    pub const fn root() -> Self {
        Self::Root
    }

    #[must_use]
    pub const fn parent(parent: ObjectAddress, placement: HierarchyPlacementV1) -> Self {
        Self::Parent { parent, placement }
    }

    #[must_use]
    pub const fn parent_address(&self) -> Option<&ObjectAddress> {
        match self {
            Self::Root => None,
            Self::Parent { parent, .. } => Some(parent),
        }
    }

    #[must_use]
    pub const fn placement(&self) -> Option<HierarchyPlacementV1> {
        match self {
            Self::Root => None,
            Self::Parent { placement, .. } => Some(*placement),
        }
    }
}

/// Revision-bound hierarchy intent. Current parent and child facts are always derived from a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyIntentV1 {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    child: ObjectAddress,
    destination: HierarchyDestinationV1,
}

impl HierarchyIntentV1 {
    #[must_use]
    pub const fn new(
        workspace_id: WorkspaceId,
        revision: WorkspaceRevision,
        child: ObjectAddress,
        destination: HierarchyDestinationV1,
    ) -> Self {
        Self {
            workspace_id,
            revision,
            child,
            destination,
        }
    }

    #[must_use]
    pub fn for_view(
        view: &dyn WorkspaceView,
        child: ObjectAddress,
        destination: HierarchyDestinationV1,
    ) -> Self {
        Self::new(view.workspace_id(), view.revision(), child, destination)
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn child(&self) -> &ObjectAddress {
        &self.child
    }

    #[must_use]
    pub const fn destination(&self) -> &HierarchyDestinationV1 {
        &self.destination
    }
}

/// Schema-aware hierarchy mutation over facts derived from one immutable workspace view.
#[derive(Debug, Clone, Copy, Default)]
pub struct HierarchyRecipe;

impl HierarchyRecipe {
    pub fn lower(
        planner: &SchemaRecipePlanner<'_>,
        intent: &HierarchyIntentV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        validate_context(planner, intent)?;
        if intent.destination.parent_address() == Some(intent.child()) {
            let mut output = RecipeOutputBuilder::new(budget);
            return Err(RecipeError::SelfParent {
                child: output.address(intent.child())?,
            });
        }

        let projection = HierarchyProjection::derive(planner, intent, budget)?;
        lower_projection(planner, intent, &projection, budget)
    }
}

pub(crate) fn validate_hierarchy_target(
    planner: &SchemaRecipePlanner<'_>,
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<(), RecipeError> {
    let mut output = RecipeOutputBuilder::new(budget);
    validate_transform_or_rect_transform_class(object, &mut output)?;
    let child = output.address(object.address())?;
    let intent = HierarchyIntentV1::new(
        planner.workspace_id(),
        planner.revision(),
        child,
        HierarchyDestinationV1::Root,
    );
    let _ = HierarchyProjection::derive(planner, &intent, output.budget())?;
    Ok(())
}

fn validate_context(
    planner: &SchemaRecipePlanner<'_>,
    intent: &HierarchyIntentV1,
) -> Result<(), RecipeError> {
    if planner.workspace_id() != intent.workspace_id() {
        return Err(RecipeError::HierarchyWorkspaceMismatch {
            expected: intent.workspace_id(),
            actual: planner.workspace_id(),
        });
    }
    if planner.revision() != intent.revision() {
        return Err(RecipeError::HierarchyRevisionMismatch {
            expected: intent.revision(),
            actual: planner.revision(),
        });
    }
    Ok(())
}

struct ObservedHierarchyNode {
    descriptor: SourceObjectDescriptor,
    address: ObjectAddress,
    observation: HierarchyObservation,
    membership: HierarchyMembership,
}

enum HierarchyObservation {
    AddressOnly,
    Parent {
        parent: Option<ObjectAddress>,
    },
    Full {
        parent: Option<ObjectAddress>,
        children: Vec<ObjectAddress>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HierarchyMembership {
    Outside,
    Included,
    Core,
}

impl ObservedHierarchyNode {
    fn address(&self) -> &ObjectAddress {
        &self.address
    }

    fn parent(&self) -> Result<Option<&ObjectAddress>, RecipeError> {
        match &self.observation {
            HierarchyObservation::Parent { parent } | HierarchyObservation::Full { parent, .. } => {
                Ok(parent.as_ref())
            }
            HierarchyObservation::AddressOnly => Err(RecipeError::InspectionContractMismatch),
        }
    }

    fn children(&self) -> Result<&[ObjectAddress], RecipeError> {
        match &self.observation {
            HierarchyObservation::Full { children, .. } => Ok(children),
            HierarchyObservation::AddressOnly | HierarchyObservation::Parent { .. } => {
                Err(RecipeError::InspectionContractMismatch)
            }
        }
    }

    const fn is_core(&self) -> bool {
        matches!(self.membership, HierarchyMembership::Core)
    }

    const fn is_included(&self) -> bool {
        !matches!(self.membership, HierarchyMembership::Outside)
    }
}

struct HierarchyProjection {
    source: SourceId,
    nodes: Vec<ObservedHierarchyNode>,
    index: HashMap<ObjectAddress, usize>,
}

#[derive(Clone, Copy)]
enum RequiredNode {
    Target,
    Parent,
}

impl HierarchyProjection {
    fn derive(
        planner: &SchemaRecipePlanner<'_>,
        intent: &HierarchyIntentV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, RecipeError> {
        let target = inspect_required(planner, intent.child(), RequiredNode::Target, budget)?;
        let source = target.source_id();
        {
            let mut output = RecipeOutputBuilder::new(budget);
            validate_transform_or_rect_transform_class(&target, &mut output)?;
        }
        drop(target);

        let mut output = RecipeOutputBuilder::new(budget);
        let object_count = planner.object_count_in_source(source, output.budget())?;
        consume_visits(
            output.budget(),
            object_count,
            "hierarchy_source_classification_visits",
        )?;
        let mut transform_count = 0_usize;
        for ordinal in 0..object_count {
            let descriptor = planner.source_object_descriptor(source, ordinal, output.budget())?;
            if is_transform_or_rect_transform_class_id(descriptor.class_id()) {
                transform_count =
                    transform_count
                        .checked_add(1)
                        .ok_or(BudgetError::ArithmeticOverflow {
                            resource: "hierarchy_transform_count",
                        })?;
            }
        }

        let mut nodes =
            output.vec::<ObservedHierarchyNode>(transform_count, "hierarchy source projection")?;
        consume_visits(
            output.budget(),
            object_count,
            "hierarchy_source_projection_visits",
        )?;
        for ordinal in 0..object_count {
            let descriptor = planner.source_object_descriptor(source, ordinal, output.budget())?;
            if !is_transform_or_rect_transform_class_id(descriptor.class_id()) {
                continue;
            }
            let address = planner.source_object_address(&descriptor, output.budget())?;
            nodes.push(ObservedHierarchyNode {
                descriptor,
                address,
                observation: HierarchyObservation::AddressOnly,
                membership: HierarchyMembership::Outside,
            });
        }
        let mut index = reserve_hash_map::<ObjectAddress, usize>(
            nodes.len(),
            output.budget(),
            "hierarchy address index",
        )?;
        consume_visits(
            output.budget(),
            nodes.len(),
            "hierarchy_projection_index_visits",
        )?;
        for (node_index, node) in nodes.iter().enumerate() {
            let address = output.address(node.address())?;
            if index.insert(address, node_index).is_some() {
                return Err(RecipeError::InspectionContractMismatch);
            }
        }

        let mut projection = Self {
            source,
            nodes,
            index,
        };
        projection.walk_parent_chain(
            planner,
            intent.child(),
            RequiredNode::Target,
            None,
            output.budget(),
        )?;
        if let Some(parent) = intent.destination.parent_address() {
            projection.walk_parent_chain(
                planner,
                parent,
                RequiredNode::Parent,
                Some(intent.child()),
                output.budget(),
            )?;
        }
        projection.expand_direct_children(planner, output.budget())?;
        projection.validate_incoming_memberships(planner, output.budget())?;
        Ok(projection)
    }

    fn walk_parent_chain(
        &mut self,
        planner: &SchemaRecipePlanner<'_>,
        start: &ObjectAddress,
        role: RequiredNode,
        forbidden: Option<&ObjectAddress>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), RecipeError> {
        let mut output = RecipeOutputBuilder::new(budget);
        let mut visiting = output.vec::<bool>(self.nodes.len(), "hierarchy parent chain marks")?;
        visiting.resize(self.nodes.len(), false);
        let mut cursor = self.required_index(planner, start, role, output.budget())?;
        let forbidden = forbidden.and_then(|address| self.node_index(address).ok());
        consume_visits(
            output.budget(),
            self.nodes.len(),
            "hierarchy_parent_chain_lookup_visits",
        )?;

        loop {
            if forbidden == Some(cursor) || visiting[cursor] {
                return Err(RecipeError::HierarchyCycle {
                    at: output.address(self.nodes[cursor].address())?,
                });
            }
            visiting[cursor] = true;
            self.observe_node(planner, cursor, true, output.budget())?;
            self.nodes[cursor].membership = HierarchyMembership::Core;
            let Some(parent) = self.nodes[cursor].parent()? else {
                break;
            };
            cursor = self.required_index(planner, parent, RequiredNode::Parent, output.budget())?;
        }
        Ok(())
    }

    fn expand_direct_children(
        &mut self,
        planner: &SchemaRecipePlanner<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), RecipeError> {
        let mut output = RecipeOutputBuilder::new(budget);
        let mut pending = output.vec::<usize>(self.nodes.len(), "hierarchy direct children")?;
        for parent_index in 0..self.nodes.len() {
            if !self.nodes[parent_index].is_core() {
                continue;
            }
            let child_count = self.nodes[parent_index].children()?.len();
            for child_ordinal in 0..child_count {
                let child_index = {
                    let child = &self.nodes[parent_index].children()?[child_ordinal];
                    consume_visits(output.budget(), 1, "hierarchy_direct_child_lookup_visits")?;
                    let Ok(child_index) = self.node_index(child) else {
                        return Err(RecipeError::MissingChild {
                            child: output.address(child)?,
                        });
                    };
                    child_index
                };
                if !self.nodes[child_index].is_included() {
                    self.nodes[child_index].membership = HierarchyMembership::Included;
                    pending.push(child_index);
                }
            }
        }
        for index in pending {
            self.observe_node(planner, index, false, output.budget())?;
        }
        Ok(())
    }

    fn validate_incoming_memberships(
        &self,
        planner: &SchemaRecipePlanner<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), RecipeError> {
        let mut output = RecipeOutputBuilder::new(budget);
        let mut incoming =
            output.vec::<Option<usize>>(self.nodes.len(), "hierarchy incoming parents")?;
        incoming.resize(self.nodes.len(), None);
        consume_visits(
            output.budget(),
            self.nodes.len(),
            "hierarchy_incoming_node_visits",
        )?;

        for parent_index in 0..self.nodes.len() {
            let projected;
            let children = if self.nodes[parent_index].is_core() {
                self.nodes[parent_index].children()?
            } else {
                let object = planner.inspect_source_object(
                    &self.nodes[parent_index].descriptor,
                    self.nodes[parent_index].address(),
                    output.budget(),
                )?;
                if !is_transform_or_rect_transform_class(object.class()) {
                    continue;
                }
                projected = observe_local_children_lenient(&object, output.budget())?;
                &projected
            };
            for child in children {
                consume_visits(output.budget(), 1, "hierarchy_incoming_edge_visits")?;
                let Ok(child_index) = self.node_index(child) else {
                    continue;
                };
                if !self.nodes[child_index].is_included() {
                    continue;
                }
                match incoming[child_index] {
                    None => incoming[child_index] = Some(parent_index),
                    Some(existing) if existing == parent_index => {
                        return Err(RecipeError::DuplicateChildMembership {
                            child: output.address(child)?,
                        });
                    }
                    Some(_) => {
                        return Err(RecipeError::MultipleParents {
                            child: output.address(child)?,
                        });
                    }
                }
            }
        }

        for (child_index, child) in self.nodes.iter().enumerate() {
            if !child.is_included() {
                continue;
            }
            let expected = match child.parent()? {
                Some(parent) => match self.node_index(parent) {
                    Ok(index) => Some(index),
                    Err(_) => {
                        return Err(RecipeError::MissingParent {
                            parent: output.address(parent)?,
                        });
                    }
                },
                None => None,
            };
            let actual = incoming[child_index];
            if actual != expected {
                return Err(if actual.is_some() && expected.is_some() {
                    RecipeError::MultipleParents {
                        child: output.address(child.address())?,
                    }
                } else {
                    RecipeError::ParentChildMismatch {
                        child: output.address(child.address())?,
                    }
                });
            }
        }
        Ok(())
    }

    fn required_index(
        &self,
        planner: &SchemaRecipePlanner<'_>,
        address: &ObjectAddress,
        role: RequiredNode,
        budget: &mut AssetLoadBudget,
    ) -> Result<usize, RecipeError> {
        if let Ok(index) = self.node_index(address) {
            return Ok(index);
        }
        let object = inspect_required(planner, address, role, budget)?;
        if object.source_id() != self.source {
            return Err(RecipeError::CrossSourceHierarchy);
        }
        let mut output = RecipeOutputBuilder::new(budget);
        validate_transform_or_rect_transform_class(&object, &mut output)?;
        Err(RecipeError::InspectionContractMismatch)
    }

    fn observe_node(
        &mut self,
        planner: &SchemaRecipePlanner<'_>,
        index: usize,
        require_children: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), RecipeError> {
        if matches!(
            (&self.nodes[index].observation, require_children),
            (HierarchyObservation::Full { .. }, _) | (HierarchyObservation::Parent { .. }, false)
        ) {
            return Ok(());
        }
        let object = planner.inspect_source_object(
            &self.nodes[index].descriptor,
            self.nodes[index].address(),
            budget,
        )?;
        if object.address() != self.nodes[index].address() {
            return Err(RecipeError::InspectionContractMismatch);
        }
        {
            let mut output = RecipeOutputBuilder::new(budget);
            validate_transform_or_rect_transform_class(&object, &mut output)?;
        }
        let observed_parent = matches!(
            &self.nodes[index].observation,
            HierarchyObservation::AddressOnly
        )
        .then(|| observe_parent(&object, budget))
        .transpose()?;
        let observed_children = require_children
            .then(|| observe_children(&object, budget))
            .transpose()?;
        let previous = std::mem::replace(
            &mut self.nodes[index].observation,
            HierarchyObservation::AddressOnly,
        );
        self.nodes[index].observation = match (previous, observed_children) {
            (HierarchyObservation::AddressOnly, Some(children)) => HierarchyObservation::Full {
                parent: observed_parent.ok_or(RecipeError::InspectionContractMismatch)?,
                children,
            },
            (HierarchyObservation::AddressOnly, None) => HierarchyObservation::Parent {
                parent: observed_parent.ok_or(RecipeError::InspectionContractMismatch)?,
            },
            (HierarchyObservation::Parent { parent }, Some(children)) => {
                HierarchyObservation::Full { parent, children }
            }
            (state @ HierarchyObservation::Parent { .. }, None)
            | (state @ HierarchyObservation::Full { .. }, _) => state,
        };
        Ok(())
    }

    fn inspect_node(
        &self,
        planner: &SchemaRecipePlanner<'_>,
        address: &ObjectAddress,
        role: RequiredNode,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeObject, RecipeError> {
        let index = match self.node_index(address) {
            Ok(index) => index,
            Err(_) => {
                return Err(match role {
                    RequiredNode::Target => RecipeError::TargetMissing,
                    RequiredNode::Parent => RecipeError::MissingParent {
                        parent: clone_address(address, budget)?,
                    },
                });
            }
        };
        let object = planner.inspect_source_object(
            &self.nodes[index].descriptor,
            self.nodes[index].address(),
            budget,
        )?;
        if object.address() != address {
            return Err(RecipeError::InspectionContractMismatch);
        }
        Ok(object)
    }

    fn node(&self, address: &ObjectAddress) -> Option<&ObservedHierarchyNode> {
        self.node_index(address)
            .ok()
            .map(|index| &self.nodes[index])
    }

    fn node_index(&self, address: &ObjectAddress) -> Result<usize, ()> {
        self.index.get(address).copied().ok_or(())
    }
}

fn inspect_required(
    planner: &SchemaRecipePlanner<'_>,
    address: &ObjectAddress,
    role: RequiredNode,
    budget: &mut AssetLoadBudget,
) -> Result<RecipeObject, RecipeError> {
    match planner.inspect(address, budget) {
        Ok(object) => Ok(object),
        Err(RecipeError::TargetUnloaded) => Err(RecipeError::TargetUnloaded),
        Err(RecipeError::TargetMissing) => match role {
            RequiredNode::Target => Err(RecipeError::TargetMissing),
            RequiredNode::Parent => Err(RecipeError::MissingParent {
                parent: clone_address(address, budget)?,
            }),
        },
        Err(error) => Err(error),
    }
}

fn observe_parent(
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<Option<ObjectAddress>, RecipeError> {
    let mut output = RecipeOutputBuilder::new(budget);
    let path = output.field_path(&["m_Father"])?;
    decode_local_reference(object, &path, &mut output)
}

fn observe_children(
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ObjectAddress>, RecipeError> {
    let mut output = RecipeOutputBuilder::new(budget);
    let path = output.field_path(&["m_Children"])?;
    let value = object.require_field(&path, &mut output)?;
    let Some(values) = value.as_array() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(&path)?,
            expected: "an ordered Transform reference array",
            actual: value_kind(value),
        });
    };
    let mut children = output.vec::<ObjectAddress>(values.len(), "hierarchy children")?;
    for (index, _) in values.iter().enumerate() {
        let child_path = output.append_index(
            &path,
            u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
        )?;
        let Some(child) = decode_local_reference(object, &child_path, &mut output)? else {
            return Err(RecipeError::InvalidReference {
                path: output.path(&child_path)?,
            });
        };
        children.push(child);
    }
    let mut unique = reserve_hash_set::<&ObjectAddress>(
        children.len(),
        output.budget(),
        "hierarchy child duplicate index",
    )?;
    consume_visits(
        output.budget(),
        children.len(),
        "hierarchy_child_duplicate_visits",
    )?;
    for child in &children {
        if !unique.insert(child) {
            return Err(RecipeError::DuplicateChildMembership {
                child: output.address(child)?,
            });
        }
    }
    Ok(children)
}

fn observe_local_children_lenient(
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ObjectAddress>, RecipeError> {
    let mut output = RecipeOutputBuilder::new(budget);
    let path = output.field_path(&["m_Children"])?;
    let Some(values) = object.field(&path).and_then(|value| value.as_array()) else {
        return output.vec(0, "hierarchy lenient children");
    };
    let mut children = output.vec(values.len(), "hierarchy lenient children")?;
    for (index, _) in values.iter().enumerate() {
        let child_path = output.append_index(
            &path,
            u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
        )?;
        match decode_local_reference(object, &child_path, &mut output) {
            Ok(Some(child)) => children.push(child),
            Ok(None) => {}
            Err(
                RecipeError::MissingField { .. }
                | RecipeError::InvalidFieldPath { .. }
                | RecipeError::WrongFieldShape { .. }
                | RecipeError::InvalidReference { .. }
                | RecipeError::UnresolvedReference { .. },
            ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(children)
}

fn lower_projection(
    planner: &SchemaRecipePlanner<'_>,
    intent: &HierarchyIntentV1,
    projection: &HierarchyProjection,
    budget: &mut AssetLoadBudget,
) -> Result<RecipeLowering, RecipeError> {
    let child = projection
        .node(intent.child())
        .ok_or(RecipeError::TargetMissing)?;
    let old_parent = child.parent()?;
    let new_parent = intent.destination.parent_address();
    let mut output = RecipeOutputBuilder::new(budget);

    if old_parent == new_parent {
        let Some(parent_address) = new_parent else {
            return Ok(RecipeLowering::unchanged(
                RecipeId::HierarchyReparentV1,
                SchemaVariantId::HierarchyLocalReferences,
            ));
        };
        let parent = projection
            .node(parent_address)
            .ok_or(RecipeError::InspectionContractMismatch)?;
        let children = parent.children()?;
        let from = children
            .iter()
            .position(|candidate| candidate == intent.child())
            .ok_or(RecipeError::InspectionContractMismatch)?;
        let placement = intent
            .destination
            .placement()
            .ok_or(RecipeError::InvalidPayload {
                reason: "a hierarchy parent requires a placement",
            })?;
        let to = move_placement_index(children.len(), placement)?;
        if from == to {
            return Ok(RecipeLowering::unchanged(
                RecipeId::HierarchyReparentV1,
                SchemaVariantId::HierarchyLocalReferences,
            ));
        }
        let parent_object = projection.inspect_node(
            planner,
            parent_address,
            RequiredNode::Parent,
            output.budget(),
        )?;
        let action = children_edit(
            &parent_object,
            SequenceMutation::Move {
                from: usize_to_u32(from)?,
                to: usize_to_u32(to)?,
            },
            &mut output,
        )?;
        let mut actions = output.vec::<GenericMutation>(1, "hierarchy recipe actions")?;
        actions.push(action);
        return RecipeLowering::changed(
            RecipeId::HierarchyReparentV1,
            SchemaVariantId::HierarchyLocalReferences,
            parent_object.fragment(planner, Vec::new(), actions, &mut output)?,
        );
    }

    let child_object = projection.inspect_node(
        planner,
        intent.child(),
        RequiredNode::Target,
        output.budget(),
    )?;
    let father_path = output.field_path(&["m_Father"])?;
    let father_guard = child_object.field_guard(&father_path, output.budget())?;
    let action_count = 1_usize
        .checked_add(usize::from(old_parent.is_some()))
        .and_then(|count| count.checked_add(usize::from(new_parent.is_some())))
        .ok_or(RecipeError::DigestLengthOverflow)?;
    let mut actions = output.vec::<GenericMutation>(action_count, "hierarchy recipe actions")?;
    let expected = match old_parent {
        Some(parent) => ReferenceTarget::object(output.address(parent)?),
        None => ReferenceTarget::null(),
    };
    let replacement = match new_parent {
        Some(parent) => ReferenceTarget::object(output.address(parent)?),
        None => ReferenceTarget::null(),
    };
    actions.push(GenericMutation::ReferenceReplace {
        target: output.address(intent.child())?,
        path: father_path,
        schema_digest: father_guard.schema_digest(),
        expected,
        replacement,
    });

    if let Some(old_parent) = old_parent {
        let parent = projection
            .node(old_parent)
            .ok_or(RecipeError::InspectionContractMismatch)?;
        let children = parent.children()?;
        let index = children
            .iter()
            .position(|candidate| candidate == intent.child())
            .ok_or(RecipeError::InspectionContractMismatch)?;
        let parent_object =
            projection.inspect_node(planner, old_parent, RequiredNode::Parent, output.budget())?;
        actions.push(children_edit(
            &parent_object,
            SequenceMutation::Remove {
                index: usize_to_u32(index)?,
            },
            &mut output,
        )?);
    }

    if let Some(new_parent) = new_parent {
        let parent = projection
            .node(new_parent)
            .ok_or(RecipeError::InspectionContractMismatch)?;
        let children = parent.children()?;
        let placement = intent
            .destination
            .placement()
            .ok_or(RecipeError::InvalidPayload {
                reason: "a hierarchy parent requires a placement",
            })?;
        let index = insertion_placement_index(children.len(), placement)?;
        let parent_object =
            projection.inspect_node(planner, new_parent, RequiredNode::Parent, output.budget())?;
        actions.push(children_edit(
            &parent_object,
            SequenceMutation::Insert {
                index: usize_to_u32(index)?,
                value: MutationValue::reference(ReferenceTarget::object(
                    output.address(intent.child())?,
                )),
            },
            &mut output,
        )?);
    }

    let mut sources = output.vec::<SourceExpectation>(1, "hierarchy recipe sources")?;
    sources.push(output.source(child_object.source_expectation())?);
    let fragment = output.fragment(
        intent.workspace_id(),
        intent.revision(),
        sources,
        Vec::new(),
        actions,
    )?;
    RecipeLowering::changed(
        RecipeId::HierarchyReparentV1,
        SchemaVariantId::HierarchyLocalReferences,
        fragment,
    )
}

fn children_edit(
    parent: &RecipeObject,
    edit: SequenceMutation,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<GenericMutation, RecipeError> {
    let path = output.field_path(&["m_Children"])?;
    let guard = parent.field_guard(&path, output.budget())?;
    Ok(GenericMutation::SequenceEdit {
        target: output.address(parent.address())?,
        path,
        guard,
        edit,
    })
}

fn move_placement_index(len: usize, placement: HierarchyPlacementV1) -> Result<usize, RecipeError> {
    let maximum = len.checked_sub(1).ok_or(RecipeError::InvalidPayload {
        reason: "cannot reorder an empty hierarchy child list",
    })?;
    match placement {
        HierarchyPlacementV1::First => Ok(0),
        HierarchyPlacementV1::Last => Ok(maximum),
        HierarchyPlacementV1::Index { index } => {
            let index = usize::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?;
            if index <= maximum {
                Ok(index)
            } else {
                Err(RecipeError::ChildPlacementOutOfBounds {
                    index: u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                    maximum: u32::try_from(maximum)
                        .map_err(|_| RecipeError::DigestLengthOverflow)?,
                })
            }
        }
    }
}

fn insertion_placement_index(
    len: usize,
    placement: HierarchyPlacementV1,
) -> Result<usize, RecipeError> {
    match placement {
        HierarchyPlacementV1::First => Ok(0),
        HierarchyPlacementV1::Last => Ok(len),
        HierarchyPlacementV1::Index { index } => {
            let index = usize::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?;
            if index <= len {
                Ok(index)
            } else {
                Err(RecipeError::ChildPlacementOutOfBounds {
                    index: u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                    maximum: u32::try_from(len).map_err(|_| RecipeError::DigestLengthOverflow)?,
                })
            }
        }
    }
}

fn usize_to_u32(value: usize) -> Result<u32, RecipeError> {
    u32::try_from(value).map_err(|_| RecipeError::DigestLengthOverflow)
}

fn clone_address(
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, RecipeError> {
    RecipeOutputBuilder::new(budget).address(address)
}

fn reserve_hash_map<K, V>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<HashMap<K, V>, RecipeError>
where
    K: Eq + Hash,
{
    let entries = hash_table_entries(count, resource)?;
    let bytes = hash_table_bytes::<K, V>(count, resource)?;
    budget.check_entries(entries)?;
    budget.check_bytes(bytes)?;
    let mut index = HashMap::new();
    index
        .try_reserve(count)
        .map_err(|error| RecipeError::AllocationFailed {
            resource,
            requested: count,
            message: error.to_string(),
        })?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(bytes)?;
    Ok(index)
}

fn reserve_hash_set<K>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<HashSet<K>, RecipeError>
where
    K: Eq + Hash,
{
    let entries = hash_table_entries(count, resource)?;
    let bytes = hash_table_bytes::<K, ()>(count, resource)?;
    budget.check_entries(entries)?;
    budget.check_bytes(bytes)?;
    let mut index = HashSet::new();
    index
        .try_reserve(count)
        .map_err(|error| RecipeError::AllocationFailed {
            resource,
            requested: count,
            message: error.to_string(),
        })?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(bytes)?;
    Ok(index)
}

fn hash_table_entries(count: usize, resource: &'static str) -> Result<u64, RecipeError> {
    u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
}

fn hash_table_bytes<K, V>(count: usize, resource: &'static str) -> Result<u64, RecipeError> {
    if count == 0 {
        return Ok(0);
    }
    // HashMap layout is unspecified. This covers load-factor slack, growth slack, and controls.
    let slots = count
        .checked_next_power_of_two()
        .and_then(|value| value.checked_mul(4))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    let slot_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<usize>())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    slots
        .checked_mul(slot_bytes)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| BudgetError::ArithmeticOverflow { resource }.into())
}

fn consume_visits(
    budget: &mut AssetLoadBudget,
    count: usize,
    resource: &'static str,
) -> Result<(), RecipeError> {
    budget.consume_entries(
        u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?,
    )?;
    Ok(())
}
