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
            ReindexOperationState::Failed => self.completion.is_none() && self.error.is_some(),
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
                None => None,
            },
            Self::Capabilities(_) | Self::ReindexCancel(_) | Self::Shutdown(_) => None,
        }
    }

    fn validate_for_request(
        &self,
        request: &RequestOperation,
    ) -> Result<(), ContractValidationError> {
        let operation_ids_match = match (request, self) {
            (RequestOperation::ReindexStatus(expected), Self::ReindexStatus(actual)) => {
                expected.operation_id == actual.operation_id
            }
            (RequestOperation::ReindexWait(expected), Self::ReindexWait(actual)) => {
                expected.operation_id == actual.operation_id
            }
            (RequestOperation::ReindexCancel(expected), Self::ReindexCancel(actual)) => {
                expected.operation_id == actual.operation_id
            }
            _ => true,
        };
        if !operation_ids_match {
            return Err(ContractValidationError::Inconsistent {
                field: "response operation ID",
            });
        }
        if let (RequestOperation::References(expected), Self::References(actual)) = (request, self)
            && expected != &actual.request
        {
            return Err(ContractValidationError::Inconsistent {
                field: "references response request echo",
            });
        }
        match (request, self) {
            (RequestOperation::Search(expected), Self::Search(actual))
                if actual.query != expected.query
                    || actual.returned_hits > expected.limit
                    || exceeds_u32_limit(actual.hits.len(), expected.limit) =>
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "search response request binding",
                });
            }
            (RequestOperation::Suggest(expected), Self::Suggest(actual))
                if actual.prefix != expected.prefix
                    || exceeds_u32_limit(actual.suggestions.len(), expected.limit) =>
            {
                return Err(ContractValidationError::Inconsistent {
                    field: "suggest response request binding",
                });
            }
            (RequestOperation::References(expected), Self::References(actual))
                if actual.coverage.returned > expected.limit
                    || exceeds_u32_limit(actual.hits.len(), expected.limit) =>
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
        Self {
            protocol_revision: request.protocol_revision,
            request_id: request.request_id,
            project_id: request.project_id,
            daemon_instance_id: request.daemon_instance_id,
            query_policy_id: request.query_policy_id,
            outcome: ResponseOutcome::Success(Box::new(response)),
        }
    }

    #[must_use]
    pub fn error(request: &RequestEnvelope, error: ApiError) -> Self {
        Self {
            protocol_revision: request.protocol_revision,
            request_id: request.request_id,
            project_id: request.project_id,
            daemon_instance_id: request.daemon_instance_id,
            query_policy_id: request.query_policy_id,
            outcome: ResponseOutcome::Error(Box::new(error)),
        }
    }

    pub fn validate_for(&self, request: &RequestEnvelope) -> Result<(), ContractValidationError> {
        request.validate()?;
        ensure_protocol_revision(self.protocol_revision)?;
        if self.request_id != request.request_id
            || self.project_id != request.project_id
            || self.daemon_instance_id != request.daemon_instance_id
            || self.query_policy_id != request.query_policy_id
        {
            return Err(ContractValidationError::Inconsistent {
                field: "response request/project/instance/query-policy binding",
            });
        }
        if let ResponseOutcome::Success(response) = &self.outcome {
            if response.kind() != request.operation.kind() {
                return Err(ContractValidationError::Inconsistent {
                    field: "response operation kind",
                });
            }
            response.validate()?;
            response.validate_for_request(&request.operation)?;
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

    #[must_use]
    pub const fn outcome(&self) -> &ResponseOutcome {
        &self.outcome
    }

    #[must_use]
    pub fn into_outcome(self) -> ResponseOutcome {
        self.outcome
    }
}

fn ensure_protocol_revision(actual: u16) -> Result<(), ContractValidationError> {
    if actual == BUSINESS_PROTOCOL_REVISION {
        Ok(())
    } else {
        Err(ContractValidationError::UnsupportedVersion {
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
        OperationKind::Capabilities | OperationKind::Status | OperationKind::Suggest => 256 * 1024,
        OperationKind::ReindexCancel | OperationKind::Shutdown => 16 * 1024,
    }
}
