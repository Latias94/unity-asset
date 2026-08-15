use std::io::{self, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    read_contract_json_slice,
};

use crate::operation::{
    ResponseExpectation, request_envelope_encoded_limit, response_encoded_limit,
};
use crate::{
    ApiError, ContractValidationError, DaemonInstanceId, OperationKind, ProjectId, QueryPolicyId,
    RequestEnvelope, RequestOperation, ResponseEnvelope, ResponseOperation, ValidateContract,
};

const BUSINESS_MAX_JSON_ENTRIES: u64 = 1_000_000;
const BUSINESS_MAX_JSON_MEMBERS: u64 = 1_000_000;

/// Maximum encoded bytes accepted for one complete business request document.
pub const MAX_REQUEST_JSON_BYTES: usize = request_envelope_encoded_limit();

/// Returns the maximum encoded bytes accepted for a response JSON document.
#[must_use]
pub const fn max_response_json_bytes(operation: OperationKind) -> usize {
    response_encoded_limit(operation)
}

/// A request whose canonical JSON, resource limits, and complete semantic contract were validated.
#[derive(Debug)]
#[must_use = "validated requests must be bound before dispatch"]
pub struct ValidatedRequest {
    request: RequestEnvelope,
}

/// One-shot authority for validating and encoding the response to a bound request.
#[derive(Debug)]
#[must_use = "bound response encoders must encode the dispatch result"]
pub struct ResponseEncoder {
    expectation: ResponseExpectation,
}

impl ValidatedRequest {
    const fn from_validated(request: RequestEnvelope) -> Self {
        Self { request }
    }

    /// Verifies daemon-owned scalar bindings and releases a one-shot response encoder.
    pub fn bind(
        self,
        expected_project: ProjectId,
        expected_instance: DaemonInstanceId,
        expected_query_policy: QueryPolicyId,
    ) -> Result<(RequestOperation, ResponseEncoder), ContractValidationError> {
        self.request
            .ensure_binding(expected_project, expected_instance, expected_query_policy)?;
        let expectation = ResponseExpectation::from_validated_request(&self.request);
        let operation = self.request.into_operation();
        Ok((operation, ResponseEncoder { expectation }))
    }
}

impl ResponseEncoder {
    /// Validates the dispatch result against the bound request and encodes canonical JSON.
    pub fn encode(
        self,
        result: Result<ResponseOperation, ApiError>,
    ) -> Result<Vec<u8>, ProtocolJsonError> {
        let operation = self.expectation.operation_kind();
        let response = self.expectation.response_envelope(result);
        self.expectation
            .validate_response(&response)
            .map_err(ProtocolJsonError::Validation)?;
        encode_json(&response, JsonLimits::response(operation))
    }
}

#[derive(Debug, Clone, Copy)]
struct JsonLimits {
    max_encoded_bytes: usize,
    contract: ContractJsonLimits,
}

impl JsonLimits {
    const fn business(max_encoded_bytes: usize) -> Self {
        Self {
            max_encoded_bytes,
            contract: ContractJsonLimits::new(
                "search_business_v1",
                max_encoded_bytes,
                32,
                BUSINESS_MAX_JSON_ENTRIES,
                BUSINESS_MAX_JSON_MEMBERS,
                ContractJsonResourceModel::new(7, 4 * 1024, 16 * 1024, 512),
            ),
        }
    }

    const fn request() -> Self {
        Self::business(MAX_REQUEST_JSON_BYTES)
    }

    const fn response(operation: OperationKind) -> Self {
        Self::business(max_response_json_bytes(operation))
    }
}

pub fn encode_request_json(request: &RequestEnvelope) -> Result<Vec<u8>, ProtocolJsonError> {
    request.validate().map_err(ProtocolJsonError::Validation)?;
    encode_json(
        request,
        JsonLimits::business(request.operation().max_encoded_bytes()),
    )
}

pub fn decode_request_json(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<ValidatedRequest, ProtocolJsonError> {
    let request: RequestEnvelope = decode_validated_json(encoded, budget, JsonLimits::request())?;
    let maximum = request.operation().max_encoded_bytes();
    if encoded.len() > maximum {
        return Err(ProtocolJsonError::OperationEncodedLimitExceeded {
            operation: request.operation().kind(),
            requested: encoded.len(),
            maximum,
        });
    }
    Ok(ValidatedRequest::from_validated(request))
}

pub fn encode_response_json(
    response: &ResponseEnvelope,
    request: &RequestEnvelope,
) -> Result<Vec<u8>, ProtocolJsonError> {
    response
        .validate_for(request)
        .map_err(ProtocolJsonError::Validation)?;
    encode_json(response, JsonLimits::response(request.operation().kind()))
}

pub fn decode_response_json(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    request: &RequestEnvelope,
) -> Result<ResponseEnvelope, ProtocolJsonError> {
    let response: ResponseEnvelope = decode_json(
        encoded,
        budget,
        JsonLimits::response(request.operation().kind()),
    )?;
    response
        .validate_for(request)
        .map_err(ProtocolJsonError::Validation)?;
    Ok(response)
}

fn encode_json<T: Serialize>(value: &T, limits: JsonLimits) -> Result<Vec<u8>, ProtocolJsonError> {
    let mut encoded = Vec::new();
    let mut writer = BoundedJsonWriter {
        encoded: &mut encoded,
        maximum: limits.max_encoded_bytes,
        failure: None,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        return Err(match writer.failure {
            Some(WriterFailure::Limit { requested }) => ProtocolJsonError::EncodedLimitExceeded {
                requested,
                maximum: limits.max_encoded_bytes,
            },
            Some(WriterFailure::Allocation { requested }) => {
                ProtocolJsonError::AllocationFailed { requested }
            }
            None => ProtocolJsonError::Encode(error),
        });
    }
    Ok(encoded)
}

fn decode_json<T: DeserializeOwned + Serialize>(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    limits: JsonLimits,
) -> Result<T, ProtocolJsonError> {
    if encoded.len() > limits.max_encoded_bytes {
        return Err(ProtocolJsonError::EncodedLimitExceeded {
            requested: encoded.len(),
            maximum: limits.max_encoded_bytes,
        });
    }
    let value = read_contract_json_slice(encoded, budget, limits.contract)
        .map_err(ProtocolJsonError::Decode)?;
    verify_canonical_json(&value, encoded)?;
    Ok(value)
}

fn decode_validated_json<T: DeserializeOwned + Serialize + ValidateContract>(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    limits: JsonLimits,
) -> Result<T, ProtocolJsonError> {
    let value: T = decode_json(encoded, budget, limits)?;
    value.validate().map_err(ProtocolJsonError::Validation)?;
    Ok(value)
}

fn verify_canonical_json<T: Serialize>(value: &T, encoded: &[u8]) -> Result<(), ProtocolJsonError> {
    let mut verifier = CanonicalJsonVerifier {
        encoded,
        offset: 0,
        mismatch: false,
    };
    serde_json::to_writer(&mut verifier, value).map_err(ProtocolJsonError::Encode)?;
    if verifier.mismatch || verifier.offset != encoded.len() {
        Err(ProtocolJsonError::NonCanonicalJson)
    } else {
        Ok(())
    }
}

struct CanonicalJsonVerifier<'encoded> {
    encoded: &'encoded [u8],
    offset: usize,
    mismatch: bool,
}

impl Write for CanonicalJsonVerifier<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self.offset.checked_add(bytes.len());
        if end.and_then(|end| self.encoded.get(self.offset..end)) != Some(bytes) {
            self.mismatch = true;
        }
        self.offset = end.unwrap_or(usize::MAX);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum WriterFailure {
    Limit { requested: usize },
    Allocation { requested: usize },
}

struct BoundedJsonWriter<'encoded> {
    encoded: &'encoded mut Vec<u8>,
    maximum: usize,
    failure: Option<WriterFailure>,
}

impl Write for BoundedJsonWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let requested = self
            .encoded
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| self.fail_limit(usize::MAX))?;
        if requested > self.maximum {
            return Err(self.fail_limit(requested));
        }
        self.encoded.try_reserve(bytes.len()).map_err(|_| {
            self.failure = Some(WriterFailure::Allocation { requested });
            io::Error::new(io::ErrorKind::OutOfMemory, "JSON allocation failed")
        })?;
        self.encoded.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BoundedJsonWriter<'_> {
    fn fail_limit(&mut self, requested: usize) -> io::Error {
        self.failure = Some(WriterFailure::Limit { requested });
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            "protocol JSON encoded-byte limit exceeded",
        )
    }
}

#[derive(Debug, Error)]
pub enum ProtocolJsonError {
    #[error("protocol JSON contains {requested} encoded bytes; maximum is {maximum}")]
    EncodedLimitExceeded { requested: usize, maximum: usize },
    #[error(
        "{operation:?} protocol JSON contains {requested} encoded bytes; operation maximum is {maximum}"
    )]
    OperationEncodedLimitExceeded {
        operation: OperationKind,
        requested: usize,
        maximum: usize,
    },
    #[error("failed to reserve {requested} protocol JSON bytes")]
    AllocationFailed { requested: usize },
    #[error("failed to encode protocol JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("failed to decode protocol JSON: {0}")]
    Decode(#[source] BudgetedJsonError),
    #[error("protocol JSON is not in the canonical wire representation")]
    NonCanonicalJson,
    #[error("decoded protocol JSON violates the contract: {0}")]
    Validation(#[source] ContractValidationError),
}
