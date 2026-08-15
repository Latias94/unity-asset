use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::validation::{ContractValidationError, ValidateContract};
use crate::{
    ApiError, DaemonInstanceId, FilesystemReindexIntent, OperationId, ProjectId, QueryPolicyId,
    ReferenceRequest, ReferencesResponse, ReindexReceipt, RequestId, SEARCH_PROTOCOL_REVISION,
    SearchCapabilities, SearchResponse, StatusResponse, SuggestResponse,
};

pub const BUSINESS_PROTOCOL_REVISION: u16 = SEARCH_PROTOCOL_REVISION;
pub const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
pub const MAX_SEARCH_RESULTS: u32 = 1_000;
pub const MAX_SUGGEST_PREFIX_BYTES: usize = 4 * 1024;
pub const MAX_SUGGEST_RESULTS: u32 = 50;
pub const MAX_REFERENCE_RESULTS: u32 = 500;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_WAIT_TIMEOUT_MS: u32 = 5 * 60 * 1_000;
pub const MAX_SHUTDOWN_DRAIN_MS: u32 = 60 * 1_000;
pub const MAX_BACKGROUND_REINDEX_OPERATIONS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Capabilities,
    Status,
    Search,
    Suggest,
    References,
    ReindexAdmit,
    ReindexStatus,
    ReindexWait,
    ReindexCancel,
    Shutdown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    pub limit: u32,
}

impl ValidateContract for SearchRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(ContractValidationError::ByteLimit {
                field: "search.query",
                actual: self.query.len(),
                maximum: MAX_SEARCH_QUERY_BYTES,
            });
        }
        ensure_numeric_limit("search.limit", self.limit, MAX_SEARCH_RESULTS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestRequest {
    pub prefix: String,
    pub limit: u32,
}

impl ValidateContract for SuggestRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.prefix.len() > MAX_SUGGEST_PREFIX_BYTES {
            return Err(ContractValidationError::ByteLimit {
                field: "suggest.prefix",
                actual: self.prefix.len(),
                maximum: MAX_SUGGEST_PREFIX_BYTES,
            });
        }
        if self.limit == 0 {
            return Err(ContractValidationError::Empty {
                field: "suggest.limit",
            });
        }
        ensure_numeric_limit("suggest.limit", self.limit, MAX_SUGGEST_RESULTS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexAdmitRequest {
    pub intent: FilesystemReindexIntent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl ValidateContract for ReindexAdmitRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        self.intent.validate()?;
        if let Some(key) = &self.idempotency_key {
            if key.is_empty() {
                return Err(ContractValidationError::Empty {
                    field: "reindex.idempotency_key",
                });
            }
            if key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
                return Err(ContractValidationError::ByteLimit {
                    field: "reindex.idempotency_key",
                    actual: key.len(),
                    maximum: MAX_IDEMPOTENCY_KEY_BYTES,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexStatusRequest {
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexWaitRequest {
    pub operation_id: OperationId,
    pub timeout_ms: u32,
}

impl ValidateContract for ReindexWaitRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.timeout_ms == 0 {
            return Err(ContractValidationError::Empty {
                field: "reindex_wait.timeout_ms",
            });
        }
        ensure_numeric_limit(
            "reindex_wait.timeout_ms",
            self.timeout_ms,
            MAX_WAIT_TIMEOUT_MS,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexCancelRequest {
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownRequest {
    pub drain_timeout_ms: u32,
}

impl ValidateContract for ShutdownRequest {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_numeric_limit(
            "shutdown.drain_timeout_ms",
            self.drain_timeout_ms,
            MAX_SHUTDOWN_DRAIN_MS,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RequestOperation {
    Capabilities(CapabilitiesRequest),
    Status(StatusRequest),
    Search(SearchRequest),
    Suggest(SuggestRequest),
    References(ReferenceRequest),
    ReindexAdmit(ReindexAdmitRequest),
    ReindexStatus(ReindexStatusRequest),
    ReindexWait(ReindexWaitRequest),
    ReindexCancel(ReindexCancelRequest),
    Shutdown(ShutdownRequest),
}

impl RequestOperation {
    #[must_use]
    pub const fn kind(&self) -> OperationKind {
        match self {
            Self::Capabilities(_) => OperationKind::Capabilities,
            Self::Status(_) => OperationKind::Status,
            Self::Search(_) => OperationKind::Search,
            Self::Suggest(_) => OperationKind::Suggest,
            Self::References(_) => OperationKind::References,
            Self::ReindexAdmit(_) => OperationKind::ReindexAdmit,
            Self::ReindexStatus(_) => OperationKind::ReindexStatus,
            Self::ReindexWait(_) => OperationKind::ReindexWait,
            Self::ReindexCancel(_) => OperationKind::ReindexCancel,
            Self::Shutdown(_) => OperationKind::Shutdown,
        }
    }

    #[must_use]
    pub const fn max_encoded_bytes(&self) -> usize {
        match self {
            Self::ReindexAdmit(_) => 512 * 1024,
            Self::References(_) => 64 * 1024,
            Self::Capabilities(_)
            | Self::Status(_)
            | Self::Search(_)
            | Self::Suggest(_)
            | Self::ReindexStatus(_)
            | Self::ReindexWait(_)
            | Self::ReindexCancel(_)
            | Self::Shutdown(_) => 16 * 1024,
        }
    }
}

impl ValidateContract for RequestOperation {
    fn validate(&self) -> Result<(), ContractValidationError> {
        match self {
            Self::Capabilities(_)
            | Self::Status(_)
            | Self::ReindexStatus(_)
            | Self::ReindexCancel(_) => Ok(()),
            Self::Search(request) => request.validate(),
            Self::Suggest(request) => request.validate(),
            Self::References(request) => request.validate(),
            Self::ReindexAdmit(request) => request.validate(),
            Self::ReindexWait(request) => request.validate(),
            Self::Shutdown(request) => request.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    protocol_revision: u16,
    request_id: RequestId,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    query_policy_id: QueryPolicyId,
    operation: RequestOperation,
}

impl RequestEnvelope {
    pub fn new(
        protocol_revision: u16,
        request_id: RequestId,
        project_id: ProjectId,
        daemon_instance_id: DaemonInstanceId,
        query_policy_id: QueryPolicyId,
        operation: RequestOperation,
    ) -> Result<Self, ContractValidationError> {
        let request = Self {
            protocol_revision,
            request_id,
            project_id,
            daemon_instance_id,
            query_policy_id,
            operation,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ContractValidationError> {
        <Self as ValidateContract>::validate(self)
    }

    pub fn validate_binding(
        &self,
        expected_project: ProjectId,
        expected_instance: DaemonInstanceId,
        expected_query_policy: QueryPolicyId,
    ) -> Result<(), ContractValidationError> {
        self.validate()?;
        self.ensure_binding(expected_project, expected_instance, expected_query_policy)
    }

    pub(crate) fn ensure_binding(
        &self,
        expected_project: ProjectId,
        expected_instance: DaemonInstanceId,
        expected_query_policy: QueryPolicyId,
    ) -> Result<(), ContractValidationError> {
        if self.project_id != expected_project {
            return Err(ContractValidationError::Inconsistent {
                field: "request project",
            });
        }
        if self.daemon_instance_id != expected_instance {
            return Err(ContractValidationError::Inconsistent {
                field: "request daemon instance",
            });
        }
        if self.query_policy_id != expected_query_policy {
            return Err(ContractValidationError::Inconsistent {
                field: "request query policy",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn protocol_revision(&self) -> u16 {
        self.protocol_revision
    }

    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn daemon_instance_id(&self) -> DaemonInstanceId {
        self.daemon_instance_id
    }

    #[must_use]
    pub const fn query_policy_id(&self) -> QueryPolicyId {
        self.query_policy_id
    }

    #[must_use]
    pub const fn operation(&self) -> &RequestOperation {
        &self.operation
    }

    #[must_use]
    pub fn into_operation(self) -> RequestOperation {
        self.operation
    }
}

impl ValidateContract for RequestEnvelope {
    fn validate(&self) -> Result<(), ContractValidationError> {
        ensure_protocol_revision(self.protocol_revision)?;
        self.operation.validate()?;
        if let RequestOperation::References(request) = &self.operation
            && let Some(cursor) = &request.cursor
            && cursor.query_policy_id != self.query_policy_id
        {
            return Err(ContractValidationError::Inconsistent {
                field: "reference cursor query policy",
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ResponseExpectation {
    binding: ResponseBinding,
    operation: ExpectedResponseOperation<'static>,
}

impl ResponseExpectation {
    pub(crate) fn from_validated_request(request: &RequestEnvelope) -> Self {
        Self {
            binding: ResponseBinding::from_request(request),
            operation: ExpectedResponseOperation::from_request(&request.operation).into_owned(),
        }
    }

    pub(crate) fn operation_kind(&self) -> OperationKind {
        self.operation.kind()
    }
}

#[derive(Debug, Clone, Copy)]
struct ResponseBinding {
    protocol_revision: u16,
    request_id: RequestId,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    query_policy_id: QueryPolicyId,
}

impl ResponseBinding {
    const fn from_request(request: &RequestEnvelope) -> Self {
        Self {
            protocol_revision: request.protocol_revision,
            request_id: request.request_id,
            project_id: request.project_id,
            daemon_instance_id: request.daemon_instance_id,
            query_policy_id: request.query_policy_id,
        }
    }
}

#[derive(Debug)]
enum ExpectedResponseOperation<'request> {
    Capabilities,
    Status,
    Search {
        query: Cow<'request, str>,
        limit: u32,
    },
    Suggest {
        prefix: Cow<'request, str>,
        limit: u32,
    },
    References {
        request: Cow<'request, ReferenceRequest>,
    },
    ReindexAdmit,
    ReindexStatus {
        operation_id: OperationId,
    },
    ReindexWait {
        operation_id: OperationId,
    },
    ReindexCancel {
        operation_id: OperationId,
    },
    Shutdown,
}

impl<'request> ExpectedResponseOperation<'request> {
    fn from_request(operation: &'request RequestOperation) -> Self {
        match operation {
            RequestOperation::Capabilities(_) => Self::Capabilities,
            RequestOperation::Status(_) => Self::Status,
            RequestOperation::Search(request) => Self::Search {
                query: Cow::Borrowed(&request.query),
                limit: request.limit,
            },
            RequestOperation::Suggest(request) => Self::Suggest {
                prefix: Cow::Borrowed(&request.prefix),
                limit: request.limit,
            },
            RequestOperation::References(request) => Self::References {
                request: Cow::Borrowed(request),
            },
            RequestOperation::ReindexAdmit(_) => Self::ReindexAdmit,
            RequestOperation::ReindexStatus(request) => Self::ReindexStatus {
                operation_id: request.operation_id,
            },
            RequestOperation::ReindexWait(request) => Self::ReindexWait {
                operation_id: request.operation_id,
            },
            RequestOperation::ReindexCancel(request) => Self::ReindexCancel {
                operation_id: request.operation_id,
            },
            RequestOperation::Shutdown(_) => Self::Shutdown,
        }
    }

    fn into_owned(self) -> ExpectedResponseOperation<'static> {
        match self {
            Self::Capabilities => ExpectedResponseOperation::Capabilities,
            Self::Status => ExpectedResponseOperation::Status,
            Self::Search { query, limit } => ExpectedResponseOperation::Search {
                query: Cow::Owned(query.into_owned()),
                limit,
            },
            Self::Suggest { prefix, limit } => ExpectedResponseOperation::Suggest {
                prefix: Cow::Owned(prefix.into_owned()),
                limit,
            },
            Self::References { request } => ExpectedResponseOperation::References {
                request: Cow::Owned(request.into_owned()),
            },
            Self::ReindexAdmit => ExpectedResponseOperation::ReindexAdmit,
            Self::ReindexStatus { operation_id } => {
                ExpectedResponseOperation::ReindexStatus { operation_id }
            }
            Self::ReindexWait { operation_id } => {
                ExpectedResponseOperation::ReindexWait { operation_id }
            }
            Self::ReindexCancel { operation_id } => {
                ExpectedResponseOperation::ReindexCancel { operation_id }
            }
            Self::Shutdown => ExpectedResponseOperation::Shutdown,
        }
    }

    const fn kind(&self) -> OperationKind {
        match self {
            Self::Capabilities => OperationKind::Capabilities,
            Self::Status => OperationKind::Status,
            Self::Search { .. } => OperationKind::Search,
            Self::Suggest { .. } => OperationKind::Suggest,
            Self::References { .. } => OperationKind::References,
            Self::ReindexAdmit => OperationKind::ReindexAdmit,
            Self::ReindexStatus { .. } => OperationKind::ReindexStatus,
            Self::ReindexWait { .. } => OperationKind::ReindexWait,
            Self::ReindexCancel { .. } => OperationKind::ReindexCancel,
            Self::Shutdown => OperationKind::Shutdown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesResponse {
    pub daemon_version: String,
    pub capabilities: SearchCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReindexOperationState {
    Queued,
    Coalesced,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundReindexOrigin {
    Startup,
    Watcher,
    WatcherOverflow,
    Timer,
    SemanticUpgrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundReindexOperation {
    pub origin: BackgroundReindexOrigin,
    pub operation_id: OperationId,
    pub state: ReindexOperationState,
}

impl ValidateContract for BackgroundReindexOperation {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.state == ReindexOperationState::Lost {
            return Err(ContractValidationError::Inconsistent {
                field: "background reindex operation state",
            });
        }
        Ok(())
    }
}

pub(crate) fn validate_background_reindex_operations(
    operations: &[BackgroundReindexOperation],
) -> Result<(), ContractValidationError> {
    if operations.len() > MAX_BACKGROUND_REINDEX_OPERATIONS {
        return Err(ContractValidationError::EntryLimit {
            field: "background reindex operations",
            actual: operations.len(),
            maximum: MAX_BACKGROUND_REINDEX_OPERATIONS,
        });
    }
    for operation in operations {
        operation.validate()?;
    }
    if operations
        .windows(2)
        .any(|pair| pair[0].origin >= pair[1].origin)
    {
        return Err(ContractValidationError::NotStrictlyIncreasing {
            field: "background reindex operation origins",
        });
    }
    for (index, operation) in operations.iter().enumerate() {
        if operations[..index]
            .iter()
            .any(|previous| previous.operation_id == operation.operation_id)
        {
            return Err(ContractValidationError::Inconsistent {
                field: "background reindex operation IDs",
            });
        }
    }
    Ok(())
}

impl ReindexOperationState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Expired | Self::Lost
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexOperationStatus {
    pub operation_id: OperationId,
    pub state: ReindexOperationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<ReindexReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<ReindexReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl ValidateContract for ReindexOperationStatus {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if let Some(admission) = &self.admission {
            admission.validate()?;
        }
        if let Some(completion) = &self.completion {
            completion.validate()?;
        }
        if let Some(status) = &self.status {
            status.validate()?;
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        let valid = match self.state {
            ReindexOperationState::Queued
            | ReindexOperationState::Coalesced
            | ReindexOperationState::Running => {
                self.completion.is_none() && self.status.is_none() && self.error.is_none()
            }
            ReindexOperationState::Succeeded => {
                self.completion.is_some() && self.status.is_some() && self.error.is_none()
            }
            ReindexOperationState::Failed => {
                self.completion.is_none() && self.status.is_none() && self.error.is_some()
            }
            ReindexOperationState::Cancelled
            | ReindexOperationState::Expired
            | ReindexOperationState::Lost => {
                self.completion.is_none() && self.status.is_none() && self.error.is_none()
            }
        };
        if valid {
            if self.state == ReindexOperationState::Succeeded
                && let (Some(completion), Some(status)) = (&self.completion, &self.status)
            {
                let completion_generation = completion.generation.as_ref();
                let status_generation = status.generation.active.as_ref();
                if !matches!(
                    completion.disposition,
                    crate::ReindexDisposition::Applied | crate::ReindexDisposition::AlreadyApplied
                ) || completion_generation.is_none()
                    || completion_generation != status_generation
                    || status.indexing
                    || status.generation.building_revision.is_some()
                {
                    return Err(ContractValidationError::Inconsistent {
                        field: "reindex succeeded state",
                    });
                }
            }
            Ok(())
        } else {
            Err(ContractValidationError::Inconsistent {
                field: "reindex operation terminal state",
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexCancelResponse {
    pub operation_id: OperationId,
    pub state: ReindexOperationState,
    pub cancelled: bool,
}

impl ValidateContract for ReindexCancelResponse {
    fn validate(&self) -> Result<(), ContractValidationError> {
        if self.cancelled != (self.state == ReindexOperationState::Cancelled) {
            return Err(ContractValidationError::Inconsistent {
                field: "reindex cancellation result",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownResponse {
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResponseOperation {
    Capabilities(CapabilitiesResponse),
    Status(StatusResponse),
    Search(SearchResponse),
    Suggest(SuggestResponse),
    References(ReferencesResponse),
    ReindexAdmit(ReindexOperationStatus),
    ReindexStatus(ReindexOperationStatus),
    ReindexWait(ReindexOperationStatus),
    ReindexCancel(ReindexCancelResponse),
    Shutdown(ShutdownResponse),
}

impl ResponseOperation {
    #[must_use]
    pub const fn kind(&self) -> OperationKind {
        match self {
            Self::Capabilities(_) => OperationKind::Capabilities,
            Self::Status(_) => OperationKind::Status,
            Self::Search(_) => OperationKind::Search,
            Self::Suggest(_) => OperationKind::Suggest,
            Self::References(_) => OperationKind::References,
            Self::ReindexAdmit(_) => OperationKind::ReindexAdmit,
            Self::ReindexStatus(_) => OperationKind::ReindexStatus,
            Self::ReindexWait(_) => OperationKind::ReindexWait,
            Self::ReindexCancel(_) => OperationKind::ReindexCancel,
            Self::Shutdown(_) => OperationKind::Shutdown,
        }
    }

    #[must_use]
    pub const fn max_encoded_bytes(&self) -> usize {
        response_encoded_limit(self.kind())
    }
}

impl ValidateContract for ResponseOperation {
    fn validate(&self) -> Result<(), ContractValidationError> {
        match self {
            Self::Capabilities(response) => {
                if response.daemon_version.is_empty() {
                    return Err(ContractValidationError::Empty {
                        field: "capabilities daemon version",
                    });
                }
                response.capabilities.validate()
            }
            Self::Status(response) => response.validate(),
            Self::Search(response) => response.validate(),
            Self::Suggest(response) => response.validate(),
            Self::References(response) => response.validate(),
            Self::ReindexAdmit(status)
            | Self::ReindexStatus(status)
            | Self::ReindexWait(status) => status.validate(),
            Self::ReindexCancel(response) => response.validate(),
            Self::Shutdown(_) => Ok(()),
        }
    }
}

impl ResponseOperation {
    const fn query_policy_id(&self) -> Option<QueryPolicyId> {
        match self {
            Self::Status(response) => Some(response.query_policy_id),
            Self::Search(response) => Some(response.query_policy_id),
            Self::Suggest(response) => Some(response.query_policy_id),
            Self::References(response) => Some(response.query_policy_id),
            Self::ReindexAdmit(status)
            | Self::ReindexStatus(status)
            | Self::ReindexWait(status) => match &status.status {
                Some(response) => Some(response.query_policy_id),
                None => match &status.error {
                    Some(error) => error.query_policy_id,
                    None => None,
                },
            },
            Self::Capabilities(_) | Self::ReindexCancel(_) | Self::Shutdown(_) => None,
        }
    }
}

impl ExpectedResponseOperation<'_> {
    fn validate_response(
        &self,
        response: &ResponseOperation,
    ) -> Result<(), ContractValidationError> {
        if response.kind() != self.kind() {
            return Err(ContractValidationError::Inconsistent {
                field: "response operation kind",
            });
        }
        response.validate()?;

        let operation_ids_match = match (self, response) {
            (Self::ReindexStatus { operation_id }, ResponseOperation::ReindexStatus(actual)) => {
                *operation_id == actual.operation_id
            }
            (Self::ReindexWait { operation_id }, ResponseOperation::ReindexWait(actual)) => {
                *operation_id == actual.operation_id
            }
            (Self::ReindexCancel { operation_id }, ResponseOperation::ReindexCancel(actual)) => {
                *operation_id == actual.operation_id
            }
            _ => true,
        };
        if !operation_ids_match {
            return Err(ContractValidationError::Inconsistent {
                field: "response operation ID",
            });
        }
        if let (Self::References { request }, ResponseOperation::References(actual)) =
            (self, response)
            && request.as_ref() != &actual.request
        {
            return Err(ContractValidationError::Inconsistent {
                field: "references response request echo",
            });
        }
        match (self, response) {
            (Self::Search { query, limit }, ResponseOperation::Search(actual))
                if actual.query != query.as_ref()
                    || actual.returned_hits > *limit
                    || exceeds_u32_limit(actual.hits.len(), *limit) =>
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "search response request binding",
                });
            }
            (Self::Suggest { prefix, limit }, ResponseOperation::Suggest(actual))
                if actual.prefix != prefix.as_ref()
                    || exceeds_u32_limit(actual.suggestions.len(), *limit) =>
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "suggest response request binding",
                });
            }
            (Self::References { request }, ResponseOperation::References(actual))
                if actual.coverage.returned > request.as_ref().limit
                    || exceeds_u32_limit(actual.hits.len(), request.as_ref().limit) =>
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "references response request limit",
                });
            }
            _ => {}
        }
        Ok(())
    }
}

impl ResponseEnvelope {
    fn from_binding(binding: ResponseBinding, outcome: ResponseOutcome) -> Self {
        Self {
            protocol_revision: binding.protocol_revision,
            request_id: binding.request_id,
            project_id: binding.project_id,
            daemon_instance_id: binding.daemon_instance_id,
            query_policy_id: binding.query_policy_id,
            outcome,
        }
    }

    fn validate_for_expectation(
        &self,
        binding: ResponseBinding,
        operation: &ExpectedResponseOperation<'_>,
    ) -> Result<(), ContractValidationError> {
        ensure_protocol_revision(self.protocol_revision)?;
        if self.request_id != binding.request_id
            || self.project_id != binding.project_id
            || self.daemon_instance_id != binding.daemon_instance_id
            || self.query_policy_id != binding.query_policy_id
        {
            return Err(ContractValidationError::Inconsistent {
                field: "response request/project/instance/query-policy binding",
            });
        }
        if let ResponseOutcome::Success(response) = &self.outcome {
            operation.validate_response(response)?;
            if let Some(query_policy_id) = response.query_policy_id()
                && query_policy_id != self.query_policy_id
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "response query-policy binding",
                });
            }
        } else if let ResponseOutcome::Error(error) = &self.outcome {
            error.validate()?;
            if let Some(query_policy_id) = error.query_policy_id
                && query_policy_id != self.query_policy_id
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "error query-policy binding",
                });
            }
        }
        Ok(())
    }
}

fn exceeds_u32_limit(length: usize, limit: u32) -> bool {
    u32::try_from(length).map_or(true, |length| length > limit)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ResponseOutcome {
    Success(Box<ResponseOperation>),
    Error(Box<ApiError>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    protocol_revision: u16,
    request_id: RequestId,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    query_policy_id: QueryPolicyId,
    #[serde(flatten)]
    outcome: ResponseOutcome,
}

impl ResponseEnvelope {
    #[must_use]
    pub fn success(request: &RequestEnvelope, response: ResponseOperation) -> Self {
        Self::from_binding(
            ResponseBinding::from_request(request),
            ResponseOutcome::Success(Box::new(response)),
        )
    }

    #[must_use]
    pub fn error(request: &RequestEnvelope, error: ApiError) -> Self {
        Self::from_binding(
            ResponseBinding::from_request(request),
            ResponseOutcome::Error(Box::new(error)),
        )
    }

    pub fn validate_for(&self, request: &RequestEnvelope) -> Result<(), ContractValidationError> {
        request.validate()?;
        let operation = ExpectedResponseOperation::from_request(&request.operation);
        self.validate_for_expectation(ResponseBinding::from_request(request), &operation)
    }

    #[must_use]
    pub const fn outcome(&self) -> &ResponseOutcome {
        &self.outcome
    }

    #[must_use]
    pub fn into_outcome(self) -> ResponseOutcome {
        self.outcome
    }
}

impl ResponseExpectation {
    pub(crate) fn response_envelope(
        &self,
        result: Result<ResponseOperation, ApiError>,
    ) -> ResponseEnvelope {
        let outcome = match result {
            Ok(response) => ResponseOutcome::Success(Box::new(response)),
            Err(error) => ResponseOutcome::Error(Box::new(error)),
        };
        ResponseEnvelope::from_binding(self.binding, outcome)
    }

    pub(crate) fn validate_response(
        &self,
        response: &ResponseEnvelope,
    ) -> Result<(), ContractValidationError> {
        response.validate_for_expectation(self.binding, &self.operation)
    }
}

fn ensure_protocol_revision(actual: u16) -> Result<(), ContractValidationError> {
    if actual == BUSINESS_PROTOCOL_REVISION {
        Ok(())
    } else {
        Err(ContractValidationError::UnsupportedRevision {
            contract: "business protocol",
            actual,
            expected: BUSINESS_PROTOCOL_REVISION,
        })
    }
}

fn ensure_numeric_limit(
    field: &'static str,
    actual: u32,
    maximum: u32,
) -> Result<(), ContractValidationError> {
    if actual <= maximum {
        Ok(())
    } else {
        Err(ContractValidationError::NumericLimit {
            field,
            actual: u64::from(actual),
            maximum: u64::from(maximum),
        })
    }
}

pub(crate) const fn request_envelope_encoded_limit() -> usize {
    512 * 1024
}

pub(crate) const fn response_encoded_limit(operation: OperationKind) -> usize {
    match operation {
        OperationKind::Search
        | OperationKind::References
        | OperationKind::ReindexAdmit
        | OperationKind::ReindexStatus
        | OperationKind::ReindexWait => 16 * 1024 * 1024,
        OperationKind::Capabilities
        | OperationKind::Status
        | OperationKind::Suggest
        | OperationKind::ReindexCancel
        | OperationKind::Shutdown => 256 * 1024,
    }
}
