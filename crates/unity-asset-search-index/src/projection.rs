use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, DiagnosticError, DigestBuildError, DigestV1,
    DigestV1Builder, FieldPath, FieldPathError, FieldPathSegment, ObjectAddress, SourceLocator,
    YamlDocumentSelector, string_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_search_core::{SearchKind, TryToTermsError, try_to_terms};

use crate::analysis::{
    AssetAnalysis, AssetAnalysisBatch, BinaryExternalProjection, ContainerEntryFact,
    GuidProjection, RawReferenceProjection, ReferenceDependencyKey, ReferenceProjectionFact,
    ReferenceResolutionProjection,
};
use crate::generation::GenerationStorageContract;

#[derive(Debug)]
pub(crate) enum ProjectionError {
    Budget(BudgetError),
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: &'static str,
        source: TryReserveError,
    },
    IdentitySerialization {
        resource: &'static str,
        source: serde_json::Error,
    },
    IdentityDigest {
        resource: &'static str,
        source: DigestBuildError,
    },
    Diagnostic(DiagnosticError),
    FieldPath(FieldPathError),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => fmt::Display::fmt(error, formatter),
            Self::Allocation {
                resource,
                requested,
                unit,
                ..
            } => write!(
                formatter,
                "failed to reserve {requested} {unit} for projection {resource}"
            ),
            Self::IdentitySerialization { resource, .. } => {
                write!(
                    formatter,
                    "failed to serialize projection identity for {resource}"
                )
            }
            Self::IdentityDigest { resource, .. } => {
                write!(
                    formatter,
                    "failed to digest projection identity for {resource}"
                )
            }
            Self::Diagnostic(error) => fmt::Display::fmt(error, formatter),
            Self::FieldPath(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl StdError for ProjectionError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::Allocation { source, .. } => Some(source),
            Self::IdentitySerialization { source, .. } => Some(source),
            Self::IdentityDigest { source, .. } => Some(source),
            Self::Diagnostic(error) => Some(error),
            Self::FieldPath(error) => Some(error),
        }
    }
}

impl From<BudgetError> for ProjectionError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<DiagnosticError> for ProjectionError {
    fn from(error: DiagnosticError) -> Self {
        Self::Diagnostic(error)
    }
}

impl From<FieldPathError> for ProjectionError {
    fn from(error: FieldPathError) -> Self {
        Self::FieldPath(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionLimits {
    pub(crate) max_references_per_asset: usize,
    pub(crate) max_container_entries_per_asset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationProjection {
    pub(crate) search_documents: Vec<SearchDocument>,
    pub(crate) reference_documents: Vec<ReferenceDocument>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) truncations: Vec<ProjectionTruncation>,
    pub(crate) metrics: ProjectionMetrics,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionMetrics {
    pub(crate) assets_projected: u64,
    pub(crate) search_documents: u64,
    pub(crate) reference_documents: u64,
    pub(crate) container_documents: u64,
    pub(crate) references_omitted: u64,
    pub(crate) containers_omitted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchDocument {
    pub(crate) stable_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) guid: Option<String>,
    pub(crate) path: String,
    pub(crate) path_terms: String,
    pub(crate) name: String,
    pub(crate) name_terms: String,
    pub(crate) kind: String,
    pub(crate) kind_terms: String,
    pub(crate) content_terms: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) hierarchy_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) script_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) container_source_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReferenceDocument {
    pub(crate) stable_id: String,
    pub(crate) source_path: String,
    pub(crate) source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_guid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_object: Option<ObjectAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_file_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_class_id: Option<i32>,
    pub(crate) fact: ReferenceProjectionFact,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) incoming_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) outgoing_keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectionCategory {
    References,
    ContainerEntries,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionTruncation {
    pub(crate) source_path: String,
    pub(crate) category: ProjectionCategory,
    pub(crate) emitted: u64,
    pub(crate) omitted: u64,
}

pub(crate) fn project_batch(
    batch: &AssetAnalysisBatch,
    limits: ProjectionLimits,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationProjection, ProjectionError> {
    let counts = ProjectionCounts::for_batch(batch, limits)?;
    counts.check_top_level_plan(budget)?;

    let scripts = script_symbols_by_guid(&batch.assets, counts.script_entries, budget)?;
    let mut search_documents =
        reserve_retained_vec(counts.search_documents, "search documents", budget)?;
    let mut reference_documents =
        reserve_retained_vec(counts.reference_documents, "reference documents", budget)?;
    let mut diagnostics =
        reserve_retained_vec(counts.diagnostics, "projection diagnostics", budget)?;
    let mut truncations =
        reserve_retained_vec(counts.truncations, "projection truncations", budget)?;
    let mut metrics = ProjectionMetrics::default();

    for asset in &batch.assets {
        metrics.assets_projected = metrics.assets_projected.saturating_add(1);
        for diagnostic in &asset.diagnostics {
            diagnostics.push(clone_diagnostic(diagnostic, budget)?);
        }
        search_documents.push(project_search_document(asset, &scripts, budget)?);

        let reference_count = asset.references.len();
        let emitted_references = reference_count.min(limits.max_references_per_asset);
        for (ordinal, fact) in asset.references[..emitted_references].iter().enumerate() {
            reference_documents.push(project_reference_document(asset, fact, ordinal, budget)?);
        }
        record_truncation(
            &mut truncations,
            &mut metrics.references_omitted,
            &asset.source.relative_path,
            ProjectionCategory::References,
            emitted_references,
            reference_count,
            budget,
        )?;

        let container_count = asset.container_entries.len();
        let emitted_containers = container_count.min(limits.max_container_entries_per_asset);
        for entry in &asset.container_entries[..emitted_containers] {
            search_documents.push(project_container_document(asset, entry, budget)?);
            metrics.container_documents = metrics.container_documents.saturating_add(1);
        }
        record_truncation(
            &mut truncations,
            &mut metrics.containers_omitted,
            &asset.source.relative_path,
            ProjectionCategory::ContainerEntries,
            emitted_containers,
            container_count,
            budget,
        )?;
    }

    search_documents.sort_unstable();
    reference_documents.sort_unstable();
    diagnostics.sort_unstable();
    diagnostics.dedup();
    truncations.sort_unstable();
    metrics.search_documents = search_documents.len().try_into().unwrap_or(u64::MAX);
    metrics.reference_documents = reference_documents.len().try_into().unwrap_or(u64::MAX);

    Ok(GenerationProjection {
        search_documents,
        reference_documents,
        diagnostics,
        truncations,
        metrics,
    })
}

#[derive(Debug, Clone, Copy)]
struct ProjectionCounts {
    script_entries: usize,
    search_documents: usize,
    reference_documents: usize,
    diagnostics: usize,
    truncations: usize,
}

impl ProjectionCounts {
    fn for_batch(
        batch: &AssetAnalysisBatch,
        limits: ProjectionLimits,
    ) -> Result<Self, ProjectionError> {
        let mut counts = Self {
            script_entries: 0,
            search_documents: batch.assets.len(),
            reference_documents: 0,
            diagnostics: 0,
            truncations: 0,
        };
        for asset in &batch.assets {
            if asset.source.search_kind == SearchKind::Script && asset.source.guid.is_some() {
                counts.script_entries = checked_add_usize(counts.script_entries, 1, "entries")?;
            }
            counts.diagnostics =
                checked_add_usize(counts.diagnostics, asset.diagnostics.len(), "entries")?;

            let references = asset.references.len().min(limits.max_references_per_asset);
            counts.reference_documents =
                checked_add_usize(counts.reference_documents, references, "entries")?;
            if references < asset.references.len() {
                counts.truncations = checked_add_usize(counts.truncations, 1, "entries")?;
            }

            let containers = asset
                .container_entries
                .len()
                .min(limits.max_container_entries_per_asset);
            counts.search_documents =
                checked_add_usize(counts.search_documents, containers, "entries")?;
            if containers < asset.container_entries.len() {
                counts.truncations = checked_add_usize(counts.truncations, 1, "entries")?;
            }
        }
        Ok(counts)
    }

    fn check_top_level_plan(self, budget: &AssetLoadBudget) -> Result<(), ProjectionError> {
        let retained_members = [
            self.search_documents,
            self.reference_documents,
            self.diagnostics,
            self.truncations,
        ]
        .into_iter()
        .try_fold(0_usize, |total, count| {
            checked_add_usize(total, count, "members")
        })?;
        let entries = checked_add_usize(retained_members, self.script_entries, "entries")?;
        let bytes = [
            vec_bytes::<ScriptSymbolsByGuid<'static>>(self.script_entries)?,
            vec_bytes::<SearchDocument>(self.search_documents)?,
            vec_bytes::<ReferenceDocument>(self.reference_documents)?,
            vec_bytes::<Diagnostic>(self.diagnostics)?,
            vec_bytes::<ProjectionTruncation>(self.truncations)?,
        ]
        .into_iter()
        .try_fold(0_u64, |total, bytes| {
            total
                .checked_add(bytes)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })
        })?;

        budget.check_entries(usize_to_u64(entries, "entries")?)?;
        budget.check_members(usize_to_u64(retained_members, "members")?)?;
        budget.check_bytes(bytes)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct ScriptSymbolsByGuid<'asset> {
    guid: &'asset str,
    symbols: &'asset [String],
    asset_ordinal: usize,
}

fn script_symbols_by_guid<'asset>(
    assets: &'asset [AssetAnalysis],
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ScriptSymbolsByGuid<'asset>>, ProjectionError> {
    let mut scripts = reserve_entry_vec(count, "script symbol lookup", budget)?;
    for (asset_ordinal, asset) in assets.iter().enumerate() {
        if asset.source.search_kind != SearchKind::Script {
            continue;
        }
        let Some(guid) = asset.source.guid.as_deref() else {
            continue;
        };
        scripts.push(ScriptSymbolsByGuid {
            guid,
            symbols: &asset.search.script_symbols,
            asset_ordinal,
        });
    }
    scripts.sort_unstable_by(|left, right| {
        left.guid
            .cmp(right.guid)
            .then_with(|| left.asset_ordinal.cmp(&right.asset_ordinal))
    });
    Ok(scripts)
}

fn lookup_script_symbols<'asset>(
    scripts: &[ScriptSymbolsByGuid<'asset>],
    guid: &str,
) -> Option<&'asset [String]> {
    let start = scripts.partition_point(|entry| entry.guid < guid);
    let end = scripts.partition_point(|entry| entry.guid <= guid);
    (start < end).then(|| scripts[end - 1].symbols)
}

fn project_search_document(
    asset: &AssetAnalysis,
    scripts: &[ScriptSymbolsByGuid<'_>],
    budget: &mut AssetLoadBudget,
) -> Result<SearchDocument, ProjectionError> {
    let resolved_symbol_capacity = asset.search.referenced_script_guids.iter().try_fold(
        asset.search.script_symbols.len(),
        |total, guid| {
            let additional = lookup_script_symbols(scripts, guid).map_or(0, <[String]>::len);
            checked_add_usize(total, additional, "members")
        },
    )?;
    let mut resolved_symbols =
        reserve_retained_vec(resolved_symbol_capacity, "resolved script symbols", budget)?;
    for symbol in &asset.search.script_symbols {
        resolved_symbols.push(clone_string(symbol, "resolved script symbol", budget)?);
    }
    for guid in &asset.search.referenced_script_guids {
        if let Some(symbols) = lookup_script_symbols(scripts, guid) {
            for symbol in symbols {
                resolved_symbols.push(clone_string(symbol, "resolved script symbol", budget)?);
            }
        }
    }
    resolved_symbols.sort_unstable();
    resolved_symbols.dedup();

    let mut content_terms =
        clone_string(&asset.search.content_terms, "search content terms", budget)?;
    for hierarchy_path in &asset.search.hierarchy_paths {
        append_terms(
            &mut content_terms,
            hierarchy_path,
            "hierarchy content terms",
            budget,
        )?;
    }
    for symbol in &resolved_symbols {
        append_terms(
            &mut content_terms,
            symbol,
            "script symbol content terms",
            budget,
        )?;
    }

    Ok(SearchDocument {
        stable_id: stable_id_budgeted(
            asset.source.guid.as_deref(),
            &asset.source.relative_path,
            None,
            budget,
        )?,
        guid: clone_optional_string(asset.source.guid.as_deref(), "search document GUID", budget)?,
        path: clone_string(&asset.source.relative_path, "search document path", budget)?,
        path_terms: clone_string(
            &asset.search.path_terms,
            "search document path terms",
            budget,
        )?,
        name: clone_string(&asset.search.display_name, "search document name", budget)?,
        name_terms: clone_string(
            &asset.search.name_terms,
            "search document name terms",
            budget,
        )?,
        kind: clone_string(
            asset.source.search_kind.canonical_name(),
            "search document kind",
            budget,
        )?,
        kind_terms: budgeted_terms(
            asset.source.search_kind.canonical_name(),
            "search document kind terms",
            budget,
        )?,
        content_terms,
        hierarchy_paths: clone_strings(
            &asset.search.hierarchy_paths,
            "search hierarchy paths",
            budget,
        )?,
        script_symbols: resolved_symbols,
        container_source_path: None,
    })
}

fn project_container_document(
    asset: &AssetAnalysis,
    entry: &ContainerEntryFact,
    budget: &mut AssetLoadBudget,
) -> Result<SearchDocument, ProjectionError> {
    let name = clone_string(
        file_name(&entry.asset_path),
        "container document name",
        budget,
    )?;
    Ok(SearchDocument {
        stable_id: container_stable_id(asset, entry, budget)?,
        guid: None,
        path: clone_string(&entry.asset_path, "container document path", budget)?,
        path_terms: budgeted_terms(&entry.asset_path, "container document path terms", budget)?,
        name_terms: budgeted_terms(&name, "container document name terms", budget)?,
        name,
        kind: clone_string(
            SearchKind::BundleContainer.canonical_name(),
            "container document kind",
            budget,
        )?,
        kind_terms: budgeted_terms(
            SearchKind::BundleContainer.canonical_name(),
            "container document kind terms",
            budget,
        )?,
        content_terms: String::new(),
        hierarchy_paths: Vec::new(),
        script_symbols: Vec::new(),
        container_source_path: Some(clone_string(
            &asset.source.relative_path,
            "container source path",
            budget,
        )?),
    })
}

fn append_terms(
    destination: &mut String,
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    let terms = budgeted_terms(value, resource, budget)?;
    if terms.is_empty() {
        return Ok(());
    }
    let separator = usize::from(!destination.is_empty());
    let required = destination
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(terms.len()))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    reserve_string_capacity(destination, required, resource, budget)?;
    if separator != 0 {
        destination.push(' ');
    }
    destination.push_str(&terms);
    Ok(())
}

fn project_reference_document(
    asset: &AssetAnalysis,
    fact: &ReferenceProjectionFact,
    ordinal: usize,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceDocument, ProjectionError> {
    let stable_id = reference_stable_id(asset, fact, ordinal, budget)?;
    let incoming_capacity = incoming_key_capacity(fact)?;
    let mut incoming_keys =
        reserve_retained_vec(incoming_capacity, "reference incoming keys", budget)?;
    for dependency in &fact.dependency_keys {
        append_dependency_keys(&mut incoming_keys, dependency, budget)?;
    }
    append_raw_target_keys(&mut incoming_keys, &fact.raw_target, budget)?;
    incoming_keys.sort_unstable();
    incoming_keys.dedup();

    let outgoing_capacity = usize::from(fact.source_object.is_some())
        + usize::from(fact.source_file_id.is_some())
        + usize::from(asset.source.guid.is_some())
        + usize::from(asset.source.guid.is_some() && fact.source_file_id.is_some());
    let mut outgoing_keys =
        reserve_retained_vec(outgoing_capacity, "reference outgoing keys", budget)?;
    if let Some(address) = fact.source_object.as_ref() {
        outgoing_keys.push(reference_object_key_budgeted(address, budget)?);
    }
    if let Some(file_id) = fact.source_file_id {
        outgoing_keys.push(source_file_key(
            &asset.source.relative_path,
            file_id,
            budget,
        )?);
    }
    if let Some(guid) = asset.source.guid.as_deref() {
        outgoing_keys.push(reference_guid_key_budgeted(guid, None, budget)?);
        if let Some(file_id) = fact.source_file_id {
            outgoing_keys.push(reference_guid_key_budgeted(guid, Some(file_id), budget)?);
        }
    }
    outgoing_keys.sort_unstable();
    outgoing_keys.dedup();

    Ok(ReferenceDocument {
        stable_id,
        source_path: clone_string(&asset.source.relative_path, "reference source path", budget)?,
        source_kind: clone_string(
            asset.source.search_kind.canonical_name(),
            "reference source kind",
            budget,
        )?,
        source_guid: clone_optional_string(
            asset.source.guid.as_deref(),
            "reference source GUID",
            budget,
        )?,
        source_object: fact
            .source_object
            .as_ref()
            .map(|address| clone_object_address(address, "reference source object", budget))
            .transpose()?,
        source_file_id: fact.source_file_id,
        source_class_id: fact.source_class_id,
        fact: clone_reference_fact(fact, budget)?,
        incoming_keys,
        outgoing_keys,
    })
}

fn reference_stable_id(
    asset: &AssetAnalysis,
    fact: &ReferenceProjectionFact,
    ordinal: usize,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let digest = streaming_json_digest(
        &(
            &asset.source.relative_path,
            fact.source_object.as_ref(),
            fact.source_file_id,
            &fact.field_path,
            &fact.raw_target,
            ordinal,
        ),
        "reference stable ID",
    )?;
    digest_key("reference-v2:", digest, "reference stable ID", budget)
}

fn append_raw_target_keys(
    keys: &mut Vec<String>,
    target: &RawReferenceProjection,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    match target {
        RawReferenceProjection::Binary {
            path_id, external, ..
        } => match external.as_ref().and_then(|external| external.guid) {
            Some(guid) => {
                let guid = encode_hex_budgeted(&guid, "binary target GUID", budget)?;
                keys.push(reference_guid_key_budgeted(&guid, None, budget)?);
                keys.push(reference_guid_key_budgeted(&guid, Some(*path_id), budget)?);
            }
            None => keys.push(binary_path_key(*path_id, budget)?),
        },
        RawReferenceProjection::Yaml { file_id, guid, .. } => {
            let Some(guid) = guid.as_ref() else {
                return Ok(());
            };
            let guid = match guid {
                GuidProjection::Parsed(bytes) => {
                    encode_hex_budgeted(bytes, "YAML target GUID", budget)?
                }
                GuidProjection::Invalid(value) => {
                    clone_string(value, "invalid YAML target GUID", budget)?
                }
            };
            keys.push(reference_guid_key_budgeted(&guid, None, budget)?);
            if let Some(file_id) = file_id {
                keys.push(reference_guid_key_budgeted(&guid, Some(*file_id), budget)?);
            }
        }
    }
    Ok(())
}

fn append_dependency_keys(
    keys: &mut Vec<String>,
    key: &ReferenceDependencyKey,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    match key {
        ReferenceDependencyKey::Guid { guid, file_id } => {
            keys.push(reference_guid_key_budgeted(guid, None, budget)?);
            if let Some(file_id) = file_id {
                keys.push(reference_guid_key_budgeted(guid, Some(*file_id), budget)?);
            }
        }
        ReferenceDependencyKey::Object { address } => {
            keys.push(reference_object_key_budgeted(address, budget)?);
        }
        ReferenceDependencyKey::Source { locator } => {
            let digest = streaming_json_digest(locator, "source reference key")?;
            keys.push(digest_key(
                "source-v1:",
                digest,
                "source reference key",
                budget,
            )?);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn reference_object_key(address: &ObjectAddress) -> String {
    reference_object_key_for(GenerationStorageContract::CurrentV2, address)
}

pub(crate) fn reference_object_key_for(
    storage: GenerationStorageContract,
    address: &ObjectAddress,
) -> String {
    let digest = match storage {
        GenerationStorageContract::LegacyV1 => legacy_object_address_digest(address)
            .unwrap_or_else(|_| DigestV1::hash_bytes(b"invalid-legacy-query-object-key")),
        GenerationStorageContract::CurrentV2 => streaming_json_digest(address, "query object key")
            .unwrap_or_else(|_| DigestV1::hash_bytes(b"invalid-query-object-key")),
    };
    let prefix = match storage {
        GenerationStorageContract::LegacyV1 => "object-v1:",
        GenerationStorageContract::CurrentV2 => "object-v2:",
    };
    digest_key_unbudgeted(prefix, digest)
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyObjectAddressRef<'address> {
    BinaryDirect {
        version: u8,
        source: &'address SourceLocator,
        path_id: i64,
    },
    BinaryBundleMember {
        version: u8,
        source: &'address SourceLocator,
        path_id: i64,
    },
    Yaml {
        version: u8,
        source: &'address SourceLocator,
        selector: LegacyYamlSelector,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyYamlSelector {
    Anchored { anchor: String },
    Unanchored { document_index: u32 },
}

fn legacy_object_address_digest(address: &ObjectAddress) -> Result<DigestV1, ProjectionError> {
    let wire = match address.kind() {
        unity_asset_core::ObjectKind::Binary => {
            let path_id = address
                .binary_path_id()
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
            if address.bundle_member().is_some() {
                LegacyObjectAddressRef::BinaryBundleMember {
                    version: 1,
                    source: address.source_locator(),
                    path_id,
                }
            } else {
                LegacyObjectAddressRef::BinaryDirect {
                    version: 1,
                    source: address.source_locator(),
                    path_id,
                }
            }
        }
        unity_asset_core::ObjectKind::Yaml => {
            let selector = match address.yaml_selector() {
                Some(YamlDocumentSelector::FileId { file_id }) => LegacyYamlSelector::Anchored {
                    anchor: file_id.to_string(),
                },
                Some(YamlDocumentSelector::Unanchored { document_index }) => {
                    LegacyYamlSelector::Unanchored {
                        document_index: *document_index,
                    }
                }
                None => return Err(BudgetError::ArithmeticOverflow { resource: "bytes" }.into()),
            };
            LegacyObjectAddressRef::Yaml {
                version: 1,
                source: address.source_locator(),
                selector,
            }
        }
    };
    streaming_json_digest(&wire, "legacy query object key")
}

fn reference_object_key_budgeted(
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let digest = streaming_json_digest(address, "object reference key")?;
    digest_key("object-v2:", digest, "object reference key", budget)
}

pub(crate) fn reference_guid_key(guid: &str, file_id: Option<i64>) -> String {
    let guid = guid.trim().to_ascii_lowercase();
    match file_id {
        Some(file_id) => format!("guid:{guid}:{file_id}"),
        None => format!("guid:{guid}"),
    }
}

fn reference_guid_key_budgeted(
    guid: &str,
    file_id: Option<i64>,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let guid = guid.trim();
    let suffix_len = file_id.map_or(0, |file_id| 1 + decimal_i64_len(file_id));
    let capacity = 5_usize
        .checked_add(guid.len())
        .and_then(|length| length.checked_add(suffix_len))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut key = reserve_string(capacity, "reference GUID key", budget)?;
    key.push_str("guid:");
    for character in guid.chars() {
        key.push(character.to_ascii_lowercase());
    }
    if let Some(file_id) = file_id {
        key.push(':');
        push_i64_decimal(&mut key, file_id);
    }
    Ok(key)
}

fn stable_id_budgeted(
    guid: Option<&str>,
    path: &str,
    file_id: Option<i64>,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let guid_hex_len = guid.map_or(0, |guid| {
        guid.bytes().filter(|byte| byte.is_ascii_hexdigit()).count()
    });
    let (prefix, identity_len) = if guid_hex_len == 0 {
        ("path:", path.len())
    } else {
        ("guid:", guid_hex_len)
    };
    let suffix_len = file_id.map_or(0, |file_id| 1 + decimal_i64_len(file_id));
    let capacity = prefix
        .len()
        .checked_add(identity_len)
        .and_then(|length| length.checked_add(suffix_len))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut stable = reserve_string(capacity, "search stable ID", budget)?;
    stable.push_str(prefix);
    if prefix == "guid:" {
        if let Some(guid) = guid {
            for byte in guid.bytes() {
                if byte.is_ascii_hexdigit() {
                    stable.push(char::from(byte.to_ascii_lowercase()));
                }
            }
        }
    } else {
        stable.push_str(path);
    }
    if let Some(file_id) = file_id {
        stable.push('#');
        push_i64_decimal(&mut stable, file_id);
    }
    Ok(stable)
}

fn container_stable_id(
    asset: &AssetAnalysis,
    entry: &ContainerEntryFact,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let digest = streaming_json_digest(
        &(
            asset.source.guid.as_deref(),
            &asset.source.relative_path,
            &entry.asset_path,
            entry.file_id,
            entry.path_id,
        ),
        "container stable ID",
    )?;
    digest_key("container-v1:", digest, "container stable ID", budget)
}

fn incoming_key_capacity(fact: &ReferenceProjectionFact) -> Result<usize, ProjectionError> {
    let dependency_keys = fact
        .dependency_keys
        .iter()
        .try_fold(0_usize, |total, dependency| {
            let additional = match dependency {
                ReferenceDependencyKey::Guid { file_id, .. } => 1 + usize::from(file_id.is_some()),
                ReferenceDependencyKey::Object { .. } | ReferenceDependencyKey::Source { .. } => 1,
            };
            checked_add_usize(total, additional, "members")
        })?;
    let raw_keys = match &fact.raw_target {
        RawReferenceProjection::Binary { external, .. } => {
            1 + usize::from(external.as_ref().and_then(|value| value.guid).is_some())
        }
        RawReferenceProjection::Yaml { file_id, guid, .. } => {
            usize::from(guid.is_some()) * (1 + usize::from(file_id.is_some()))
        }
    };
    checked_add_usize(dependency_keys, raw_keys, "members").map_err(Into::into)
}

fn clone_reference_fact(
    fact: &ReferenceProjectionFact,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceProjectionFact, ProjectionError> {
    Ok(ReferenceProjectionFact {
        source_object: fact
            .source_object
            .as_ref()
            .map(|address| clone_object_address(address, "reference fact source object", budget))
            .transpose()?,
        source_file_id: fact.source_file_id,
        source_class_id: fact.source_class_id,
        field_path: clone_field_path(&fact.field_path, "reference fact field path", budget)?,
        raw_target: clone_raw_reference(&fact.raw_target, budget)?,
        resolution: clone_reference_resolution(&fact.resolution, budget)?,
        diagnostics: clone_diagnostics(&fact.diagnostics, "reference fact diagnostics", budget)?,
        dependency_keys: clone_dependency_keys(&fact.dependency_keys, budget)?,
    })
}

fn clone_raw_reference(
    target: &RawReferenceProjection,
    budget: &mut AssetLoadBudget,
) -> Result<RawReferenceProjection, ProjectionError> {
    Ok(match target {
        RawReferenceProjection::Binary {
            file_id,
            path_id,
            external,
        } => RawReferenceProjection::Binary {
            file_id: *file_id,
            path_id: *path_id,
            external: external
                .as_ref()
                .map(|external| {
                    Ok::<_, ProjectionError>(BinaryExternalProjection {
                        index: external.index,
                        guid: external.guid,
                        type_id: external.type_id,
                        path: clone_string(
                            &external.path,
                            "binary external reference path",
                            budget,
                        )?,
                    })
                })
                .transpose()?,
        },
        RawReferenceProjection::Yaml {
            file_id,
            guid,
            type_id,
        } => {
            RawReferenceProjection::Yaml {
                file_id: *file_id,
                guid: guid
                    .as_ref()
                    .map(|guid| {
                        Ok::<_, ProjectionError>(match guid {
                            GuidProjection::Parsed(bytes) => GuidProjection::Parsed(*bytes),
                            GuidProjection::Invalid(value) => GuidProjection::Invalid(
                                clone_string(value, "invalid reference GUID", budget)?,
                            ),
                        })
                    })
                    .transpose()?,
                type_id: *type_id,
            }
        }
    })
}

fn clone_reference_resolution(
    resolution: &ReferenceResolutionProjection,
    budget: &mut AssetLoadBudget,
) -> Result<ReferenceResolutionProjection, ProjectionError> {
    Ok(match resolution {
        ReferenceResolutionProjection::Null => ReferenceResolutionProjection::Null,
        ReferenceResolutionProjection::Resolved { target } => {
            ReferenceResolutionProjection::Resolved {
                target: clone_object_address(target, "resolved reference target", budget)?,
            }
        }
        ReferenceResolutionProjection::Unloaded { source } => {
            ReferenceResolutionProjection::Unloaded {
                source: source
                    .as_ref()
                    .map(|locator| {
                        clone_source_locator(locator, "unloaded reference source", budget)
                    })
                    .transpose()?,
            }
        }
        ReferenceResolutionProjection::Missing { target } => {
            ReferenceResolutionProjection::Missing {
                target: target
                    .as_ref()
                    .map(|address| {
                        clone_object_address(address, "missing reference target", budget)
                    })
                    .transpose()?,
            }
        }
        ReferenceResolutionProjection::Ambiguous { candidates } => {
            let mut cloned =
                reserve_retained_vec(candidates.len(), "ambiguous reference targets", budget)?;
            for candidate in candidates {
                cloned.push(clone_object_address(
                    candidate,
                    "ambiguous reference target",
                    budget,
                )?);
            }
            ReferenceResolutionProjection::Ambiguous { candidates: cloned }
        }
        ReferenceResolutionProjection::Invalid => ReferenceResolutionProjection::Invalid,
    })
}

fn clone_dependency_keys(
    keys: &[ReferenceDependencyKey],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ReferenceDependencyKey>, ProjectionError> {
    let mut cloned = reserve_retained_vec(keys.len(), "reference dependency keys", budget)?;
    for key in keys {
        cloned.push(match key {
            ReferenceDependencyKey::Guid { guid, file_id } => ReferenceDependencyKey::Guid {
                guid: clone_string(guid, "reference dependency GUID", budget)?,
                file_id: *file_id,
            },
            ReferenceDependencyKey::Object { address } => ReferenceDependencyKey::Object {
                address: clone_object_address(address, "reference dependency object", budget)?,
            },
            ReferenceDependencyKey::Source { locator } => ReferenceDependencyKey::Source {
                locator: clone_source_locator(locator, "reference dependency source", budget)?,
            },
        });
    }
    Ok(cloned)
}

fn clone_diagnostics(
    diagnostics: &[Diagnostic],
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<Diagnostic>, ProjectionError> {
    let mut cloned = reserve_retained_vec(diagnostics.len(), resource, budget)?;
    for diagnostic in diagnostics {
        cloned.push(clone_diagnostic(diagnostic, budget)?);
    }
    Ok(cloned)
}

fn clone_diagnostic(
    diagnostic: &Diagnostic,
    budget: &mut AssetLoadBudget,
) -> Result<Diagnostic, ProjectionError> {
    let code = clone_string(diagnostic.code(), "diagnostic code", budget)?;
    let message = clone_string(diagnostic.message(), "diagnostic message", budget)?;
    let address = diagnostic
        .address()
        .map(|address| clone_object_address(address, "diagnostic object address", budget))
        .transpose()?;
    let field_path = diagnostic
        .field_path()
        .map(|path| clone_field_path(path, "diagnostic field path", budget))
        .transpose()?;
    let mut cloned = Diagnostic::new(diagnostic.severity(), code, message)?;
    if let Some(address) = address {
        cloned = cloned.at_address(address);
    }
    if let Some(field_path) = field_path {
        cloned = cloned.at_field(field_path);
    }
    Ok(cloned)
}

fn clone_field_path(
    path: &FieldPath,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, ProjectionError> {
    let mut segments = reserve_retained_vec(path.segments().len(), resource, budget)?;
    for segment in path.segments() {
        segments.push(match segment {
            FieldPathSegment::Field(name) => {
                FieldPathSegment::field(clone_string(name, resource, budget)?)?
            }
            FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    Ok(FieldPath::from_segments(segments)?)
}

fn clone_object_address(
    address: &ObjectAddress,
    _resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, ProjectionError> {
    let members = address.source_locator().members().len();
    let bytes = address
        .retained_clone_bytes()
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    charge_foreign_clone(members, bytes, budget)?;
    Ok(address.clone())
}

fn clone_source_locator(
    locator: &SourceLocator,
    _resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, ProjectionError> {
    let bytes = locator
        .retained_clone_bytes()
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    charge_foreign_clone(locator.members().len(), bytes, budget)?;
    Ok(locator.clone())
}

fn charge_foreign_clone(
    member_count: usize,
    bytes: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    let members = usize_to_u64(member_count, "members")?;
    let bytes = usize_to_u64(bytes, "bytes")?;
    budget.check_entries(members)?;
    budget.check_members(members)?;
    budget.check_bytes(bytes)?;
    budget.consume_entries(members)?;
    budget.consume_members(members)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn clone_strings(
    values: &[String],
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<String>, ProjectionError> {
    let mut cloned = reserve_retained_vec(values.len(), resource, budget)?;
    for value in values {
        cloned.push(clone_string(value, resource, budget)?);
    }
    Ok(cloned)
}

fn clone_optional_string(
    value: Option<&str>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, ProjectionError> {
    value
        .map(|value| clone_string(value, resource, budget))
        .transpose()
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let mut cloned = reserve_string(value.len(), resource, budget)?;
    cloned.push_str(value);
    Ok(cloned)
}

fn budgeted_terms(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    try_to_terms(value, |requested| {
        let requested = string_bytes(requested)?;
        budget.check_bytes(requested)?;
        budget.consume_bytes(requested)?;
        Ok(())
    })
    .map_err(|error| match error {
        TryToTermsError::ReserveHook { source, .. } => source,
        TryToTermsError::Allocation { requested, source } => ProjectionError::Allocation {
            resource,
            requested,
            unit: "bytes",
            source,
        },
    })
}

fn reserve_retained_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ProjectionError> {
    let count = usize_to_u64(capacity, "entries")?;
    let planned_bytes = vec_bytes::<T>(capacity)?;
    budget.check_entries(count)?;
    budget.check_members(count)?;
    budget.check_bytes(planned_bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| ProjectionError::Allocation {
            resource,
            requested: capacity,
            unit: "elements",
            source,
        })?;
    budget.consume_entries(count)?;
    budget.consume_members(count)?;
    budget.consume_bytes(planned_bytes)?;
    Ok(values)
}

fn reserve_entry_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ProjectionError> {
    let entries = usize_to_u64(capacity, "entries")?;
    let planned_bytes = vec_bytes::<T>(capacity)?;
    budget.check_entries(entries)?;
    budget.check_bytes(planned_bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| ProjectionError::Allocation {
            resource,
            requested: capacity,
            unit: "elements",
            source,
        })?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(planned_bytes)?;
    Ok(values)
}

fn reserve_string(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let planned_bytes = string_bytes(capacity)?;
    budget.check_bytes(planned_bytes)?;
    let mut value = String::new();
    value
        .try_reserve_exact(capacity)
        .map_err(|source| ProjectionError::Allocation {
            resource,
            requested: capacity,
            unit: "bytes",
            source,
        })?;
    budget.consume_bytes(planned_bytes)?;
    Ok(value)
}

fn reserve_string_capacity(
    value: &mut String,
    required: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    if required <= value.capacity() {
        return Ok(());
    }
    let requested_bytes = string_bytes(required)?;
    budget.check_bytes(requested_bytes)?;
    let additional = required
        .checked_sub(value.len())
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    value
        .try_reserve_exact(additional)
        .map_err(|source| ProjectionError::Allocation {
            resource,
            requested: required,
            unit: "bytes",
            source,
        })?;
    budget.consume_bytes(requested_bytes)?;
    Ok(())
}

fn digest_key(
    prefix: &str,
    digest: DigestV1,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let capacity = prefix
        .len()
        .checked_add(DigestV1::BYTE_LEN * 2)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut key = reserve_string(capacity, resource, budget)?;
    key.push_str(prefix);
    push_hex(&mut key, digest.as_bytes());
    Ok(key)
}

fn digest_key_unbudgeted(prefix: &str, digest: DigestV1) -> String {
    let mut key = String::with_capacity(prefix.len() + DigestV1::BYTE_LEN * 2);
    key.push_str(prefix);
    push_hex(&mut key, digest.as_bytes());
    key
}

fn encode_hex_budgeted(
    bytes: &[u8],
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let capacity = bytes
        .len()
        .checked_mul(2)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut encoded = reserve_string(capacity, resource, budget)?;
    push_hex(&mut encoded, bytes);
    Ok(encoded)
}

fn push_hex(destination: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        destination.push(char::from(DIGITS[usize::from(byte >> 4)]));
        destination.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
}

fn source_file_key(
    path: &str,
    file_id: i64,
    budget: &mut AssetLoadBudget,
) -> Result<String, ProjectionError> {
    let capacity = "source-file:"
        .len()
        .checked_add(path.len())
        .and_then(|length| length.checked_add(1 + decimal_i64_len(file_id)))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut key = reserve_string(capacity, "source file reference key", budget)?;
    key.push_str("source-file:");
    key.push_str(path);
    key.push(':');
    push_i64_decimal(&mut key, file_id);
    Ok(key)
}

fn binary_path_key(path_id: i64, budget: &mut AssetLoadBudget) -> Result<String, ProjectionError> {
    let capacity = "binary-path:"
        .len()
        .checked_add(decimal_i64_len(path_id))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let mut key = reserve_string(capacity, "binary path reference key", budget)?;
    key.push_str("binary-path:");
    push_i64_decimal(&mut key, path_id);
    Ok(key)
}

fn decimal_i64_len(value: i64) -> usize {
    usize::from(value.is_negative()) + decimal_u64_len(value.unsigned_abs())
}

fn decimal_u64_len(mut value: u64) -> usize {
    let mut length = 1;
    while value >= 10 {
        length += 1;
        value /= 10;
    }
    length
}

fn push_i64_decimal(destination: &mut String, value: i64) {
    if value.is_negative() {
        destination.push('-');
    }
    push_u64_decimal(destination, value.unsigned_abs());
}

fn push_u64_decimal(destination: &mut String, mut value: u64) {
    let mut digits = [0_u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for digit in &digits[start..] {
        destination.push(char::from(*digit));
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
    overflowed: bool,
}

impl io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Ok(length) = u64::try_from(buffer.len()) else {
            self.overflowed = true;
            return Err(io::Error::other("projection identity length overflow"));
        };
        let Some(bytes) = self.bytes.checked_add(length) else {
            self.overflowed = true;
            return Err(io::Error::other("projection identity length overflow"));
        };
        self.bytes = bytes;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'builder> {
    builder: &'builder mut DigestV1Builder,
    digest_error: Option<DigestBuildError>,
}

impl io::Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.builder.update(buffer) {
            Ok(()) => Ok(buffer.len()),
            Err(error) => {
                self.digest_error = Some(error);
                Err(io::Error::other("projection digest writer rejected bytes"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn streaming_json_digest<T: Serialize + ?Sized>(
    value: &T,
    resource: &'static str,
) -> Result<DigestV1, ProjectionError> {
    let mut counter = CountingWriter::default();
    if let Err(source) = serde_json::to_writer(&mut counter, value) {
        if counter.overflowed {
            return Err(BudgetError::ArithmeticOverflow { resource: "bytes" }.into());
        }
        return Err(ProjectionError::IdentitySerialization { resource, source });
    }

    let mut builder = DigestV1Builder::new(counter.bytes);
    let mut writer = DigestWriter {
        builder: &mut builder,
        digest_error: None,
    };
    let serialized = serde_json::to_writer(&mut writer, value);
    if let Some(source) = writer.digest_error {
        return Err(ProjectionError::IdentityDigest { resource, source });
    }
    serialized.map_err(|source| ProjectionError::IdentitySerialization { resource, source })?;
    builder
        .finalize()
        .map_err(|source| ProjectionError::IdentityDigest { resource, source })
}

fn checked_add_usize(
    left: usize,
    right: usize,
    resource: &'static str,
) -> Result<usize, BudgetError> {
    left.checked_add(right)
        .ok_or(BudgetError::ArithmeticOverflow { resource })
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

fn vec_bytes<T>(capacity: usize) -> Result<u64, ProjectionError> {
    vec_allocation_bytes::<T>(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn string_bytes(capacity: usize) -> Result<u64, ProjectionError> {
    string_allocation_bytes(capacity)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn record_truncation(
    truncations: &mut Vec<ProjectionTruncation>,
    omitted_metric: &mut u64,
    source_path: &str,
    category: ProjectionCategory,
    emitted: usize,
    total: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), ProjectionError> {
    let omitted = total.saturating_sub(emitted);
    if omitted == 0 {
        return Ok(());
    }
    let omitted = omitted.try_into().unwrap_or(u64::MAX);
    *omitted_metric = omitted_metric.saturating_add(omitted);
    truncations.push(ProjectionTruncation {
        source_path: clone_string(source_path, "projection truncation source path", budget)?,
        category,
        emitted: emitted.try_into().unwrap_or(u64::MAX),
        omitted,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{AnalysisMetrics, AnalyzedSource, SearchFacts};
    use unity_asset_core::{
        AssetLoadLimits, AssetLoadUsage, DiagnosticSeverity, WorkspaceId, WorkspaceRevision,
        YamlFileId,
    };

    fn analyzed_asset(search: SearchFacts) -> AssetAnalysis {
        AssetAnalysis::new(
            AnalyzedSource {
                relative_path: "Assets/Player.prefab".to_owned(),
                content_digest: DigestV1::hash_bytes(b"player"),
                length: 6,
                search_kind: SearchKind::Prefab,
                guid: Some("prefab-guid".to_owned()),
                workspace_source: None,
                workspace_fingerprint: None,
                locator: None,
            },
            search,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
        )
    }

    #[test]
    fn search_projection_indexes_persisted_hierarchy_and_resolved_script_symbols() {
        let asset = analyzed_asset(SearchFacts {
            display_name: "Player".to_owned(),
            path_terms: "assets player prefab".to_owned(),
            name_terms: "player".to_owned(),
            content_terms: "game object".to_owned(),
            hierarchy_paths: vec!["Root/Playable Character".to_owned()],
            script_symbols: Vec::new(),
            referenced_script_guids: vec!["script-guid".to_owned()],
        });
        let symbols = ["PlayerController".to_owned()];
        let scripts = [ScriptSymbolsByGuid {
            guid: "script-guid",
            symbols: symbols.as_slice(),
            asset_ordinal: 0,
        }];
        let mut budget = AssetLoadBudget::default();

        let document = project_search_document(&asset, &scripts, &mut budget).unwrap();

        assert_eq!(
            document.hierarchy_paths,
            vec!["Root/Playable Character".to_owned()]
        );
        assert_eq!(document.script_symbols, vec!["PlayerController".to_owned()]);
        assert!(document.content_terms.contains("root playable character"));
        assert!(document.content_terms.contains("player controller"));
    }

    #[test]
    fn identical_container_entries_from_different_bundles_have_distinct_ids() {
        let entry = ContainerEntryFact {
            asset_path: "Assets/Shared.prefab".to_owned(),
            file_id: -3,
            path_id: -99,
        };
        let first = analyzed_asset(SearchFacts::default());
        let mut second = first.clone();
        second.source.relative_path = "Bundles/Other.bundle".to_owned();
        second.source.guid = Some("other-bundle-guid".to_owned());
        let mut budget = AssetLoadBudget::default();

        assert_ne!(
            container_stable_id(&first, &entry, &mut budget).unwrap(),
            container_stable_id(&second, &entry, &mut budget).unwrap()
        );
    }

    #[test]
    fn streaming_container_identity_preserves_the_existing_json_digest_domain() {
        let entry = ContainerEntryFact {
            asset_path: "Assets/Shared.prefab".to_owned(),
            file_id: -3,
            path_id: -99,
        };
        let asset = analyzed_asset(SearchFacts::default());
        let identity = serde_json::to_vec(&(
            asset.source.guid.as_deref(),
            &asset.source.relative_path,
            &entry.asset_path,
            entry.file_id,
            entry.path_id,
        ))
        .unwrap();
        let expected = format!(
            "container-v1:{}",
            hex::encode(DigestV1::hash_bytes(&identity).as_bytes())
        );
        let mut budget = AssetLoadBudget::default();

        assert_eq!(
            container_stable_id(&asset, &entry, &mut budget).unwrap(),
            expected
        );
    }

    #[test]
    fn streaming_reference_identity_uses_the_v2_prefix_and_exact_json_digest() {
        let asset = analyzed_asset(SearchFacts::default());
        let fact = ReferenceProjectionFact {
            source_object: None,
            source_file_id: Some(-1),
            source_class_id: Some(114),
            field_path: FieldPath::root(),
            raw_target: RawReferenceProjection::Yaml {
                file_id: Some(i64::MAX),
                guid: Some(GuidProjection::Parsed([0xab; 16])),
                type_id: Some(3),
            },
            resolution: ReferenceResolutionProjection::Invalid,
            diagnostics: Vec::new(),
            dependency_keys: Vec::new(),
        };
        let ordinal = 7;
        let identity = serde_json::to_vec(&(
            &asset.source.relative_path,
            fact.source_object.as_ref(),
            fact.source_file_id,
            &fact.field_path,
            &fact.raw_target,
            ordinal,
        ))
        .unwrap();
        let expected = format!(
            "reference-v2:{}",
            hex::encode(DigestV1::hash_bytes(&identity).as_bytes())
        );
        let mut budget = AssetLoadBudget::default();

        assert_eq!(
            reference_stable_id(&asset, &fact, ordinal, &mut budget).unwrap(),
            expected
        );
    }

    #[test]
    fn reference_object_keys_distinguish_equal_numeric_file_ids_and_ordinals() {
        let locator = SourceLocator::path("Assets/Scene.unity").unwrap();
        let file_id = ObjectAddress::yaml(locator.clone(), YamlFileId::new(1).unwrap()).unwrap();
        let ordinal = ObjectAddress::yaml_document(locator, 1).unwrap();

        assert_ne!(
            reference_object_key(&file_id),
            reference_object_key(&ordinal)
        );
    }

    #[test]
    fn reference_integer_keys_preserve_signed_decimal_boundaries() {
        let cases = [
            (i64::MIN, "-9223372036854775808"),
            (i64::MAX, "9223372036854775807"),
            (-1, "-1"),
            (0, "0"),
        ];
        let mut budget = AssetLoadBudget::default();

        for (value, decimal) in cases {
            assert_eq!(
                source_file_key("Assets/Player.prefab", value, &mut budget).unwrap(),
                format!("source-file:Assets/Player.prefab:{decimal}")
            );
            assert_eq!(
                binary_path_key(value, &mut budget).unwrap(),
                format!("binary-path:{decimal}")
            );
            assert_eq!(
                reference_guid_key_budgeted(" AAbb ", Some(value), &mut budget).unwrap(),
                format!("guid:aabb:{decimal}")
            );
            assert_eq!(
                reference_guid_key(" AAbb ", Some(value)),
                format!("guid:aabb:{decimal}")
            );
        }
    }

    #[test]
    fn stable_id_filters_guid_spelling_and_falls_back_when_no_hex_remains() {
        let mut budget = AssetLoadBudget::default();

        assert_eq!(
            stable_id_budgeted(
                Some("AA-BB_cc:00 / zz"),
                "Assets/Fallback.prefab",
                Some(-7),
                &mut budget,
            )
            .unwrap(),
            "guid:aabbcc00#-7"
        );
        assert_eq!(
            stable_id_budgeted(
                Some(" \t-_:/\r\n"),
                "Assets/Fallback.prefab",
                None,
                &mut budget,
            )
            .unwrap(),
            "path:Assets/Fallback.prefab"
        );
    }

    #[test]
    fn stable_id_budget_uses_the_filtered_guid_layout() {
        let guid = "AA-BB_cc:00 / zz";
        let expected = "guid:aabbcc00#-7";
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(expected.len()).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert_eq!(
            stable_id_budgeted(Some(guid), "Assets/Fallback.prefab", Some(-7), &mut exact).unwrap(),
            expected
        );
        assert_eq!(exact.usage().bytes, u64::try_from(expected.len()).unwrap());

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(expected.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            stable_id_budgeted(
                Some(guid),
                "Assets/Fallback.prefab",
                Some(-7),
                &mut short,
            ),
            Err(ProjectionError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit + 1 == requested
        ));
        assert_eq!(short.usage().bytes, 0);
    }

    #[test]
    fn normalized_terms_use_exact_requested_layout_budget() {
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 6,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert_eq!(
            budgeted_terms("Simple", "test normalized terms", &mut exact).unwrap(),
            "simple"
        );
        assert_eq!(exact.usage().bytes, 6);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 5,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            budgeted_terms("Simple", "test normalized terms", &mut short),
            Err(ProjectionError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 5,
                requested: 6,
            }))
        ));
        assert_eq!(short.usage().bytes, 0);
    }

    #[test]
    fn entry_limit_rejects_the_top_level_projection_before_usage_is_charged() {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let result = project_batch(
            &batch_with_diagnostic(),
            unconstrained_projection_limits(),
            &mut budget,
        );

        let error = result.unwrap_err();
        assert!(matches!(
            &error,
            ProjectionError::Budget(BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<BudgetError>())
                .is_some()
        );
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn member_limit_rejects_the_top_level_projection_before_usage_is_charged() {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let result = project_batch(
            &batch_with_diagnostic(),
            unconstrained_projection_limits(),
            &mut budget,
        );

        assert!(matches!(
            result,
            Err(ProjectionError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            }))
        ));
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn byte_limit_rejects_the_top_level_projection_before_usage_is_charged() {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let result = project_batch(
            &batch_with_diagnostic(),
            unconstrained_projection_limits(),
            &mut budget,
        );

        assert!(matches!(
            result,
            Err(ProjectionError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 1,
                requested,
            })) if requested > 1
        ));
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    fn batch_with_diagnostic() -> AssetAnalysisBatch {
        let diagnostic = Diagnostic::new(
            DiagnosticSeverity::Warning,
            "PROJECTION_TEST",
            "projection budget test",
        )
        .unwrap();
        let asset = AssetAnalysis::new(
            AnalyzedSource {
                relative_path: "Assets/Budget.prefab".to_owned(),
                content_digest: DigestV1::hash_bytes(b"budget"),
                length: 6,
                search_kind: SearchKind::Prefab,
                guid: Some("budget-guid".to_owned()),
                workspace_source: None,
                workspace_fingerprint: None,
                locator: None,
            },
            SearchFacts::default(),
            Vec::new(),
            Vec::new(),
            vec![diagnostic],
            true,
        );
        AssetAnalysisBatch::new(
            WorkspaceId::from_u128(1).unwrap(),
            WorkspaceRevision::new(DigestV1::hash_bytes(b"revision")),
            Vec::new(),
            vec![asset],
            AnalysisMetrics::default(),
        )
    }

    const fn unconstrained_projection_limits() -> ProjectionLimits {
        ProjectionLimits {
            max_references_per_asset: usize::MAX,
            max_container_entries_per_asset: usize::MAX,
        }
    }
}
