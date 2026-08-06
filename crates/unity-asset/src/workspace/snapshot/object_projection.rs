use std::io::Read;
use std::sync::Arc;

use unity_asset_binary::asset::{ObjectInfo, SerializedFile};
use unity_asset_binary::error::BinaryObjectReplacementError;
use unity_asset_binary::object::{ObjectHandle, ObjectSchemaOrigin};
use unity_asset_binary::typetree::TypeTreeSemanticDigestError;
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, ObjectId, RevisionedObjectHandle,
    SemanticDigestError, SourceId, SourceKind, UnityDocument, arc_value_allocation_bytes,
    yaml_schema_digest,
};
use unity_asset_yaml::YamlDocument;

use crate::BinaryError;
use crate::schema::{BinarySchemaVersion, DeclaredUnityVersion, SchemaOrigin, SchemaProvenance};

use super::{
    WorkspaceSnapshot, consume_object_table_scan, consume_single_result, source_object_index_error,
    yaml_object_id,
};
use crate::workspace::view::{
    SourceObjectDescriptor, WorkspaceError, WorkspaceObject, WorkspaceObjectValue,
    WorkspaceYamlObject,
};

impl WorkspaceSnapshot {
    pub(super) fn source_object_count(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<usize, WorkspaceError> {
        if source.workspace() != self.workspace_id() {
            return Err(ContractError::WorkspaceMismatch {
                expected: self.workspace_id(),
                actual: source.workspace(),
            }
            .into());
        }
        let entry = self
            .state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?;
        match source.kind() {
            SourceKind::SerializedFile => Ok(self.cached_serialized(entry, budget)?.object_count()),
            SourceKind::Yaml => Ok(self.cached_yaml(entry, budget)?.entries().len()),
            SourceKind::AssetBundle
            | SourceKind::WebFile
            | SourceKind::Archive
            | SourceKind::StreamedResource => Ok(0),
        }
    }

    pub(super) fn describe_object_at_in_source(
        &self,
        source: SourceId,
        index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceObjectDescriptor, WorkspaceError> {
        if source.workspace() != self.workspace_id() {
            return Err(ContractError::WorkspaceMismatch {
                expected: self.workspace_id(),
                actual: source.workspace(),
            }
            .into());
        }
        let entry = self
            .state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?;
        match source.kind() {
            SourceKind::SerializedFile => {
                let file = self.cached_serialized(entry, budget)?;
                let info = file
                    .objects()
                    .get(index)
                    .ok_or_else(source_object_index_error)?;
                let candidate = ObjectHandle::new(file, info);
                consume_single_result(0, "workspace_object_projection", budget)?;
                let handle =
                    self.handle_for_object(ObjectId::binary(source, candidate.path_id())?)?;
                Ok(SourceObjectDescriptor::new(
                    handle,
                    candidate.class_id(),
                    index,
                ))
            }
            SourceKind::Yaml => {
                let document = self.cached_yaml(entry, budget)?;
                let class = document
                    .entries()
                    .get(index)
                    .ok_or_else(source_object_index_error)?;
                consume_single_result(0, "workspace_object_projection", budget)?;
                let handle = self.handle_for_object(yaml_object_id(source, index, class)?)?;
                Ok(SourceObjectDescriptor::new(handle, class.class_id(), index))
            }
            SourceKind::AssetBundle
            | SourceKind::WebFile
            | SourceKind::Archive
            | SourceKind::StreamedResource => Err(source_object_index_error()),
        }
    }

    pub(super) fn materialize_described_object_at_in_source(
        &self,
        descriptor: &SourceObjectDescriptor,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        let handle = descriptor.handle();
        handle.validate_context(self.workspace_id(), self.revision())?;
        let object = handle.object();
        let source = object.source();
        let entry = self
            .state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?;
        match source.kind() {
            SourceKind::SerializedFile => {
                let file = self.cached_serialized(entry, budget)?;
                let info = file
                    .objects()
                    .get(descriptor.ordinal())
                    .ok_or_else(source_object_index_error)?;
                let candidate = ObjectHandle::new(file, info);
                if object.kind() != unity_asset_core::ObjectKind::Binary
                    || object.binary_path_id() != Some(candidate.path_id())
                    || descriptor.class_id() != candidate.class_id()
                {
                    return Err(source_object_descriptor_mismatch());
                }
                consume_single_result(
                    handle.retained_clone_bytes(),
                    "workspace_object_projection",
                    budget,
                )?;
                self.materialize_binary_object(handle.clone(), file, candidate, budget)
            }
            SourceKind::Yaml => {
                let document = self.cached_yaml(entry, budget)?;
                let class = document
                    .entries()
                    .get(descriptor.ordinal())
                    .ok_or_else(source_object_index_error)?;
                if object != &yaml_object_id(source, descriptor.ordinal(), class)?
                    || descriptor.class_id() != class.class_id()
                {
                    return Err(source_object_descriptor_mismatch());
                }
                consume_single_result(
                    handle.retained_clone_bytes(),
                    "workspace_object_projection",
                    budget,
                )?;
                self.materialize_yaml_object(
                    handle.clone(),
                    Arc::clone(document),
                    descriptor.ordinal(),
                    budget,
                )
            }
            SourceKind::AssetBundle
            | SourceKind::WebFile
            | SourceKind::Archive
            | SourceKind::StreamedResource => Err(source_object_index_error()),
        }
    }

    pub(in crate::workspace) fn materialize_prepared_binary_object(
        &self,
        object: &ObjectId,
        exact_info: &ObjectInfo,
        reader: &mut impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        let path_id = object.binary_path_id().ok_or_else(|| {
            WorkspaceError::operation(
                "prepared binary object identity",
                std::io::Error::other("binary object has no path ID"),
            )
        })?;
        if exact_info.path_id() != path_id {
            return Err(WorkspaceError::operation(
                "prepared binary object proof",
                std::io::Error::other(
                    "artifact object metadata does not match the requested baseline identity",
                ),
            ));
        }
        let entry = self
            .state
            .store()
            .get(object.source())
            .ok_or(WorkspaceError::MissingSource(object.source()))?;
        let file = self.cached_serialized(entry, budget)?;
        consume_object_table_scan(file.object_count(), budget)?;
        let candidate = file.find_object_handle(path_id).ok_or_else(|| {
            WorkspaceError::operation(
                "prepared binary object proof",
                std::io::Error::other("artifact object is absent from the immutable baseline"),
            )
        })?;
        if candidate.class_id() != exact_info.class_id() {
            return Err(WorkspaceError::operation(
                "prepared binary object proof",
                std::io::Error::other("artifact changed the baseline object class identity"),
            ));
        }
        let replacement_len = usize::try_from(exact_info.byte_size()).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "prepared_binary_object_bytes",
            }
        })?;
        let materialized =
            match candidate.materialize_replacement_from_reader(reader, replacement_len, budget) {
                Ok(materialized) => materialized,
                Err(BinaryError::ObjectReplacement(
                    BinaryObjectReplacementError::MissingSchema { .. },
                )) => candidate.materialize_raw_replacement_from_reader(
                    reader,
                    replacement_len,
                    budget,
                )?,
                Err(error) => return Err(error.into()),
            };
        let schema_digest = materialized
            .schema()
            .map(|schema| schema.semantic_digest_with_budget(budget))
            .transpose()
            .map_err(map_typetree_semantic_digest_error)?;
        let origin = match materialized.schema_origin() {
            Some(ObjectSchemaOrigin::EmbeddedTypeTree) => SchemaOrigin::EmbeddedTypeTree,
            Some(ObjectSchemaOrigin::ExternalRegistry) => SchemaOrigin::FrozenRegistry,
            None => SchemaOrigin::Unavailable,
        };
        let version = declared_unity_version(&file.unity_version, budget)?;
        let script_id = object_script_id(file, candidate.info());
        let schema = SchemaProvenance::binary(
            exact_info.class_id(),
            origin,
            schema_digest,
            BinarySchemaVersion::new(version, file.format().version()),
            script_id,
        );
        let mut exact = materialized.into_object();
        exact.info = exact_info.clone();
        budget.consume_bytes(
            arc_value_allocation_bytes::<unity_asset_binary::object::UnityObject>().map_err(
                |error| WorkspaceError::operation("prepared binary object allocation", error),
            )?,
        )?;
        budget.consume_bytes(arc_value_allocation_bytes::<SchemaProvenance>().map_err(
            |error| WorkspaceError::operation("prepared schema provenance allocation", error),
        )?)?;
        let handle =
            RevisionedObjectHandle::new(self.workspace_id(), self.revision(), object.clone())?;
        Ok(WorkspaceObject::from_shared(
            handle,
            WorkspaceObjectValue::Binary(Arc::new(exact)),
            Arc::new(schema),
        ))
    }

    pub(super) fn materialize_binary_object(
        &self,
        handle: RevisionedObjectHandle,
        file: &SerializedFile,
        candidate: ObjectHandle<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        let materialized = candidate.materialize_with_options(budget, self.config.typetree)?;
        let schema_digest = materialized
            .schema()
            .map(|schema| schema.semantic_digest_with_budget(budget))
            .transpose()
            .map_err(map_typetree_semantic_digest_error)?;
        let origin = match materialized.schema_origin() {
            Some(ObjectSchemaOrigin::EmbeddedTypeTree) => SchemaOrigin::EmbeddedTypeTree,
            Some(ObjectSchemaOrigin::ExternalRegistry) => SchemaOrigin::FrozenRegistry,
            None => SchemaOrigin::Unavailable,
        };
        let version = declared_unity_version(&file.unity_version, budget)?;
        let script_id = object_script_id(file, candidate.info());
        let provenance = SchemaProvenance::binary(
            candidate.class_id(),
            origin,
            schema_digest,
            BinarySchemaVersion::new(version, file.format().version()),
            script_id,
        );
        budget.consume_bytes(
            arc_value_allocation_bytes::<unity_asset_binary::object::UnityObject>().map_err(
                |error| WorkspaceError::operation("workspace binary object allocation", error),
            )?,
        )?;
        budget.consume_bytes(arc_value_allocation_bytes::<SchemaProvenance>().map_err(
            |error| WorkspaceError::operation("workspace schema provenance allocation", error),
        )?)?;
        Ok(WorkspaceObject::new(
            handle,
            WorkspaceObjectValue::Binary(Arc::new(materialized.into_object())),
            provenance,
        ))
    }

    pub(super) fn materialize_yaml_object(
        &self,
        handle: RevisionedObjectHandle,
        document: Arc<YamlDocument>,
        document_index: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        let class = document
            .entries()
            .get(document_index)
            .ok_or_else(source_object_index_error)?;
        let provenance = SchemaProvenance::yaml(
            class.class_id(),
            yaml_schema_digest(class, budget).map_err(map_yaml_semantic_digest_error)?,
        );
        budget.consume_bytes(arc_value_allocation_bytes::<SchemaProvenance>().map_err(
            |error| WorkspaceError::operation("workspace schema provenance allocation", error),
        )?)?;
        Ok(WorkspaceObject::new(
            handle,
            WorkspaceObjectValue::Yaml(WorkspaceYamlObject::new(document, document_index)),
            provenance,
        ))
    }
}

fn map_typetree_semantic_digest_error(error: TypeTreeSemanticDigestError) -> WorkspaceError {
    match error {
        TypeTreeSemanticDigestError::Budget(error) => WorkspaceError::Budget(error),
        TypeTreeSemanticDigestError::Digest(error) => {
            WorkspaceError::operation("TypeTree semantic digest", error)
        }
    }
}

fn map_yaml_semantic_digest_error(error: SemanticDigestError) -> WorkspaceError {
    match error {
        SemanticDigestError::Budget(error) => WorkspaceError::Budget(error),
        error => WorkspaceError::operation("YAML semantic schema digest", error),
    }
}

fn object_script_id(file: &SerializedFile, object: &ObjectInfo) -> Option<[u8; 16]> {
    let serialized_type = object
        .serialized_type_index()
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| file.types().get(index))
        .or_else(|| {
            file.types()
                .iter()
                .find(|candidate| candidate.class_id == object.class_id())
        })?;
    serialized_type
        .is_script_type()
        .then_some(serialized_type.script_id)
        .filter(|script_id| *script_id != [0; 16])
}

fn source_object_descriptor_mismatch() -> WorkspaceError {
    WorkspaceError::operation(
        "source object descriptor",
        std::io::Error::other("source object metadata changed during immutable projection"),
    )
}

fn declared_unity_version(
    raw: &str,
    budget: &mut AssetLoadBudget,
) -> Result<DeclaredUnityVersion, WorkspaceError> {
    if raw.trim().is_empty() {
        return Ok(DeclaredUnityVersion::Absent);
    }
    let raw_bytes = u64::try_from(raw.len()).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "workspace_unity_version",
    })?;
    budget.check_bytes(raw_bytes)?;
    match UnityVersion::parse_version(raw) {
        Ok(version) => {
            if let Some(custom) = version.type_str.as_deref() {
                budget.consume_bytes(u64::try_from(custom.len()).map_err(|_| {
                    BudgetError::ArithmeticOverflow {
                        resource: "workspace_unity_version",
                    }
                })?)?;
            }
            Ok(DeclaredUnityVersion::Parsed { version })
        }
        Err(_) => {
            budget.consume_bytes(raw_bytes)?;
            Ok(DeclaredUnityVersion::Unparseable)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use unity_asset_core::AssetLoadLimits;

    use crate::workspace::AssetWorkspace;

    use super::*;

    #[test]
    fn semantic_digest_budget_errors_remain_workspace_budget_errors() {
        let expected = BudgetError::Exceeded {
            resource: "semantic_digest",
            limit: 1,
            requested: 2,
        };
        assert!(matches!(
            map_typetree_semantic_digest_error(TypeTreeSemanticDigestError::Budget(
                expected.clone()
            )),
            WorkspaceError::Budget(actual) if actual == expected
        ));
        assert!(matches!(
            map_yaml_semantic_digest_error(SemanticDigestError::Budget(expected.clone())),
            WorkspaceError::Budget(actual) if actual == expected
        ));
    }

    #[test]
    fn binary_descriptor_does_not_materialize_object_payload() {
        let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin",
        );
        let mut workspace = AssetWorkspace::new().unwrap();
        let source = workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let mut metadata_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let descriptor = snapshot
            .describe_object_at_in_source(source, 0, &mut metadata_budget)
            .unwrap();
        assert_eq!(descriptor.class_id(), 4);
        assert_eq!(metadata_budget.usage().bytes, 0);

        let mut payload_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            snapshot.materialize_described_object_at_in_source(&descriptor, &mut payload_budget),
            Err(WorkspaceError::Budget(_))
        ));
    }
}
