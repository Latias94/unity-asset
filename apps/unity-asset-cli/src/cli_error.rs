use std::fmt;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use unity_asset::workspace::{
    CommitContractError, CommitDestinationState, CommitError, PrepareError, PublicationTargetError,
    RecoveryBlockedReason, RecoveryDiscoveryBlockedReason, RecoveryDiscoveryError, RecoveryError,
    RecoveryLocator, WorkspaceLookup,
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
