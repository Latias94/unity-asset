//! Bounded, deterministic payload storage for reference projection documents.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use unity_asset_core::{
    AssetLoadBudget, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    DigestBuildError, DigestV1, DigestV1Builder, read_contract_json,
};

use crate::analysis::ReferenceProjectionFact;
use crate::anchored_fs::{AnchoredFsError as SecureReadError, RegularFile, RegularFileRange};
use crate::projection::ReferenceDocument;

pub(crate) const REFERENCE_PAYLOAD_FILE: &str = "reference-payload-v4.jsonl";
pub(crate) const MAX_REFERENCE_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_REFERENCE_PAYLOAD_BYTES_U64: u64 = 4 * 1024 * 1024;
const REFERENCE_PAYLOAD_CONTRACT_VERSION: u16 = 4;
const REFERENCE_PAYLOAD_JSON_MAX_DEPTH: u32 = 40;
const REFERENCE_PAYLOAD_JSON_MAX_VALUES: u64 = 128 * 1024;
// The fixed materialization reserve covers the current payload envelope and parser scratch. The
// per-entry reserve covers owned strings, vectors, and Serde temporaries in one decoded payload.
const REFERENCE_PAYLOAD_JSON_RESOURCES: ContractJsonResourceModel =
    ContractJsonResourceModel::new(6, 4 * 1024, MAX_REFERENCE_PAYLOAD_BYTES_U64, 1024);
const REFERENCE_PAYLOAD_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.reference_payload",
    MAX_REFERENCE_PAYLOAD_BYTES,
    REFERENCE_PAYLOAD_JSON_MAX_DEPTH,
    REFERENCE_PAYLOAD_JSON_MAX_VALUES,
    REFERENCE_PAYLOAD_JSON_MAX_VALUES,
    REFERENCE_PAYLOAD_JSON_RESOURCES,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReferencePayloadLocation {
    offset: u64,
    length: u64,
    digest: DigestV1,
}

impl ReferencePayloadLocation {
    pub(crate) const fn offset(self) -> u64 {
        self.offset
    }

    pub(crate) const fn length(self) -> u64 {
        self.length
    }

    pub(crate) const fn digest(self) -> DigestV1 {
        self.digest
    }
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct BorrowedReferencePayload<'document> {
    contract_version: u16,
    stable_id: &'document str,
    source_path: &'document str,
    source_kind: &'document str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_guid: Option<&'document str>,
    fact: &'document ReferenceProjectionFact,
}

impl<'document> From<&'document ReferenceDocument> for BorrowedReferencePayload<'document> {
    fn from(document: &'document ReferenceDocument) -> Self {
        Self {
            contract_version: REFERENCE_PAYLOAD_CONTRACT_VERSION,
            stable_id: &document.stable_id,
            source_path: &document.source_path,
            source_kind: &document.source_kind,
            source_guid: document.source_guid.as_deref(),
            fact: &document.fact,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReferencePayload {
    contract_version: u16,
    pub(crate) stable_id: String,
    pub(crate) source_path: String,
    pub(crate) source_kind: String,
    #[serde(default)]
    pub(crate) source_guid: Option<String>,
    pub(crate) fact: ReferenceProjectionFact,
}

impl ReferencePayload {
    pub(crate) fn validate(
        &self,
        expected_stable_id: &str,
    ) -> Result<(), ReferencePayloadValidationError> {
        if self.contract_version != REFERENCE_PAYLOAD_CONTRACT_VERSION {
            return Err(ReferencePayloadValidationError::UnsupportedVersion {
                actual: self.contract_version,
                expected: REFERENCE_PAYLOAD_CONTRACT_VERSION,
            });
        }
        if self.stable_id.is_empty() {
            return Err(ReferencePayloadValidationError::EmptyStableId);
        }
        if self.stable_id != expected_stable_id {
            return Err(ReferencePayloadValidationError::StableIdMismatch);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn from_document(document: ReferenceDocument) -> Self {
        Self {
            contract_version: REFERENCE_PAYLOAD_CONTRACT_VERSION,
            stable_id: document.stable_id,
            source_path: document.source_path,
            source_kind: document.source_kind,
            source_guid: document.source_guid,
            fact: document.fact,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferencePayloadValidationError {
    UnsupportedVersion { actual: u16, expected: u16 },
    EmptyStableId,
    StableIdMismatch,
}

impl fmt::Display for ReferencePayloadValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { actual, expected } => write!(
                formatter,
                "payload contract version {actual} does not match {expected}"
            ),
            Self::EmptyStableId => formatter.write_str("payload stable ID is empty"),
            Self::StableIdMismatch => {
                formatter.write_str("payload stable ID differs from the fast-field stable ID")
            }
        }
    }
}

impl Error for ReferencePayloadValidationError {}

pub(crate) struct ReferencePayloadWriter {
    file: File,
    next_offset: u64,
}

impl ReferencePayloadWriter {
    pub(crate) fn create(directory: &Path) -> Result<Self, ReferencePayloadWriteError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(REFERENCE_PAYLOAD_FILE))
            .map_err(ReferencePayloadWriteError::Io)?;
        Ok(Self {
            file,
            next_offset: 0,
        })
    }

    pub(crate) fn append(
        &mut self,
        document: &ReferenceDocument,
        max_fact_json_bytes: usize,
    ) -> Result<ReferencePayloadLocation, ReferencePayloadWriteError> {
        let fact_length = serialized_length(&document.fact)?;
        let max_fact_json_bytes_u64 = u64::try_from(max_fact_json_bytes).unwrap_or(u64::MAX);
        if fact_length > max_fact_json_bytes_u64 {
            return Err(ReferencePayloadWriteError::FactTooLarge {
                actual: fact_length,
                maximum: max_fact_json_bytes,
            });
        }

        let payload = BorrowedReferencePayload::from(document);
        let payload_length = serialized_length(&payload)?;
        if payload_length == 0 {
            return Err(ReferencePayloadWriteError::EmptyPayload);
        }
        if payload_length > MAX_REFERENCE_PAYLOAD_BYTES_U64 {
            return Err(ReferencePayloadWriteError::PayloadTooLarge {
                actual: payload_length,
                maximum: MAX_REFERENCE_PAYLOAD_BYTES,
            });
        }

        let mut writer = MeasuringWriter::new(&mut self.file, payload_length);
        serde_json::to_writer(&mut writer, &payload).map_err(ReferencePayloadWriteError::Json)?;
        let actual = writer.bytes_written();
        let digest = writer.finish()?;
        if actual != payload_length {
            return Err(ReferencePayloadWriteError::EncodedLengthChanged {
                expected: payload_length,
                actual,
            });
        }
        let location = ReferencePayloadLocation {
            offset: self.next_offset,
            length: payload_length,
            digest,
        };
        self.file
            .write_all(b"\n")
            .map_err(ReferencePayloadWriteError::Io)?;
        self.next_offset = self
            .next_offset
            .checked_add(payload_length)
            .and_then(|offset| offset.checked_add(1))
            .ok_or(ReferencePayloadWriteError::FileLengthOverflow)?;
        Ok(location)
    }

    pub(crate) fn finish(mut self) -> Result<(), ReferencePayloadWriteError> {
        self.file.flush().map_err(ReferencePayloadWriteError::Io)?;
        self.file.sync_all().map_err(ReferencePayloadWriteError::Io)
    }
}

#[derive(Debug)]
pub(crate) enum ReferencePayloadWriteError {
    Io(io::Error),
    Json(serde_json::Error),
    FactTooLarge { actual: u64, maximum: usize },
    EmptyPayload,
    PayloadTooLarge { actual: u64, maximum: usize },
    FileLengthOverflow,
    EncodedLengthChanged { expected: u64, actual: u64 },
    Digest(DigestBuildError),
}

impl fmt::Display for ReferencePayloadWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "reference payload I/O failed: {source}"),
            Self::Json(source) => {
                write!(
                    formatter,
                    "reference payload JSON encoding failed: {source}"
                )
            }
            Self::FactTooLarge { actual, maximum } => write!(
                formatter,
                "reference fact JSON is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::EmptyPayload => formatter.write_str("reference payload JSON is empty"),
            Self::PayloadTooLarge { actual, maximum } => write!(
                formatter,
                "reference payload JSON is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::FileLengthOverflow => {
                formatter.write_str("reference payload sidecar length overflows u64")
            }
            Self::EncodedLengthChanged { expected, actual } => write!(
                formatter,
                "reference payload JSON changed length between measurement ({expected}) and \
                 writing ({actual})"
            ),
            Self::Digest(source) => write!(formatter, "reference payload digest failed: {source}"),
        }
    }
}

impl Error for ReferencePayloadWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::FactTooLarge { .. }
            | Self::EmptyPayload
            | Self::PayloadTooLarge { .. }
            | Self::FileLengthOverflow
            | Self::EncodedLengthChanged { .. } => None,
            Self::Digest(source) => Some(source),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedReferencePayloadRange {
    offset: u64,
    length: u64,
    encoded_bytes: usize,
}

impl ValidatedReferencePayloadRange {
    pub(crate) const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }
}

#[derive(Clone)]
pub(crate) struct ReferencePayloadReader {
    file: Arc<RegularFile>,
}

impl ReferencePayloadReader {
    pub(crate) fn new(file: RegularFile) -> Self {
        Self {
            file: Arc::new(file),
        }
    }

    pub(crate) fn validate_range(
        &self,
        offset: u64,
        length: u64,
        maximum_payload_bytes: usize,
    ) -> Result<ValidatedReferencePayloadRange, ReferencePayloadReadError> {
        if length == 0 {
            return Err(ReferencePayloadReadError::EmptyRange);
        }
        let maximum = maximum_payload_bytes.min(MAX_REFERENCE_PAYLOAD_BYTES);
        let maximum_u64 = u64::try_from(maximum).unwrap_or(u64::MAX);
        if length > maximum_u64 {
            return Err(ReferencePayloadReadError::PayloadTooLarge {
                actual: length,
                maximum,
            });
        }
        let end = offset
            .checked_add(length)
            .ok_or(ReferencePayloadReadError::RangeOverflow { offset, length })?;
        if end > self.file.length() {
            return Err(ReferencePayloadReadError::RangeOutOfBounds {
                offset,
                length,
                file_length: self.file.length(),
            });
        }
        let encoded_bytes =
            usize::try_from(length).map_err(|_| ReferencePayloadReadError::PayloadTooLarge {
                actual: length,
                maximum,
            })?;
        Ok(ValidatedReferencePayloadRange {
            offset,
            length,
            encoded_bytes,
        })
    }

    pub(crate) fn read(
        &self,
        range: ValidatedReferencePayloadRange,
        expected_digest: DigestV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferencePayload, ReferencePayloadReadError> {
        let reader = self
            .file
            .range(range.offset, range.length)
            .map_err(ReferencePayloadReadError::SecureRead)?;
        let mut reader = DigestingReader::new(reader, range.length);
        let decoded = read_contract_json::<ReferencePayload>(
            &mut reader,
            budget,
            REFERENCE_PAYLOAD_JSON_LIMITS,
        );
        let digest = reader.finalize();
        self.file
            .ensure_unchanged()
            .map_err(ReferencePayloadReadError::SecureRead)?;
        let decoded = decoded.map_err(ReferencePayloadReadError::Json)?;
        let digest = digest.map_err(ReferencePayloadReadError::Digest)?;
        if digest != expected_digest {
            return Err(ReferencePayloadReadError::DigestMismatch {
                expected: expected_digest,
                actual: digest,
            });
        }
        Ok(decoded)
    }

    #[cfg(test)]
    pub(crate) fn file_length(&self) -> u64 {
        self.file.length()
    }
}

impl fmt::Debug for ReferencePayloadReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReferencePayloadReader")
            .field("length", &self.file.length())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) enum ReferencePayloadReadError {
    EmptyRange,
    PayloadTooLarge {
        actual: u64,
        maximum: usize,
    },
    RangeOverflow {
        offset: u64,
        length: u64,
    },
    RangeOutOfBounds {
        offset: u64,
        length: u64,
        file_length: u64,
    },
    SecureRead(SecureReadError),
    Digest(DigestBuildError),
    DigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    Json(BudgetedJsonError),
}

impl fmt::Display for ReferencePayloadReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRange => formatter.write_str("reference payload range is empty"),
            Self::PayloadTooLarge { actual, maximum } => write!(
                formatter,
                "reference payload range is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::RangeOverflow { offset, length } => write!(
                formatter,
                "reference payload range offset {offset} plus length {length} overflows u64"
            ),
            Self::RangeOutOfBounds {
                offset,
                length,
                file_length,
            } => write!(
                formatter,
                "reference payload range {offset}+{length} exceeds sidecar length {file_length}"
            ),
            Self::SecureRead(source) => write!(
                formatter,
                "reference payload secure positional read failed: {source}"
            ),
            Self::Digest(source) => write!(formatter, "reference payload digest failed: {source}"),
            Self::DigestMismatch { expected, actual } => write!(
                formatter,
                "reference payload content digest differs from its anchored index binding: expected {expected}, got {actual}"
            ),
            Self::Json(source) => write!(formatter, "reference payload JSON is invalid: {source}"),
        }
    }
}

impl Error for ReferencePayloadReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SecureRead(source) => Some(source),
            Self::Digest(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::EmptyRange
            | Self::PayloadTooLarge { .. }
            | Self::RangeOverflow { .. }
            | Self::RangeOutOfBounds { .. }
            | Self::DigestMismatch { .. } => None,
        }
    }
}

#[derive(Default)]
struct LengthWriter {
    bytes: u64,
}

impl Write for LengthWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(
                u64::try_from(buffer.len()).map_err(|_| {
                    io::Error::other("serialized reference payload chunk exceeds u64")
                })?,
            )
            .ok_or_else(|| io::Error::other("serialized reference payload length overflows u64"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_length(value: &impl Serialize) -> Result<u64, ReferencePayloadWriteError> {
    let mut writer = LengthWriter::default();
    serde_json::to_writer(&mut writer, value).map_err(ReferencePayloadWriteError::Json)?;
    Ok(writer.bytes)
}

struct MeasuringWriter<'writer> {
    writer: &'writer mut File,
    bytes: u64,
    digest: DigestV1Builder,
}

impl<'writer> MeasuringWriter<'writer> {
    fn new(writer: &'writer mut File, declared_length: u64) -> Self {
        Self {
            writer,
            bytes: 0,
            digest: DigestV1Builder::new(declared_length),
        }
    }

    const fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn finish(self) -> Result<DigestV1, ReferencePayloadWriteError> {
        self.digest
            .finalize()
            .map_err(ReferencePayloadWriteError::Digest)
    }
}

impl Write for MeasuringWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(buffer)?;
        self.bytes = self
            .bytes
            .checked_add(
                u64::try_from(written)
                    .map_err(|_| io::Error::other("written payload chunk exceeds u64"))?,
            )
            .ok_or_else(|| io::Error::other("written payload length overflows u64"))?;
        self.digest.update(&buffer[..written]).map_err(|error| {
            io::Error::other(format!("reference payload digest update failed: {error}"))
        })?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct DigestingReader<'reader> {
    reader: RegularFileRange<'reader>,
    digest: DigestV1Builder,
}

impl<'reader> DigestingReader<'reader> {
    fn new(reader: RegularFileRange<'reader>, declared_length: u64) -> Self {
        Self {
            reader,
            digest: DigestV1Builder::new(declared_length),
        }
    }

    fn finalize(self) -> Result<DigestV1, DigestBuildError> {
        self.digest.finalize()
    }
}

impl io::Read for DigestingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = io::Read::read(&mut self.reader, buffer)?;
        self.digest.update(&buffer[..read]).map_err(|error| {
            io::Error::other(format!("reference payload digest update failed: {error}"))
        })?;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;

    use tempfile::{TempDir, tempdir};
    use unity_asset_core::{AssetLoadLimits, BudgetError, FieldPath, ObjectAddress, SourceLocator};

    use super::*;
    use crate::analysis::{RawReferenceProjection, ReferenceResolutionProjection};
    #[cfg(unix)]
    use crate::anchored_fs::AnchoredFsError;
    use crate::anchored_fs::{OpenPolicy, ReadDirectory};

    fn projected_reference() -> ReferenceDocument {
        let source_object =
            ObjectAddress::binary_direct(SourceLocator::path("Assets/Source.asset").unwrap(), 7)
                .unwrap();
        ReferenceDocument {
            stable_id: "reference-a".to_owned(),
            source_path: "Assets/Source.asset".to_owned(),
            source_kind: "SerializedAsset".to_owned(),
            source_guid: Some("abababababababababababababababab".to_owned()),
            fact: ReferenceProjectionFact {
                source_object,
                source_class_id: Some(1),
                field_path: FieldPath::root().push_field("m_Target").unwrap(),
                raw_target: RawReferenceProjection::Binary {
                    file_id: 0,
                    path_id: 0,
                    external: None,
                },
                resolution: ReferenceResolutionProjection::Null,
                diagnostics: Vec::new(),
                dependency_keys: Vec::new(),
            },
            incoming_keys: vec!["guid:abababababababababababababababab".to_owned()],
            outgoing_keys: Vec::new(),
        }
    }

    fn open_reader(bytes: &[u8]) -> (TempDir, ReferencePayloadReader) {
        let directory = crate::secure_test_tempdir();
        fs::write(directory.path().join(REFERENCE_PAYLOAD_FILE), bytes).unwrap();
        let opened = ReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap();
        let file = opened
            .open_regular(OsStr::new(REFERENCE_PAYLOAD_FILE))
            .unwrap();
        (directory, ReferencePayloadReader::new(file))
    }

    fn open_written_reader(
        document: &ReferenceDocument,
    ) -> (TempDir, ReferencePayloadReader, ReferencePayloadLocation) {
        let directory = crate::secure_test_tempdir();
        let mut writer = ReferencePayloadWriter::create(directory.path()).unwrap();
        let location = writer.append(document, 1024 * 1024).unwrap();
        writer.finish().unwrap();
        let opened = ReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap();
        let file = opened
            .open_regular(OsStr::new(REFERENCE_PAYLOAD_FILE))
            .unwrap();
        (directory, ReferencePayloadReader::new(file), location)
    }

    #[test]
    fn range_validation_rejects_empty_overflow_eof_and_one_short_limit() {
        let (_directory, reader) = open_reader(b"1234");

        assert_eq!(reader.file_length(), 4);
        assert_eq!(reader.validate_range(0, 4, 4).unwrap().encoded_bytes(), 4);
        assert!(matches!(
            reader.validate_range(0, 0, 4).unwrap_err(),
            ReferencePayloadReadError::EmptyRange
        ));
        assert!(matches!(
            reader.validate_range(0, 4, 3).unwrap_err(),
            ReferencePayloadReadError::PayloadTooLarge {
                actual: 4,
                maximum: 3
            }
        ));
        assert!(matches!(
            reader.validate_range(u64::MAX, 2, 4).unwrap_err(),
            ReferencePayloadReadError::RangeOverflow { .. }
        ));
        assert!(matches!(
            reader.validate_range(2, 3, 4).unwrap_err(),
            ReferencePayloadReadError::RangeOutOfBounds { file_length: 4, .. }
        ));
    }

    #[test]
    fn payload_encoding_is_deterministic_and_location_excludes_newline() {
        let document = projected_reference();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let mut first_writer = ReferencePayloadWriter::create(first.path()).unwrap();
        let first_location = first_writer.append(&document, 1024 * 1024).unwrap();
        first_writer.finish().unwrap();
        let mut second_writer = ReferencePayloadWriter::create(second.path()).unwrap();
        let second_location = second_writer.append(&document, 1024 * 1024).unwrap();
        second_writer.finish().unwrap();

        let first_bytes = fs::read(first.path().join(REFERENCE_PAYLOAD_FILE)).unwrap();
        let second_bytes = fs::read(second.path().join(REFERENCE_PAYLOAD_FILE)).unwrap();
        assert_eq!(first_bytes, second_bytes);
        assert_eq!(first_location, second_location);
        assert_eq!(first_location.offset(), 0);
        assert_eq!(
            usize::try_from(first_location.length()).unwrap(),
            first_bytes.len() - 1
        );
        assert_eq!(first_bytes.last(), Some(&b'\n'));
    }

    #[test]
    fn oversized_payload_is_rejected_before_writing_any_record_bytes() {
        let directory = tempdir().unwrap();
        let mut document = projected_reference();
        document.source_path = "x".repeat(MAX_REFERENCE_PAYLOAD_BYTES);
        let mut writer = ReferencePayloadWriter::create(directory.path()).unwrap();

        let error = writer.append(&document, 1024 * 1024).unwrap_err();

        assert!(matches!(
            error,
            ReferencePayloadWriteError::PayloadTooLarge { .. }
        ));
        assert_eq!(
            fs::metadata(directory.path().join(REFERENCE_PAYLOAD_FILE))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn corrupt_and_deep_json_are_rejected_by_the_contract_reader() {
        let corrupt = b"{not-json";
        let (_corrupt_directory, corrupt_reader) = open_reader(corrupt);
        let corrupt_range = corrupt_reader
            .validate_range(0, corrupt.len() as u64, corrupt.len())
            .unwrap();
        let corrupt_error = corrupt_reader
            .read(
                corrupt_range,
                DigestV1::hash_bytes(corrupt),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            corrupt_error,
            ReferencePayloadReadError::Json(BudgetedJsonError::Json(_))
        ));

        let nesting = usize::try_from(REFERENCE_PAYLOAD_JSON_MAX_DEPTH).unwrap() + 1;
        let deep = format!("{}null{}", "[".repeat(nesting), "]".repeat(nesting));
        let (_deep_directory, deep_reader) = open_reader(deep.as_bytes());
        let deep_range = deep_reader
            .validate_range(0, deep.len() as u64, deep.len())
            .unwrap();
        let deep_error = deep_reader
            .read(
                deep_range,
                DigestV1::hash_bytes(deep.as_bytes()),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            deep_error,
            ReferencePayloadReadError::Json(BudgetedJsonError::StructureLimitExceeded {
                resource: "depth",
                ..
            })
        ));
    }

    #[test]
    fn valid_payload_accepts_exact_and_rejects_one_short_budget() {
        let document = projected_reference();
        let (_directory, reader, location) = open_written_reader(&document);
        let range = reader
            .validate_range(
                location.offset(),
                location.length(),
                MAX_REFERENCE_PAYLOAD_BYTES,
            )
            .unwrap();
        let mut measured = AssetLoadBudget::default();
        let decoded = reader
            .read(range, location.digest(), &mut measured)
            .unwrap();
        decoded.validate(&document.stable_id).unwrap();
        let usage = measured.usage();
        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        reader.read(range, location.digest(), &mut exact).unwrap();
        assert_eq!(exact.usage(), usage);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .unwrap();
        let error = reader
            .read(range, location.digest(), &mut one_short)
            .unwrap_err();
        assert!(matches!(
            error,
            ReferencePayloadReadError::Json(BudgetedJsonError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn valid_payload_rejects_a_mismatched_expected_digest() {
        let document = projected_reference();
        let (_directory, reader, location) = open_written_reader(&document);
        let range = reader
            .validate_range(
                location.offset(),
                location.length(),
                MAX_REFERENCE_PAYLOAD_BYTES,
            )
            .unwrap();
        let expected = DigestV1::hash_bytes(b"another reference payload");

        let error = reader
            .read(range, expected, &mut AssetLoadBudget::default())
            .unwrap_err();

        assert!(matches!(
            error,
            ReferencePayloadReadError::DigestMismatch { expected: actual_expected, actual }
                if actual_expected == expected && actual == location.digest()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn opened_sidecar_rejects_same_length_in_place_rewrite() {
        use std::time::{Duration, UNIX_EPOCH};

        let document = projected_reference();
        let (directory, reader, location) = open_written_reader(&document);
        let path = directory.path().join(REFERENCE_PAYLOAD_FILE);
        let mut bytes = fs::read(&path).unwrap();
        let stable_id = b"reference-a";
        let offset = bytes
            .windows(stable_id.len())
            .position(|window| window == stable_id)
            .unwrap();
        bytes[offset..offset + stable_id.len()].copy_from_slice(b"reference-x");
        fs::write(&path, bytes).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800)),
            )
            .unwrap();

        let range = reader
            .validate_range(
                location.offset(),
                location.length(),
                MAX_REFERENCE_PAYLOAD_BYTES,
            )
            .unwrap();
        let error = reader
            .read(range, location.digest(), &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ReferencePayloadReadError::SecureRead(AnchoredFsError::IdentityChanged)
        ));
    }
}
