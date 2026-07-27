use std::io::{Read, Write};
use std::mem::size_of;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use unity_asset_binary::asset::class_ids;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, Diagnostic, FieldPath, FieldPathSegment,
    ObjectAddress, SourceLocator, UnityValue, WorkspaceId, WorkspaceRevision,
};

use super::json_contract::{large_contract_limits, read_json_bounded, small_contract_limits};
use super::manifest::{ExtractionCanonicalError, canonical_json, write_canonical_json};
use super::selection::ExtractionPlanError;
use crate::reference::{RawReferenceTarget, ReferenceFact, ReferenceGraph, ReferenceResolution};
use crate::workspace::{WorkspaceObject, WorkspaceObjectValue, WorkspaceView};

pub const BUNDLE_CONTAINER_QUERY_CONTRACT: &str = "unity_asset.bundle_container_query";
pub const BUNDLE_CONTAINER_QUERY_VERSION: u8 = 1;
pub const BUNDLE_CONTAINER_RESULT_CONTRACT: &str = "unity_asset.bundle_container_result";
pub const BUNDLE_CONTAINER_RESULT_VERSION: u8 = 1;

const BUNDLE_CONTAINER_QUERY_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    small_contract_limits(BUNDLE_CONTAINER_QUERY_CONTRACT);
const BUNDLE_CONTAINER_RESULT_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(BUNDLE_CONTAINER_RESULT_CONTRACT);

const MAX_PATTERN_BYTES: usize = 4_096;
const MAX_UNBUDGETED_PATTERN_MATCH_WORK: u64 = 16 * 1024 * 1024;

/// A validated, versioned query over `AssetBundle.m_Container` occurrences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleContainerQuery {
    pattern: String,
}

impl BundleContainerQuery {
    pub fn new(pattern: impl Into<String>) -> Result<Self, BundleContainerContractError> {
        let pattern = normalize_pattern(pattern.into())?;
        Ok(Self { pattern })
    }

    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget, BUNDLE_CONTAINER_QUERY_JSON_LIMITS)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ExtractionCanonicalError> {
        canonical_json(self)
    }

    pub fn write_canonical_json(&self, writer: impl Write) -> Result<(), ExtractionCanonicalError> {
        write_canonical_json(writer, self)
    }
}

#[derive(Serialize)]
struct BundleContainerQueryRef<'query> {
    contract: &'static str,
    version: u8,
    pattern: &'query str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleContainerQueryWire {
    contract: String,
    version: u8,
    pattern: String,
}

impl Serialize for BundleContainerQuery {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        BundleContainerQueryRef {
            contract: BUNDLE_CONTAINER_QUERY_CONTRACT,
            version: BUNDLE_CONTAINER_QUERY_VERSION,
            pattern: &self.pattern,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BundleContainerQuery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BundleContainerQueryWire::deserialize(deserializer)?;
        if wire.contract != BUNDLE_CONTAINER_QUERY_CONTRACT {
            return Err(serde::de::Error::custom(
                BundleContainerContractError::UnexpectedQueryContract {
                    expected: BUNDLE_CONTAINER_QUERY_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != BUNDLE_CONTAINER_QUERY_VERSION {
            return Err(serde::de::Error::custom(
                BundleContainerContractError::UnsupportedQueryVersion(wire.version),
            ));
        }
        Self::new(wire.pattern).map_err(serde::de::Error::custom)
    }
}

/// Format-faithful pointer retained for one container occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleContainerRawTarget {
    file_id: i32,
    path_id: i64,
}

impl BundleContainerRawTarget {
    #[must_use]
    pub const fn file_id(self) -> i32 {
        self.file_id
    }

    #[must_use]
    pub const fn path_id(self) -> i64 {
        self.path_id
    }
}

/// Resolution of one exact `m_Container` pointer against the queried workspace revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BundleContainerResolution {
    Null,
    Resolved { target: ObjectAddress },
    Unloaded { source: Option<SourceLocator> },
    Missing { target: Option<ObjectAddress> },
    Ambiguous { candidates: Box<[ObjectAddress]> },
    Invalid { diagnostic: Diagnostic },
}

impl BundleContainerResolution {
    #[must_use]
    pub const fn resolved(&self) -> Option<&ObjectAddress> {
        match self {
            Self::Resolved { target } => Some(target),
            Self::Null
            | Self::Unloaded { .. }
            | Self::Missing { .. }
            | Self::Ambiguous { .. }
            | Self::Invalid { .. } => None,
        }
    }
}

/// One occurrence in the original container order, without target-level deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleContainerOccurrence {
    ordinal: u32,
    owner: ObjectAddress,
    field_path: FieldPath,
    asset_path: String,
    raw_target: BundleContainerRawTarget,
    resolution: BundleContainerResolution,
    diagnostics: Box<[Diagnostic]>,
}

impl BundleContainerOccurrence {
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    #[must_use]
    pub const fn owner(&self) -> &ObjectAddress {
        &self.owner
    }

    #[must_use]
    pub const fn field_path(&self) -> &FieldPath {
        &self.field_path
    }

    #[must_use]
    pub fn asset_path(&self) -> &str {
        &self.asset_path
    }

    #[must_use]
    pub const fn raw_target(&self) -> BundleContainerRawTarget {
        self.raw_target
    }

    #[must_use]
    pub const fn resolution(&self) -> &BundleContainerResolution {
        &self.resolution
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// Deterministic, revision-bound result for one container query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleContainerResult {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    query: BundleContainerQuery,
    complete: bool,
    occurrences: Box<[BundleContainerOccurrence]>,
}

impl BundleContainerResult {
    fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        query: BundleContainerQuery,
        complete: bool,
        occurrences: Vec<BundleContainerOccurrence>,
    ) -> Result<Self, BundleContainerContractError> {
        validate_occurrences(&query, &occurrences)?;
        Ok(Self {
            workspace,
            revision,
            query,
            complete,
            occurrences: occurrences.into_boxed_slice(),
        })
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn query(&self) -> &BundleContainerQuery {
        &self.query
    }

    /// Whether the supplied reference graph covered every workspace object and occurrence.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn occurrences(&self) -> &[BundleContainerOccurrence] {
        &self.occurrences
    }

    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_json_bounded(reader, budget, BUNDLE_CONTAINER_RESULT_JSON_LIMITS)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ExtractionCanonicalError> {
        canonical_json(self)
    }

    pub fn write_canonical_json(&self, writer: impl Write) -> Result<(), ExtractionCanonicalError> {
        write_canonical_json(writer, self)
    }
}

#[derive(Serialize)]
struct BundleContainerResultRef<'result> {
    contract: &'static str,
    version: u8,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    query: &'result BundleContainerQuery,
    complete: bool,
    occurrences: &'result [BundleContainerOccurrence],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleContainerResultWire {
    contract: String,
    version: u8,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    query: BundleContainerQuery,
    complete: bool,
    occurrences: Vec<BundleContainerOccurrence>,
}

impl Serialize for BundleContainerResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        BundleContainerResultRef {
            contract: BUNDLE_CONTAINER_RESULT_CONTRACT,
            version: BUNDLE_CONTAINER_RESULT_VERSION,
            workspace: self.workspace,
            revision: self.revision,
            query: &self.query,
            complete: self.complete,
            occurrences: &self.occurrences,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BundleContainerResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BundleContainerResultWire::deserialize(deserializer)?;
        if wire.contract != BUNDLE_CONTAINER_RESULT_CONTRACT {
            return Err(serde::de::Error::custom(
                BundleContainerContractError::UnexpectedResultContract {
                    expected: BUNDLE_CONTAINER_RESULT_CONTRACT,
                    actual: wire.contract,
                },
            ));
        }
        if wire.version != BUNDLE_CONTAINER_RESULT_VERSION {
            return Err(serde::de::Error::custom(
                BundleContainerContractError::UnsupportedResultVersion(wire.version),
            ));
        }
        Self::new(
            wire.workspace,
            wire.revision,
            wire.query,
            wire.complete,
            wire.occurrences,
        )
        .map_err(serde::de::Error::custom)
    }
}

pub(super) fn query_bundle_container_occurrences(
    view: &dyn WorkspaceView,
    references: &ReferenceGraph,
    query: BundleContainerQuery,
    budget: &mut AssetLoadBudget,
) -> Result<BundleContainerResult, ExtractionPlanError> {
    let mut occurrences = Vec::new();
    let mut current_owner: Option<(WorkspaceObject, Option<ObjectAddress>)> = None;
    for fact in references.facts() {
        budget.consume_members(1)?;
        let Some(ordinal) = container_ordinal(fact.field_path()) else {
            continue;
        };
        if current_owner.as_ref().map(|(object, _)| object.handle()) != Some(fact.source()) {
            let object = view.read_object(fact.source(), budget)?;
            let is_asset_bundle = matches!(
                object.value(),
                WorkspaceObjectValue::Binary(binary)
                    if binary.class_id() == class_ids::ASSET_BUNDLE
            );
            let owner = is_asset_bundle
                .then(|| view.object_address(fact.source(), budget))
                .transpose()?;
            current_owner = Some((object, owner));
        }
        let Some((object, Some(owner))) = current_owner.as_ref() else {
            continue;
        };
        let WorkspaceObjectValue::Binary(binary) = object.value() else {
            return Err(ExtractionPlanError::ReferenceInvariant(
                "cached AssetBundle owner changed object format",
            ));
        };
        let Some(UnityValue::Array(entries)) = binary.get("m_Container") else {
            continue;
        };
        let Some(entry) = usize::try_from(ordinal)
            .ok()
            .and_then(|ordinal| entries.get(ordinal))
        else {
            return Err(ExtractionPlanError::ReferenceInvariant(
                "container reference ordinal is outside the materialized m_Container",
            ));
        };
        let Some((asset_path, file_id, path_id)) = container_entry(entry) else {
            continue;
        };
        let RawReferenceTarget::Binary {
            file_id: fact_file_id,
            path_id: fact_path_id,
            ..
        } = fact.raw_target()
        else {
            continue;
        };
        if *fact_file_id != file_id || *fact_path_id != path_id {
            continue;
        }
        if !asset_path_matches_budgeted(query.pattern(), asset_path, budget)? {
            continue;
        }

        budget.check_entries(1)?;
        let occurrence = BundleContainerOccurrence {
            ordinal,
            owner: clone_object_address(owner, "bundle container owner", budget)?,
            field_path: clone_field_path(fact.field_path(), "bundle container field path", budget)?,
            asset_path: clone_string(asset_path, "bundle container asset path", budget)?,
            raw_target: BundleContainerRawTarget { file_id, path_id },
            resolution: clone_resolution(view, fact.resolution(), budget)?,
            diagnostics: clone_diagnostics(fact, budget)?.into_boxed_slice(),
        };
        push_value(
            &mut occurrences,
            occurrence,
            "bundle container occurrences",
            budget,
        )?;
        budget.consume_entries(1)?;
    }

    BundleContainerResult::new(
        view.workspace_id(),
        view.revision(),
        query,
        references.is_complete(),
        occurrences,
    )
    .map_err(ExtractionPlanError::ContainerContract)
}

pub(super) fn resolved_addresses(
    result: &BundleContainerResult,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ObjectAddress>, ExtractionPlanError> {
    let mut addresses = Vec::new();
    for occurrence in result.occurrences() {
        let Some(target) = occurrence.resolution().resolved() else {
            continue;
        };
        budget.check_entries(1)?;
        let target = clone_object_address(target, "bundle container resolved target", budget)?;
        push_value(
            &mut addresses,
            target,
            "bundle container resolved targets",
            budget,
        )?;
        budget.consume_entries(1)?;
    }
    Ok(addresses)
}

fn container_entry(value: &UnityValue) -> Option<(&str, i32, i64)> {
    match value {
        UnityValue::Array(pair) if pair.len() == 2 => {
            let asset_path = pair[0].as_str()?;
            let (file_id, path_id) = scan_pptr(&pair[1])?;
            Some((asset_path, file_id, path_id))
        }
        UnityValue::Object(pair) => {
            let asset_path = pair.get("first")?.as_str()?;
            let target = pair.get("second").or_else(|| pair.get("value"))?;
            let (file_id, path_id) = scan_pptr(target)?;
            Some((asset_path, file_id, path_id))
        }
        _ => None,
    }
}

fn scan_pptr(value: &UnityValue) -> Option<(i32, i64)> {
    match value {
        UnityValue::Object(object) => {
            let file_id = object
                .get("fileID")
                .or_else(|| object.get("m_FileID"))
                .and_then(UnityValue::as_i64)
                .and_then(|value| i32::try_from(value).ok());
            let path_id = object
                .get("pathID")
                .or_else(|| object.get("m_PathID"))
                .and_then(UnityValue::as_i64);
            match (file_id, path_id) {
                (Some(file_id), Some(path_id)) => Some((file_id, path_id)),
                _ => object.values().find_map(scan_pptr),
            }
        }
        UnityValue::Array(values) => values.iter().find_map(scan_pptr),
        _ => None,
    }
}

fn container_ordinal(path: &FieldPath) -> Option<u32> {
    match path.segments() {
        [
            FieldPathSegment::Field(field),
            FieldPathSegment::Index(ordinal),
            ..,
        ] if field == "m_Container" => Some(*ordinal),
        _ => None,
    }
}

fn clone_resolution(
    view: &dyn WorkspaceView,
    resolution: &ReferenceResolution,
    budget: &mut AssetLoadBudget,
) -> Result<BundleContainerResolution, ExtractionPlanError> {
    Ok(match resolution {
        ReferenceResolution::Null => BundleContainerResolution::Null,
        ReferenceResolution::Resolved(target) => BundleContainerResolution::Resolved {
            target: view.object_address(target, budget)?,
        },
        ReferenceResolution::Unloaded { source } => BundleContainerResolution::Unloaded {
            source: source
                .as_ref()
                .map(|source| {
                    clone_source_locator(source, "bundle container unloaded source", budget)
                })
                .transpose()?,
        },
        ReferenceResolution::Missing { target } => BundleContainerResolution::Missing {
            target: target
                .as_ref()
                .map(|target| {
                    clone_object_address(target, "bundle container missing target", budget)
                })
                .transpose()?,
        },
        ReferenceResolution::Ambiguous { candidates } => {
            let candidate_count =
                usize_to_u64(candidates.len(), "bundle container ambiguous target count")?;
            budget.check_entries(candidate_count)?;
            let mut cloned = reserve_vec(
                candidates.len(),
                "bundle container ambiguous targets",
                budget,
            )?;
            for candidate in candidates.iter() {
                cloned.push(clone_object_address(
                    candidate,
                    "bundle container ambiguous target",
                    budget,
                )?);
            }
            budget.consume_entries(candidate_count)?;
            BundleContainerResolution::Ambiguous {
                candidates: cloned.into_boxed_slice(),
            }
        }
        ReferenceResolution::Invalid { diagnostic } => BundleContainerResolution::Invalid {
            diagnostic: clone_diagnostic(
                diagnostic,
                "bundle container invalid resolution",
                budget,
            )?,
        },
    })
}

fn clone_diagnostics(
    fact: &ReferenceFact,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<Diagnostic>, ExtractionPlanError> {
    let diagnostic_count = usize_to_u64(
        fact.diagnostics().len(),
        "bundle container diagnostic count",
    )?;
    budget.check_entries(diagnostic_count)?;
    let mut diagnostics = reserve_vec(
        fact.diagnostics().len(),
        "bundle container diagnostics",
        budget,
    )?;
    for diagnostic in fact.diagnostics() {
        diagnostics.push(clone_diagnostic(
            diagnostic,
            "bundle container diagnostic",
            budget,
        )?);
    }
    budget.consume_entries(diagnostic_count)?;
    Ok(diagnostics)
}

fn clone_diagnostic(
    value: &Diagnostic,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Diagnostic, ExtractionPlanError> {
    let mut cloned = Diagnostic::new(
        value.severity(),
        clone_string(value.code(), resource, budget)?,
        clone_string(value.message(), resource, budget)?,
    )?;
    if let Some(address) = value.address() {
        cloned = cloned.at_address(clone_object_address(address, resource, budget)?);
    }
    if let Some(field_path) = value.field_path() {
        cloned = cloned.at_field(clone_field_path(field_path, resource, budget)?);
    }
    Ok(cloned)
}

fn clone_field_path(
    value: &FieldPath,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, ExtractionPlanError> {
    let mut segments = reserve_vec(value.segments().len(), resource, budget)?;
    for segment in value.segments() {
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
    value: &ObjectAddress,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn clone_source_locator(
    value: &SourceLocator,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, ExtractionPlanError> {
    let bytes = value
        .retained_clone_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let cloned = value.clone();
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ExtractionPlanError> {
    let bytes =
        u64::try_from(value.len()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: value.len(),
            source,
        })?;
    cloned.push_str(value);
    budget.consume_bytes(bytes)?;
    Ok(cloned)
}

fn reserve_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ExtractionPlanError> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource,
            requested: capacity,
            source,
        })?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, ExtractionPlanError> {
    u64::try_from(value)
        .map_err(|_| ExtractionPlanError::Budget(BudgetError::ArithmeticOverflow { resource }))
}

fn push_value<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExtractionPlanError> {
    if values.len() == values.capacity() {
        let additional = values.capacity().max(1);
        let bytes = additional
            .checked_mul(size_of::<T>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(bytes)?;
        values
            .try_reserve_exact(additional)
            .map_err(|source| ExtractionPlanError::Allocation {
                resource,
                requested: additional,
                source,
            })?;
        budget.consume_bytes(bytes)?;
    }
    values.push(value);
    Ok(())
}

fn normalize_pattern(mut pattern: String) -> Result<String, BundleContainerContractError> {
    let start = pattern.len() - pattern.trim_start().len();
    let end = pattern.trim_end().len();
    if start >= end {
        return Err(BundleContainerContractError::InvalidPattern(pattern));
    }
    pattern.truncate(end);
    if start != 0 {
        pattern.drain(..start);
    }
    if pattern.is_empty()
        || pattern.len() > MAX_PATTERN_BYTES
        || pattern.chars().any(char::is_control)
    {
        return Err(BundleContainerContractError::InvalidPattern(pattern));
    }
    Ok(pattern)
}

fn validate_occurrences(
    query: &BundleContainerQuery,
    occurrences: &[BundleContainerOccurrence],
) -> Result<(), BundleContainerContractError> {
    let mut match_work = 0_u64;
    for (index, occurrence) in occurrences.iter().enumerate() {
        if container_ordinal(occurrence.field_path()) != Some(occurrence.ordinal()) {
            return Err(BundleContainerContractError::OccurrenceFieldPathMismatch { index });
        }
        let occurrence_work = pattern_match_work(query.pattern(), occurrence.asset_path())
            .map_err(|_| BundleContainerContractError::PatternMatchWorkExceeded {
                maximum: MAX_UNBUDGETED_PATTERN_MATCH_WORK,
            })?;
        match_work = match_work.checked_add(occurrence_work).ok_or(
            BundleContainerContractError::PatternMatchWorkExceeded {
                maximum: MAX_UNBUDGETED_PATTERN_MATCH_WORK,
            },
        )?;
        if match_work > MAX_UNBUDGETED_PATTERN_MATCH_WORK {
            return Err(BundleContainerContractError::PatternMatchWorkExceeded {
                maximum: MAX_UNBUDGETED_PATTERN_MATCH_WORK,
            });
        }
        if !asset_path_matches(query.pattern(), occurrence.asset_path()) {
            return Err(BundleContainerContractError::OccurrencePatternMismatch { index });
        }
        let is_null = matches!(occurrence.resolution(), BundleContainerResolution::Null);
        if (occurrence.raw_target().path_id() == 0) != is_null {
            return Err(BundleContainerContractError::OccurrenceNullMismatch { index });
        }
    }
    Ok(())
}

fn asset_path_matches_budgeted(
    pattern: &str,
    asset_path: &str,
    budget: &mut AssetLoadBudget,
) -> Result<bool, ExtractionPlanError> {
    budget.consume_members(pattern_match_work(pattern, asset_path)?)?;
    Ok(asset_path_matches(pattern, asset_path))
}

fn pattern_match_work(pattern: &str, asset_path: &str) -> Result<u64, BudgetError> {
    let pattern_len =
        u64::try_from(pattern.len()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "bundle_container_pattern_match",
        })?;
    let path_len =
        u64::try_from(asset_path.len()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "bundle_container_pattern_match",
        })?;
    if pattern.as_bytes().contains(&b'*') || pattern.as_bytes().contains(&b'?') {
        return pattern_len
            .checked_add(1)
            .and_then(|pattern_len| {
                path_len
                    .checked_add(1)
                    .and_then(|path_len| pattern_len.checked_mul(path_len))
            })
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "bundle_container_pattern_match",
            });
    }
    let windows = path_len
        .checked_sub(pattern_len)
        .and_then(|difference| difference.checked_add(1))
        .unwrap_or(0);
    windows
        .checked_mul(pattern_len.max(1))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "bundle_container_pattern_match",
        })
}

fn asset_path_matches(pattern: &str, asset_path: &str) -> bool {
    let pattern = pattern.as_bytes();
    let asset_path = asset_path.as_bytes();
    if !pattern.contains(&b'*') && !pattern.contains(&b'?') {
        return asset_path
            .windows(pattern.len())
            .any(|window| bytes_eq_ignore_ascii_case(window, pattern));
    }
    glob_matches(pattern, asset_path)
}

fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star, mut retry_value) = (None, 0);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?'
                || pattern[pattern_index].eq_ignore_ascii_case(&value[value_index]))
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry_value += 1;
            value_index = retry_value;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

fn bytes_eq_ignore_ascii_case(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

#[derive(Debug, Error)]
pub enum BundleContainerContractError {
    #[error("bundle container query contract {actual:?} is unsupported; expected {expected:?}")]
    UnexpectedQueryContract {
        expected: &'static str,
        actual: String,
    },
    #[error("bundle container result contract {actual:?} is unsupported; expected {expected:?}")]
    UnexpectedResultContract {
        expected: &'static str,
        actual: String,
    },
    #[error("bundle container query version {0} is unsupported")]
    UnsupportedQueryVersion(u8),
    #[error("bundle container result version {0} is unsupported")]
    UnsupportedResultVersion(u8),
    #[error("bundle container query pattern is invalid: {0:?}")]
    InvalidPattern(String),
    #[error("bundle container pattern matching exceeds the {maximum}-comparison contract limit")]
    PatternMatchWorkExceeded { maximum: u64 },
    #[error("bundle container occurrence {index} field path does not encode its ordinal")]
    OccurrenceFieldPathMismatch { index: usize },
    #[error("bundle container occurrence {index} does not match its query pattern")]
    OccurrencePatternMismatch { index: usize },
    #[error("bundle container occurrence {index} null resolution contradicts its raw path ID")]
    OccurrenceNullMismatch { index: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::DigestV1;

    #[test]
    fn ascii_glob_matching_is_case_insensitive_without_normalization_allocations() {
        assert!(asset_path_matches(
            "ASSETS/*/ICON?.PNG",
            "assets/ui/icon1.png"
        ));
        assert!(asset_path_matches("ui/icon", "Assets/UI/Icon.png"));
        assert!(!asset_path_matches("*.wav", "assets/audio/theme.ogg"));
    }

    #[test]
    fn adversarial_globs_require_explicit_caller_work_budget() {
        let pattern = format!("*{}b", "a".repeat(MAX_PATTERN_BYTES - 2));
        let asset_path = "a".repeat(MAX_PATTERN_BYTES);
        let mut budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_members: 1_000,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();

        assert!(matches!(
            asset_path_matches_budgeted(&pattern, &asset_path, &mut budget),
            Err(ExtractionPlanError::Budget(BudgetError::Exceeded {
                resource: "members",
                ..
            }))
        ));
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn query_contract_rejects_unknown_versions_and_fields() {
        let unknown_version = br#"{
            "contract":"unity_asset.bundle_container_query",
            "version":2,
            "pattern":"*"
        }"#;
        assert!(
            BundleContainerQuery::read_json(
                unknown_version.as_slice(),
                &mut AssetLoadBudget::default()
            )
            .is_err()
        );

        let unknown_field = br#"{
            "contract":"unity_asset.bundle_container_query",
            "version":1,
            "pattern":"*",
            "extra":true
        }"#;
        assert!(
            BundleContainerQuery::read_json(
                unknown_field.as_slice(),
                &mut AssetLoadBudget::default()
            )
            .is_err()
        );

        assert!(matches!(
            BundleContainerQuery::new(" \t "),
            Err(BundleContainerContractError::InvalidPattern(_))
        ));
        let whitespace_pattern = br#"{
            "contract":"unity_asset.bundle_container_query",
            "version":1,
            "pattern":"   "
        }"#;
        assert!(
            BundleContainerQuery::read_json(
                whitespace_pattern.as_slice(),
                &mut AssetLoadBudget::default()
            )
            .is_err()
        );
    }

    #[test]
    fn result_preserves_duplicate_occurrences_in_input_order() {
        let source = SourceLocator::path("bundle.assets").unwrap();
        let owner = ObjectAddress::binary_direct(source.clone(), 1).unwrap();
        let target = ObjectAddress::binary_direct(source, 2).unwrap();
        let field_path = FieldPath::root()
            .push_field("m_Container")
            .unwrap()
            .push_index(0)
            .unwrap()
            .push_index(1)
            .unwrap();
        let occurrence = BundleContainerOccurrence {
            ordinal: 0,
            owner,
            field_path,
            asset_path: "assets/icon.png".to_owned(),
            raw_target: BundleContainerRawTarget {
                file_id: 0,
                path_id: 2,
            },
            resolution: BundleContainerResolution::Resolved { target },
            diagnostics: Box::new([]),
        };
        let result = BundleContainerResult::new(
            WorkspaceId::from_u128(1).unwrap(),
            WorkspaceRevision::new(DigestV1::hash_bytes(b"revision")),
            BundleContainerQuery::new("*").unwrap(),
            true,
            vec![occurrence.clone(), occurrence],
        )
        .unwrap();

        assert_eq!(result.occurrences().len(), 2);
        assert_eq!(result.occurrences()[0], result.occurrences()[1]);
    }
}
