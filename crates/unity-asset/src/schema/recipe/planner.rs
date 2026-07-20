use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, FieldPath, ObjectAddress, ObjectKind, UnityClass,
    UnityValue, WorkspaceRevision, class_names, field_schema_digest, observe_semantic_value,
    semantic_value_digest, yaml_field_schema_digest,
};

use super::contract::{
    CAPABILITIES, RecipeApplicability, RecipeApplicabilityStatus, RecipeError, RecipeId,
    RecipeLowering, RecipeRejectionCode, RecipeValueKind, SchemaOrigin, SchemaProvenance,
    SchemaVariantId,
};
use super::output::RecipeOutputBuilder;
use crate::workspace::{
    FieldGuard, GenericMutation, MutationPlanFragment, PlanPayload, SourceExpectation,
    WorkspaceLookup, WorkspaceObject, WorkspaceSource, WorkspaceView,
};

/// Trusted immutable observation used by every built-in recipe.
#[derive(Debug)]
pub struct RecipeObject {
    address: ObjectAddress,
    source: SourceExpectation,
    object: WorkspaceObject,
}

impl RecipeObject {
    #[must_use]
    pub const fn address(&self) -> &ObjectAddress {
        &self.address
    }

    #[must_use]
    pub const fn source_expectation(&self) -> &SourceExpectation {
        &self.source
    }

    #[must_use]
    pub fn class(&self) -> &UnityClass {
        self.object.class()
    }

    #[must_use]
    pub fn provenance(&self) -> &SchemaProvenance {
        self.object.schema_provenance()
    }

    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.object.handle().revision()
    }

    pub(crate) const fn workspace_id(&self) -> unity_asset_core::WorkspaceId {
        self.object.handle().workspace()
    }

    pub(crate) fn field<'value>(&'value self, path: &FieldPath) -> Option<&'value UnityValue> {
        self.class().value_at_path(path).ok()
    }

    pub(crate) fn require_field<'value>(
        &'value self,
        path: &FieldPath,
        output: &mut RecipeOutputBuilder<'_>,
    ) -> Result<&'value UnityValue, RecipeError> {
        match self.field(path) {
            Some(value) => Ok(value),
            None => Err(RecipeError::MissingField {
                path: output.path(path)?,
            }),
        }
    }

    pub(crate) fn field_guard(
        &self,
        path: &FieldPath,
        budget: &mut AssetLoadBudget,
    ) -> Result<FieldGuard, RecipeError> {
        let object_schema = self
            .provenance()
            .schema_digest()
            .ok_or(RecipeError::MissingSchemaProvenance)?;
        let value = match self.field(path) {
            Some(value) => value,
            None => {
                let mut output = RecipeOutputBuilder::new(budget);
                return Err(RecipeError::MissingField {
                    path: output.path(path)?,
                });
            }
        };
        let schema = match self.address.kind() {
            ObjectKind::Binary => field_schema_digest(object_schema, path)?,
            ObjectKind::Yaml => yaml_field_schema_digest(self.class(), path, value, budget)?,
        };
        Ok(FieldGuard::new(
            schema,
            semantic_value_digest(value, budget)?,
        ))
    }

    pub(crate) fn fragment(
        &self,
        planner: &SchemaRecipePlanner<'_>,
        payloads: Vec<PlanPayload>,
        actions: Vec<GenericMutation>,
        output: &mut RecipeOutputBuilder<'_>,
    ) -> Result<MutationPlanFragment, RecipeError> {
        planner.validate_object(self)?;
        let mut sources = output.vec::<SourceExpectation>(1, "recipe fragment sources")?;
        sources.push(output.source(&self.source)?);
        output.fragment(self.object.handle().revision(), sources, payloads, actions)
    }
}

pub struct SchemaRecipePlanner<'view> {
    view: &'view dyn WorkspaceView,
}

impl<'view> SchemaRecipePlanner<'view> {
    #[must_use]
    pub const fn new(view: &'view dyn WorkspaceView) -> Self {
        Self { view }
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.view.revision()
    }

    pub fn inspect(
        &self,
        address: &ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeObject, RecipeError> {
        let handle = match self.view.resolve_object(address, budget)? {
            WorkspaceLookup::Resolved(handle) => handle,
            WorkspaceLookup::Unloaded => return Err(RecipeError::TargetUnloaded),
            WorkspaceLookup::Missing => return Err(RecipeError::TargetMissing),
            WorkspaceLookup::Ambiguous { candidates } => {
                return Err(RecipeError::TargetAmbiguous {
                    candidates: candidates.len(),
                });
            }
            WorkspaceLookup::Invalid { diagnostic } => {
                return Err(RecipeError::TargetInvalid {
                    code: invalid_target_code(diagnostic, budget)?,
                });
            }
        };
        let source = match self.view.source(handle.object().source(), budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded => return Err(RecipeError::TargetUnloaded),
            WorkspaceLookup::Missing => return Err(RecipeError::TargetMissing),
            WorkspaceLookup::Ambiguous { candidates } => {
                return Err(RecipeError::TargetAmbiguous {
                    candidates: candidates.len(),
                });
            }
            WorkspaceLookup::Invalid { diagnostic } => {
                return Err(RecipeError::TargetInvalid {
                    code: invalid_target_code(diagnostic, budget)?,
                });
            }
        };
        let object = self.view.read_object(&handle, budget)?;
        validate_inspection(address, &handle, &source, &object)?;
        let mut output = RecipeOutputBuilder::new(budget);
        Ok(RecipeObject {
            address: output.address(address)?,
            source: SourceExpectation::new(output.locator(source.locator())?, source.fingerprint()),
            object,
        })
    }

    /// Reports target-specific recipe applicability without lowering or mutating state.
    pub fn capabilities_for(
        &self,
        object: &RecipeObject,
        budget: &mut AssetLoadBudget,
    ) -> Result<[RecipeApplicability; 6], RecipeError> {
        if let Err(error) = self
            .validate_object(object)
            .and_then(|()| validate_recipe_provenance(object))
        {
            let rejection = error
                .code()
                .unwrap_or(RecipeRejectionCode::MissingSchemaProvenance);
            return Ok(std::array::from_fn(|index| {
                RecipeApplicability::rejected(CAPABILITIES[index].id(), rejection)
            }));
        }
        budget.consume_entries(u64::try_from(CAPABILITIES.len()).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "recipe_capabilities",
            }
        })?)?;
        Ok([
            RecipeApplicability::applicable(
                RecipeId::ReferenceRetargetV1,
                SchemaVariantId::GenericReference,
            ),
            transform_applicability(object),
            material_applicability(object, budget)?,
            event_applicability(object, budget)?,
            hierarchy_applicability(object),
            resource_applicability(object),
        ])
    }

    pub fn lower_reference(
        &self,
        object: &RecipeObject,
        path: FieldPath,
        expected: crate::workspace::ReferenceTarget,
        replacement: crate::workspace::ReferenceTarget,
        budget: &mut AssetLoadBudget,
    ) -> Result<RecipeLowering, RecipeError> {
        self.validate_object(object)?;
        validate_recipe_provenance(object)?;
        if expected == replacement {
            return Ok(RecipeLowering::unchanged(
                RecipeId::ReferenceRetargetV1,
                SchemaVariantId::GenericReference,
            ));
        }
        let mut output = RecipeOutputBuilder::new(budget);
        validate_reference_shape(object, &path, &mut output)?;
        let guard = object.field_guard(&path, output.budget())?;
        let action = GenericMutation::ReferenceReplace {
            target: output.address(&object.address)?,
            path,
            schema_digest: guard.schema_digest(),
            expected,
            replacement,
        };
        let mut actions = output.vec::<GenericMutation>(1, "reference recipe actions")?;
        actions.push(action);
        RecipeLowering::changed(
            RecipeId::ReferenceRetargetV1,
            SchemaVariantId::GenericReference,
            object.fragment(self, Vec::new(), actions, &mut output)?,
        )
    }

    pub(crate) fn validate_object(&self, object: &RecipeObject) -> Result<(), RecipeError> {
        object
            .object
            .handle()
            .validate_context(self.view.workspace_id(), self.view.revision())
            .map_err(|_| RecipeError::InspectionContractMismatch)
    }
}

fn validate_inspection(
    address: &ObjectAddress,
    handle: &unity_asset_core::RevisionedObjectHandle,
    source: &WorkspaceSource,
    object: &WorkspaceObject,
) -> Result<(), RecipeError> {
    let identity = handle.object();
    let key_matches = match address.kind() {
        ObjectKind::Binary => identity.binary_path_id() == address.binary_path_id(),
        ObjectKind::Yaml => {
            identity.yaml_anchor() == address.yaml_anchor()
                && identity.yaml_document_ordinal() == address.yaml_document_ordinal()
        }
    };
    if source.locator() != address.source_locator()
        || source.id() != identity.source()
        || object.handle() != handle
        || !key_matches
        || object.schema_provenance().object_kind() != address.kind()
        || object.schema_provenance().class_id() != object.class().class_id
    {
        return Err(RecipeError::InspectionContractMismatch);
    }
    Ok(())
}

fn invalid_target_code(
    diagnostic: Diagnostic,
    budget: &mut AssetLoadBudget,
) -> Result<String, RecipeError> {
    let mut output = RecipeOutputBuilder::new(budget);
    output.string(diagnostic.code(), "recipe target diagnostic")
}

pub(crate) fn value_kind(value: &UnityValue) -> RecipeValueKind {
    match value {
        UnityValue::Null => RecipeValueKind::Null,
        UnityValue::Bool(_) => RecipeValueKind::Bool,
        UnityValue::Integer(_) => RecipeValueKind::Signed,
        UnityValue::Unsigned(_) => RecipeValueKind::Unsigned,
        UnityValue::Float(_) => RecipeValueKind::Float,
        UnityValue::String(_) => RecipeValueKind::String,
        UnityValue::Bytes(_) => RecipeValueKind::Bytes,
        UnityValue::Array(_) => RecipeValueKind::Array,
        UnityValue::Object(_) => RecipeValueKind::Object,
    }
}

pub(crate) fn validate_recipe_provenance(object: &RecipeObject) -> Result<(), RecipeError> {
    let provenance = object.provenance();
    if provenance.schema_digest().is_none() {
        return Err(RecipeError::MissingSchemaProvenance);
    }
    match provenance.object_kind() {
        ObjectKind::Yaml => {
            if provenance.origin() != SchemaOrigin::YamlShape
                || provenance.binary_version().is_some()
            {
                return Err(RecipeError::InspectionContractMismatch);
            }
        }
        ObjectKind::Binary => {
            if !matches!(
                provenance.origin(),
                SchemaOrigin::EmbeddedTypeTree | SchemaOrigin::FrozenRegistry
            ) {
                return Err(RecipeError::MissingSchemaProvenance);
            }
            let version = provenance
                .binary_version()
                .ok_or(RecipeError::InspectionContractMismatch)?;
            if version.unity().is_none() {
                return Err(RecipeError::UnsupportedVersion);
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_reference_shape<'value>(
    object: &'value RecipeObject,
    path: &FieldPath,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<&'value indexmap::IndexMap<String, UnityValue>, RecipeError> {
    let Some(value) = object.field(path) else {
        return Err(RecipeError::MissingField {
            path: output.path(path)?,
        });
    };
    let Some(fields) = value.as_object() else {
        return Err(RecipeError::WrongFieldShape {
            path: output.path(path)?,
            expected: "a Unity pointer object",
            actual: value_kind(value),
        });
    };
    let valid = match object.address.kind() {
        ObjectKind::Yaml => fields.get("fileID").and_then(UnityValue::as_i64).is_some(),
        ObjectKind::Binary => {
            let file = aliased_reference_integer(fields, "m_FileID", "fileID");
            let path_id = aliased_reference_integer(fields, "m_PathID", "pathID");
            file.is_some() && path_id.is_some()
        }
    };
    if !valid {
        return Err(RecipeError::InvalidReference {
            path: output.path(path)?,
        });
    }
    Ok(fields)
}

pub(crate) fn local_reference_matches(
    object: &RecipeObject,
    path: &FieldPath,
    expected: Option<&ObjectAddress>,
    output: &mut RecipeOutputBuilder<'_>,
) -> Result<bool, RecipeError> {
    let fields = validate_reference_shape(object, path, output)?;
    match object.address().kind() {
        ObjectKind::Yaml => {
            if fields
                .get("guid")
                .is_some_and(|guid| !matches!(guid, UnityValue::String(value) if value.is_empty()))
            {
                return Err(RecipeError::UnresolvedReference {
                    path: output.path(path)?,
                });
            }
            let Some(file_id) = fields.get("fileID").and_then(UnityValue::as_i64) else {
                return Err(RecipeError::InvalidReference {
                    path: output.path(path)?,
                });
            };
            Ok(if file_id == 0 {
                expected.is_none()
            } else {
                expected.is_some_and(|address| {
                    address.kind() == ObjectKind::Yaml
                        && address.source_locator() == object.address().source_locator()
                        && address
                            .yaml_anchor()
                            .and_then(|anchor| anchor.parse::<i64>().ok())
                            == Some(file_id)
                })
            })
        }
        ObjectKind::Binary => {
            let Some(file_id) = aliased_reference_integer(fields, "m_FileID", "fileID") else {
                return Err(RecipeError::InvalidReference {
                    path: output.path(path)?,
                });
            };
            let Some(path_id) = aliased_reference_integer(fields, "m_PathID", "pathID") else {
                return Err(RecipeError::InvalidReference {
                    path: output.path(path)?,
                });
            };
            if file_id != 0 {
                return Err(RecipeError::UnresolvedReference {
                    path: output.path(path)?,
                });
            }
            Ok(if path_id == 0 {
                expected.is_none()
            } else {
                expected.is_some_and(|address| {
                    address.kind() == ObjectKind::Binary
                        && address.source_locator() == object.address().source_locator()
                        && address.binary_path_id() == Some(path_id)
                })
            })
        }
    }
}

fn aliased_reference_integer(
    fields: &indexmap::IndexMap<String, UnityValue>,
    primary: &str,
    compatibility: &str,
) -> Option<i64> {
    match (fields.get(primary), fields.get(compatibility)) {
        (Some(primary), Some(compatibility)) => {
            let primary = primary.as_i64()?;
            (compatibility.as_i64()? == primary).then_some(primary)
        }
        (Some(value), None) | (None, Some(value)) => value.as_i64(),
        (None, None) => None,
    }
}

fn transform_applicability(object: &RecipeObject) -> RecipeApplicability {
    let class = object.class();
    if class.class_id == unity_asset_core::class_ids::TRANSFORM
        && class.class_name == class_names::TRANSFORM
    {
        return RecipeApplicability::applicable(RecipeId::TransformV1, SchemaVariantId::Transform);
    }
    if class.class_id == unity_asset_core::class_ids::RECT_TRANSFORM
        && class.class_name == class_names::RECT_TRANSFORM
    {
        let modern = class.has_property("m_AnchoredPosition");
        let legacy = class.has_property("m_Position");
        return match (modern, legacy) {
            (true, false) => RecipeApplicability::applicable(
                RecipeId::TransformV1,
                SchemaVariantId::RectTransformAnchoredPosition,
            ),
            (false, true) => RecipeApplicability::applicable(
                RecipeId::TransformV1,
                SchemaVariantId::RectTransformLegacyPosition,
            ),
            _ => RecipeApplicability::rejected(
                RecipeId::TransformV1,
                RecipeRejectionCode::AmbiguousFieldVariant,
            ),
        };
    }
    RecipeApplicability::rejected(RecipeId::TransformV1, RecipeRejectionCode::WrongClass)
}

fn material_applicability(
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<RecipeApplicability, RecipeError> {
    let class = object.class();
    if class.class_id != unity_asset_core::class_ids::MATERIAL
        || class.class_name != class_names::MATERIAL
    {
        return Ok(RecipeApplicability::rejected(
            RecipeId::MaterialTextureEnvironmentV1,
            RecipeRejectionCode::WrongClass,
        ));
    }
    let variant = class
        .get("m_SavedProperties")
        .and_then(UnityValue::as_object)
        .and_then(|saved| saved.get("m_TexEnvs"))
        .map(|value| material_container_variant(value, budget, 1))
        .transpose()?
        .flatten();
    Ok(variant.map_or_else(
        || {
            RecipeApplicability::rejected(
                RecipeId::MaterialTextureEnvironmentV1,
                RecipeRejectionCode::UnsupportedSchema,
            )
        },
        |variant| RecipeApplicability::applicable(RecipeId::MaterialTextureEnvironmentV1, variant),
    ))
}

fn material_container_variant(
    value: &UnityValue,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<Option<SchemaVariantId>, RecipeError> {
    observe_semantic_value(depth, budget)?;
    match value {
        UnityValue::Array(entries) => Ok(entries.first().and_then(material_entry_variant)),
        UnityValue::Object(fields) if fields.len() == 1 && fields.contains_key("data") => {
            fields.get("data").map_or(Ok(None), |value| {
                material_container_variant(value, budget, depth + 1)
            })
        }
        UnityValue::Object(fields) => Ok(material_entry_variant(value).or_else(|| {
            fields
                .first()
                .filter(|(_, value)| matches!(value, UnityValue::Object(_)))
                .map(|_| SchemaVariantId::MaterialYamlPropertyName)
        })),
        _ => Ok(None),
    }
}

fn material_entry_variant(entry: &UnityValue) -> Option<SchemaVariantId> {
    let key = match entry {
        UnityValue::Array(values) if values.len() == 2 => &values[0],
        UnityValue::Object(fields)
            if fields.len() == 2
                && fields.contains_key("first")
                && fields.contains_key("second") =>
        {
            fields.get("first")?
        }
        UnityValue::Object(fields) if fields.len() == 1 => {
            return fields
                .first()
                .filter(|(_, value)| matches!(value, UnityValue::Object(_)))
                .map(|_| SchemaVariantId::MaterialYamlPropertyName);
        }
        _ => return None,
    };
    match key {
        UnityValue::String(_) => Some(SchemaVariantId::MaterialStringPropertyName),
        UnityValue::Object(fields) if matches!(fields.get("name"), Some(UnityValue::String(_))) => {
            Some(SchemaVariantId::MaterialFastPropertyName)
        }
        _ => None,
    }
}

fn event_applicability(
    object: &RecipeObject,
    budget: &mut AssetLoadBudget,
) -> Result<RecipeApplicability, RecipeError> {
    let mut found = false;
    for value in object.class().properties().values() {
        observe_semantic_value(1, budget)?;
        found = value.as_object().is_some_and(|event| {
            event
                .get("m_PersistentCalls")
                .and_then(UnityValue::as_object)
                .and_then(|calls| calls.get("m_Calls"))
                .is_some_and(|calls| matches!(calls, UnityValue::Array(_)))
                || event
                    .get("m_PersistentListeners")
                    .and_then(UnityValue::as_object)
                    .and_then(|listeners| listeners.get("m_Listeners"))
                    .is_some_and(|listeners| matches!(listeners, UnityValue::Array(_)))
        });
        if found {
            break;
        }
    }
    if found {
        Ok(RecipeApplicability::applicable(
            RecipeId::UnityEventPersistentCallsV1,
            SchemaVariantId::UnityEventPersistentCalls,
        ))
    } else {
        Ok(RecipeApplicability::rejected(
            RecipeId::UnityEventPersistentCallsV1,
            RecipeRejectionCode::UnsupportedSchema,
        ))
    }
}

fn hierarchy_applicability(object: &RecipeObject) -> RecipeApplicability {
    let transform = transform_applicability(object);
    if transform.status() == RecipeApplicabilityStatus::Applicable
        && object.class().has_property("m_Father")
        && matches!(object.class().get("m_Children"), Some(UnityValue::Array(_)))
    {
        RecipeApplicability::applicable(
            RecipeId::HierarchyReparentV1,
            SchemaVariantId::HierarchyLocalReferences,
        )
    } else {
        RecipeApplicability::rejected(
            RecipeId::HierarchyReparentV1,
            if transform.status() == RecipeApplicabilityStatus::Rejected {
                RecipeRejectionCode::WrongClass
            } else {
                RecipeRejectionCode::UnsupportedSchema
            },
        )
    }
}

fn resource_applicability(object: &RecipeObject) -> RecipeApplicability {
    let class = object.class();
    if class.class_id != unity_asset_core::class_ids::AUDIO_CLIP
        || class.class_name != class_names::AUDIO_CLIP
    {
        return RecipeApplicability::rejected(
            RecipeId::AudioClipStreamedResourceV1,
            RecipeRejectionCode::WrongClass,
        );
    }
    if matches!(class.get("m_Resource"), Some(UnityValue::Object(_))) {
        RecipeApplicability::applicable(
            RecipeId::AudioClipStreamedResourceV1,
            SchemaVariantId::AudioClipResource,
        )
    } else if matches!(class.get("m_StreamData"), Some(UnityValue::Object(_))) {
        RecipeApplicability::applicable(
            RecipeId::AudioClipStreamedResourceV1,
            SchemaVariantId::AudioClipStreamDataCompatibility,
        )
    } else {
        RecipeApplicability::rejected(
            RecipeId::AudioClipStreamedResourceV1,
            RecipeRejectionCode::UnsupportedSchema,
        )
    }
}

pub(crate) fn ensure_finite(values: &[f64]) -> Result<(), RecipeError> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(RecipeError::NonFiniteValue)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use unity_asset_binary::object::UnityObject;
    use unity_asset_core::{AssetLoadBudget, FieldPath, SourceLocator, UnityValue};

    use crate::workspace::{
        AssetWorkspace, ReferenceTarget, WorkspaceObject, WorkspaceObjectValue,
    };

    use super::*;

    #[test]
    fn reference_lowering_rejects_each_conflicting_binary_alias_before_building_a_fragment() {
        let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin",
        );
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let planner = SchemaRecipePlanner::new(&snapshot);
        let locator = SourceLocator::path("transform_hierarchy_v22.assets.bin").unwrap();
        let address = ObjectAddress::binary_direct(locator.clone(), 1).unwrap();
        for (case, compatibility_file_id, compatibility_path_id) in
            [("file ID", 1, 0), ("path ID", 0, 99)]
        {
            let inspected = planner
                .inspect(&address, &mut AssetLoadBudget::default())
                .unwrap();
            let RecipeObject {
                address,
                source,
                object,
            } = inspected;
            let handle = object.handle().clone();
            let provenance = object.schema_provenance().clone();
            let WorkspaceObjectValue::Binary(binary) = object.into_value() else {
                panic!("expected the binary fixture to yield a binary object");
            };
            let mut binary = (*binary).clone();
            let Some(UnityValue::Object(fields)) = binary.class.get_mut("m_Father") else {
                panic!("expected the Transform fixture to contain m_Father");
            };
            assert_eq!(fields.get("m_FileID").and_then(UnityValue::as_i64), Some(0));
            assert_eq!(fields.get("m_PathID").and_then(UnityValue::as_i64), Some(0));
            fields.insert(
                "fileID".to_owned(),
                UnityValue::Integer(compatibility_file_id),
            );
            fields.insert(
                "pathID".to_owned(),
                UnityValue::Integer(compatibility_path_id),
            );
            let object = RecipeObject {
                address,
                source,
                object: WorkspaceObject::new(
                    handle,
                    WorkspaceObjectValue::Binary(Arc::new(UnityObject::from_info_and_class(
                        binary.info,
                        binary.class,
                    ))),
                    provenance,
                ),
            };

            let result = planner.lower_reference(
                &object,
                FieldPath::root().push_field("m_Father").unwrap(),
                ReferenceTarget::null(),
                ReferenceTarget::object(ObjectAddress::binary_direct(locator.clone(), 2).unwrap()),
                &mut AssetLoadBudget::default(),
            );
            assert!(
                result
                    .as_ref()
                    .ok()
                    .and_then(|lowering| lowering.fragment())
                    .is_none(),
                "{case} alias conflict must not produce a fragment"
            );
            assert!(
                matches!(result, Err(RecipeError::InvalidReference { .. })),
                "{case} alias conflict must be rejected as an invalid reference"
            );
        }
    }
}
