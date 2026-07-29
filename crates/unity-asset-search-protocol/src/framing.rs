use std::io::{self, Write};
use std::mem::size_of;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    read_contract_json_slice,
};

use crate::operation::{request_envelope_encoded_limit, response_encoded_limit};
use crate::{
    ContractValidationError, OperationKind, RequestEnvelope, ResponseEnvelope, ValidateContract,
};

const HEADER_BYTES: usize = size_of::<u32>();
const BOOTSTRAP_MAX_ENCODED_BYTES: usize = 16 * 1024;
const BUSINESS_MAX_JSON_ENTRIES: u64 = 1_000_000;
const BUSINESS_MAX_JSON_MEMBERS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy)]
pub struct FrameLimits {
    max_encoded_bytes: usize,
    json_limits: ContractJsonLimits,
}

impl FrameLimits {
    #[must_use]
    pub const fn bootstrap() -> Self {
        Self {
            max_encoded_bytes: BOOTSTRAP_MAX_ENCODED_BYTES,
            json_limits: ContractJsonLimits::new(
                "search_bootstrap_v1",
                BOOTSTRAP_MAX_ENCODED_BYTES,
                8,
                128,
                128,
                ContractJsonResourceModel::new(7, 4 * 1024, 4 * 1024, 256),
            ),
        }
    }

    const fn business(max_encoded_bytes: usize) -> Self {
        Self {
            max_encoded_bytes,
            json_limits: ContractJsonLimits::new(
                "search_business_v1",
                max_encoded_bytes,
                32,
                BUSINESS_MAX_JSON_ENTRIES,
                BUSINESS_MAX_JSON_MEMBERS,
                ContractJsonResourceModel::new(7, 4 * 1024, 16 * 1024, 512),
            ),
        }
    }

    #[must_use]
    pub const fn request_envelope() -> Self {
        Self::business(request_envelope_encoded_limit())
    }

    #[must_use]
    pub const fn response(operation: OperationKind) -> Self {
        Self::business(response_encoded_limit(operation))
    }

    #[must_use]
    pub const fn max_encoded_bytes(self) -> usize {
        self.max_encoded_bytes
    }
}

pub fn encode_frame<T: Serialize>(value: &T, limits: FrameLimits) -> Result<Vec<u8>, FramingError> {
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(HEADER_BYTES)
        .map_err(|_| FramingError::AllocationFailed {
            requested: HEADER_BYTES,
        })?;
    frame.extend_from_slice(&[0; HEADER_BYTES]);
    let mut writer = BoundedFrameWriter {
        frame: &mut frame,
        maximum: limits.max_encoded_bytes,
        failure: None,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        return Err(match writer.failure {
            Some(WriterFailure::Limit { requested }) => FramingError::EncodedLimitExceeded {
                requested,
                maximum: limits.max_encoded_bytes,
            },
            Some(WriterFailure::Allocation { requested }) => {
                FramingError::AllocationFailed { requested }
            }
            None => FramingError::Encode(error),
        });
    }
    let encoded_length = frame
        .len()
        .checked_sub(HEADER_BYTES)
        .ok_or(FramingError::LengthOverflow)?;
    let length = u32::try_from(encoded_length).map_err(|_| FramingError::LengthOverflow)?;
    frame[..HEADER_BYTES].copy_from_slice(&length.to_be_bytes());
    Ok(frame)
}

pub fn decode_frame<T: DeserializeOwned + Serialize>(
    frame: &[u8],
    budget: &mut AssetLoadBudget,
    limits: FrameLimits,
) -> Result<T, FramingError> {
    let header: [u8; HEADER_BYTES] = frame
        .get(..HEADER_BYTES)
        .ok_or(FramingError::TruncatedHeader)?
        .try_into()
        .expect("the slice length is checked");
    let declared = u32::from_be_bytes(header) as usize;
    if declared > limits.max_encoded_bytes {
        return Err(FramingError::EncodedLimitExceeded {
            requested: declared,
            maximum: limits.max_encoded_bytes,
        });
    }
    let actual = frame.len().saturating_sub(HEADER_BYTES);
    if declared != actual {
        return Err(FramingError::LengthMismatch { declared, actual });
    }
    let encoded = &frame[HEADER_BYTES..];
    let value = read_contract_json_slice(encoded, budget, limits.json_limits)
        .map_err(FramingError::Decode)?;
    verify_canonical_json(&value, encoded)?;
    Ok(value)
}

pub fn decode_validated_frame<T: DeserializeOwned + Serialize + ValidateContract>(
    frame: &[u8],
    budget: &mut AssetLoadBudget,
    limits: FrameLimits,
) -> Result<T, FramingError> {
    let value: T = decode_frame(frame, budget, limits)?;
    value.validate().map_err(FramingError::Validation)?;
    Ok(value)
}

fn verify_canonical_json<T: Serialize>(value: &T, encoded: &[u8]) -> Result<(), FramingError> {
    let mut verifier = CanonicalJsonVerifier {
        encoded,
        offset: 0,
        mismatch: false,
    };
    serde_json::to_writer(&mut verifier, value).map_err(FramingError::Encode)?;
    if verifier.mismatch || verifier.offset != encoded.len() {
        Err(FramingError::NonCanonicalJson)
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

pub fn encode_request_frame(request: &RequestEnvelope) -> Result<Vec<u8>, FramingError> {
    request.validate().map_err(FramingError::Validation)?;
    encode_frame(
        request,
        FrameLimits::business(request.operation().max_encoded_bytes()),
    )
}

pub fn decode_request_frame(
    frame: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<RequestEnvelope, FramingError> {
    let request: RequestEnvelope =
        decode_validated_frame(frame, budget, FrameLimits::request_envelope())?;
    let actual = frame.len().saturating_sub(HEADER_BYTES);
    let maximum = request.operation().max_encoded_bytes();
    if actual > maximum {
        return Err(FramingError::OperationEncodedLimitExceeded {
            operation: request.operation().kind(),
            requested: actual,
            maximum,
        });
    }
    Ok(request)
}

pub fn encode_response_frame(
    response: &ResponseEnvelope,
    request: &RequestEnvelope,
) -> Result<Vec<u8>, FramingError> {
    response
        .validate_for(request)
        .map_err(FramingError::Validation)?;
    encode_frame(response, FrameLimits::response(request.operation().kind()))
}

pub fn decode_response_frame(
    frame: &[u8],
    budget: &mut AssetLoadBudget,
    request: &RequestEnvelope,
) -> Result<ResponseEnvelope, FramingError> {
    let response: ResponseEnvelope = decode_frame(
        frame,
        budget,
        FrameLimits::response(request.operation().kind()),
    )?;
    response
        .validate_for(request)
        .map_err(FramingError::Validation)?;
    Ok(response)
}

#[derive(Debug, Clone, Copy)]
enum WriterFailure {
    Limit { requested: usize },
    Allocation { requested: usize },
}

struct BoundedFrameWriter<'frame> {
    frame: &'frame mut Vec<u8>,
    maximum: usize,
    failure: Option<WriterFailure>,
}

impl Write for BoundedFrameWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let encoded = self.frame.len().saturating_sub(HEADER_BYTES);
        let requested = encoded
            .checked_add(bytes.len())
            .ok_or_else(|| self.fail_limit(usize::MAX))?;
        if requested > self.maximum {
            return Err(self.fail_limit(requested));
        }
        self.frame.try_reserve(bytes.len()).map_err(|_| {
            self.failure = Some(WriterFailure::Allocation { requested });
            io::Error::new(io::ErrorKind::OutOfMemory, "frame allocation failed")
        })?;
        self.frame.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BoundedFrameWriter<'_> {
    fn fail_limit(&mut self, requested: usize) -> io::Error {
        self.failure = Some(WriterFailure::Limit { requested });
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            "frame encoded-byte limit exceeded",
        )
    }
}

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("frame header is truncated")]
    TruncatedHeader,
    #[error("frame declares {declared} encoded bytes but contains {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("frame contains {requested} encoded bytes; maximum is {maximum}")]
    EncodedLimitExceeded { requested: usize, maximum: usize },
    #[error(
        "{operation:?} frame contains {requested} encoded bytes; operation maximum is {maximum}"
    )]
    OperationEncodedLimitExceeded {
        operation: OperationKind,
        requested: usize,
        maximum: usize,
    },
    #[error("frame length cannot be represented")]
    LengthOverflow,
    #[error("failed to reserve {requested} frame bytes")]
    AllocationFailed { requested: usize },
    #[error("failed to encode frame JSON: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("failed to decode frame JSON: {0}")]
    Decode(#[source] BudgetedJsonError),
    #[error("frame JSON is not in the canonical wire representation")]
    NonCanonicalJson,
    #[error("decoded frame violates the protocol contract: {0}")]
    Validation(#[source] ContractValidationError),
}
