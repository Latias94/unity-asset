//! Format-local reference occurrence discovery for parsed Unity YAML documents.

use std::mem::size_of;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, FieldPath, FieldPathError, FieldPathSegment,
    UnityClass, UnityDocument, UnityValue, YamlAnchor, YamlDocumentSelector,
};

use crate::YamlDocument;

const FILE_ID_KEYS: [&str; 2] = ["fileID", "m_FileID"];
const GUID_KEYS: [&str; 2] = ["guid", "m_GUID"];
const TYPE_ID_KEYS: [&str; 2] = ["type", "m_Type"];
const MAX_REFERENCE_PATH_SEGMENTS: usize = 512;

/// The result of scanning one already parsed YAML source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlReferenceScan {
    /// Occurrences in document and structural traversal order.
    pub occurrences: Vec<YamlReferenceOccurrence>,
    /// True when every document and value was visited.
    pub complete: bool,
    /// Deterministic work and result counters for this scan.
    pub stats: YamlReferenceScanStats,
}

/// One reference-shaped mapping at a stable object and field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlReferenceOccurrence {
    /// The object that owns the occurrence.
    pub object: YamlDocumentSelector,
    /// The path of the PPtr mapping, excluding its scalar wire fields.
    pub field_path: FieldPath,
    /// The structurally validated shape and its raw target fields.
    pub shape: YamlReferenceShape,
}

/// A reference-shaped YAML mapping after structural validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YamlReferenceShape {
    /// A well-formed null PPtr. Raw GUID and type fields remain available if present.
    Null(YamlReferenceTarget),
    /// A well-formed non-null PPtr.
    Valid(YamlReferenceTarget),
    /// A mapping with a file ID marker that is not a legal Unity PPtr shape.
    Invalid {
        raw: YamlReferenceRawTarget,
        diagnostic: YamlReferenceDiagnostic,
    },
}

impl YamlReferenceShape {
    /// Returns the decoded target when the shape is valid, including null targets.
    #[must_use]
    pub const fn target(&self) -> Option<&YamlReferenceTarget> {
        match self {
            Self::Null(target) | Self::Valid(target) => Some(target),
            Self::Invalid { .. } => None,
        }
    }

    /// Returns the best-effort raw fields for every shape.
    #[must_use]
    pub fn raw(&self) -> YamlReferenceRawTargetRef<'_> {
        match self {
            Self::Null(target) | Self::Valid(target) => YamlReferenceRawTargetRef {
                file_id: Some(target.file_id),
                guid: target.guid.as_deref(),
                type_id: target.type_id,
            },
            Self::Invalid { raw, .. } => YamlReferenceRawTargetRef {
                file_id: raw.file_id,
                guid: raw.guid.as_deref(),
                type_id: raw.type_id,
            },
        }
    }
}

/// Decoded raw fields of a structurally valid YAML PPtr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlReferenceTarget {
    /// Signed Unity YAML object identifier.
    pub file_id: i64,
    /// Exact GUID spelling from the YAML scalar.
    pub guid: Option<String>,
    /// Raw serialized reference type.
    pub type_id: Option<i64>,
}

/// Best-effort decoded fields retained for an invalid reference-shaped mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlReferenceRawTarget {
    /// Decoded file ID, or `None` when absent, non-integer, or supplied through conflicting aliases.
    pub file_id: Option<i64>,
    /// Exact GUID scalar, or `None` when absent, non-string, or supplied through conflicting aliases.
    pub guid: Option<String>,
    /// Decoded type, or `None` when absent, non-integer, or supplied through conflicting aliases.
    pub type_id: Option<i64>,
}

/// Borrowed view over raw fields, independent of shape validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YamlReferenceRawTargetRef<'a> {
    pub file_id: Option<i64>,
    pub guid: Option<&'a str>,
    pub type_id: Option<i64>,
}

/// Structured reason why a reference-shaped mapping is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YamlReferenceDiagnostic {
    ConflictingAliases {
        field: YamlReferenceField,
    },
    InvalidValueType {
        field: YamlReferenceField,
        actual: YamlValueKind,
    },
    InvalidGuidLength {
        actual: usize,
    },
    InvalidGuidHex,
    IncompleteExternalReference {
        missing: YamlReferenceField,
    },
    UnexpectedField {
        field: String,
    },
}

/// Logical fields accepted in Unity YAML PPtr mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YamlReferenceField {
    FileId,
    Guid,
    Type,
}

/// Stable scalar/container categories used by malformed-shape diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YamlValueKind {
    Null,
    Bool,
    Integer,
    Unsigned,
    Float,
    String,
    Array,
    Bytes,
    Object,
}

/// Work performed while scanning a YAML document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct YamlReferenceScanStats {
    pub documents_visited: u64,
    pub values_visited: u64,
    pub mapping_members_visited: u64,
    pub sequence_items_visited: u64,
    pub candidates_found: u64,
    pub occurrences_emitted: u64,
    pub null_occurrences: u64,
    pub valid_occurrences: u64,
    pub invalid_occurrences: u64,
    pub diagnostics_emitted: u64,
    pub max_depth: u32,
}

/// Failures that prevent a complete occurrence scan.
#[derive(Debug, Error)]
pub enum YamlReferenceScanError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    FieldPath(#[from] FieldPathError),
    #[error("invalid YAML document selector at index {document_index}: {source}")]
    InvalidDocumentSelector {
        document_index: usize,
        #[source]
        source: ContractError,
    },
    #[error("YAML document index {document_index} exceeds the u32 selector range")]
    DocumentIndexOverflow { document_index: usize },
    #[error("YAML class source omitted declared document index {document_index}")]
    MissingDocument { document_index: usize },
    #[error(
        "YAML documents {first_document_index} and {second_document_index} use the same anchor"
    )]
    DuplicateDocumentAnchor {
        first_document_index: usize,
        second_document_index: usize,
    },
    #[error("failed to reserve {requested} bytes for {resource}: {source}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        #[source]
        source: std::collections::TryReserveError,
    },
    #[error("YAML reference scan counter overflowed for {counter}")]
    CounterOverflow { counter: &'static str },
}

/// Scans format-local reference occurrences without reparsing YAML or using textual heuristics.
///
/// Every mapping containing `fileID` or `m_FileID` is represented. Malformed candidates are
/// emitted as [`YamlReferenceShape::Invalid`] instead of being silently discarded. A successful
/// result is complete; hard budget exhaustion is returned as an error.
pub fn scan_reference_occurrences(
    document: &YamlDocument,
    budget: &mut AssetLoadBudget,
) -> Result<YamlReferenceScan, YamlReferenceScanError> {
    scan_reference_class_occurrences(
        document.entries().len(),
        |index| document.entries().get(index),
        budget,
    )
}

/// Scans a stable indexed projection of Unity YAML classes.
///
/// The callback may overlay selected classes on an immutable parsed document without cloning all
/// unchanged classes. It must return the same class for an index throughout this call.
pub fn scan_reference_class_occurrences<'class>(
    document_count: usize,
    mut class_at: impl FnMut(usize) -> Option<&'class UnityClass>,
    budget: &mut AssetLoadBudget,
) -> Result<YamlReferenceScan, YamlReferenceScanError> {
    let selectors = prepare_selectors(document_count, &mut class_at, budget)?;
    let mut state = ScanState::new(budget);
    let mut path = Vec::new();
    let mut traversal = Vec::new();

    for (document_index, selector) in selectors.into_iter().enumerate() {
        let class = class_at(document_index)
            .ok_or(YamlReferenceScanError::MissingDocument { document_index })?;
        state.visit_document(class, selector, &mut path, &mut traversal)?;
    }

    Ok(YamlReferenceScan {
        occurrences: state.occurrences,
        complete: true,
        stats: state.stats,
    })
}

#[derive(Debug, Clone, Copy)]
enum SelectorRef<'a> {
    Anchored {
        anchor: &'a str,
        document_index: usize,
    },
    Ordinal(u32),
}

#[derive(Debug, Clone, Copy)]
enum PathSegmentRef<'a> {
    Field(&'a str),
    Index(u32),
}

#[derive(Debug, Clone, Copy)]
enum TraversalEvent<'a> {
    Visit {
        value: &'a UnityValue,
        selector: SelectorRef<'a>,
        segment: PathSegmentRef<'a>,
        depth: u32,
    },
    ExitPath,
}

struct ScanState<'budget> {
    budget: &'budget mut AssetLoadBudget,
    occurrences: Vec<YamlReferenceOccurrence>,
    occurrence_capacity: usize,
    path_capacity: usize,
    traversal_capacity: usize,
    stats: YamlReferenceScanStats,
}

impl<'budget> ScanState<'budget> {
    fn new(budget: &'budget mut AssetLoadBudget) -> Self {
        Self {
            budget,
            occurrences: Vec::new(),
            occurrence_capacity: 0,
            path_capacity: 0,
            traversal_capacity: 0,
            stats: YamlReferenceScanStats::default(),
        }
    }

    fn visit_document<'value>(
        &mut self,
        class: &'value UnityClass,
        selector: SelectorRef<'value>,
        path: &mut Vec<PathSegmentRef<'value>>,
        traversal: &mut Vec<TraversalEvent<'value>>,
    ) -> Result<(), YamlReferenceScanError> {
        self.budget.consume_entries(1)?;
        self.budget.observe_depth(0)?;
        increment(&mut self.stats.documents_visited, "documents_visited")?;

        let members = usize_to_u64(class.properties().len())?;
        self.budget.consume_members(members)?;
        add(
            &mut self.stats.mapping_members_visited,
            members,
            "mapping_members_visited",
        )?;

        for (field, value) in class.properties().iter().rev() {
            if !can_contain_occurrence(value) {
                continue;
            }
            self.push_event(
                traversal,
                TraversalEvent::Visit {
                    value,
                    selector,
                    segment: PathSegmentRef::Field(field),
                    depth: 1,
                },
            )?;
        }

        while let Some(event) = traversal.pop() {
            match event {
                TraversalEvent::Visit {
                    value,
                    selector,
                    segment,
                    depth,
                } => {
                    self.push_path(path, segment)?;
                    self.push_event(traversal, TraversalEvent::ExitPath)?;
                    self.visit_value(value, selector, path, traversal, depth)?;
                }
                TraversalEvent::ExitPath => {
                    path.pop();
                }
            }
        }
        Ok(())
    }

    fn visit_value<'value>(
        &mut self,
        value: &'value UnityValue,
        selector: SelectorRef<'value>,
        path: &[PathSegmentRef<'value>],
        traversal: &mut Vec<TraversalEvent<'value>>,
        depth: u32,
    ) -> Result<(), YamlReferenceScanError> {
        self.budget.consume_entries(1)?;
        self.budget.observe_depth(depth)?;
        increment(&mut self.stats.values_visited, "values_visited")?;
        self.stats.max_depth = self.stats.max_depth.max(depth);

        match value {
            UnityValue::Array(items) => {
                let members = usize_to_u64(items.len())?;
                self.budget.consume_members(members)?;
                add(
                    &mut self.stats.sequence_items_visited,
                    members,
                    "sequence_items_visited",
                )?;
                let mut child_depth = None;
                for (index, item) in items.iter().enumerate().rev() {
                    if !can_contain_occurrence(item) {
                        continue;
                    }
                    let depth_for_child = match child_depth {
                        Some(depth) => depth,
                        None => {
                            let next = next_depth(depth)?;
                            child_depth = Some(next);
                            next
                        }
                    };
                    let index = u32::try_from(index).map_err(|_| {
                        YamlReferenceScanError::CounterOverflow {
                            counter: "sequence_index",
                        }
                    })?;
                    self.push_event(
                        traversal,
                        TraversalEvent::Visit {
                            value: item,
                            selector,
                            segment: PathSegmentRef::Index(index),
                            depth: depth_for_child,
                        },
                    )?;
                }
            }
            UnityValue::Object(map) => {
                let members = usize_to_u64(map.len())?;
                self.budget.consume_members(members)?;
                add(
                    &mut self.stats.mapping_members_visited,
                    members,
                    "mapping_members_visited",
                )?;

                if let Some(candidate) = inspect_candidate(map) {
                    increment(&mut self.stats.candidates_found, "candidates_found")?;
                    self.emit(selector, path, candidate)?;
                }

                let mut child_depth = None;
                for (field, child) in map.iter().rev() {
                    if !can_contain_occurrence(child) {
                        continue;
                    }
                    let depth_for_child = match child_depth {
                        Some(depth) => depth,
                        None => {
                            let next = next_depth(depth)?;
                            child_depth = Some(next);
                            next
                        }
                    };
                    self.push_event(
                        traversal,
                        TraversalEvent::Visit {
                            value: child,
                            selector,
                            segment: PathSegmentRef::Field(field),
                            depth: depth_for_child,
                        },
                    )?;
                }
            }
            UnityValue::Null
            | UnityValue::Bool(_)
            | UnityValue::Integer(_)
            | UnityValue::Unsigned(_)
            | UnityValue::Float(_)
            | UnityValue::String(_)
            | UnityValue::Bytes(_) => {}
        }
        Ok(())
    }

    fn push_event<'value>(
        &mut self,
        traversal: &mut Vec<TraversalEvent<'value>>,
        event: TraversalEvent<'value>,
    ) -> Result<(), YamlReferenceScanError> {
        reserve_budgeted_vec(
            traversal,
            &mut self.traversal_capacity,
            1,
            self.budget,
            "YAML reference traversal stack",
        )?;
        traversal.push(event);
        Ok(())
    }

    fn push_path<'value>(
        &mut self,
        path: &mut Vec<PathSegmentRef<'value>>,
        segment: PathSegmentRef<'value>,
    ) -> Result<(), YamlReferenceScanError> {
        reserve_budgeted_vec(
            path,
            &mut self.path_capacity,
            1,
            self.budget,
            "YAML reference path stack",
        )?;
        path.push(segment);
        Ok(())
    }

    fn emit(
        &mut self,
        selector: SelectorRef<'_>,
        path: &[PathSegmentRef<'_>],
        candidate: Candidate<'_>,
    ) -> Result<(), YamlReferenceScanError> {
        reserve_budgeted_vec(
            &mut self.occurrences,
            &mut self.occurrence_capacity,
            1,
            self.budget,
            "YAML reference occurrences",
        )?;

        let object = build_selector(selector, self.budget)?;
        let field_path = build_field_path(path, self.budget)?;
        let guid = candidate
            .guid
            .map(|value| clone_string_budgeted(value, self.budget, "YAML reference GUID"))
            .transpose()?;
        let raw = YamlReferenceRawTarget {
            file_id: candidate.file_id,
            guid,
            type_id: candidate.type_id,
        };

        let shape = match candidate.outcome {
            CandidateOutcome::Null(file_id) => {
                increment(&mut self.stats.null_occurrences, "null_occurrences")?;
                YamlReferenceShape::Null(YamlReferenceTarget {
                    file_id,
                    guid: raw.guid,
                    type_id: raw.type_id,
                })
            }
            CandidateOutcome::Valid(file_id) => {
                increment(&mut self.stats.valid_occurrences, "valid_occurrences")?;
                YamlReferenceShape::Valid(YamlReferenceTarget {
                    file_id,
                    guid: raw.guid,
                    type_id: raw.type_id,
                })
            }
            CandidateOutcome::Invalid(diagnostic) => {
                increment(&mut self.stats.invalid_occurrences, "invalid_occurrences")?;
                increment(&mut self.stats.diagnostics_emitted, "diagnostics_emitted")?;
                YamlReferenceShape::Invalid {
                    raw,
                    diagnostic: diagnostic.into_owned(self.budget)?,
                }
            }
        };

        self.occurrences.push(YamlReferenceOccurrence {
            object,
            field_path,
            shape,
        });
        increment(&mut self.stats.occurrences_emitted, "occurrences_emitted")?;
        Ok(())
    }
}

fn prepare_selectors<'class>(
    document_count: usize,
    class_at: &mut impl FnMut(usize) -> Option<&'class UnityClass>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SelectorRef<'class>>, YamlReferenceScanError> {
    let mut selectors = Vec::new();
    let mut selector_capacity = 0;
    let mut anchors = Vec::new();
    let mut anchor_capacity = 0;
    reserve_budgeted_vec(
        &mut selectors,
        &mut selector_capacity,
        document_count,
        budget,
        "YAML document selector table",
    )?;
    reserve_budgeted_vec(
        &mut anchors,
        &mut anchor_capacity,
        document_count,
        budget,
        "YAML document anchor validation",
    )?;

    for document_index in 0..document_count {
        let class = class_at(document_index)
            .ok_or(YamlReferenceScanError::MissingDocument { document_index })?;
        let selector = selector_ref(class, document_index)?;
        if let SelectorRef::Anchored { anchor, .. } = selector {
            anchors.push((anchor, document_index));
        }
        selectors.push(selector);
    }

    anchors.sort_unstable_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(&right.1)));
    if let Some(pair) = anchors.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(YamlReferenceScanError::DuplicateDocumentAnchor {
            first_document_index: pair[0].1,
            second_document_index: pair[1].1,
        });
    }
    Ok(selectors)
}

const fn can_contain_occurrence(value: &UnityValue) -> bool {
    matches!(value, UnityValue::Array(_) | UnityValue::Object(_))
}

fn selector_ref(
    class: &UnityClass,
    document_index: usize,
) -> Result<SelectorRef<'_>, YamlReferenceScanError> {
    if is_synthetic_document_anchor(class, document_index) {
        let document_index = u32::try_from(document_index)
            .map_err(|_| YamlReferenceScanError::DocumentIndexOverflow { document_index })?;
        return Ok(SelectorRef::Ordinal(document_index));
    }

    YamlAnchor::validate(class.anchor()).map_err(|source| {
        YamlReferenceScanError::InvalidDocumentSelector {
            document_index,
            source,
        }
    })?;
    Ok(SelectorRef::Anchored {
        anchor: class.anchor(),
        document_index,
    })
}

fn is_synthetic_document_anchor(class: &UnityClass, document_index: usize) -> bool {
    class.class_id() == 0
        && class
            .anchor()
            .strip_prefix("doc_")
            .and_then(|ordinal| ordinal.parse::<usize>().ok())
            == Some(document_index)
}

fn build_selector(
    selector: SelectorRef<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<YamlDocumentSelector, YamlReferenceScanError> {
    match selector {
        SelectorRef::Anchored {
            anchor,
            document_index,
        } => {
            let anchor = clone_string_budgeted(anchor, budget, "YAML reference object anchor")?;
            YamlDocumentSelector::anchor(anchor).map_err(|source| {
                YamlReferenceScanError::InvalidDocumentSelector {
                    document_index,
                    source,
                }
            })
        }
        SelectorRef::Ordinal(document_index) => Ok(YamlDocumentSelector::ordinal(document_index)),
    }
}

fn build_field_path(
    path: &[PathSegmentRef<'_>],
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, YamlReferenceScanError> {
    let mut segments = Vec::new();
    reserve_budgeted_vec(
        &mut segments,
        &mut 0,
        path.len(),
        budget,
        "YAML reference FieldPath segments",
    )?;
    for segment in path {
        segments.push(match segment {
            PathSegmentRef::Field(field) => FieldPathSegment::field(clone_string_budgeted(
                field,
                budget,
                "YAML reference FieldPath field",
            )?)?,
            PathSegmentRef::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    Ok(FieldPath::from_segments(segments)?)
}

#[derive(Debug, Clone, Copy)]
struct AliasValue<'a> {
    value: Option<&'a UnityValue>,
    duplicate: bool,
}

impl<'a> AliasValue<'a> {
    const fn new() -> Self {
        Self {
            value: None,
            duplicate: false,
        }
    }

    fn observe(&mut self, value: &'a UnityValue) {
        if self.value.is_some() {
            self.duplicate = true;
        } else {
            self.value = Some(value);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Candidate<'a> {
    file_id: Option<i64>,
    guid: Option<&'a str>,
    type_id: Option<i64>,
    outcome: CandidateOutcome<'a>,
}

#[derive(Debug, Clone, Copy)]
enum CandidateOutcome<'a> {
    Null(i64),
    Valid(i64),
    Invalid(BorrowedDiagnostic<'a>),
}

#[derive(Debug, Clone, Copy)]
enum BorrowedDiagnostic<'a> {
    ConflictingAliases {
        field: YamlReferenceField,
    },
    InvalidValueType {
        field: YamlReferenceField,
        actual: YamlValueKind,
    },
    InvalidGuidLength {
        actual: usize,
    },
    InvalidGuidHex,
    IncompleteExternalReference {
        missing: YamlReferenceField,
    },
    UnexpectedField {
        field: &'a str,
    },
}

impl BorrowedDiagnostic<'_> {
    fn into_owned(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<YamlReferenceDiagnostic, YamlReferenceScanError> {
        Ok(match self {
            Self::ConflictingAliases { field } => {
                YamlReferenceDiagnostic::ConflictingAliases { field }
            }
            Self::InvalidValueType { field, actual } => {
                YamlReferenceDiagnostic::InvalidValueType { field, actual }
            }
            Self::InvalidGuidLength { actual } => {
                YamlReferenceDiagnostic::InvalidGuidLength { actual }
            }
            Self::InvalidGuidHex => YamlReferenceDiagnostic::InvalidGuidHex,
            Self::IncompleteExternalReference { missing } => {
                YamlReferenceDiagnostic::IncompleteExternalReference { missing }
            }
            Self::UnexpectedField { field } => YamlReferenceDiagnostic::UnexpectedField {
                field: clone_string_budgeted(field, budget, "YAML reference diagnostic field")?,
            },
        })
    }
}

fn inspect_candidate(map: &indexmap::IndexMap<String, UnityValue>) -> Option<Candidate<'_>> {
    let mut file_id = AliasValue::new();
    let mut guid = AliasValue::new();
    let mut type_id = AliasValue::new();
    let mut unexpected = None;

    for (field, value) in map {
        if FILE_ID_KEYS.contains(&field.as_str()) {
            file_id.observe(value);
        } else if GUID_KEYS.contains(&field.as_str()) {
            guid.observe(value);
        } else if TYPE_ID_KEYS.contains(&field.as_str()) {
            type_id.observe(value);
        } else if unexpected.is_none() {
            unexpected = Some(field.as_str());
        }
    }

    let file_value = file_id.value?;
    let decoded_file_id = file_value.as_i64();
    let decoded_guid = guid.value.and_then(UnityValue::as_str);
    let decoded_type_id = type_id.value.and_then(UnityValue::as_i64);

    let diagnostic = conflicting_alias(file_id, YamlReferenceField::FileId)
        .or_else(|| conflicting_alias(guid, YamlReferenceField::Guid))
        .or_else(|| conflicting_alias(type_id, YamlReferenceField::Type))
        .or_else(|| invalid_integer(file_id, YamlReferenceField::FileId))
        .or_else(|| invalid_string(guid, YamlReferenceField::Guid))
        .or_else(|| invalid_integer(type_id, YamlReferenceField::Type))
        .or_else(|| invalid_guid(decoded_guid))
        .or(match (guid.value.is_some(), type_id.value.is_some()) {
            (false, true) => Some(BorrowedDiagnostic::IncompleteExternalReference {
                missing: YamlReferenceField::Guid,
            }),
            (true, false) => Some(BorrowedDiagnostic::IncompleteExternalReference {
                missing: YamlReferenceField::Type,
            }),
            _ => None,
        })
        .or_else(|| unexpected.map(|field| BorrowedDiagnostic::UnexpectedField { field }));

    let outcome = match (diagnostic, decoded_file_id) {
        (Some(diagnostic), _) => CandidateOutcome::Invalid(diagnostic),
        (None, None) => CandidateOutcome::Invalid(BorrowedDiagnostic::InvalidValueType {
            field: YamlReferenceField::FileId,
            actual: value_kind(file_value),
        }),
        (None, Some(file_id @ 0)) => CandidateOutcome::Null(file_id),
        (None, Some(file_id)) => CandidateOutcome::Valid(file_id),
    };

    Some(Candidate {
        file_id: (!file_id.duplicate).then_some(decoded_file_id).flatten(),
        guid: (!guid.duplicate).then_some(decoded_guid).flatten(),
        type_id: (!type_id.duplicate).then_some(decoded_type_id).flatten(),
        outcome,
    })
}

fn conflicting_alias(
    alias: AliasValue<'_>,
    field: YamlReferenceField,
) -> Option<BorrowedDiagnostic<'static>> {
    alias
        .duplicate
        .then_some(BorrowedDiagnostic::ConflictingAliases { field })
}

fn invalid_integer(
    alias: AliasValue<'_>,
    field: YamlReferenceField,
) -> Option<BorrowedDiagnostic<'static>> {
    let value = alias.value?;
    value
        .as_i64()
        .is_none()
        .then_some(BorrowedDiagnostic::InvalidValueType {
            field,
            actual: value_kind(value),
        })
}

fn invalid_string(
    alias: AliasValue<'_>,
    field: YamlReferenceField,
) -> Option<BorrowedDiagnostic<'static>> {
    let value = alias.value?;
    value
        .as_str()
        .is_none()
        .then_some(BorrowedDiagnostic::InvalidValueType {
            field,
            actual: value_kind(value),
        })
}

fn invalid_guid(guid: Option<&str>) -> Option<BorrowedDiagnostic<'static>> {
    let guid = guid?;
    if guid.len() != 32 {
        return Some(BorrowedDiagnostic::InvalidGuidLength { actual: guid.len() });
    }
    (!guid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(BorrowedDiagnostic::InvalidGuidHex)
}

const fn value_kind(value: &UnityValue) -> YamlValueKind {
    match value {
        UnityValue::Null => YamlValueKind::Null,
        UnityValue::Bool(_) => YamlValueKind::Bool,
        UnityValue::Integer(_) => YamlValueKind::Integer,
        UnityValue::Unsigned(_) => YamlValueKind::Unsigned,
        UnityValue::Float(_) => YamlValueKind::Float,
        UnityValue::String(_) => YamlValueKind::String,
        UnityValue::Array(_) => YamlValueKind::Array,
        UnityValue::Bytes(_) => YamlValueKind::Bytes,
        UnityValue::Object(_) => YamlValueKind::Object,
    }
}

fn reserve_budgeted_vec<T>(
    values: &mut Vec<T>,
    accounted_capacity: &mut usize,
    additional: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), YamlReferenceScanError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let target = geometric_capacity(*accounted_capacity, required)?;
    if target <= *accounted_capacity {
        return Ok(());
    }
    let slots = target - *accounted_capacity;
    let bytes = slots
        .checked_mul(size_of::<T>().max(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let budget_bytes = usize_to_u64(bytes)?;
    budget.check_bytes(budget_bytes)?;
    values
        .try_reserve_exact(target - values.len())
        .map_err(|source| YamlReferenceScanError::AllocationFailed {
            resource,
            requested: bytes,
            source,
        })?;
    budget.consume_bytes(budget_bytes)?;
    *accounted_capacity = target;
    Ok(())
}

fn geometric_capacity(current: usize, required: usize) -> Result<usize, YamlReferenceScanError> {
    if required <= current {
        return Ok(current);
    }
    required
        .max(4)
        .checked_next_power_of_two()
        .ok_or_else(|| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn clone_string_budgeted(
    value: &str,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<String, YamlReferenceScanError> {
    let bytes = usize_to_u64(value.len())?;
    budget.check_bytes(bytes)?;
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|source| {
        YamlReferenceScanError::AllocationFailed {
            resource,
            requested: value.len(),
            source,
        }
    })?;
    budget.consume_bytes(bytes)?;
    owned.push_str(value);
    Ok(owned)
}

fn next_depth(depth: u32) -> Result<u32, YamlReferenceScanError> {
    let next = depth
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
    if usize::try_from(next).map_or(true, |depth| depth > MAX_REFERENCE_PATH_SEGMENTS) {
        return Err(FieldPathError::TooManySegments {
            maximum: MAX_REFERENCE_PATH_SEGMENTS,
        }
        .into());
    }
    Ok(next)
}

fn usize_to_u64(value: usize) -> Result<u64, YamlReferenceScanError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn increment(value: &mut u64, counter: &'static str) -> Result<(), YamlReferenceScanError> {
    add(value, 1, counter)
}

fn add(value: &mut u64, amount: u64, counter: &'static str) -> Result<(), YamlReferenceScanError> {
    *value = value
        .checked_add(amount)
        .ok_or(YamlReferenceScanError::CounterOverflow { counter })?;
    Ok(())
}
