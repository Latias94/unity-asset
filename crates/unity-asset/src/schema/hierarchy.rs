use unity_asset_core::{
    AssetLoadBudget, BudgetError, FieldPath, ObjectAddress, UnityValue, class_ids, class_names,
};

use crate::workspace::{
    GenericMutation, MutationField, MutationValue, ReferenceTarget, SequenceMutation,
};

use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, ensure_finite, local_reference_matches, validate_recipe_provenance,
    validate_reference_shape, value_kind,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vector2 {
    x: f64,
    y: f64,
}

impl Vector2 {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub const fn x(self) -> f64 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> f64 {
        self.y
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vector3 {
    x: f64,
    y: f64,
    z: f64,
}

impl Vector3 {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    x: f64,
    y: f64,
    z: f64,
    w: f64,
}

impl Quaternion {
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64, w: f64) -> Self {
        Self { x, y, z, w }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformChange {
    LocalPosition(Vector3),
    LocalRotation(Quaternion),
    LocalScale(Vector3),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RectTransformChange {
    AnchoredPosition(Vector2),
    SizeDelta(Vector2),
    AnchorMin(Vector2),
    AnchorMax(Vector2),
    Pivot(Vector2),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransformRecipe;

impl TransformRecipe {
    pub fn lower_transform(
        planner: &SchemaRecipePlanner<'_>,
        transform: &RecipeObject,
        change: TransformChange,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(transform)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_transform_or_rect_transform_class(transform, &mut output)?;
        let (field, replacement) = match change {
            TransformChange::LocalPosition(value) => {
                ensure_finite(&[value.x, value.y, value.z])?;
                ("m_LocalPosition", vector3_value(value, &mut output)?)
            }
            TransformChange::LocalRotation(value) => {
                ensure_finite(&[value.x, value.y, value.z, value.w])?;
                ("m_LocalRotation", quaternion_value(value, &mut output)?)
            }
            TransformChange::LocalScale(value) => {
                ensure_finite(&[value.x, value.y, value.z])?;
                ("m_LocalScale", vector3_value(value, &mut output)?)
            }
        };
        lower_field(
            planner,
            transform,
            field,
            replacement,
            SchemaVariantId::Transform,
            &mut output,
        )
    }

    pub fn lower_rect_transform(
        planner: &SchemaRecipePlanner<'_>,
        transform: &RecipeObject,
        change: RectTransformChange,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(transform)?;
        let mut output = RecipeOutputBuilder::new(budget);
        validate_rect_transform_class(transform, &mut output)?;
        let variant = rect_transform_variant(transform, &mut output)?;
        let (field, value, variant) = match change {
            RectTransformChange::AnchoredPosition(value) => (
                match variant {
                    SchemaVariantId::RectTransformAnchoredPosition => "m_AnchoredPosition",
                    SchemaVariantId::RectTransformLegacyPosition => "m_Position",
                    _ => {
                        return Err(RecipeError::UnsupportedSchema {
                            variant: "RectTransform position field",
                        });
                    }
                },
                value,
                variant,
            ),
            RectTransformChange::SizeDelta(value) => ("m_SizeDelta", value, variant),
            RectTransformChange::AnchorMin(value) => ("m_AnchorMin", value, variant),
            RectTransformChange::AnchorMax(value) => ("m_AnchorMax", value, variant),
            RectTransformChange::Pivot(value) => ("m_Pivot", value, variant),
        };
        ensure_finite(&[value.x, value.y])?;
        lower_field(
            planner,
            transform,
            field,
            vector2_value(value, &mut output)?,
            variant,
            &mut output,
        )
    }
}

fn rect_transform_variant(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<SchemaVariantId, RecipeError> {
    let modern = output.field_path(&["m_AnchoredPosition"])?;
    let legacy = output.field_path(&["m_Position"])?;
    match (object.field(&modern), object.field(&legacy)) {
        (Some(_), None) => Ok(SchemaVariantId::RectTransformAnchoredPosition),
        (None, Some(_)) => Ok(SchemaVariantId::RectTransformLegacyPosition),
        (Some(_), Some(_)) => Err(RecipeError::AmbiguousFieldVariant {
            first: "m_AnchoredPosition",
            second: "m_Position",
        }),
        (None, None) => Err(RecipeError::MissingField { path: modern }),
    }
}

fn lower_field(
    planner: &SchemaRecipePlanner<'_>,
    transform: &RecipeObject,
    field: &'static str,
    replacement: MutationValue,
    variant: SchemaVariantId,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<RecipeLowering, RecipeError> {
    let path = output.field_path(&[field])?;
    validate_vector_shape(
        transform.require_field(&path, output)?,
        &path,
        replacement.view(),
        output,
    )?;
    let guard = transform.field_guard(&path, output.budget())?;
    let action = GenericMutation::FieldReplace {
        target: output.address(transform.address())?,
        path,
        guard,
        replacement,
    };
    let mut actions = output.vec::<GenericMutation>(1, "Transform recipe actions")?;
    actions.push(action);
    RecipeLowering::changed(
        RecipeId::TransformV1,
        variant,
        transform.fragment(planner, Vec::new(), actions, output)?,
    )
}

fn validate_vector_shape(
    existing: &UnityValue,
    path: &FieldPath,
    replacement: crate::workspace::MutationValueRef<'_>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let Some(fields) = existing.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a vector or quaternion object",
            actual: value_kind(existing),
        });
    };
    let crate::workspace::MutationValueRef::Object(expected) = replacement else {
        return Err(RecipeError::InvalidPayload {
            reason: "transform replacement must be an object",
        });
    };
    if expected.iter().all(|field| {
        fields
            .get(field.name())
            .and_then(UnityValue::as_f64)
            .is_some()
    }) {
        Ok(())
    } else {
        Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "numeric components matching the requested transform value",
            actual: value_kind(existing),
        })
    }
}

fn validate_transform_or_rect_transform_class(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    validate_recipe_provenance(object)?;
    let class = object.class();
    let valid = class.class_id == class_ids::TRANSFORM
        && class.class_name == class_names::TRANSFORM
        || class.class_id == class_ids::RECT_TRANSFORM
            && class.class_name == class_names::RECT_TRANSFORM;
    if valid {
        Ok(())
    } else {
        Err(RecipeError::WrongClass {
            expected_id: class_ids::TRANSFORM,
            expected_name: "Transform or RectTransform",
            actual_id: class.class_id,
            actual_name: output.string(&class.class_name, "recipe class diagnostic")?,
        })
    }
}

fn validate_rect_transform_class(
    object: &RecipeObject,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let class = object.class();
    if class.class_id == class_ids::RECT_TRANSFORM
        && class.class_name == class_names::RECT_TRANSFORM
    {
        validate_recipe_provenance(object)
    } else {
        Err(RecipeError::WrongClass {
            expected_id: class_ids::RECT_TRANSFORM,
            expected_name: class_names::RECT_TRANSFORM,
            actual_id: class.class_id,
            actual_name: output.string(&class.class_name, "recipe class diagnostic")?,
        })
    }
}

fn vector2_value(
    value: Vector2,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(2, "Vector2 recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    Ok(MutationValue::object(fields)?)
}

fn vector3_value(
    value: Vector3,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(3, "Vector3 recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    fields.push(output.field("z", MutationValue::float64(value.z))?);
    Ok(MutationValue::object(fields)?)
}

fn quaternion_value(
    value: Quaternion,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let mut fields = output.vec::<MutationField>(4, "Quaternion recipe fields")?;
    fields.push(output.field("x", MutationValue::float64(value.x))?);
    fields.push(output.field("y", MutationValue::float64(value.y))?);
    fields.push(output.field("z", MutationValue::float64(value.z))?);
    fields.push(output.field("w", MutationValue::float64(value.w))?);
    Ok(MutationValue::object(fields)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildPlacement {
    Append,
    At(usize),
}

#[derive(Debug)]
pub struct HierarchyNode {
    object: RecipeObject,
    parent: Option<ObjectAddress>,
    children: Vec<ObjectAddress>,
}

impl HierarchyNode {
    #[must_use]
    pub fn new(
        object: RecipeObject,
        parent: Option<ObjectAddress>,
        children: Vec<ObjectAddress>,
    ) -> Self {
        Self {
            object,
            parent,
            children,
        }
    }

    #[must_use]
    pub const fn object(&self) -> &RecipeObject {
        &self.object
    }

    #[must_use]
    pub const fn parent(&self) -> Option<&ObjectAddress> {
        self.parent.as_ref()
    }

    #[must_use]
    pub fn children(&self) -> &[ObjectAddress] {
        &self.children
    }
}

#[derive(Debug)]
pub struct HierarchyState {
    nodes: Vec<HierarchyNode>,
}

impl HierarchyState {
    pub fn new(
        mut nodes: Vec<HierarchyNode>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, RecipeError> {
        consume_hierarchy_visits(budget, nodes.len(), "hierarchy_node_visits")?;
        nodes.sort_unstable_by(|left, right| left.object.address().cmp(right.object.address()));
        let mut output = RecipeOutputBuilder::new(budget);
        validate_hierarchy_nodes(&nodes, &mut output)?;
        Ok(Self { nodes })
    }

    #[must_use]
    pub fn nodes(&self) -> &[HierarchyNode] {
        &self.nodes
    }

    fn find(&self, address: &ObjectAddress) -> Option<&HierarchyNode> {
        node_index(&self.nodes, address).map(|index| &self.nodes[index])
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HierarchyRecipe;

impl HierarchyRecipe {
    pub fn reparent(
        planner: &SchemaRecipePlanner<'_>,
        hierarchy: &HierarchyState,
        child: &ObjectAddress,
        new_parent: Option<&ObjectAddress>,
        placement: ChildPlacement,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        consume_hierarchy_visits(
            budget,
            hierarchy.nodes().len(),
            "hierarchy_reparent_node_visits",
        )?;
        for node in hierarchy.nodes() {
            planner.validate_object(node.object())?;
        }
        let mut output = RecipeOutputBuilder::new(budget);
        if new_parent == Some(child) {
            return Err(RecipeError::SelfParent {
                child: output.address(child)?,
            });
        }
        let child_node = hierarchy
            .find(child)
            .ok_or_else(|| RecipeError::TargetMissing)?;
        let new_parent_node = if let Some(parent) = new_parent {
            match hierarchy.find(parent) {
                Some(node) => Some(node),
                None => {
                    return Err(RecipeError::MissingParent {
                        parent: output.address(parent)?,
                    });
                }
            }
        } else {
            None
        };
        ensure_not_descendant(hierarchy, child, new_parent, &mut output)?;

        let old_parent = child_node.parent.as_ref();
        if old_parent == new_parent {
            let Some(parent) = new_parent_node else {
                return Ok(RecipeLowering::unchanged(
                    RecipeId::HierarchyReparentV1,
                    SchemaVariantId::HierarchyLocalReferences,
                ));
            };
            let from = parent
                .children
                .iter()
                .position(|candidate| candidate == child)
                .ok_or(RecipeError::ParentChildMismatch {
                    child: output.address(child)?,
                })?;
            let to = move_placement_index(parent.children.len(), placement)?;
            if from == to {
                return Ok(RecipeLowering::unchanged(
                    RecipeId::HierarchyReparentV1,
                    SchemaVariantId::HierarchyLocalReferences,
                ));
            }
            let action = children_edit(
                parent,
                SequenceMutation::Move {
                    from: u32::try_from(from).map_err(|_| RecipeError::DigestLengthOverflow)?,
                    to: u32::try_from(to).map_err(|_| RecipeError::DigestLengthOverflow)?,
                },
                &mut output,
            )?;
            let mut actions = output.vec::<GenericMutation>(1, "hierarchy recipe actions")?;
            actions.push(action);
            return RecipeLowering::changed(
                RecipeId::HierarchyReparentV1,
                SchemaVariantId::HierarchyLocalReferences,
                parent
                    .object
                    .fragment(planner, Vec::new(), actions, &mut output)?,
            );
        }

        let father_path = output.field_path(&["m_Father"])?;
        validate_reference_shape(&child_node.object, &father_path, &mut output)?;
        let father_guard = child_node
            .object
            .field_guard(&father_path, output.budget())?;
        let action_count = 1_usize
            .checked_add(usize::from(old_parent.is_some()))
            .and_then(|count| count.checked_add(usize::from(new_parent_node.is_some())))
            .ok_or(RecipeError::DigestLengthOverflow)?;
        let mut actions =
            output.vec::<GenericMutation>(action_count, "hierarchy recipe actions")?;
        let mut sources =
            output.vec::<crate::workspace::SourceExpectation>(action_count, "hierarchy sources")?;
        let expected = match old_parent {
            Some(parent) => ReferenceTarget::object(output.address(parent)?),
            None => ReferenceTarget::null(),
        };
        let replacement = match new_parent {
            Some(parent) => ReferenceTarget::object(output.address(parent)?),
            None => ReferenceTarget::null(),
        };
        actions.push(GenericMutation::ReferenceReplace {
            target: output.address(child)?,
            path: father_path,
            schema_digest: father_guard.schema_digest(),
            expected,
            replacement,
        });
        sources.push(output.source(child_node.object.source_expectation())?);

        if let Some(old_parent) = old_parent {
            let old_parent = match hierarchy.find(old_parent) {
                Some(node) => node,
                None => {
                    return Err(RecipeError::MissingParent {
                        parent: output.address(old_parent)?,
                    });
                }
            };
            let index = old_parent
                .children
                .iter()
                .position(|candidate| candidate == child)
                .ok_or(RecipeError::ParentChildMismatch {
                    child: output.address(child)?,
                })?;
            actions.push(children_edit(
                old_parent,
                SequenceMutation::Remove {
                    index: u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                },
                &mut output,
            )?);
            sources.push(output.source(old_parent.object.source_expectation())?);
        }
        if let Some(new_parent) = new_parent_node {
            let index = insertion_placement_index(new_parent.children.len(), placement)?;
            let child_reference =
                MutationValue::reference(ReferenceTarget::object(output.address(child)?));
            actions.push(children_edit(
                new_parent,
                SequenceMutation::Insert {
                    index: u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                    value: child_reference,
                },
                &mut output,
            )?);
            sources.push(output.source(new_parent.object.source_expectation())?);
        }

        let fragment =
            output.fragment(child_node.object.revision(), sources, Vec::new(), actions)?;
        RecipeLowering::changed(
            RecipeId::HierarchyReparentV1,
            SchemaVariantId::HierarchyLocalReferences,
            fragment,
        )
    }
}

fn children_edit(
    parent: &HierarchyNode,
    edit: SequenceMutation,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<GenericMutation, RecipeError> {
    let path = output.field_path(&["m_Children"])?;
    validate_children_shape(&parent.object, &path, &parent.children, output)?;
    let guard = parent.object.field_guard(&path, output.budget())?;
    Ok(GenericMutation::SequenceEdit {
        target: output.address(parent.object.address())?,
        path,
        guard,
        edit,
    })
}

fn validate_hierarchy_nodes(
    nodes: &[HierarchyNode],
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    if let Some(pair) = nodes
        .windows(2)
        .find(|pair| pair[0].object.address() == pair[1].object.address())
    {
        return Err(RecipeError::DuplicateHierarchyNode {
            node: output.address(pair[0].object.address())?,
        });
    }
    let Some(first) = nodes.first() else {
        return Err(RecipeError::TargetMissing);
    };
    let edge_count = nodes.iter().try_fold(0_usize, |count, node| {
        count
            .checked_add(node.children.len())
            .ok_or(RecipeError::DigestLengthOverflow)
    })?;
    consume_hierarchy_visits(output.budget(), edge_count, "hierarchy_edge_visits")?;
    let mut edges =
        output.vec::<(&ObjectAddress, &ObjectAddress)>(edge_count, "hierarchy edges")?;
    let mut states = output.vec::<u8>(nodes.len(), "hierarchy cycle states")?;
    states.resize(nodes.len(), 0_u8);
    let mut stack = output.vec::<usize>(nodes.len(), "hierarchy cycle stack")?;

    let source = first.object.address().source_locator();
    let kind = first.object.address().kind();
    let workspace = first.object.workspace_id();
    let revision = first.object.revision();
    for node in nodes {
        if node.object.address().source_locator() != source
            || node.object.address().kind() != kind
            || node.object.workspace_id() != workspace
            || node.object.revision() != revision
        {
            return Err(RecipeError::CrossSourceHierarchy);
        }
        validate_transform_or_rect_transform_class(&node.object, output)?;
        let father = output.field_path(&["m_Father"])?;
        let children = output.field_path(&["m_Children"])?;
        if !local_reference_matches(&node.object, &father, node.parent.as_ref(), output)? {
            return Err(RecipeError::ParentChildMismatch {
                child: output.address(node.object.address())?,
            });
        }
        validate_children_shape(&node.object, &children, &node.children, output)?;
        if let Some(parent) = &node.parent
            && find_node(nodes, parent).is_none()
        {
            return Err(RecipeError::MissingParent {
                parent: output.address(parent)?,
            });
        }
        for child in &node.children {
            if find_node(nodes, child).is_none() {
                return Err(RecipeError::MissingChild {
                    child: output.address(child)?,
                });
            }
            edges.push((child, node.object.address()));
        }
    }

    edges.sort_unstable_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
    for pair in edges.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(if pair[0].1 == pair[1].1 {
                RecipeError::DuplicateChildMembership {
                    child: output.address(pair[0].0)?,
                }
            } else {
                RecipeError::MultipleParents {
                    child: output.address(pair[0].0)?,
                }
            });
        }
    }

    for node in nodes {
        let membership = edges
            .binary_search_by(|edge| edge.0.cmp(node.object.address()))
            .ok()
            .map(|index| edges[index].1);
        if membership != node.parent.as_ref() {
            return Err(RecipeError::ParentChildMismatch {
                child: output.address(node.object.address())?,
            });
        }
    }

    for start in 0..nodes.len() {
        if states[start] == 2 {
            continue;
        }
        stack.clear();
        let mut cursor = Some(start);
        while let Some(index) = cursor {
            match states[index] {
                0 => {
                    states[index] = 1;
                    stack.push(index);
                }
                1 => {
                    return Err(RecipeError::HierarchyCycle {
                        at: output.address(nodes[index].object.address())?,
                    });
                }
                2 => break,
                _ => {
                    return Err(RecipeError::InvalidPayload {
                        reason: "hierarchy traversal state is invalid",
                    });
                }
            }
            cursor = nodes[index]
                .parent
                .as_ref()
                .and_then(|parent| node_index(nodes, parent));
        }
        while let Some(index) = stack.pop() {
            states[index] = 2;
        }
    }
    Ok(())
}

fn validate_children_shape(
    object: &RecipeObject,
    path: &FieldPath,
    expected: &[ObjectAddress],
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let value = object.require_field(path, output)?;
    let Some(children) = value.as_array() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "an ordered Transform reference array",
            actual: value_kind(value),
        });
    };
    if children.len() != expected.len() {
        return Err(RecipeError::ParentChildMismatch {
            child: output.address(object.address())?,
        });
    }
    for (index, expected) in expected.iter().enumerate() {
        let path = output.append_index(
            path,
            u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
        )?;
        if !local_reference_matches(object, &path, Some(expected), output)? {
            return Err(RecipeError::ParentChildMismatch {
                child: output.address(expected)?,
            });
        }
    }
    Ok(())
}

fn find_node<'state>(
    nodes: &'state [HierarchyNode],
    address: &ObjectAddress,
) -> Option<&'state HierarchyNode> {
    node_index(nodes, address).map(|index| &nodes[index])
}

fn ensure_not_descendant(
    hierarchy: &HierarchyState,
    child: &ObjectAddress,
    parent: Option<&ObjectAddress>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let mut cursor = parent;
    while let Some(address) = cursor {
        output.budget().consume_entries(1)?;
        if address == child {
            return Err(RecipeError::HierarchyCycle {
                at: output.address(address)?,
            });
        }
        cursor = hierarchy.find(address).and_then(HierarchyNode::parent);
    }
    Ok(())
}

fn node_index(nodes: &[HierarchyNode], address: &ObjectAddress) -> Option<usize> {
    nodes
        .binary_search_by(|node| node.object.address().cmp(address))
        .ok()
}

fn consume_hierarchy_visits(
    budget: &mut AssetLoadBudget,
    count: usize,
    resource: &'static str,
) -> Result<(), RecipeError> {
    let count = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.consume_entries(count)?;
    Ok(())
}

fn move_placement_index(len: usize, placement: ChildPlacement) -> Result<usize, RecipeError> {
    match placement {
        ChildPlacement::Append => len.checked_sub(1).ok_or(RecipeError::InvalidPayload {
            reason: "cannot move a child inside an empty parent collection",
        }),
        ChildPlacement::At(index) if index < len => Ok(index),
        ChildPlacement::At(index) => Err(RecipeError::ChildPlacementOutOfBounds {
            index,
            maximum: len.saturating_sub(1),
        }),
    }
}

fn insertion_placement_index(len: usize, placement: ChildPlacement) -> Result<usize, RecipeError> {
    match placement {
        ChildPlacement::Append => Ok(len),
        ChildPlacement::At(index) if index <= len => Ok(index),
        ChildPlacement::At(index) => Err(RecipeError::ChildPlacementOutOfBounds {
            index,
            maximum: len,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use unity_asset_core::{AssetLoadLimits, SourceLocator};

    use crate::workspace::AssetWorkspace;

    use super::*;

    const HIERARCHY_YAML: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!4 &1
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 2}
--- !u!4 &2
Transform:
  m_Father: {fileID: 1}
  m_Children: []
--- !u!4 &3
Transform:
  m_Father: {fileID: 0}
  m_Children:
  - {fileID: 99}
"#;

    struct HierarchyFixture {
        _directory: tempfile::TempDir,
        workspace: AssetWorkspace,
        alias: &'static str,
    }

    impl HierarchyFixture {
        fn open() -> Self {
            let directory = tempfile::tempdir().expect("fixture directory should be created");
            let alias = "hierarchy-budget.prefab";
            let path = directory.path().join(alias);
            fs::write(&path, HIERARCHY_YAML).expect("fixture should be written");
            let mut workspace = AssetWorkspace::new().expect("workspace should be created");
            workspace
                .load_path(&path, &mut AssetLoadBudget::default())
                .expect("fixture should load");
            Self {
                _directory: directory,
                workspace,
                alias,
            }
        }

        fn address(&self, anchor: &str) -> ObjectAddress {
            ObjectAddress::yaml(
                SourceLocator::path(self.alias).expect("fixture alias should be valid"),
                anchor,
            )
            .expect("fixture address should be valid")
        }
    }

    fn entry_budget(max_entries: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_entries,
            ..AssetLoadLimits::default()
        })
        .expect("test budget should be valid")
    }

    fn assert_entry_budget_error(result: Result<impl Sized, RecipeError>, limit: u64) {
        assert!(matches!(
            result,
            Err(RecipeError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: actual_limit,
                requested: 2,
            })) if actual_limit == limit
        ));
    }

    #[test]
    fn state_admits_node_and_edge_visits_before_structural_validation() {
        let fixture = HierarchyFixture::open();
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);

        let first = planner
            .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
            .expect("first duplicate should be inspected");
        let second = planner
            .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
            .expect("second duplicate should be inspected");
        let mut node_short = entry_budget(1);
        assert_entry_budget_error(
            HierarchyState::new(
                vec![
                    HierarchyNode::new(first, None, Vec::new()),
                    HierarchyNode::new(second, None, Vec::new()),
                ],
                &mut node_short,
            ),
            1,
        );
        assert_eq!(node_short.usage().entries, 0);

        let dangling = planner
            .inspect(&fixture.address("3"), &mut AssetLoadBudget::default())
            .expect("dangling node should be inspected");
        let mut edge_short = entry_budget(1);
        assert_entry_budget_error(
            HierarchyState::new(
                vec![HierarchyNode::new(
                    dangling,
                    None,
                    vec![fixture.address("99")],
                )],
                &mut edge_short,
            ),
            1,
        );
        assert_eq!(edge_short.usage().entries, 1);
    }

    #[test]
    fn edge_visits_are_charged_before_edge_scratch_preflight() {
        let fixture = HierarchyFixture::open();
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let dangling = planner
            .inspect(&fixture.address("3"), &mut AssetLoadBudget::default())
            .expect("dangling node should be inspected");
        let edge_scratch_bytes =
            u64::try_from(std::mem::size_of::<(&ObjectAddress, &ObjectAddress)>())
                .expect("edge scratch size should fit the byte ledger");
        let byte_limit = edge_scratch_bytes
            .checked_sub(1)
            .expect("an edge scratch entry should occupy bytes");
        let mut scratch_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: byte_limit,
            ..AssetLoadLimits::default()
        })
        .expect("test budget should be valid");

        assert!(matches!(
            HierarchyState::new(
                vec![HierarchyNode::new(
                    dangling,
                    None,
                    vec![fixture.address("99")],
                )],
                &mut scratch_short,
            ),
            Err(RecipeError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == byte_limit && requested == edge_scratch_bytes
        ));
        assert_eq!(scratch_short.usage().entries, 2);
        assert_eq!(scratch_short.usage().bytes, 0);
    }

    #[test]
    fn reparent_admits_all_context_visits_before_context_validation() {
        let fixture = HierarchyFixture::open();
        let snapshot = fixture.workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let root = planner
            .inspect(&fixture.address("1"), &mut AssetLoadBudget::default())
            .expect("root should be inspected");
        let child = planner
            .inspect(&fixture.address("2"), &mut AssetLoadBudget::default())
            .expect("child should be inspected");
        let hierarchy = HierarchyState::new(
            vec![
                HierarchyNode::new(root, None, vec![fixture.address("2")]),
                HierarchyNode::new(child, Some(fixture.address("1")), Vec::new()),
            ],
            &mut AssetLoadBudget::default(),
        )
        .expect("valid hierarchy should be built");

        let other_workspace = AssetWorkspace::new().expect("other workspace should be created");
        let other_snapshot = other_workspace.snapshot();
        let stale_planner = SchemaRecipePlanner::new(&other_snapshot);
        let mut one_short = entry_budget(1);
        assert_entry_budget_error(
            HierarchyRecipe::reparent(
                &stale_planner,
                &hierarchy,
                &fixture.address("2"),
                None,
                ChildPlacement::Append,
                &mut one_short,
            ),
            1,
        );
        assert_eq!(one_short.usage().entries, 0);

        let mut exact_admission = entry_budget(2);
        assert!(matches!(
            HierarchyRecipe::reparent(
                &stale_planner,
                &hierarchy,
                &fixture.address("2"),
                None,
                ChildPlacement::Append,
                &mut exact_admission,
            ),
            Err(RecipeError::InspectionContractMismatch)
        ));
        assert_eq!(exact_admission.usage().entries, 2);
    }
}
