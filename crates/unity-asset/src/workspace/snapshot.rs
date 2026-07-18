//! Immutable workspace query implementation.

use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use unity_asset_binary::asset::SerializedFile;
use unity_asset_binary::object::ObjectSchemaOrigin;
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, Diagnostic, DiagnosticSeverity, ObjectAddress,
    ObjectId, ObjectKind, RevisionedObjectHandle, SourceId, SourceKind, SourceLocator, UnityClass,
    UnityDocument, WorkspaceId, WorkspaceRevision, YamlDocumentSelector,
};
use unity_asset_yaml::YamlDocument;

use crate::schema::{
    BinarySchemaVersion, DeclaredUnityVersion, SchemaOrigin, SchemaProvenance, digest_yaml_schema,
};

use super::interface::WorkspaceConfig;
use super::source_catalog::LocatorResolution;
use super::state::WorkspaceState;
use super::store::SourceEntry;
use super::view::{
    self, WorkspaceAllocationUnit, WorkspaceBytes, WorkspaceError, WorkspaceLookup,
    WorkspaceObject, WorkspaceObjectValue, WorkspaceSource, WorkspaceView, WorkspaceYamlObject,
};

/// Immutable read boundary for one exact workspace revision.
#[derive(Clone)]
pub struct WorkspaceSnapshot {
    state: Arc<WorkspaceState>,
    config: Arc<WorkspaceConfig>,
    reference_store: Arc<crate::reference::ReferenceStore>,
}

impl WorkspaceSnapshot {
    pub(crate) fn new(
        state: Arc<WorkspaceState>,
        config: Arc<WorkspaceConfig>,
        reference_store: Arc<crate::reference::ReferenceStore>,
    ) -> Self {
        Self {
            state,
            config,
            reference_store,
        }
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.state.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.state.revision()
    }

    pub fn reference_graph(
        &self,
        options: crate::reference::ReferenceGraphBuildOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<crate::reference::ReferenceGraph, crate::reference::ReferenceGraphError> {
        crate::reference::ReferenceGraph::build(self, options, budget)
    }

    fn project_source(
        &self,
        source: SourceId,
        include_result_entry: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceSource, WorkspaceError> {
        let catalog = self.state.catalog();
        let descriptor = catalog.resolve(source)?;
        let locator = catalog.source_locator(source)?;
        let origin = catalog.physical_origin(source)?;
        let retained_bytes = locator
            .retained_clone_bytes()
            .and_then(|total| total.checked_add(origin.path().as_os_str().as_encoded_bytes().len()))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "workspace_source_projection",
            })?;
        if include_result_entry {
            consume_single_result(retained_bytes, "workspace_source_projection", budget)?;
        } else {
            consume_retained_bytes(retained_bytes, "workspace_source_projection", budget)?;
        }
        Ok(WorkspaceSource::new(
            source,
            locator.clone(),
            catalog.fingerprint(source)?,
            descriptor.parent(),
            descriptor.location_kind(),
            origin.path().to_path_buf(),
        ))
    }

    fn cached_serialized<'entry>(
        &self,
        entry: &'entry Arc<SourceEntry>,
        _budget: &mut AssetLoadBudget,
    ) -> Result<&'entry Arc<SerializedFile>, WorkspaceError> {
        entry.cached_serialized().ok_or_else(|| {
            WorkspaceError::operation(
                "snapshot state validation",
                std::io::Error::other("serialized source was published without its frozen parse"),
            )
        })
    }

    fn cached_yaml<'entry>(
        &self,
        entry: &'entry Arc<SourceEntry>,
        _budget: &mut AssetLoadBudget,
    ) -> Result<&'entry Arc<YamlDocument>, WorkspaceError> {
        entry.cached_yaml().ok_or_else(|| {
            WorkspaceError::operation(
                "snapshot state validation",
                std::io::Error::other("YAML source was published without its frozen parse"),
            )
        })
    }

    fn handle_for_object(
        &self,
        object: ObjectId,
    ) -> Result<RevisionedObjectHandle, WorkspaceError> {
        Ok(RevisionedObjectHandle::new(
            self.workspace_id(),
            self.revision(),
            object,
        )?)
    }

    fn missing_object_error(
        &self,
        object: &ObjectId,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceError, WorkspaceError> {
        let locator_bytes = self
            .state
            .catalog()
            .source_locator(object.source())?
            .retained_clone_bytes()
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "workspace_missing_object_address",
            })?;
        let retained_bytes = size_of::<ObjectAddress>()
            .checked_add(locator_bytes)
            .and_then(|bytes| bytes.checked_add(object.retained_clone_bytes()))
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "workspace_missing_object_address",
            })?;
        consume_single_result(retained_bytes, "workspace_missing_object_address", budget)?;
        let address = self.state.catalog().address_for_object(object)?;
        Ok(WorkspaceError::MissingObject(Box::new(address)))
    }
}

impl fmt::Debug for WorkspaceSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceSnapshot")
            .field("workspace_id", &self.workspace_id())
            .field("revision", &self.revision())
            .field("source_count", &self.state.store().len())
            .finish()
    }
}

impl view::sealed::Sealed for WorkspaceSnapshot {
    fn reference_view_parts(&self) -> super::ReferenceViewParts<'_> {
        super::ReferenceViewParts {
            state: &self.state,
            store: &self.reference_store,
            typetree: self.config.typetree,
        }
    }
}

impl WorkspaceView for WorkspaceSnapshot {
    fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id()
    }

    fn revision(&self) -> WorkspaceRevision {
        self.revision()
    }

    fn sources(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<WorkspaceSource>, WorkspaceError> {
        let count = self.state.catalog().len();
        reserve_result_items::<WorkspaceSource>(count, budget)?;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(count)
            .map_err(|error| WorkspaceError::Allocation {
                resource: "workspace source results",
                requested: count,
                unit: WorkspaceAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        for (source, _) in self.state.catalog().iter() {
            sources.push(self.project_source(source, false, budget)?);
        }
        Ok(sources)
    }

    fn source(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
        if source.workspace() != self.workspace_id() {
            return Err(ContractError::WorkspaceMismatch {
                expected: self.workspace_id(),
                actual: source.workspace(),
            }
            .into());
        }
        if !self.state.store().contains(source) {
            return Ok(WorkspaceLookup::Missing);
        }
        Ok(WorkspaceLookup::Resolved(
            self.project_source(source, true, budget)?,
        ))
    }

    fn resolve_source(
        &self,
        locator: &SourceLocator,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceSource>, WorkspaceError> {
        match self.state.catalog().classify_locator(locator) {
            LocatorResolution::Resolved(source) => Ok(WorkspaceLookup::Resolved(
                self.project_source(source, true, budget)?,
            )),
            LocatorResolution::Unloaded => Ok(WorkspaceLookup::Unloaded),
            LocatorResolution::Missing => Ok(WorkspaceLookup::Missing),
            LocatorResolution::Invalid => invalid_lookup(
                "WORKSPACE_INVALID_SOURCE_LOCATOR",
                "source locator containment does not match the loaded source hierarchy",
                budget,
            ),
        }
    }

    fn objects(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<RevisionedObjectHandle>, WorkspaceError> {
        let mut count = 0_usize;
        let mut yaml_anchor_bytes = 0_usize;
        for (source, entry) in self.state.store().iter() {
            match source.kind() {
                SourceKind::SerializedFile => {
                    count = count
                        .checked_add(self.cached_serialized(entry, budget)?.object_count())
                        .ok_or(BudgetError::ArithmeticOverflow {
                            resource: "workspace_object_results",
                        })?;
                }
                SourceKind::Yaml => {
                    let document = self.cached_yaml(entry, budget)?;
                    count = count.checked_add(document.entries().len()).ok_or(
                        BudgetError::ArithmeticOverflow {
                            resource: "workspace_object_results",
                        },
                    )?;
                    for (index, class) in document.entries().iter().enumerate() {
                        if !is_plain_yaml_document(index, class) {
                            yaml_anchor_bytes = yaml_anchor_bytes
                                .checked_add(class.anchor.len())
                                .ok_or(BudgetError::ArithmeticOverflow {
                                resource: "workspace_object_results",
                            })?;
                        }
                    }
                }
                SourceKind::AssetBundle
                | SourceKind::WebFile
                | SourceKind::Archive
                | SourceKind::StreamedResource => {}
            }
        }
        reserve_result_items::<RevisionedObjectHandle>(count, budget)?;
        budget.check_bytes(u64::try_from(yaml_anchor_bytes).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "workspace_object_results",
            }
        })?)?;
        let mut objects = Vec::new();
        objects
            .try_reserve_exact(count)
            .map_err(|error| WorkspaceError::Allocation {
                resource: "workspace object results",
                requested: count,
                unit: WorkspaceAllocationUnit::Elements,
                message: error.to_string(),
            })?;
        budget.consume_bytes(u64::try_from(yaml_anchor_bytes).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "workspace_object_results",
            }
        })?)?;

        for (source, entry) in self.state.store().iter() {
            match source.kind() {
                SourceKind::SerializedFile => {
                    for object in self.cached_serialized(entry, budget)?.objects() {
                        objects.push(
                            self.handle_for_object(ObjectId::binary(source, object.path_id())?)?,
                        );
                    }
                }
                SourceKind::Yaml => {
                    for (index, class) in self
                        .cached_yaml(entry, budget)?
                        .entries()
                        .iter()
                        .enumerate()
                    {
                        objects
                            .push(self.handle_for_object(yaml_object_id(source, index, class)?)?);
                    }
                }
                SourceKind::AssetBundle
                | SourceKind::WebFile
                | SourceKind::Archive
                | SourceKind::StreamedResource => {}
            }
        }
        Ok(objects)
    }

    fn resolve_object(
        &self,
        address: &ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<RevisionedObjectHandle>, WorkspaceError> {
        let source = match self
            .state
            .catalog()
            .classify_locator(address.source_locator())
        {
            LocatorResolution::Resolved(source) => source,
            LocatorResolution::Unloaded => return Ok(WorkspaceLookup::Unloaded),
            LocatorResolution::Missing => return Ok(WorkspaceLookup::Missing),
            LocatorResolution::Invalid => {
                return invalid_lookup(
                    "WORKSPACE_INVALID_OBJECT_LOCATOR",
                    "object locator containment does not match the loaded source hierarchy",
                    budget,
                );
            }
        };
        let expected_kind = match address.kind() {
            ObjectKind::Binary => SourceKind::SerializedFile,
            ObjectKind::Yaml => SourceKind::Yaml,
        };
        if source.kind() != expected_kind {
            return invalid_lookup(
                "WORKSPACE_OBJECT_KIND_MISMATCH",
                "object address kind does not match the resolved source kind",
                budget,
            );
        }
        let entry = self
            .state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?;
        let matches = match address.kind() {
            ObjectKind::Binary => {
                let path_id = address.binary_path_id().ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary object address",
                        std::io::Error::other("binary address has no path ID"),
                    )
                })?;
                let file = self.cached_serialized(entry, budget)?;
                consume_object_table_scan(file.object_count(), budget)?;
                file.objects()
                    .iter()
                    .filter(|object| object.path_id() == path_id)
                    .count()
            }
            ObjectKind::Yaml => {
                let selector = address.yaml_selector().ok_or_else(|| {
                    WorkspaceError::operation(
                        "YAML object address",
                        std::io::Error::other("YAML address has no document selector"),
                    )
                })?;
                let document = self.cached_yaml(entry, budget)?;
                consume_object_table_scan(document.entries().len(), budget)?;
                document
                    .entries()
                    .iter()
                    .enumerate()
                    .filter(|(index, class)| yaml_selector_matches(selector, *index, class))
                    .count()
            }
        };
        match matches {
            0 => Ok(WorkspaceLookup::Missing),
            1 => {
                let retained_bytes = address.yaml_anchor().map_or(0, str::len);
                consume_single_result(
                    retained_bytes,
                    "workspace_object_handle_projection",
                    budget,
                )?;
                let object = match address.kind() {
                    ObjectKind::Binary => ObjectId::binary(
                        source,
                        address.binary_path_id().ok_or_else(|| {
                            WorkspaceError::operation(
                                "binary object address",
                                std::io::Error::other("binary address has no path ID"),
                            )
                        })?,
                    ),
                    ObjectKind::Yaml => ObjectId::from_yaml_selector(
                        source,
                        address.yaml_selector().ok_or_else(|| {
                            WorkspaceError::operation(
                                "YAML object address",
                                std::io::Error::other("YAML address has no document selector"),
                            )
                        })?,
                    ),
                }?;
                Ok(WorkspaceLookup::Resolved(self.handle_for_object(object)?))
            }
            _ => invalid_lookup(
                "WORKSPACE_DUPLICATE_OBJECT_IDENTITY",
                "loaded source contains duplicate object identities",
                budget,
            ),
        }
    }

    fn read_object(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObject, WorkspaceError> {
        handle.validate_context(self.workspace_id(), self.revision())?;
        let object = handle.object();
        let source = object.source();
        let entry = self
            .state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?;
        let (value, schema) = match object.kind() {
            ObjectKind::Binary => {
                let path_id = object.binary_path_id().ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary object identity",
                        std::io::Error::other("binary object has no path ID"),
                    )
                })?;
                let file = self.cached_serialized(entry, budget)?;
                consume_object_table_scan(file.object_count(), budget)?;
                let mut matches = 0_usize;
                let mut matched = None;
                for candidate in file.object_handles() {
                    if candidate.path_id() == path_id {
                        matches =
                            matches
                                .checked_add(1)
                                .ok_or(BudgetError::ArithmeticOverflow {
                                    resource: "binary_object_matches",
                                })?;
                        matched.get_or_insert(candidate);
                    }
                }
                let candidate = match (matches, matched) {
                    (1, Some(candidate)) => candidate,
                    (0, _) => {
                        return Err(self.missing_object_error(object, budget)?);
                    }
                    _ => {
                        return Err(WorkspaceError::AmbiguousObject {
                            source_id: source,
                            matches,
                        });
                    }
                };
                consume_single_result(
                    handle.retained_clone_bytes(),
                    "workspace_object_projection",
                    budget,
                )?;
                let materialized =
                    candidate.materialize_with_options(budget, self.config.typetree)?;
                let schema_digest = materialized
                    .schema()
                    .map(|schema| schema.semantic_digest_with_budget(budget))
                    .transpose()
                    .map_err(|error| {
                        WorkspaceError::operation("TypeTree semantic digest", error)
                    })?;
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
                let object = materialized.into_object();
                budget.consume_bytes(
                    u64::try_from(size_of::<unity_asset_binary::object::UnityObject>()).map_err(
                        |_| BudgetError::ArithmeticOverflow {
                            resource: "workspace_binary_object_box",
                        },
                    )?,
                )?;
                (WorkspaceObjectValue::Binary(Box::new(object)), provenance)
            }
            ObjectKind::Yaml => {
                let document = self.cached_yaml(entry, budget)?;
                consume_object_table_scan(document.entries().len(), budget)?;
                let document = Arc::clone(document);
                let mut match_count = 0_usize;
                let mut matched_index = 0_usize;
                for (index, class) in document.entries().iter().enumerate() {
                    if yaml_object_matches(object, index, class) {
                        match_count =
                            match_count
                                .checked_add(1)
                                .ok_or(BudgetError::ArithmeticOverflow {
                                    resource: "yaml_object_matches",
                                })?;
                        matched_index = index;
                    }
                }
                if match_count != 1 {
                    return if match_count == 0 {
                        Err(self.missing_object_error(object, budget)?)
                    } else {
                        Err(WorkspaceError::AmbiguousObject {
                            source_id: source,
                            matches: match_count,
                        })
                    };
                }
                consume_single_result(
                    handle.retained_clone_bytes(),
                    "workspace_object_projection",
                    budget,
                )?;
                let provenance = SchemaProvenance::yaml(
                    document.entries()[matched_index].class_id,
                    digest_yaml_schema(&document.entries()[matched_index], budget).map_err(
                        |error| WorkspaceError::operation("YAML semantic schema digest", error),
                    )?,
                );
                (
                    WorkspaceObjectValue::Yaml(WorkspaceYamlObject::new(document, matched_index)),
                    provenance,
                )
            }
        };
        Ok(WorkspaceObject::new(handle.clone(), value, schema))
    }

    fn read_source_range(
        &self,
        source: SourceId,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceBytes, WorkspaceError> {
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
        let end = offset
            .checked_add(size)
            .ok_or(WorkspaceError::RangeOverflow { offset, size })?;
        let source_len = entry.image().bytes().len();
        let start = usize::try_from(offset).map_err(|_| WorkspaceError::RangeOutOfBounds {
            source_id: source,
            offset,
            end,
            source_len,
        })?;
        let end_usize = usize::try_from(end).map_err(|_| WorkspaceError::RangeOutOfBounds {
            source_id: source,
            offset,
            end,
            source_len,
        })?;
        if end_usize > source_len {
            return Err(WorkspaceError::RangeOutOfBounds {
                source_id: source,
                offset,
                end,
                source_len,
            });
        }
        budget.consume_bytes(size)?;
        Ok(WorkspaceBytes::new(
            source,
            Arc::clone(entry.image().bytes_arc()),
            start..end_usize,
        ))
    }
}

fn object_script_id(
    file: &SerializedFile,
    object: &unity_asset_binary::asset::ObjectInfo,
) -> Option<[u8; 16]> {
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

fn consume_single_result(
    retained_bytes: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let retained_bytes =
        u64::try_from(retained_bytes).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(1)?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(())
}

fn consume_retained_bytes(
    retained_bytes: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let retained_bytes =
        u64::try_from(retained_bytes).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(())
}

fn consume_object_table_scan(
    candidate_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let candidate_count =
        u64::try_from(candidate_count).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "workspace_object_table_scan",
        })?;
    budget.check_entries(candidate_count)?;
    budget.consume_entries(candidate_count)?;
    Ok(())
}

fn invalid_lookup<T>(
    code: &'static str,
    message: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceLookup<T>, WorkspaceError> {
    let retained_bytes =
        code.len()
            .checked_add(message.len())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "workspace_invalid_diagnostic",
            })?;
    consume_single_result(retained_bytes, "workspace_invalid_diagnostic", budget)?;
    let diagnostic = Diagnostic::new(DiagnosticSeverity::Error, code, message)
        .map_err(|error| WorkspaceError::operation("invalid lookup diagnostic", error))?;
    Ok(WorkspaceLookup::Invalid { diagnostic })
}

fn reserve_result_items<T>(
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "workspace_query_results",
    })?;
    let bytes = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_query_results",
        })?;
    budget.check_entries(entries)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn yaml_object_id(
    source: SourceId,
    index: usize,
    class: &UnityClass,
) -> Result<ObjectId, WorkspaceError> {
    if is_plain_yaml_document(index, class) {
        let index = u32::try_from(index).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "yaml_document_ordinal",
        })?;
        Ok(ObjectId::yaml_document(source, index)?)
    } else {
        Ok(ObjectId::yaml(source, class.anchor.as_str())?)
    }
}

fn is_plain_yaml_document(index: usize, class: &UnityClass) -> bool {
    class.class_id == 0
        && class.class_name == "YamlDocument"
        && class
            .anchor
            .strip_prefix("doc_")
            .and_then(|ordinal| ordinal.parse::<usize>().ok())
            == Some(index)
}

fn yaml_selector_matches(
    selector: &YamlDocumentSelector,
    index: usize,
    class: &UnityClass,
) -> bool {
    match selector {
        YamlDocumentSelector::Anchored { anchor } => {
            !is_plain_yaml_document(index, class) && class.anchor == anchor.as_str()
        }
        YamlDocumentSelector::Unanchored { document_index } => {
            usize::try_from(*document_index) == Ok(index) && is_plain_yaml_document(index, class)
        }
    }
}

fn yaml_object_matches(object: &ObjectId, index: usize, class: &UnityClass) -> bool {
    if let Some(anchor) = object.yaml_anchor() {
        !is_plain_yaml_document(index, class) && class.anchor == anchor
    } else {
        object
            .yaml_document_ordinal()
            .and_then(|ordinal| usize::try_from(ordinal).ok())
            == Some(index)
            && is_plain_yaml_document(index, class)
    }
}
