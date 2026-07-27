use std::mem::size_of;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, FieldPath, FieldPathSegment, ObjectAddress, SourceLocator,
    WorkspaceId, WorkspaceRevision,
};

use crate::workspace::{GenericMutation, MutationPlanFragment, PlanPayload, SourceExpectation};

use super::contract::RecipeError;

pub(crate) struct RecipeOutputBuilder<'budget> {
    budget: &'budget mut AssetLoadBudget,
}

impl<'budget> RecipeOutputBuilder<'budget> {
    pub(crate) const fn new(budget: &'budget mut AssetLoadBudget) -> Self {
        Self { budget }
    }

    pub(crate) fn budget(&mut self) -> &mut AssetLoadBudget {
        self.budget
    }

    pub(crate) fn vec<T>(
        &mut self,
        capacity: usize,
        resource: &'static str,
    ) -> Result<Vec<T>, RecipeError> {
        self.check_vec::<T>(capacity)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(capacity)
            .map_err(|error| RecipeError::AllocationFailed {
                resource,
                requested: capacity,
                message: error.to_string(),
            })?;
        self.consume_vec::<T>(capacity)?;
        Ok(values)
    }

    pub(crate) fn string(
        &mut self,
        value: &str,
        resource: &'static str,
    ) -> Result<String, RecipeError> {
        let bytes = u64::try_from(value.len()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "recipe_output_bytes",
        })?;
        self.budget.check_bytes(bytes)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|error| RecipeError::AllocationFailed {
                resource,
                requested: value.len(),
                message: error.to_string(),
            })?;
        self.budget.consume_bytes(bytes)?;
        owned.push_str(value);
        Ok(owned)
    }

    pub(crate) fn mutation_string(
        &mut self,
        value: &str,
    ) -> Result<crate::workspace::MutationValue, RecipeError> {
        crate::workspace::MutationValue::validate_string_value(value)?;
        Ok(crate::workspace::MutationValue::string(
            self.string(value, "recipe mutation string")?,
        )?)
    }

    pub(crate) fn field(
        &mut self,
        name: &str,
        value: crate::workspace::MutationValue,
    ) -> Result<crate::workspace::MutationField, RecipeError> {
        Ok(crate::workspace::MutationField::new(
            self.string(name, "recipe mutation field name")?,
            value,
        )?)
    }

    pub(crate) fn address(
        &mut self,
        address: &ObjectAddress,
    ) -> Result<ObjectAddress, RecipeError> {
        let bytes = address
            .retained_clone_bytes()
            .ok_or(RecipeError::DigestLengthOverflow)?;
        self.charge_clone_bytes(bytes)?;
        Ok(address.clone())
    }

    pub(crate) fn locator(
        &mut self,
        locator: &SourceLocator,
    ) -> Result<SourceLocator, RecipeError> {
        let bytes = locator
            .retained_clone_bytes()
            .ok_or(RecipeError::DigestLengthOverflow)?;
        self.charge_clone_bytes(bytes)?;
        Ok(locator.clone())
    }

    pub(crate) fn path(&mut self, path: &FieldPath) -> Result<FieldPath, RecipeError> {
        self.path_with_segment(path, None)
    }

    pub(crate) fn field_path(&mut self, fields: &[&str]) -> Result<FieldPath, RecipeError> {
        let string_bytes = fields.iter().try_fold(0_usize, |total, field| {
            total
                .checked_add(field.len())
                .ok_or(RecipeError::DigestLengthOverflow)
        })?;
        let total_bytes = path_allocation_bytes(fields.len(), string_bytes)?;
        self.check_allocation(fields.len(), total_bytes)?;
        let mut segments = reserve_recipe_vec(fields.len(), "recipe field path")?;
        for field in fields {
            segments.push(FieldPathSegment::field(reserve_recipe_string(
                field,
                "recipe field path segment",
            )?)?);
        }
        let path = FieldPath::from_segments(segments)?;
        self.consume_allocation(fields.len(), total_bytes)?;
        Ok(path)
    }

    pub(crate) fn append_field(
        &mut self,
        path: &FieldPath,
        field: &str,
    ) -> Result<FieldPath, RecipeError> {
        self.path_with_segment(path, Some((field, None)))
    }

    pub(crate) fn append_index(
        &mut self,
        path: &FieldPath,
        index: u32,
    ) -> Result<FieldPath, RecipeError> {
        self.path_with_segment(path, Some(("", Some(index))))
    }

    pub(crate) fn source(
        &mut self,
        source: &SourceExpectation,
    ) -> Result<SourceExpectation, RecipeError> {
        let bytes = source
            .locator()
            .retained_clone_bytes()
            .ok_or(RecipeError::DigestLengthOverflow)?;
        self.charge_clone_bytes(bytes)?;
        Ok(source.clone())
    }

    pub(crate) fn reference(
        &mut self,
        reference: &crate::workspace::ReferenceTarget,
    ) -> Result<crate::workspace::ReferenceTarget, RecipeError> {
        match reference {
            crate::workspace::ReferenceTarget::Null => Ok(crate::workspace::ReferenceTarget::Null),
            crate::workspace::ReferenceTarget::Object { address } => Ok(
                crate::workspace::ReferenceTarget::object(self.address(address)?),
            ),
        }
    }

    pub(crate) fn fragment(
        &mut self,
        workspace_id: WorkspaceId,
        revision: WorkspaceRevision,
        sources: Vec<SourceExpectation>,
        payloads: Vec<PlanPayload>,
        actions: Vec<GenericMutation>,
    ) -> Result<MutationPlanFragment, RecipeError> {
        Ok(MutationPlanFragment::from_recipe(
            workspace_id,
            revision,
            sources,
            payloads,
            actions,
        )?)
    }

    fn charge_clone_bytes(&mut self, bytes: usize) -> Result<(), RecipeError> {
        let bytes = u64::try_from(bytes).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "recipe_output_bytes",
        })?;
        self.budget.check_bytes(bytes)?;
        self.budget.consume_bytes(bytes)?;
        Ok(())
    }

    fn path_with_segment(
        &mut self,
        path: &FieldPath,
        appended: Option<(&str, Option<u32>)>,
    ) -> Result<FieldPath, RecipeError> {
        let capacity = path
            .segments()
            .len()
            .checked_add(usize::from(appended.is_some()))
            .ok_or(RecipeError::DigestLengthOverflow)?;
        let mut total_bytes = path
            .retained_clone_bytes()
            .ok_or(RecipeError::DigestLengthOverflow)?;
        if let Some((field, index)) = appended {
            total_bytes = total_bytes
                .checked_add(size_of::<FieldPathSegment>())
                .ok_or(RecipeError::DigestLengthOverflow)?;
            if index.is_none() {
                total_bytes = total_bytes
                    .checked_add(field.len())
                    .ok_or(RecipeError::DigestLengthOverflow)?;
            }
        }
        self.check_allocation(capacity, total_bytes)?;
        let mut segments = reserve_recipe_vec(capacity, "recipe field path")?;
        for segment in path.segments() {
            segments.push(match segment {
                FieldPathSegment::Field(name) => FieldPathSegment::field(reserve_recipe_string(
                    name,
                    "recipe field path segment",
                )?)?,
                FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
            });
        }
        if let Some((field, index)) = appended {
            segments.push(match index {
                Some(index) => FieldPathSegment::Index(index),
                None => FieldPathSegment::field(reserve_recipe_string(
                    field,
                    "recipe field path segment",
                )?)?,
            });
        }
        let path = FieldPath::from_segments(segments)?;
        self.consume_allocation(capacity, total_bytes)?;
        Ok(path)
    }

    fn check_allocation(&self, entries: usize, bytes: usize) -> Result<(), RecipeError> {
        self.budget
            .check_entries(u64::try_from(entries).map_err(|_| {
                BudgetError::ArithmeticOverflow {
                    resource: "recipe_output_entries",
                }
            })?)?;
        self.budget.check_bytes(u64::try_from(bytes).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "recipe_output_bytes",
            }
        })?)?;
        Ok(())
    }

    fn consume_allocation(&mut self, entries: usize, bytes: usize) -> Result<(), RecipeError> {
        self.budget
            .consume_entries(u64::try_from(entries).map_err(|_| {
                BudgetError::ArithmeticOverflow {
                    resource: "recipe_output_entries",
                }
            })?)?;
        self.budget
            .consume_bytes(u64::try_from(bytes).map_err(|_| {
                BudgetError::ArithmeticOverflow {
                    resource: "recipe_output_bytes",
                }
            })?)?;
        Ok(())
    }

    fn check_vec<T>(&self, capacity: usize) -> Result<(), RecipeError> {
        let entries = u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "recipe_output_entries",
        })?;
        let bytes = capacity
            .checked_mul(size_of::<T>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "recipe_output_bytes",
            })?;
        self.budget.check_entries(entries)?;
        self.budget.check_bytes(bytes)?;
        Ok(())
    }

    fn consume_vec<T>(&mut self, capacity: usize) -> Result<(), RecipeError> {
        let entries = u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "recipe_output_entries",
        })?;
        let bytes = capacity
            .checked_mul(size_of::<T>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "recipe_output_bytes",
            })?;
        self.budget.consume_entries(entries)?;
        self.budget.consume_bytes(bytes)?;
        Ok(())
    }
}

fn path_allocation_bytes(capacity: usize, string_bytes: usize) -> Result<usize, RecipeError> {
    capacity
        .checked_mul(size_of::<FieldPathSegment>())
        .and_then(|bytes| bytes.checked_add(string_bytes))
        .ok_or(RecipeError::DigestLengthOverflow)
}

fn reserve_recipe_vec<T>(capacity: usize, resource: &'static str) -> Result<Vec<T>, RecipeError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| RecipeError::AllocationFailed {
            resource,
            requested: capacity,
            message: error.to_string(),
        })?;
    Ok(values)
}

fn reserve_recipe_string(value: &str, resource: &'static str) -> Result<String, RecipeError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|error| RecipeError::AllocationFailed {
            resource,
            requested: value.len(),
            message: error.to_string(),
        })?;
    owned.push_str(value);
    Ok(owned)
}
