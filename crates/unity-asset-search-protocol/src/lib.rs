//! Versioned, transport-only contracts shared by search daemon clients.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use unity_asset_search_index::{
    ApiError, ReferencesResponse, ReindexReceipt, SEARCH_GENERATION_CONTRACT_VERSION,
    SearchResponse, StatusResponse, SuggestResponse,
};

/// Current HTTP envelope and route contract.
pub const HTTP_CONTRACT_VERSION: u16 = 2;
/// Prefix for every endpoint in the current HTTP contract.
pub const HTTP_API_PREFIX: &str = "/v2";

/// `GET /v2/health`.
pub const HEALTH_ENDPOINT: &str = "/v2/health";
/// `GET /v2/status`.
pub const STATUS_ENDPOINT: &str = "/v2/status";
/// `GET /v2/search`.
pub const SEARCH_ENDPOINT: &str = "/v2/search";
/// `GET /v2/suggest`.
pub const SUGGEST_ENDPOINT: &str = "/v2/suggest";
/// `POST /v2/references`.
pub const REFERENCES_ENDPOINT: &str = "/v2/references";
/// `POST /v2/reindex`.
pub const REINDEX_ENDPOINT: &str = "/v2/reindex";
/// `POST /v2/token/rotate`.
pub const TOKEN_ROTATE_ENDPOINT: &str = "/v2/token/rotate";

/// Stable response envelope for the daemon health endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResponse {
    /// HTTP envelope version.
    pub contract_version: u16,
    /// Whether the daemon can serve HTTP requests.
    pub ok: bool,
    /// Daemon package version.
    pub version: String,
}

impl HealthResponse {
    /// Builds a healthy response for one daemon package version.
    #[must_use]
    pub fn healthy(version: impl Into<String>) -> Self {
        Self {
            contract_version: HTTP_CONTRACT_VERSION,
            ok: true,
            version: version.into(),
        }
    }
}

/// Stable response envelope for reindex admission and optional completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReindexResponse {
    /// HTTP envelope version.
    pub contract_version: u16,
    /// Immediate coordinator admission receipt.
    pub admission: ReindexReceipt,
    /// Terminal build receipt, present only when the caller waited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<ReindexReceipt>,
    /// Index status observed after successful completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusResponse>,
}

impl ReindexResponse {
    /// Builds a response for asynchronously accepted work.
    #[must_use]
    pub const fn accepted(admission: ReindexReceipt) -> Self {
        Self {
            contract_version: HTTP_CONTRACT_VERSION,
            admission,
            completion: None,
            status: None,
        }
    }

    /// Builds a response for work whose terminal result was awaited.
    #[must_use]
    pub const fn waited(
        admission: ReindexReceipt,
        completion: ReindexReceipt,
        status: StatusResponse,
    ) -> Self {
        Self {
            contract_version: HTTP_CONTRACT_VERSION,
            admission,
            completion: Some(completion),
            status: Some(status),
        }
    }
}

/// Validates the version fields at one public protocol boundary.
pub trait ValidateContractVersion {
    /// Rejects an unsupported HTTP or Search Generation contract version.
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError>;
}

/// Identifies one unsupported version at a public protocol boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersionError {
    contract: &'static str,
    actual: u16,
    expected: u16,
}

impl ProtocolVersionError {
    /// Returns the contract component containing the mismatch.
    #[must_use]
    pub const fn contract(self) -> &'static str {
        self.contract
    }

    /// Returns the unsupported version.
    #[must_use]
    pub const fn actual(self) -> u16 {
        self.actual
    }

    /// Returns the supported version.
    #[must_use]
    pub const fn expected(self) -> u16 {
        self.expected
    }
}

impl fmt::Display for ProtocolVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported {} contract version {}, expected {}",
            self.contract, self.actual, self.expected
        )
    }
}

impl Error for ProtocolVersionError {}

impl ValidateContractVersion for HealthResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_http_version("health response", self.contract_version)
    }
}

impl ValidateContractVersion for ReindexResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_http_version("reindex response", self.contract_version)?;
        validate_receipt("reindex response admission", &self.admission)?;
        if let Some(completion) = &self.completion {
            validate_receipt("reindex response completion", completion)?;
        }
        if let Some(status) = &self.status {
            validate_status("reindex response status", status)?;
        }
        Ok(())
    }
}

impl ValidateContractVersion for SearchResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_generation_version("search response", self.contract_version)?;
        ensure_generation_version(
            "search response generation",
            self.generation.contract_version,
        )
    }
}

impl ValidateContractVersion for StatusResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        validate_status("status response", self)
    }
}

impl ValidateContractVersion for SuggestResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_generation_version("suggest response", self.contract_version)?;
        ensure_generation_version(
            "suggest response generation",
            self.generation.contract_version,
        )
    }
}

impl ValidateContractVersion for ReferencesResponse {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_generation_version("references response", self.contract_version)?;
        ensure_generation_version(
            "references response generation",
            self.generation.contract_version,
        )?;
        ensure_generation_version("references response request", self.request.contract_version)
    }
}

impl ValidateContractVersion for ApiError {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        ensure_generation_version("API error", self.contract_version)?;
        if let Some(generation) = &self.generation {
            ensure_generation_version("API error generation", generation.contract_version)?;
        }
        Ok(())
    }
}

impl ValidateContractVersion for ReindexReceipt {
    fn validate_contract_version(&self) -> Result<(), ProtocolVersionError> {
        validate_receipt("reindex receipt", self)
    }
}

fn validate_status(
    contract: &'static str,
    status: &StatusResponse,
) -> Result<(), ProtocolVersionError> {
    ensure_generation_version(contract, status.contract_version)?;
    ensure_generation_version(
        "status response generation status",
        status.generation.contract_version,
    )?;
    if let Some(generation) = &status.generation.active {
        ensure_generation_version(
            "status response active generation",
            generation.contract_version,
        )?;
    }
    ensure_generation_version(
        "status response capabilities",
        status.capabilities.contract_version,
    )
}

fn validate_receipt(
    contract: &'static str,
    receipt: &ReindexReceipt,
) -> Result<(), ProtocolVersionError> {
    ensure_generation_version(contract, receipt.contract_version)?;
    if let Some(generation) = &receipt.generation {
        ensure_generation_version("reindex receipt generation", generation.contract_version)?;
    }
    Ok(())
}

fn ensure_http_version(contract: &'static str, actual: u16) -> Result<(), ProtocolVersionError> {
    ensure_version(contract, actual, HTTP_CONTRACT_VERSION)
}

fn ensure_generation_version(
    contract: &'static str,
    actual: u16,
) -> Result<(), ProtocolVersionError> {
    ensure_version(contract, actual, SEARCH_GENERATION_CONTRACT_VERSION)
}

fn ensure_version(
    contract: &'static str,
    actual: u16,
    expected: u16,
) -> Result<(), ProtocolVersionError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolVersionError {
            contract,
            actual,
            expected,
        })
    }
}
