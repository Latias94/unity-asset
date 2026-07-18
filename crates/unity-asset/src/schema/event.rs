use unity_asset_core::{AssetLoadBudget, FieldPath, UnityValue};

use crate::workspace::{
    Float64Bits, GenericMutation, MutationField, MutationValue, ReferenceTarget, SequenceMutation,
};

use super::recipe::{
    RecipeError, RecipeId, RecipeLowering, RecipeObject, RecipeOutputBuilder, SchemaRecipePlanner,
    SchemaVariantId, validate_recipe_provenance, validate_reference_shape, value_kind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentCallState {
    Off,
    EditorAndRuntime,
    RuntimeOnly,
}

impl PersistentCallState {
    const fn wire_value(self) -> i64 {
        match self {
            Self::Off => 0,
            Self::EditorAndRuntime => 1,
            Self::RuntimeOnly => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistentArgument {
    EventDefined,
    Void,
    Object {
        target: ReferenceTarget,
        assembly_type_name: String,
    },
    Int(i32),
    Float(Float64Bits),
    String(String),
    Bool(bool),
}

impl PersistentArgument {
    fn wire_mode(&self) -> i64 {
        match self {
            Self::EventDefined => 0,
            Self::Void => 1,
            Self::Object { .. } => 2,
            Self::Int(_) => 3,
            Self::Float(_) => 4,
            Self::String(_) => 5,
            Self::Bool(_) => 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentCall {
    target: ReferenceTarget,
    target_assembly_type_name: String,
    method_name: String,
    argument: PersistentArgument,
    state: PersistentCallState,
}

impl PersistentCall {
    pub fn new(
        target: ReferenceTarget,
        target_assembly_type_name: impl Into<String>,
        method_name: impl Into<String>,
        argument: PersistentArgument,
        state: PersistentCallState,
    ) -> Result<Self, RecipeError> {
        let target_assembly_type_name = target_assembly_type_name.into();
        let method_name = method_name.into();
        let invalid_object_type = matches!(
            &argument,
            PersistentArgument::Object {
                assembly_type_name,
                ..
            } if assembly_type_name.contains('\0')
        );
        let invalid_float = matches!(
            &argument,
            PersistentArgument::Float(value) if !value.to_f64().is_finite()
        );
        let invalid_string_size = MutationValue::validate_string_value(&target_assembly_type_name)
            .and_then(|()| MutationValue::validate_string_value(&method_name))
            .is_err()
            || match &argument {
                PersistentArgument::Object {
                    assembly_type_name, ..
                }
                | PersistentArgument::String(assembly_type_name) => {
                    MutationValue::validate_string_value(assembly_type_name).is_err()
                }
                _ => false,
            };
        if method_name.is_empty()
            || method_name.contains('\0')
            || target_assembly_type_name.contains('\0')
            || invalid_object_type
            || invalid_float
            || invalid_string_size
        {
            return Err(RecipeError::ListenerArgumentMismatch);
        }
        Ok(Self {
            target,
            target_assembly_type_name,
            method_name,
            argument,
            state,
        })
    }

    #[must_use]
    pub const fn target(&self) -> &ReferenceTarget {
        &self.target
    }

    #[must_use]
    pub fn target_assembly_type_name(&self) -> &str {
        &self.target_assembly_type_name
    }

    #[must_use]
    pub fn method_name(&self) -> &str {
        &self.method_name
    }

    #[must_use]
    pub const fn argument(&self) -> &PersistentArgument {
        &self.argument
    }

    #[must_use]
    pub const fn state(&self) -> PersistentCallState {
        self.state
    }
}

/// PersistentCall field shape required when appending to an empty event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistentCallShape {
    WithoutTargetAssemblyTypeName,
    WithTargetAssemblyTypeName,
}

impl PersistentCallShape {
    const fn has_target_assembly_type_name(self) -> bool {
        matches!(self, Self::WithTargetAssemblyTypeName)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnityEventEdit {
    Add {
        call: PersistentCall,
        shape: PersistentCallShape,
    },
    Replace {
        index: usize,
        call: PersistentCall,
    },
    Clear,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UnityEventRecipe;

impl UnityEventRecipe {
    pub fn lower(
        planner: &SchemaRecipePlanner<'_>,
        object: &RecipeObject,
        event_path: FieldPath,
        edit: UnityEventEdit,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        planner.validate_object(object)?;
        validate_recipe_provenance(object)?;
        let mut output = RecipeOutputBuilder::new(budget);
        let observed = inspect_event_shape(object, &event_path, &mut output)?;
        let edit = match edit {
            UnityEventEdit::Add { call, shape } => {
                if observed.shape.is_some_and(|observed| observed != shape) {
                    return Err(RecipeError::UnsupportedSchema {
                        variant: "declared PersistentCall shape does not match observed calls",
                    });
                }
                let guard = object.field_guard(&observed.path, output.budget())?;
                let edit = SequenceMutation::Insert {
                    index: u32::try_from(observed.len)
                        .map_err(|_| RecipeError::DigestLengthOverflow)?,
                    value: call_value(&call, shape, &mut output)?,
                };
                (edit, guard)
            }
            UnityEventEdit::Replace { index, call } => {
                if index >= observed.len {
                    return Err(RecipeError::CallIndexOutOfBounds {
                        index,
                        len: observed.len,
                    });
                }
                let shape = observed.shape.ok_or(RecipeError::UnsupportedSchema {
                    variant: "non-empty UnityEvent without a PersistentCall shape",
                })?;
                let guard = object.field_guard(&observed.path, output.budget())?;
                let edit = SequenceMutation::Replace {
                    index: u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
                    value: call_value(&call, shape, &mut output)?,
                };
                (edit, guard)
            }
            UnityEventEdit::Clear if observed.len == 0 => {
                return Ok(RecipeLowering::unchanged(
                    RecipeId::UnityEventPersistentCallsV1,
                    SchemaVariantId::UnityEventPersistentCalls,
                ));
            }
            UnityEventEdit::Clear => (
                SequenceMutation::Clear,
                object.field_guard(&observed.path, output.budget())?,
            ),
        };
        let (edit, guard) = edit;
        let action = GenericMutation::SequenceEdit {
            target: output.address(object.address())?,
            path: observed.path,
            guard,
            edit,
        };
        let mut actions = output.vec::<GenericMutation>(1, "UnityEvent recipe actions")?;
        actions.push(action);
        RecipeLowering::changed(
            RecipeId::UnityEventPersistentCallsV1,
            SchemaVariantId::UnityEventPersistentCalls,
            object.fragment(planner, Vec::new(), actions, &mut output)?,
        )
    }
}

struct ObservedEvent {
    path: FieldPath,
    len: usize,
    shape: Option<PersistentCallShape>,
}

fn inspect_event_shape(
    object: &RecipeObject,
    event_path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<ObservedEvent, RecipeError> {
    let path = calls_path(object, event_path, output)?;
    let value = object.require_field(&path, output)?;
    let Some(raw_calls) = value.as_array() else {
        return Err(RecipeError::WrongFieldShape {
            path,
            expected: "a UnityEvent persistent-call array",
            actual: value_kind(value),
        });
    };
    let mut shape = None;
    for (index, raw) in raw_calls.iter().enumerate() {
        let call_path = output.append_index(
            &path,
            u32::try_from(index).map_err(|_| RecipeError::DigestLengthOverflow)?,
        )?;
        let fields = match raw.as_object() {
            Some(fields) => fields,
            None => {
                return Err(RecipeError::WrongFieldShape {
                    path: output.path(&call_path)?,
                    expected: "a PersistentCall object",
                    actual: value_kind(raw),
                });
            }
        };
        for field in [
            "m_Target",
            "m_MethodName",
            "m_Mode",
            "m_Arguments",
            "m_CallState",
        ] {
            if !fields.contains_key(field) {
                return Err(RecipeError::MissingField {
                    path: output.append_field(&call_path, field)?,
                });
            }
        }
        let target_path = output.append_field(&call_path, "m_Target")?;
        validate_reference_shape(object, &target_path, output)?;
        let valid_call = fields
            .get("m_MethodName")
            .and_then(UnityValue::as_str)
            .is_some()
            && fields
                .get("m_Mode")
                .and_then(UnityValue::as_i64)
                .is_some_and(|mode| (0..=6).contains(&mode))
            && fields
                .get("m_CallState")
                .and_then(UnityValue::as_i64)
                .is_some_and(|state| (0..=2).contains(&state));
        if !valid_call {
            return Err(RecipeError::WrongFieldShape {
                path: call_path,
                expected: "a valid PersistentCall method, mode, and call state",
                actual: value_kind(raw),
            });
        }
        validate_argument_cache(object, &call_path, fields, output)?;
        let current = if let Some(value) = fields.get("m_TargetAssemblyTypeName") {
            if value.as_str().is_none() {
                return Err(RecipeError::WrongFieldShape {
                    path: output.append_field(&call_path, "m_TargetAssemblyTypeName")?,
                    expected: "a string target assembly type name",
                    actual: value_kind(value),
                });
            }
            PersistentCallShape::WithTargetAssemblyTypeName
        } else {
            PersistentCallShape::WithoutTargetAssemblyTypeName
        };
        if shape.is_some_and(|previous| previous != current) {
            return Err(RecipeError::UnsupportedSchema {
                variant: "mixed PersistentCall field shapes",
            });
        }
        shape = Some(current);
    }
    Ok(ObservedEvent {
        path,
        len: raw_calls.len(),
        shape,
    })
}

fn validate_argument_cache(
    object: &RecipeObject,
    call_path: &FieldPath,
    call: &indexmap::IndexMap<String, UnityValue>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<(), RecipeError> {
    let path = output.append_field(call_path, "m_Arguments")?;
    let Some(value) = call.get("m_Arguments") else {
        return Err(RecipeError::MissingField {
            path: output.path(&path)?,
        });
    };
    let Some(fields) = value.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(&path)?,
            expected: "an ArgumentCache object",
            actual: value_kind(value),
        });
    };
    for field in [
        "m_ObjectArgument",
        "m_ObjectArgumentAssemblyTypeName",
        "m_IntArgument",
        "m_FloatArgument",
        "m_StringArgument",
        "m_BoolArgument",
    ] {
        if !fields.contains_key(field) {
            return Err(RecipeError::MissingField {
                path: output.append_field(&path, field)?,
            });
        }
    }
    let object_argument_path = output.append_field(&path, "m_ObjectArgument")?;
    validate_reference_shape(object, &object_argument_path, output)?;
    let valid = fields
        .get("m_ObjectArgumentAssemblyTypeName")
        .and_then(UnityValue::as_str)
        .is_some()
        && fields
            .get("m_IntArgument")
            .and_then(UnityValue::as_i64)
            .is_some()
        && fields
            .get("m_FloatArgument")
            .and_then(UnityValue::as_f64)
            .is_some_and(|value| value.is_finite())
        && fields
            .get("m_StringArgument")
            .and_then(UnityValue::as_str)
            .is_some()
        && fields.get("m_BoolArgument").is_some_and(|value| {
            value.as_bool().is_some()
                || value
                    .as_i64()
                    .is_some_and(|boolean| boolean == 0 || boolean == 1)
        });
    if !valid {
        return Err(RecipeError::WrongFieldShape {
            path,
            expected: "a complete, finite ArgumentCache",
            actual: value_kind(value),
        });
    }
    Ok(())
}

fn calls_path(
    object: &RecipeObject,
    event_path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<FieldPath, RecipeError> {
    let modern_base = output.append_field(event_path, "m_PersistentCalls")?;
    let modern = output.append_field(&modern_base, "m_Calls")?;
    let legacy_base = output.append_field(event_path, "m_PersistentListeners")?;
    let legacy = output.append_field(&legacy_base, "m_Listeners")?;
    match (object.field(&modern), object.field(&legacy)) {
        (Some(_), None) => Ok(modern),
        (None, Some(_)) => Ok(legacy),
        (Some(_), Some(_)) => Err(RecipeError::AmbiguousFieldVariant {
            first: "m_PersistentCalls.m_Calls",
            second: "m_PersistentListeners.m_Listeners",
        }),
        (None, None) => Err(RecipeError::MissingField { path: modern }),
    }
}

fn call_value(
    call: &PersistentCall,
    shape: PersistentCallShape,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let field_count = 5 + usize::from(shape.has_target_assembly_type_name());
    let mut fields = output.vec::<MutationField>(field_count, "PersistentCall fields")?;
    let target = MutationValue::reference(output.reference(&call.target)?);
    fields.push(output.field("m_Target", target)?);
    let method = output.mutation_string(&call.method_name)?;
    fields.push(output.field("m_MethodName", method)?);
    fields.push(output.field("m_Mode", MutationValue::signed(call.argument.wire_mode()))?);
    let arguments = arguments_value(&call.argument, output)?;
    fields.push(output.field("m_Arguments", arguments)?);
    fields.push(output.field(
        "m_CallState",
        MutationValue::signed(call.state.wire_value()),
    )?);
    if shape.has_target_assembly_type_name() {
        let assembly = output.mutation_string(&call.target_assembly_type_name)?;
        fields.push(output.field("m_TargetAssemblyTypeName", assembly)?);
    }
    Ok(MutationValue::object(fields)?)
}

fn arguments_value(
    argument: &PersistentArgument,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<MutationValue, RecipeError> {
    let (object, object_type, int, float, string, boolean) = match argument {
        PersistentArgument::Object {
            target,
            assembly_type_name,
        } => (
            output.reference(target)?,
            assembly_type_name.as_str(),
            0,
            Float64Bits::from_f64(0.0),
            "",
            false,
        ),
        PersistentArgument::Int(value) => (
            ReferenceTarget::null(),
            "",
            i64::from(*value),
            Float64Bits::from_f64(0.0),
            "",
            false,
        ),
        PersistentArgument::Float(value) => (ReferenceTarget::null(), "", 0, *value, "", false),
        PersistentArgument::String(value) => (
            ReferenceTarget::null(),
            "",
            0,
            Float64Bits::from_f64(0.0),
            value.as_str(),
            false,
        ),
        PersistentArgument::Bool(value) => (
            ReferenceTarget::null(),
            "",
            0,
            Float64Bits::from_f64(0.0),
            "",
            *value,
        ),
        PersistentArgument::EventDefined | PersistentArgument::Void => (
            ReferenceTarget::null(),
            "",
            0,
            Float64Bits::from_f64(0.0),
            "",
            false,
        ),
    };
    let object = MutationValue::reference(object);
    let object_type = output.mutation_string(object_type)?;
    let string = output.mutation_string(string)?;
    let mut fields = output.vec::<MutationField>(6, "ArgumentCache fields")?;
    fields.push(output.field("m_ObjectArgument", object)?);
    fields.push(output.field("m_ObjectArgumentAssemblyTypeName", object_type)?);
    fields.push(output.field("m_IntArgument", MutationValue::signed(int))?);
    fields.push(output.field("m_FloatArgument", MutationValue::float64(float.to_f64()))?);
    fields.push(output.field("m_StringArgument", string)?);
    fields.push(output.field("m_BoolArgument", MutationValue::bool(boolean))?);
    Ok(MutationValue::object(fields)?)
}
