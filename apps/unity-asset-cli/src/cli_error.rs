use std::fmt;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
#[cfg(test)]
use unity_asset::extraction::ExtractionAllocationUnit;
#[cfg(feature = "decode")]
use unity_asset::extraction::MediaInspectionError;
use unity_asset::extraction::{ExtractionExecutionError, ExtractionPlanError};
use unity_asset::reference::ReferenceGraphError;
use unity_asset::workspace::{
    CommitContractError, CommitDestinationState, CommitError, PrepareError, PublicationTargetError,
    RecoveryBlockedReason, RecoveryDiscoveryBlockedReason, RecoveryDiscoveryError, RecoveryError,
    RecoveryLocator, WorkspaceError, WorkspaceLookup,
};
use unity_asset::{BudgetError, BudgetedJsonError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliErrorCode {
    ContractInvalid,
    ContractBudgetExceeded,
    LookupUnloaded,
    LookupMissing,
    LookupAmbiguous,
    LookupInvalid,
    PrepareRejected,
    CommitWorkspaceMismatch,
    CommitStaleRevision,
    CommitNoEffect,
    CommitBudgetExceeded,
    CommitSourceConflict,
    CommitDestinationConflict,
    CommitPublicationBlocked,
    CommitRetryable,
    CommitRecoveryRequired,
    CommitContractInvalid,
    PublicationTargetInvalid,
    RecoveryBusy,
    RecoveryBlocked,
    RecoveryBudgetExceeded,
    RecoveryDiscoveryBusy,
    RecoveryDiscoveryBlocked,
    RecoveryDiscoveryBudgetExceeded,
    ExportArgumentInvalid,
    ExportPlanRejected,
    ExportRepresentationUnavailable,
    ExportWorkspaceMismatch,
    ExportSourceChanged,
    ExportBudgetExceeded,
    ExportResourceLimit,
    ExportResumeMismatch,
    ExportOutputInvalid,
    ExportRecoveryRequired,
    ExportExecutionFailed,
}

impl CliErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ContractInvalid => "CLI_CONTRACT_INVALID",
            Self::ContractBudgetExceeded => "CLI_CONTRACT_BUDGET_EXCEEDED",
            Self::LookupUnloaded => "CLI_WORKSPACE_LOOKUP_UNLOADED",
            Self::LookupMissing => "CLI_WORKSPACE_LOOKUP_MISSING",
            Self::LookupAmbiguous => "CLI_WORKSPACE_LOOKUP_AMBIGUOUS",
            Self::LookupInvalid => "CLI_WORKSPACE_LOOKUP_INVALID",
            Self::PrepareRejected => "CLI_WORKSPACE_PREPARE_REJECTED",
            Self::CommitWorkspaceMismatch => "CLI_WORKSPACE_COMMIT_WORKSPACE_MISMATCH",
            Self::CommitStaleRevision => "CLI_WORKSPACE_COMMIT_STALE_REVISION",
            Self::CommitNoEffect => "CLI_WORKSPACE_COMMIT_NO_EFFECT",
            Self::CommitBudgetExceeded => "CLI_WORKSPACE_COMMIT_BUDGET_EXCEEDED",
            Self::CommitSourceConflict => "CLI_WORKSPACE_COMMIT_SOURCE_CONFLICT",
            Self::CommitDestinationConflict => "CLI_WORKSPACE_COMMIT_DESTINATION_CONFLICT",
            Self::CommitPublicationBlocked => "CLI_WORKSPACE_COMMIT_PUBLICATION_BLOCKED",
            Self::CommitRetryable => "CLI_WORKSPACE_COMMIT_RETRYABLE",
            Self::CommitRecoveryRequired => "CLI_WORKSPACE_COMMIT_RECOVERY_REQUIRED",
            Self::CommitContractInvalid => "CLI_WORKSPACE_COMMIT_CONTRACT_INVALID",
            Self::PublicationTargetInvalid => "CLI_WORKSPACE_PUBLICATION_TARGET_INVALID",
            Self::RecoveryBusy => "CLI_WORKSPACE_RECOVERY_BUSY",
            Self::RecoveryBlocked => "CLI_WORKSPACE_RECOVERY_BLOCKED",
            Self::RecoveryBudgetExceeded => "CLI_WORKSPACE_RECOVERY_BUDGET_EXCEEDED",
            Self::RecoveryDiscoveryBusy => "CLI_WORKSPACE_RECOVERY_DISCOVERY_BUSY",
            Self::RecoveryDiscoveryBlocked => "CLI_WORKSPACE_RECOVERY_DISCOVERY_BLOCKED",
            Self::RecoveryDiscoveryBudgetExceeded => {
                "CLI_WORKSPACE_RECOVERY_DISCOVERY_BUDGET_EXCEEDED"
            }
            Self::ExportArgumentInvalid => "CLI_EXPORT_ARGUMENT_INVALID",
            Self::ExportPlanRejected => "CLI_EXPORT_PLAN_REJECTED",
            Self::ExportRepresentationUnavailable => "CLI_EXPORT_REPRESENTATION_UNAVAILABLE",
            Self::ExportWorkspaceMismatch => "CLI_EXPORT_WORKSPACE_MISMATCH",
            Self::ExportSourceChanged => "CLI_EXPORT_SOURCE_CHANGED",
            Self::ExportBudgetExceeded => "CLI_EXPORT_BUDGET_EXCEEDED",
            Self::ExportResourceLimit => "CLI_EXPORT_RESOURCE_LIMIT",
            Self::ExportResumeMismatch => "CLI_EXPORT_RESUME_MISMATCH",
            Self::ExportOutputInvalid => "CLI_EXPORT_OUTPUT_INVALID",
            Self::ExportRecoveryRequired => "CLI_EXPORT_RECOVERY_REQUIRED",
            Self::ExportExecutionFailed => "CLI_EXPORT_EXECUTION_FAILED",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::ContractInvalid => "structured input contract is invalid",
            Self::ContractBudgetExceeded => {
                "structured input contract exceeded its resource budget"
            }
            Self::LookupUnloaded => "workspace object source is not loaded",
            Self::LookupMissing => "workspace object was not found",
            Self::LookupAmbiguous => "workspace object lookup is ambiguous",
            Self::LookupInvalid => "workspace object address is invalid",
            Self::PrepareRejected => "workspace prepare was rejected",
            Self::CommitWorkspaceMismatch => "prepared change belongs to another workspace",
            Self::CommitStaleRevision => "prepared change is stale",
            Self::CommitNoEffect => "prepared change has no semantic effect",
            Self::CommitBudgetExceeded => "workspace commit exceeded its resource budget",
            Self::CommitSourceConflict => "workspace source changed after prepare",
            Self::CommitDestinationConflict => "publication destination changed after prepare",
            Self::CommitPublicationBlocked => "workspace publication is blocked",
            Self::CommitRetryable => "workspace commit can be retried",
            Self::CommitRecoveryRequired => "workspace publication requires recovery",
            Self::CommitContractInvalid => "workspace commit contract is invalid",
            Self::PublicationTargetInvalid => "workspace publication target is invalid",
            Self::RecoveryBusy => "workspace recovery transaction is busy",
            Self::RecoveryBlocked => "workspace recovery is blocked",
            Self::RecoveryBudgetExceeded => "workspace recovery exceeded its resource budget",
            Self::RecoveryDiscoveryBusy => "workspace recovery discovery is busy",
            Self::RecoveryDiscoveryBlocked => "workspace recovery discovery is blocked",
            Self::RecoveryDiscoveryBudgetExceeded => {
                "workspace recovery discovery exceeded its resource budget"
            }
            Self::ExportArgumentInvalid => "export arguments are invalid",
            Self::ExportPlanRejected => "export planning was rejected",
            Self::ExportRepresentationUnavailable => {
                "the requested export representation is unavailable"
            }
            Self::ExportWorkspaceMismatch => "export plan belongs to another workspace revision",
            Self::ExportSourceChanged => "an export source changed or became unavailable",
            Self::ExportBudgetExceeded => "export exceeded its caller-owned resource budget",
            Self::ExportResourceLimit => "export exceeded an execution resource limit",
            Self::ExportResumeMismatch => "resume evidence does not match the export plan",
            Self::ExportOutputInvalid => "export output layout is invalid or unavailable",
            Self::ExportRecoveryRequired => "export publication requires recovery",
            Self::ExportExecutionFailed => "export execution failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportManifestPathErrorKind {
    NonUtf8,
    Invalid,
}

impl ExportManifestPathErrorKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NonUtf8 => "manifest_path_non_utf8",
            Self::Invalid => "manifest_path_invalid",
        }
    }
}

#[derive(Debug, Clone)]
struct CliErrorMetadata {
    code: CliErrorCode,
    details: Option<Value>,
}

impl fmt::Display for CliErrorMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code.message())
    }
}

impl std::error::Error for CliErrorMetadata {}

pub(crate) fn mark_contract_error(error: anyhow::Error) -> anyhow::Error {
    let direct_budget = error.downcast_ref::<BudgetError>();
    let json_error = error.downcast_ref::<BudgetedJsonError>();
    let (code, details) = match (direct_budget, json_error) {
        (Some(source), _) | (_, Some(BudgetedJsonError::Budget(source))) => (
            CliErrorCode::ContractBudgetExceeded,
            Some(json!({ "budget": budget_details(source) })),
        ),
        (
            None,
            Some(BudgetedJsonError::EncodedLimitExceeded {
                contract,
                limit,
                requested,
            }),
        ) => (
            CliErrorCode::ContractBudgetExceeded,
            Some(json!({
                "contract": contract,
                "resource": "encoded_bytes",
                "limit": limit,
                "requested": requested,
            })),
        ),
        (
            None,
            Some(BudgetedJsonError::StructureLimitExceeded {
                contract,
                resource,
                limit,
                requested,
            }),
        ) => (
            CliErrorCode::ContractBudgetExceeded,
            Some(json!({
                "contract": contract,
                "resource": resource,
                "limit": limit,
                "requested": requested,
            })),
        ),
        _ => (CliErrorCode::ContractInvalid, None),
    };
    mark(error, code, details)
}

pub(crate) fn mark_prepare_error(error: PrepareError) -> anyhow::Error {
    let details = serde_json::to_value(error.report())
        .ok()
        .map(|report| json!({ "report": report }));
    mark(error, CliErrorCode::PrepareRejected, details)
}

pub(crate) fn mark_commit_error(error: CommitError) -> anyhow::Error {
    let (code, details) = match &error {
        CommitError::WorkspaceMismatch { expected, actual } => (
            CliErrorCode::CommitWorkspaceMismatch,
            json!({
                "kind": "workspace_mismatch",
                "expected_workspace_id": expected,
                "actual_workspace_id": actual,
            }),
        ),
        CommitError::StaleRevision { expected, actual } => (
            CliErrorCode::CommitStaleRevision,
            json!({
                "kind": "stale_revision",
                "expected_revision": expected,
                "actual_revision": actual,
            }),
        ),
        CommitError::NoEffect => (CliErrorCode::CommitNoEffect, json!({ "kind": "no_effect" })),
        CommitError::Budget { source, .. } => (
            CliErrorCode::CommitBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "budget": budget_details(source),
            }),
        ),
        CommitError::SourceConflict {
            source_id,
            expected,
            actual,
        } => (
            CliErrorCode::CommitSourceConflict,
            json!({
                "kind": "source_conflict",
                "source_id": source_id,
                "expected_fingerprint": expected,
                "actual_fingerprint": actual,
            }),
        ),
        CommitError::DestinationConflict {
            output,
            expected,
            actual,
        } => (
            CliErrorCode::CommitDestinationConflict,
            json!({
                "kind": "destination_conflict",
                "output": output,
                "expected": destination_state(*expected),
                "actual": destination_state(*actual),
            }),
        ),
        CommitError::PublishBlocked { message } => (
            CliErrorCode::CommitPublicationBlocked,
            json!({
                "kind": "publication_blocked",
                "reason": message,
            }),
        ),
        CommitError::Retryable { message, .. } => (
            CliErrorCode::CommitRetryable,
            json!({
                "kind": "retryable",
                "reason": message,
            }),
        ),
        CommitError::RecoveryRequired { locator, message } => (
            CliErrorCode::CommitRecoveryRequired,
            json!({
                "kind": "recovery_required",
                "locator": recovery_locator_details(locator),
                "reason": message,
            }),
        ),
        CommitError::Contract(contract) => (
            CliErrorCode::CommitContractInvalid,
            commit_contract_details(contract),
        ),
    };
    mark(error, code, Some(details))
}

pub(crate) fn mark_recovery_error(error: RecoveryError) -> anyhow::Error {
    let (code, details) = match &error {
        RecoveryError::Busy { locator, message } => (
            CliErrorCode::RecoveryBusy,
            json!({
                "kind": "busy",
                "locator": recovery_locator_details(locator),
                "reason": message,
            }),
        ),
        RecoveryError::Blocked { locator, reason } => (
            CliErrorCode::RecoveryBlocked,
            json!({
                "kind": "blocked",
                "locator": recovery_locator_details(locator),
                "reason": recovery_blocked_reason_details(reason),
            }),
        ),
        RecoveryError::Budget { locator, source } => (
            CliErrorCode::RecoveryBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "locator": recovery_locator_details(locator),
                "budget": budget_details(source),
            }),
        ),
    };
    mark(error, code, Some(details))
}

pub(crate) fn mark_recovery_discovery_error(error: RecoveryDiscoveryError) -> anyhow::Error {
    let (code, details) = match &error {
        RecoveryDiscoveryError::Busy => (
            CliErrorCode::RecoveryDiscoveryBusy,
            json!({ "kind": "busy" }),
        ),
        RecoveryDiscoveryError::Budget { source } => (
            CliErrorCode::RecoveryDiscoveryBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "budget": budget_details(source),
            }),
        ),
        RecoveryDiscoveryError::Blocked { reason } => (
            CliErrorCode::RecoveryDiscoveryBlocked,
            json!({
                "kind": "blocked",
                "reason": recovery_discovery_blocked_reason(reason),
            }),
        ),
    };
    mark(error, code, Some(details))
}

pub(crate) fn mark_publication_target_error(error: PublicationTargetError) -> anyhow::Error {
    let details = match &error {
        PublicationTargetError::NotAbsolute(path) => {
            json!({ "kind": "not_absolute", "path": path_details(path) })
        }
        PublicationTargetError::SymbolicLink(path) => {
            json!({ "kind": "symbolic_link", "path": path_details(path) })
        }
        PublicationTargetError::NotDirectory(path) => {
            json!({ "kind": "not_directory", "path": path_details(path) })
        }
        PublicationTargetError::Io { path, message } => {
            json!({
                "kind": "io",
                "path": path_details(path),
                "reason": message,
            })
        }
    };
    mark(error, CliErrorCode::PublicationTargetInvalid, Some(details))
}

pub(crate) fn mark_export_shared_stdin_error(inputs: &[&str]) -> anyhow::Error {
    mark(
        anyhow::Error::msg("Only one structured export input may read from stdin"),
        CliErrorCode::ExportArgumentInvalid,
        Some(json!({
            "kind": "structured_inputs_share_stdin",
            "inputs": inputs,
        })),
    )
}

pub(crate) fn mark_export_manifest_path_error(
    error: impl Into<anyhow::Error>,
    path: &Path,
    kind: ExportManifestPathErrorKind,
) -> anyhow::Error {
    mark(
        error,
        CliErrorCode::ExportOutputInvalid,
        Some(json!({
            "kind": kind.as_str(),
            "path": path_details(path),
        })),
    )
}

pub(crate) fn mark_export_workspace_load_error(
    error: anyhow::Error,
    input: &Path,
) -> anyhow::Error {
    let budget = error
        .downcast_ref::<BudgetError>()
        .or_else(|| match error.downcast_ref::<WorkspaceError>() {
            Some(WorkspaceError::Budget(source)) => Some(source),
            _ => None,
        })
        .map(budget_details);
    match budget {
        Some(budget) => mark(
            error,
            CliErrorCode::ExportBudgetExceeded,
            Some(json!({
                "kind": "budget_exceeded",
                "budget": budget,
            })),
        ),
        None => mark(
            error,
            CliErrorCode::ExportSourceChanged,
            Some(json!({
                "kind": "workspace_load_failed",
                "input": path_details(input),
            })),
        ),
    }
}

pub(crate) fn mark_export_plan_error(error: ExtractionPlanError) -> anyhow::Error {
    let (code, details) = export_plan_error_metadata(&error);
    mark(error, code, Some(details))
}

fn export_plan_error_metadata(error: &ExtractionPlanError) -> (CliErrorCode, Value) {
    match error {
        ExtractionPlanError::Budget(source)
        | ExtractionPlanError::Workspace(WorkspaceError::Budget(source)) => (
            CliErrorCode::ExportBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "budget": budget_details(source),
            }),
        ),
        ExtractionPlanError::Allocation {
            resource,
            requested,
            unit,
            ..
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "allocation_failed",
                "resource": resource,
                "requested": requested,
                "unit": unit.as_str(),
            }),
        ),
        ExtractionPlanError::MediaAllocation {
            resource,
            requested,
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "allocation_failed",
                "resource": resource,
                "requested": requested,
                "unit": "bytes",
            }),
        ),
        #[cfg(feature = "decode")]
        ExtractionPlanError::InvalidMediaDescriptor { address, source } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "invalid_media_descriptor",
                "address": address,
                "inspection": media_inspection_error_details(source),
            }),
        ),
        ExtractionPlanError::MediaPreparation { address } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "media_preparation_failed",
                "address": address,
            }),
        ),
        ExtractionPlanError::MediaPayloadChanged { resource } => (
            CliErrorCode::ExportSourceChanged,
            json!({
                "kind": "media_payload_changed",
                "resource": resource,
            }),
        ),
        ExtractionPlanError::RequiredDecodedUnavailable { address, reason } => (
            CliErrorCode::ExportRepresentationUnavailable,
            json!({
                "kind": "representation_unavailable",
                "address": address,
                "diagnostic": reason,
            }),
        ),
        ExtractionPlanError::ExecutionCapabilityUnavailable {
            ordinal,
            capability,
        } => (
            CliErrorCode::ExportRepresentationUnavailable,
            json!({
                "kind": "execution_capability_unavailable",
                "ordinal": ordinal,
                "capability": capability,
            }),
        ),
        ExtractionPlanError::ObjectUnloaded(address) => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "object_unloaded", "address": address }),
        ),
        ExtractionPlanError::ObjectMissing(address) => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "object_missing", "address": address }),
        ),
        ExtractionPlanError::ObjectAmbiguous {
            address,
            candidates,
        } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "object_ambiguous",
                "address": address,
                "candidate_count": candidates,
            }),
        ),
        ExtractionPlanError::ObjectInvalid(address) => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "object_invalid", "address": address }),
        ),
        ExtractionPlanError::InvalidStreamPath(stream_path) => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "invalid_stream_resource_path",
                "stream_path": stream_path,
            }),
        ),
        ExtractionPlanError::InvalidStreamRange { offset, size } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "invalid_stream_resource_range",
                "offset": offset,
                "size": size,
            }),
        ),
        ExtractionPlanError::MissingStreamResource { owner, stream_path } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "stream_resource_missing",
                "owner": owner,
                "stream_path": stream_path,
            }),
        ),
        ExtractionPlanError::AmbiguousStreamResource { owner, stream_path } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "stream_resource_ambiguous",
                "owner": owner,
                "stream_path": stream_path,
            }),
        ),
        ExtractionPlanError::Workspace(WorkspaceError::RangeOutOfBounds {
            source_id,
            offset,
            end,
            source_len,
        }) => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "stream_resource_out_of_range",
                "source_id": source_id,
                "offset": offset,
                "end": end,
                "source_length": source_len,
            }),
        ),
        ExtractionPlanError::Reference(source) => reference_plan_error_metadata(source),
        ExtractionPlanError::PlanDerivationMismatch { kind } => (
            CliErrorCode::ExportPlanRejected,
            json!({
                "kind": "plan_derivation_mismatch",
                "mismatch": kind.to_string(),
            }),
        ),
        ExtractionPlanError::Model(_) | ExtractionPlanError::ModelValidation(_) => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "plan_rejected" }),
        ),
        _ => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "plan_rejected" }),
        ),
    }
}

fn reference_plan_error_metadata(error: &ReferenceGraphError) -> (CliErrorCode, Value) {
    match error {
        ReferenceGraphError::Budget(source)
        | ReferenceGraphError::Workspace(WorkspaceError::Budget(source)) => (
            CliErrorCode::ExportBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "budget": budget_details(source),
            }),
        ),
        ReferenceGraphError::Workspace(_) => (
            CliErrorCode::ExportSourceChanged,
            json!({ "kind": "reference_graph_source_failed" }),
        ),
        _ => (
            CliErrorCode::ExportPlanRejected,
            json!({ "kind": "reference_graph_rejected" }),
        ),
    }
}

#[cfg(feature = "decode")]
fn media_inspection_error_details(error: &MediaInspectionError) -> Value {
    match error {
        MediaInspectionError::NotApplicable { expected, actual } => json!({
            "kind": "wrong_class",
            "expected_class_id": expected,
            "actual_class_id": actual,
        }),
        MediaInspectionError::TypeTreeUnavailable => {
            json!({ "kind": "typetree_unavailable" })
        }
        MediaInspectionError::InvalidDescriptor { field, reason } => json!({
            "kind": "invalid_descriptor_field",
            "field": field,
            "reason": reason,
        }),
        MediaInspectionError::UnsupportedEncoding { family, value } => json!({
            "kind": "unsupported_encoding",
            "family": family,
            "value": value,
        }),
        MediaInspectionError::UnsupportedLayout { family, layout } => json!({
            "kind": "unsupported_layout",
            "family": family,
            "layout": layout,
        }),
        MediaInspectionError::MissingPayload => json!({ "kind": "missing_payload" }),
        MediaInspectionError::AmbiguousPayload => json!({ "kind": "ambiguous_payload" }),
        MediaInspectionError::StreamRangeOverflow { offset, size } => json!({
            "kind": "stream_range_overflow",
            "offset": offset,
            "size": size,
        }),
        MediaInspectionError::UnsupportedRawLayout { layout } => json!({
            "kind": "unsupported_raw_layout",
            "layout": layout,
        }),
    }
}

pub(crate) fn mark_export_execution_error(error: ExtractionExecutionError) -> anyhow::Error {
    let (code, details) = match &error {
        ExtractionExecutionError::Budget(source)
        | ExtractionExecutionError::Workspace(WorkspaceError::Budget(source)) => (
            CliErrorCode::ExportBudgetExceeded,
            json!({
                "kind": "budget_exceeded",
                "budget": budget_details(source),
            }),
        ),
        ExtractionExecutionError::PlanVerification(source) => export_plan_error_metadata(source),
        ExtractionExecutionError::Allocation {
            resource,
            requested,
            unit,
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "allocation_failed",
                "resource": resource,
                "requested": requested,
                "unit": unit.as_str(),
            }),
        ),
        ExtractionExecutionError::WorkspaceContextMismatch => (
            CliErrorCode::ExportWorkspaceMismatch,
            json!({ "kind": "workspace_revision_mismatch" }),
        ),
        ExtractionExecutionError::SourceChanged { locator } => (
            CliErrorCode::ExportSourceChanged,
            json!({ "kind": "source_changed", "locator": locator }),
        ),
        ExtractionExecutionError::InvalidLimit { resource } => (
            CliErrorCode::ExportResourceLimit,
            json!({ "kind": "invalid_limit", "resource": resource }),
        ),
        ExtractionExecutionError::OpenFileLimitTooSmall { minimum, limit } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "open_file_limit_too_small",
                "minimum": minimum,
                "limit": limit,
            }),
        ),
        ExtractionExecutionError::WorkingSetExceedsLimit {
            ordinal,
            required,
            limit,
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "working_set_exceeds_limit",
                "ordinal": ordinal,
                "required": required,
                "limit": limit,
            }),
        ),
        ExtractionExecutionError::WorkingSetUnderdeclared {
            ordinal,
            declared,
            required,
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "working_set_underdeclared",
                "ordinal": ordinal,
                "declared": declared,
                "required": required,
            }),
        ),
        ExtractionExecutionError::WorkingSetProofFailed { ordinal } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "working_set_proof_failed",
                "ordinal": ordinal,
            }),
        ),
        ExtractionExecutionError::MediaDescriptorChanged { ordinal } => (
            CliErrorCode::ExportExecutionFailed,
            json!({
                "kind": "media_descriptor_changed",
                "ordinal": ordinal,
            }),
        ),
        ExtractionExecutionError::MediaPreparationFailed { ordinal } => (
            CliErrorCode::ExportExecutionFailed,
            json!({
                "kind": "media_preparation_failed",
                "ordinal": ordinal,
            }),
        ),
        ExtractionExecutionError::ReportLimitExceeded { required, limit } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "report_limit_exceeded",
                "required": required,
                "limit": limit,
            }),
        ),
        ExtractionExecutionError::ManifestOutputLimitExceeded { required, limit } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "manifest_output_limit_exceeded",
                "required": required,
                "limit": limit,
            }),
        ),
        ExtractionExecutionError::ResumePlanMismatch => (
            CliErrorCode::ExportResumeMismatch,
            json!({ "kind": "resume_plan_mismatch" }),
        ),
        ExtractionExecutionError::OutputLayout { kind, .. } => (
            CliErrorCode::ExportOutputInvalid,
            json!({ "kind": kind.as_str() }),
        ),
        ExtractionExecutionError::PublicationJournalInvalid { .. } => (
            CliErrorCode::ExportRecoveryRequired,
            json!({ "kind": "publication_journal_invalid" }),
        ),
        ExtractionExecutionError::PublicationJournalLimitExceeded { required, limit } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "publication_journal_limit_exceeded",
                "required": required,
                "limit": limit,
            }),
        ),
        ExtractionExecutionError::EvidenceVerificationLimitExceeded {
            required,
            remaining,
        } => (
            CliErrorCode::ExportResourceLimit,
            json!({
                "kind": "evidence_verification_limit_exceeded",
                "required": required,
                "remaining": remaining,
            }),
        ),
        ExtractionExecutionError::PublicationJournalConflict { reason } => (
            CliErrorCode::ExportRecoveryRequired,
            json!({
                "kind": "publication_journal_conflict",
                "reason": reason,
            }),
        ),
        ExtractionExecutionError::PublicationRecoveryRequired { stage } => (
            CliErrorCode::ExportRecoveryRequired,
            json!({
                "kind": "publication_recovery_required",
                "stage": stage,
            }),
        ),
        _ => (
            CliErrorCode::ExportExecutionFailed,
            json!({ "kind": "execution_failed" }),
        ),
    };
    mark(error, code, Some(details))
}

pub(crate) fn resolve_lookup<T: Serialize>(lookup: WorkspaceLookup<T>) -> Result<T, anyhow::Error> {
    match lookup {
        WorkspaceLookup::Resolved(value) => Ok(value),
        WorkspaceLookup::Unloaded => Err(mark(
            anyhow::Error::msg("Object source is not loaded"),
            CliErrorCode::LookupUnloaded,
            Some(json!({ "kind": "unloaded" })),
        )),
        WorkspaceLookup::Missing => Err(mark(
            anyhow::Error::msg("ObjectAddress does not resolve to an object"),
            CliErrorCode::LookupMissing,
            Some(json!({ "kind": "missing" })),
        )),
        WorkspaceLookup::Ambiguous { candidates } => {
            let candidate_count = candidates.len();
            let candidates = serde_json::to_value(&candidates).ok();
            Err(mark(
                anyhow::Error::msg("ObjectAddress resolves to multiple loaded objects"),
                CliErrorCode::LookupAmbiguous,
                Some(json!({
                    "kind": "ambiguous",
                    "candidate_count": candidate_count,
                    "candidates": candidates,
                })),
            ))
        }
        WorkspaceLookup::Invalid { diagnostic } => {
            let diagnostic = serde_json::to_value(&diagnostic).ok();
            Err(mark(
                anyhow::Error::msg("ObjectAddress is invalid"),
                CliErrorCode::LookupInvalid,
                Some(json!({
                    "kind": "invalid",
                    "diagnostic": diagnostic,
                })),
            ))
        }
    }
}

pub(crate) fn report_parts(error: &anyhow::Error) -> (&'static str, Option<Value>) {
    error
        .downcast_ref::<CliErrorMetadata>()
        .map_or(("CLI_COMMAND_FAILED", None), |metadata| {
            (metadata.code.as_str(), metadata.details.clone())
        })
}

fn mark(
    error: impl Into<anyhow::Error>,
    code: CliErrorCode,
    details: Option<Value>,
) -> anyhow::Error {
    error.into().context(CliErrorMetadata { code, details })
}

fn destination_state(state: CommitDestinationState) -> Value {
    match state {
        CommitDestinationState::Existing(fingerprint) => {
            json!({ "state": "existing", "fingerprint": fingerprint })
        }
        CommitDestinationState::Absent => json!({ "state": "absent" }),
        CommitDestinationState::Directory => json!({ "state": "directory" }),
        CommitDestinationState::SymbolicLink => json!({ "state": "symbolic_link" }),
        CommitDestinationState::Other => json!({ "state": "other" }),
    }
}

fn recovery_locator_details(locator: &RecoveryLocator) -> Value {
    serde_json::to_value(locator).unwrap_or_else(|error| {
        json!({
            "version": locator.version(),
            "root": path_details(locator.root()),
            "transaction": locator.transaction().to_string(),
            "round_trip": false,
            "serialization_error": error.to_string(),
        })
    })
}

fn path_details(path: &Path) -> Value {
    let display = path.to_string_lossy().into_owned();
    match path.to_str() {
        Some(value) => json!({
            "display": display,
            "encoding": "utf8",
            "value": value,
        }),
        None => native_path_details(path, display),
    }
}

#[cfg(unix)]
fn native_path_details(path: &Path, display: String) -> Value {
    use std::os::unix::ffi::OsStrExt as _;

    let value = path
        .as_os_str()
        .as_bytes()
        .iter()
        .copied()
        .map(Value::from)
        .collect::<Vec<_>>();
    json!({
        "display": display,
        "encoding": "unix_bytes",
        "value": value,
    })
}

#[cfg(windows)]
fn native_path_details(path: &Path, display: String) -> Value {
    use std::os::windows::ffi::OsStrExt as _;

    let value = path
        .as_os_str()
        .encode_wide()
        .map(Value::from)
        .collect::<Vec<_>>();
    json!({
        "display": display,
        "encoding": "windows_utf16",
        "value": value,
    })
}

#[cfg(not(any(unix, windows)))]
fn native_path_details(_path: &Path, display: String) -> Value {
    let value = display.clone();
    json!({
        "display": display,
        "encoding": "lossy_utf8",
        "value": value,
    })
}

fn budget_details(error: &BudgetError) -> Value {
    match error {
        BudgetError::InvalidLimit { resource } => {
            json!({ "kind": "invalid_limit", "resource": resource })
        }
        BudgetError::ArithmeticOverflow { resource } => {
            json!({ "kind": "arithmetic_overflow", "resource": resource })
        }
        BudgetError::DomainMismatch { resource } => {
            json!({ "kind": "domain_mismatch", "resource": resource })
        }
        BudgetError::Exceeded {
            resource,
            limit,
            requested,
        } => {
            json!({
                "kind": "exceeded",
                "resource": resource,
                "limit": limit,
                "requested": requested,
            })
        }
        BudgetError::ExpansionRatioExceeded {
            compressed_bytes,
            decompressed_bytes,
            max_ratio,
        } => {
            json!({
                "kind": "expansion_ratio_exceeded",
                "compressed_bytes": compressed_bytes,
                "decompressed_bytes": decompressed_bytes,
                "max_ratio": max_ratio,
            })
        }
    }
}

fn commit_contract_details(error: &CommitContractError) -> Value {
    match error {
        CommitContractError::UnsupportedVersion(version) => {
            json!({ "kind": "unsupported_version", "version": version })
        }
        CommitContractError::TransactionMismatch => {
            json!({ "kind": "transaction_mismatch" })
        }
        CommitContractError::WorkspaceMismatch => json!({ "kind": "workspace_mismatch" }),
        CommitContractError::RevisionMismatch => json!({ "kind": "revision_mismatch" }),
        CommitContractError::RecoveryTransactionMismatch => {
            json!({ "kind": "recovery_transaction_mismatch" })
        }
        CommitContractError::EmptyArtifactSet => json!({ "kind": "empty_artifact_set" }),
        CommitContractError::ArtifactOrder => json!({ "kind": "artifact_order" }),
    }
}

fn recovery_discovery_blocked_reason(reason: &RecoveryDiscoveryBlockedReason) -> Value {
    let kind = match reason {
        RecoveryDiscoveryBlockedReason::UnsupportedEvidence => "unsupported_evidence",
        RecoveryDiscoveryBlockedReason::LegacyTransactionEvidence => "legacy_transaction_evidence",
        RecoveryDiscoveryBlockedReason::FutureProtocolVersion => "future_protocol_version",
        RecoveryDiscoveryBlockedReason::UnsafeFilesystemState => "unsafe_filesystem_state",
    };
    json!({ "kind": kind })
}

fn recovery_blocked_reason_details(reason: &RecoveryBlockedReason) -> Value {
    match reason {
        RecoveryBlockedReason::InvalidLocator { message } => {
            json!({ "kind": "invalid_locator", "reason": message })
        }
        RecoveryBlockedReason::InvalidJournal { message } => {
            json!({ "kind": "invalid_journal", "reason": message })
        }
        RecoveryBlockedReason::UnsafePath { artifact, role } => {
            json!({ "kind": "unsafe_path", "artifact": artifact, "role": role })
        }
        RecoveryBlockedReason::UnexpectedEvidence { artifact } => {
            json!({ "kind": "unexpected_evidence", "artifact": artifact })
        }
        RecoveryBlockedReason::ConflictingDecision => {
            json!({ "kind": "conflicting_decision" })
        }
        RecoveryBlockedReason::InvalidEventSequence { message } => {
            json!({ "kind": "invalid_event_sequence", "reason": message })
        }
        RecoveryBlockedReason::WorkspaceMismatch { expected, actual } => {
            json!({
                "kind": "workspace_mismatch",
                "expected_workspace_id": expected,
                "actual_workspace_id": actual,
            })
        }
        RecoveryBlockedReason::FilesystemRecoveryRequired => {
            json!({ "kind": "filesystem_recovery_required" })
        }
        RecoveryBlockedReason::BaselineUnavailable { expected, actual } => {
            json!({
                "kind": "baseline_unavailable",
                "expected_revision": expected,
                "actual_revision": actual,
            })
        }
        RecoveryBlockedReason::InstallationUnavailable {
            base,
            committed,
            actual,
        } => {
            json!({
                "kind": "installation_unavailable",
                "base_installation": base,
                "committed_installation": committed,
                "actual_installation": actual,
            })
        }
        RecoveryBlockedReason::BaselineRebuild { message } => {
            json!({ "kind": "baseline_rebuild", "reason": message })
        }
        RecoveryBlockedReason::Io { message } => {
            json!({ "kind": "io", "reason": message })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_output_error_preserves_its_stable_stage() {
        let error = mark_export_execution_error(ExtractionExecutionError::OutputLayout {
            kind: unity_asset::extraction::ExtractionOutputErrorKind::LockRoot,
            message: "fixture lock failure".to_owned(),
        });
        let (code, details) = report_parts(&error);

        assert_eq!(code, "CLI_EXPORT_OUTPUT_INVALID");
        assert_eq!(details.unwrap()["kind"], "lock_root");
    }

    #[test]
    fn reference_graph_budgets_remain_budget_errors_during_plan_verification() {
        for nested_workspace in [false, true] {
            let make_plan_error = || {
                let source = BudgetError::Exceeded {
                    resource: "reference nodes",
                    limit: 4,
                    requested: 5,
                };
                if nested_workspace {
                    ExtractionPlanError::Reference(Box::new(ReferenceGraphError::Workspace(
                        WorkspaceError::Budget(source),
                    )))
                } else {
                    ExtractionPlanError::Reference(Box::new(ReferenceGraphError::Budget(source)))
                }
            };

            for error in [
                mark_export_plan_error(make_plan_error()),
                mark_export_execution_error(ExtractionExecutionError::PlanVerification(Box::new(
                    make_plan_error(),
                ))),
            ] {
                let (code, details) = report_parts(&error);
                let details = details.expect("typed nested budget details");

                assert_eq!(code, "CLI_EXPORT_BUDGET_EXCEEDED");
                assert_eq!(details["kind"], "budget_exceeded");
                assert_eq!(details["budget"]["kind"], "exceeded");
                assert_eq!(details["budget"]["resource"], "reference nodes");
            }
        }
    }

    #[test]
    fn plan_verification_preserves_plan_rejection_categories() {
        let mismatch = mark_export_execution_error(ExtractionExecutionError::PlanVerification(
            Box::new(ExtractionPlanError::PlanDerivationMismatch {
                kind: unity_asset::extraction::ExtractionPlanMismatchKind::Artifacts,
            }),
        ));
        let (code, details) = report_parts(&mismatch);
        let details = details.expect("typed derivation mismatch details");
        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details["kind"], "plan_derivation_mismatch");
        assert_eq!(details["mismatch"], "artifact contracts");

        let model =
            mark_export_execution_error(ExtractionExecutionError::PlanVerification(Box::new(
                ExtractionPlanError::Model("fixture model rejection".to_owned()),
            )));
        let (code, details) = report_parts(&model);
        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details.unwrap()["kind"], "plan_rejected");

        let unavailable = mark_export_execution_error(ExtractionExecutionError::PlanVerification(
            Box::new(ExtractionPlanError::ExecutionCapabilityUnavailable {
                ordinal: 3,
                capability: "media decode",
            }),
        ));
        let (code, details) = report_parts(&unavailable);
        let details = details.expect("typed execution capability details");
        assert_eq!(code, "CLI_EXPORT_REPRESENTATION_UNAVAILABLE");
        assert_eq!(details["kind"], "execution_capability_unavailable");
        assert_eq!(details["ordinal"], 3);
        assert_eq!(details["capability"], "media decode");
    }

    #[test]
    fn media_execution_failures_preserve_the_artifact_and_failure_kind() {
        for (error, expected_kind) in [
            (
                ExtractionExecutionError::MediaDescriptorChanged { ordinal: 7 },
                "media_descriptor_changed",
            ),
            (
                ExtractionExecutionError::MediaPreparationFailed { ordinal: 7 },
                "media_preparation_failed",
            ),
        ] {
            let error = mark_export_execution_error(error);
            let (code, details) = report_parts(&error);
            let details = details.expect("typed media execution details");

            assert_eq!(code, "CLI_EXPORT_EXECUTION_FAILED");
            assert_eq!(details["kind"], expected_kind);
            assert_eq!(details["ordinal"], 7);
        }
    }

    #[test]
    fn evidence_verification_limit_remains_a_resource_error() {
        let error = mark_export_execution_error(
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: 8,
                remaining: 7,
            },
        );
        let (code, details) = report_parts(&error);
        let details = details.expect("typed physical verification details");

        assert_eq!(code, "CLI_EXPORT_RESOURCE_LIMIT");
        assert_eq!(details["kind"], "evidence_verification_limit_exceeded");
        assert_eq!(details["required"], 8);
        assert_eq!(details["remaining"], 7);
    }

    #[cfg(feature = "decode")]
    #[test]
    fn invalid_media_descriptor_preserves_structured_inspection_evidence() {
        let address = unity_asset::ObjectAddress::binary_direct(
            unity_asset::SourceLocator::path("content.assets").unwrap(),
            41,
        )
        .unwrap();
        let error = mark_export_plan_error(ExtractionPlanError::InvalidMediaDescriptor {
            address,
            source: MediaInspectionError::InvalidDescriptor {
                field: "stream.size",
                reason: "must be non-zero",
            },
        });
        let (code, details) = report_parts(&error);
        let details = details.expect("typed media inspection details");

        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details["kind"], "invalid_media_descriptor");
        assert_eq!(details["inspection"]["kind"], "invalid_descriptor_field");
        assert_eq!(details["inspection"]["field"], "stream.size");
        assert_eq!(details["inspection"]["reason"], "must be non-zero");
    }

    #[test]
    fn media_plan_failures_preserve_remediation_category() {
        let address = unity_asset::ObjectAddress::binary_direct(
            unity_asset::SourceLocator::path("content.assets").unwrap(),
            41,
        )
        .unwrap();
        let preparation = mark_export_plan_error(ExtractionPlanError::MediaPreparation { address });
        let (code, details) = report_parts(&preparation);
        let details = details.expect("typed media preparation details");
        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details["kind"], "media_preparation_failed");

        let changed = mark_export_plan_error(ExtractionPlanError::MediaPayloadChanged {
            resource: "planned embedded texture",
        });
        let (code, details) = report_parts(&changed);
        let details = details.expect("typed media source details");
        assert_eq!(code, "CLI_EXPORT_SOURCE_CHANGED");
        assert_eq!(details["kind"], "media_payload_changed");
        assert_eq!(details["resource"], "planned embedded texture");

        let invalid_path =
            mark_export_plan_error(ExtractionPlanError::InvalidStreamPath(".".to_owned()));
        let (code, details) = report_parts(&invalid_path);
        let details = details.expect("typed streamed-resource path details");
        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details["kind"], "invalid_stream_resource_path");
        assert_eq!(details["stream_path"], ".");

        let invalid_range = mark_export_plan_error(ExtractionPlanError::InvalidStreamRange {
            offset: u64::MAX,
            size: 1,
        });
        let (code, details) = report_parts(&invalid_range);
        let details = details.expect("typed streamed-resource range details");
        assert_eq!(code, "CLI_EXPORT_PLAN_REJECTED");
        assert_eq!(details["kind"], "invalid_stream_resource_range");
        assert_eq!(details["offset"], u64::MAX);
        assert_eq!(details["size"], 1);
    }

    #[test]
    fn export_allocation_failures_are_resource_limits() {
        let mut impossible = Vec::<u8>::new();
        let source = impossible
            .try_reserve(usize::MAX)
            .expect_err("impossible capacity must fail");
        let generic = mark_export_plan_error(ExtractionPlanError::Allocation {
            resource: "planned artifacts",
            requested: usize::MAX,
            unit: ExtractionAllocationUnit::CapacityUnits,
            source,
        });
        let (code, details) = report_parts(&generic);
        let details = details.expect("typed generic allocation details");
        assert_eq!(code, "CLI_EXPORT_RESOURCE_LIMIT");
        assert_eq!(details["kind"], "allocation_failed");
        assert_eq!(details["resource"], "planned artifacts");
        assert_eq!(details["unit"], "capacity_units");

        let plan_error = mark_export_plan_error(ExtractionPlanError::MediaAllocation {
            resource: "decoded texture",
            requested: 4_096,
        });
        let (code, details) = report_parts(&plan_error);
        let details = details.expect("typed planning allocation details");
        assert_eq!(code, "CLI_EXPORT_RESOURCE_LIMIT");
        assert_eq!(details["kind"], "allocation_failed");
        assert_eq!(details["resource"], "decoded texture");
        assert_eq!(details["requested"], 4_096);
        assert_eq!(details["unit"], "bytes");

        let execution_error = mark_export_execution_error(ExtractionExecutionError::Allocation {
            resource: "extraction outcomes",
            requested: 32,
            unit: ExtractionAllocationUnit::Bytes,
        });
        let (code, details) = report_parts(&execution_error);
        let details = details.expect("typed execution allocation details");
        assert_eq!(code, "CLI_EXPORT_RESOURCE_LIMIT");
        assert_eq!(details["kind"], "allocation_failed");
        assert_eq!(details["resource"], "extraction outcomes");
        assert_eq!(details["requested"], 32);
        assert_eq!(details["unit"], "bytes");
    }

    #[test]
    fn recovery_installation_mismatch_preserves_all_digest_evidence() {
        let base = unity_asset::workspace::WorkspaceInstallationDigest::new(
            unity_asset::DigestV1::hash_bytes(b"base"),
        );
        let committed = unity_asset::workspace::WorkspaceInstallationDigest::new(
            unity_asset::DigestV1::hash_bytes(b"committed"),
        );
        let actual = unity_asset::workspace::WorkspaceInstallationDigest::new(
            unity_asset::DigestV1::hash_bytes(b"actual"),
        );

        let details =
            recovery_blocked_reason_details(&RecoveryBlockedReason::InstallationUnavailable {
                base,
                committed,
                actual,
            });

        assert_eq!(details["kind"], "installation_unavailable");
        assert_eq!(details["base_installation"], json!(base));
        assert_eq!(details["committed_installation"], json!(committed));
        assert_eq!(details["actual_installation"], json!(actual));
    }

    #[cfg(unix)]
    #[test]
    fn publication_error_preserves_non_unicode_unix_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_vec(vec![b'f', 0x80]));
        let error = mark_publication_target_error(PublicationTargetError::NotAbsolute(path));
        let (code, details) = report_parts(&error);
        let details = details.expect("typed details");

        assert_eq!(code, "CLI_WORKSPACE_PUBLICATION_TARGET_INVALID");
        assert_eq!(details["path"]["encoding"], "unix_bytes");
        assert_eq!(details["path"]["value"], json!([102, 128]));
    }

    #[cfg(windows)]
    #[test]
    fn publication_error_preserves_non_unicode_windows_path_units() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_wide(&[u16::from(b'f'), 0xd800]));
        let error = mark_publication_target_error(PublicationTargetError::NotAbsolute(path));
        let (code, details) = report_parts(&error);
        let details = details.expect("typed details");

        assert_eq!(code, "CLI_WORKSPACE_PUBLICATION_TARGET_INVALID");
        assert_eq!(details["path"]["encoding"], "windows_utf16");
        assert_eq!(details["path"]["value"], json!([102, 0xd800]));
    }
}
