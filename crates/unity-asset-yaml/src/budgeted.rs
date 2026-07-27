//! Budgeted Unity YAML parsing for caller-owned source images.

use std::collections::TryReserveError;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::str::{CharIndices, Utf8Error};
use std::sync::Arc;
use std::time::SystemTime;

use same_file::Handle as FileIdentity;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, UnityClass, UnityClassHeader, UnityValue,
    YamlAnchor, arc_slice_allocation_bytes, arc_value_allocation_bytes,
};
use yaml_rust2::ScanError;
use yaml_rust2::parser::{Event, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

use crate::YamlDocument;

const MAX_YAML_DEPTH: u32 = 59;
// These constants bound requested container capacities in the locked parser; they do not claim an
// exact allocator RSS value or include allocator-private bookkeeping.
// yaml-rust2 0.11.0 requests four 32-byte strings before scanning each plain scalar. A block scalar's
// only fixed buffer is smaller (100 bytes), so this covers fixed scanner strings at every site.
const YAML_RUST2_0_11_FIXED_SCANNER_BYTES_PER_LEXICAL_SITE: u64 = 4 * 32;
// Token, simple-key, indent, parser-state, anchor, and tag slots are each bounded by 16 machine
// words in yaml-rust2 0.11.0. One lexical start can account for at most four delayed tokens, one slot
// in each of three stacks, and one entry in either parser map. Doubling eight slots covers the
// cumulative requested capacities after geometric growth; the fixed allowance below covers the
// initial minimum capacities of small containers.
const YAML_RUST2_0_11_CONTAINER_BYTES_PER_LEXICAL_SITE: u64 = 16 * 16 * size_of::<usize>() as u64;
const YAML_RUST2_0_11_BYTES_PER_LEXICAL_SITE: u64 =
    YAML_RUST2_0_11_FIXED_SCANNER_BYTES_PER_LEXICAL_SITE
        + YAML_RUST2_0_11_CONTAINER_BYTES_PER_LEXICAL_SITE;
// Scalar contents can pass through four scanner strings, token/event ownership, and one tag clone.
// Sixteen input-sized byte ranges bound those requested capacities, including geometric growth.
const YAML_RUST2_0_11_VARIABLE_BYTES_PER_INPUT_BYTE: u64 = 16;
// The scanner's flow level is a u8 and the adapter rejects retained depth 60. At 64-bit pointer
// width, two-times geometric growth for 256 simple keys plus 60 parser states and 60 indents stays
// below 96 KiB using the 16-word slot bound. Round up for fixed control/error allocations; any
// unresolved source-proportional nesting is covered by the per-site term above.
const YAML_RUST2_0_11_FIXED_CONTAINER_BYTES: u64 = 128 * 1024;
const UNITY_TAG_PREFIX: &str = "tag:unity3d.com,2011:";
const UNITY_DOCUMENT_CLASS_NAME: &str = "YamlDocument";

/// A parsed YAML document that retains the exact bytes used to fingerprint its source.
#[derive(Debug, Clone)]
pub struct BudgetedYamlSource {
    encoded: Arc<[u8]>,
    budgeted_encoded: BudgetedSourceBytes,
    document: Arc<YamlDocument>,
}

impl BudgetedYamlSource {
    /// Returns the exact encoded bytes retained by the parsed source.
    pub fn encoded(&self) -> &Arc<[u8]> {
        &self.encoded
    }

    /// Returns the parsed Unity YAML document.
    pub fn document(&self) -> &Arc<YamlDocument> {
        &self.document
    }

    /// Returns the budget-bound source proof and parsed document.
    pub fn into_budgeted_parts(
        self,
        budget: &AssetLoadBudget,
    ) -> Result<(BudgetedSourceBytes, Arc<YamlDocument>), BudgetError> {
        self.budgeted_encoded.validate_budget(budget)?;
        Ok((self.budgeted_encoded, self.document))
    }

    fn attach_path(&mut self, path: PathBuf) {
        let document = Arc::get_mut(&mut self.document)
            .expect("a newly parsed YAML source uniquely owns its document");
        document.set_file_path(path);
    }
}

/// Typed failures produced while parsing a budgeted Unity YAML source.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BudgetedYamlError {
    #[error("failed to {operation} YAML source {path:?}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "YAML source {path:?} has length {length}, which cannot fit in memory on this platform"
    )]
    SourceTooLarge { path: PathBuf, length: u64 },
    #[error("YAML source changed while it was being read: {path:?}")]
    SourceChanged { path: PathBuf },
    #[error("Unity YAML input is not valid UTF-8 at byte {valid_up_to}: {source}")]
    InvalidUtf8 {
        valid_up_to: usize,
        #[source]
        source: Utf8Error,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Syntax(#[from] ScanError),
    #[error("failed to reserve {requested} bytes for {context}: {source}")]
    AllocationFailed {
        context: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("failed to reserve {requested} bytes for {context}: {source}")]
    IndexMapAllocationFailed {
        context: &'static str,
        requested: usize,
        #[source]
        source: indexmap::TryReserveError,
    },
    #[error("invalid Unity YAML header at line {line}, column {column}: {reason}")]
    InvalidHeader {
        line: usize,
        column: usize,
        reason: &'static str,
    },
    #[error("YAML aliases are not supported at line {line}, column {column}")]
    AliasUnsupported { line: usize, column: usize },
    #[error("unexpected YAML anchor at line {line}, column {column}")]
    UnexpectedAnchor { line: usize, column: usize },
    #[error("unexpected YAML tag at line {line}, column {column}")]
    UnexpectedTag { line: usize, column: usize },
    #[error("YAML merge keys are not supported at line {line}, column {column}")]
    MergeKeyUnsupported { line: usize, column: usize },
    #[error("complex YAML mapping keys are not supported at line {line}, column {column}")]
    ComplexKeyUnsupported { line: usize, column: usize },
    #[error("duplicate YAML mapping key {key:?} at line {line}, column {column}")]
    DuplicateKey {
        key: String,
        line: usize,
        column: usize,
    },
    #[error("Unity YAML nesting depth {actual} exceeds the hard limit {limit}")]
    DepthExceeded { actual: u32, limit: u32 },
    #[error("invalid Unity YAML document at line {line}, column {column}: {reason}")]
    InvalidDocument {
        line: usize,
        column: usize,
        reason: &'static str,
    },
}

use BudgetedYamlError as YamlAdapterError;

/// Parses an unaccounted YAML image and charges its shared allocation to `budget`.
///
/// Parsing uses an iterative event stream and does not materialize an intermediate value tree.
pub fn parse_budgeted_yaml_source(
    encoded: Arc<[u8]>,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedYamlSource, BudgetedYamlError> {
    let encoded = BudgetedSourceBytes::from_arc(encoded, budget)?;
    parse_prebudgeted_yaml_source(encoded, budget)
}

/// Parses a source image whose shared allocation is already charged to `budget`.
///
/// A proof minted by another budget is rejected before parser work is charged.
pub fn parse_prebudgeted_yaml_source(
    budgeted_encoded: BudgetedSourceBytes,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedYamlSource, BudgetedYamlError> {
    let encoded = budgeted_encoded.clone_backing(budget)?;
    let input = std::str::from_utf8(&encoded).map_err(|source| YamlAdapterError::InvalidUtf8 {
        valid_up_to: source.valid_up_to(),
        source,
    })?;
    charge_yaml_rust2_allocation_envelope(input, budget)?;

    let headers = scan_headers(input, budget)?;
    let parser_input = StrippedHeaderChars::new(input, &headers)?;
    // Unity declares `!u!` once at the beginning of a multi-document file and reuses it for every
    // following document, despite standard YAML limiting tag directives to one document.
    let mut parser = Parser::new(parser_input).keep_tags(true);
    let mut document_entries = Vec::new();
    let mut document_capacity = 0;
    let mut header_cursor = 0;
    let mut document_ordinal = 0_u64;
    let mut current: Option<DocumentBuilder<'_>> = None;

    loop {
        let (event, mark) = parser.next_token()?;
        match event {
            Event::Nothing => {
                return Err(invalid_document(mark, "parser emitted an internal event"));
            }
            Event::StreamStart => {}
            Event::StreamEnd => {
                if current.is_some() {
                    return Err(invalid_document(mark, "stream ended inside a document"));
                }
                if header_cursor != headers.len() {
                    let header = headers[header_cursor];
                    return Err(YamlAdapterError::InvalidHeader {
                        line: header.line,
                        column: 1,
                        reason: "Unity document header was not consumed by the YAML parser",
                    });
                }
                break;
            }
            Event::DocumentStart => {
                if current.is_some() {
                    return Err(invalid_document(mark, "nested document start"));
                }
                let header = take_header_for_line(&headers, &mut header_cursor, mark.line())?;
                current = Some(DocumentBuilder::new(header));
            }
            Event::DocumentEnd => {
                let builder = current
                    .take()
                    .ok_or_else(|| invalid_document(mark, "document end without a start"))?;
                let class = builder.finish(document_ordinal, budget, mark)?;
                push_document_entry(&mut document_entries, &mut document_capacity, class, budget)?;
                document_ordinal =
                    document_ordinal
                        .checked_add(1)
                        .ok_or(YamlAdapterError::InvalidDocument {
                            line: mark.line(),
                            column: display_column(mark),
                            reason: "document ordinal overflow",
                        })?;
            }
            Event::Alias(_) => {
                return Err(YamlAdapterError::AliasUnsupported {
                    line: mark.line(),
                    column: display_column(mark),
                });
            }
            Event::Scalar(value, style, anchor, tag) => current_builder(&mut current, mark)?
                .scalar(value, style, anchor, tag, budget, mark)?,
            Event::SequenceStart(anchor, tag) => current_builder(&mut current, mark)?
                .start_container(ContainerKind::Sequence, anchor, tag, budget, mark)?,
            Event::SequenceEnd => current_builder(&mut current, mark)?.end_container(
                ContainerKind::Sequence,
                budget,
                mark,
            )?,
            Event::MappingStart(anchor, tag) => current_builder(&mut current, mark)?
                .start_container(ContainerKind::Mapping, anchor, tag, budget, mark)?,
            Event::MappingEnd => current_builder(&mut current, mark)?.end_container(
                ContainerKind::Mapping,
                budget,
                mark,
            )?,
        }
    }

    let document_allocation = arc_value_allocation_bytes::<YamlDocument>()
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(document_allocation)?;
    budget.consume_bytes(document_allocation)?;
    let document = YamlDocument::from_entries(document_entries);
    Ok(BudgetedYamlSource {
        encoded,
        budgeted_encoded,
        document: Arc::new(document),
    })
}

/// Opens, verifies, accounts, and parses one YAML file under a caller-owned budget.
///
/// The file length is checked before allocation. The implementation then reads the file twice
/// through the same handle and rejects truncation, growth, replacement, or same-length content
/// changes before parsing the already-accounted source image.
pub fn load_budgeted_yaml_path(
    path: impl AsRef<Path>,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedYamlSource, BudgetedYamlError> {
    let path = path.as_ref();
    let mut file = File::open(path).map_err(|source| io_error("open", path, source))?;
    let before = file
        .metadata()
        .map_err(|source| io_error("inspect", path, source))?;
    let before = SourceMetadata::from_metadata(&before);
    let encoded = read_budgeted_yaml_image(&mut file, path, before.length, budget)?;
    verify_open_file_binding(file, path, before)?;

    let mut source = parse_prebudgeted_yaml_source(encoded, budget)?;
    source.attach_path(clone_path_budgeted(path, budget)?);
    Ok(source)
}

/// Asynchronously opens, verifies, accounts, and parses one YAML file.
#[cfg(feature = "async")]
pub async fn load_budgeted_yaml_path_async<P>(
    path: P,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedYamlSource, BudgetedYamlError>
where
    P: AsRef<Path> + Send + Sync,
{
    let path = path.as_ref();
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| io_error("open", path, source))?;
    let before = file
        .metadata()
        .await
        .map_err(|source| io_error("inspect", path, source))?;
    let before = SourceMetadata::from_metadata(&before);
    let encoded = read_budgeted_yaml_image_async(&mut file, path, before.length, budget).await?;
    verify_open_file_binding_async(file, path, before).await?;

    let mut source = parse_prebudgeted_yaml_source(encoded, budget)?;
    source.attach_path(clone_path_budgeted(path, budget)?);
    Ok(source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceMetadata {
    length: u64,
    modified: Option<SystemTime>,
}

impl SourceMetadata {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

fn verify_open_file_binding(
    file: File,
    path: &Path,
    before: SourceMetadata,
) -> Result<(), BudgetedYamlError> {
    let after = file
        .metadata()
        .map_err(|source| io_error("reinspect", path, source))?;
    let opened_identity = FileIdentity::from_file(file)
        .map_err(|source| io_error("identify opened", path, source))?;
    let current_identity = FileIdentity::from_path(path)
        .map_err(|source| io_error("identify current", path, source))?;
    let current = current_identity
        .as_file()
        .metadata()
        .map_err(|source| io_error("reinspect path for", path, source))?;

    if before != SourceMetadata::from_metadata(&after)
        || before != SourceMetadata::from_metadata(&current)
        || opened_identity != current_identity
    {
        return Err(source_changed(path));
    }
    Ok(())
}

#[cfg(feature = "async")]
async fn verify_open_file_binding_async(
    file: tokio::fs::File,
    path: &Path,
    before: SourceMetadata,
) -> Result<(), BudgetedYamlError> {
    let after = file
        .metadata()
        .await
        .map_err(|source| io_error("reinspect", path, source))?;
    let current_file = tokio::fs::File::open(path)
        .await
        .map_err(|source| io_error("reopen current", path, source))?;
    let current = current_file
        .metadata()
        .await
        .map_err(|source| io_error("reinspect path for", path, source))?;

    if before != SourceMetadata::from_metadata(&after)
        || before != SourceMetadata::from_metadata(&current)
    {
        return Err(source_changed(path));
    }

    let opened_file = file.into_std().await;
    let current_file = current_file.into_std().await;
    let same_identity = tokio::task::spawn_blocking(move || {
        let opened_identity = FileIdentity::from_file(opened_file)?;
        let current_identity = FileIdentity::from_file(current_file)?;
        Ok::<_, std::io::Error>(opened_identity == current_identity)
    })
    .await
    .map_err(|source| {
        io_error(
            "join identity check for",
            path,
            std::io::Error::other(source),
        )
    })?
    .map_err(|source| io_error("compare identity for", path, source))?;
    if !same_identity {
        return Err(source_changed(path));
    }
    Ok(())
}

fn read_budgeted_yaml_image(
    reader: &mut (impl Read + Seek),
    path: &Path,
    length: u64,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedSourceBytes, BudgetedYamlError> {
    let length_usize = checked_source_length(path, length)?;
    preflight_source_image(length, length_usize, budget)?;

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length_usize)
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context: "YAML source image",
            requested: length_usize,
            source,
        })?;
    budget.consume_bytes(length)?;
    bytes.resize(length_usize, 0);
    read_exact_stable(reader, &mut bytes, path)?;
    verify_stable_contents(reader, &bytes, path, budget)?;
    BudgetedSourceBytes::from_vec(bytes, budget).map_err(YamlAdapterError::from)
}

#[cfg(feature = "async")]
async fn read_budgeted_yaml_image_async(
    reader: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin),
    path: &Path,
    length: u64,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedSourceBytes, BudgetedYamlError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let length_usize = checked_source_length(path, length)?;
    preflight_source_image(length, length_usize, budget)?;

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length_usize)
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context: "YAML source image",
            requested: length_usize,
            source,
        })?;
    budget.consume_bytes(length)?;
    bytes.resize(length_usize, 0);
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|source| stable_read_error(path, source))?;

    budget.consume_bytes(length)?;
    reader
        .seek(SeekFrom::Start(0))
        .await
        .map_err(|source| io_error("seek", path, source))?;
    let mut verified = 0;
    let mut chunk = [0_u8; 64 * 1024];
    while verified < bytes.len() {
        let count = chunk.len().min(bytes.len() - verified);
        reader
            .read_exact(&mut chunk[..count])
            .await
            .map_err(|source| stable_read_error(path, source))?;
        if chunk[..count] != bytes[verified..verified + count] {
            return Err(source_changed(path));
        }
        verified += count;
    }
    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .await
        .map_err(|source| io_error("verify", path, source))?
        != 0
    {
        return Err(source_changed(path));
    }

    BudgetedSourceBytes::from_vec(bytes, budget).map_err(YamlAdapterError::from)
}

fn checked_source_length(path: &Path, length: u64) -> Result<usize, BudgetedYamlError> {
    usize::try_from(length).map_err(|_| YamlAdapterError::SourceTooLarge {
        path: path.to_path_buf(),
        length,
    })
}

fn preflight_source_image(
    length: u64,
    length_usize: usize,
    budget: &AssetLoadBudget,
) -> Result<(), BudgetedYamlError> {
    let retained_bytes = arc_slice_allocation_bytes::<u8>(length_usize).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "YAML source image",
        }
    })?;
    let planned_bytes = length
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(retained_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "YAML source image",
        })?;
    budget.check_bytes(planned_bytes)?;
    Ok(())
}

fn verify_stable_contents(
    reader: &mut (impl Read + Seek),
    expected: &[u8],
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), BudgetedYamlError> {
    budget.consume_bytes(usize_to_u64(expected.len())?)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|source| io_error("seek", path, source))?;

    let mut verified = 0;
    let mut chunk = [0_u8; 64 * 1024];
    while verified < expected.len() {
        let count = chunk.len().min(expected.len() - verified);
        read_exact_stable(reader, &mut chunk[..count], path)?;
        if chunk[..count] != expected[verified..verified + count] {
            return Err(source_changed(path));
        }
        verified += count;
    }

    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .map_err(|source| io_error("verify", path, source))?
        != 0
    {
        return Err(source_changed(path));
    }
    Ok(())
}

fn read_exact_stable(
    reader: &mut impl Read,
    bytes: &mut [u8],
    path: &Path,
) -> Result<(), BudgetedYamlError> {
    reader
        .read_exact(bytes)
        .map_err(|source| stable_read_error(path, source))
}

fn stable_read_error(path: &Path, source: std::io::Error) -> BudgetedYamlError {
    if source.kind() == std::io::ErrorKind::UnexpectedEof {
        source_changed(path)
    } else {
        io_error("read", path, source)
    }
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> BudgetedYamlError {
    YamlAdapterError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn source_changed(path: &Path) -> BudgetedYamlError {
    YamlAdapterError::SourceChanged {
        path: path.to_path_buf(),
    }
}

fn clone_path_budgeted(
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, BudgetedYamlError> {
    let requested = path.as_os_str().len();
    budget.check_bytes(usize_to_u64(requested)?)?;
    let mut owned = OsString::new();
    owned
        .try_reserve_exact(requested)
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context: "YAML source path",
            requested,
            source,
        })?;
    budget.consume_bytes(usize_to_u64(requested)?)?;
    owned.push(path);
    Ok(PathBuf::from(owned))
}

fn current_builder<'a, 'input>(
    current: &'a mut Option<DocumentBuilder<'input>>,
    mark: Marker,
) -> Result<&'a mut DocumentBuilder<'input>, YamlAdapterError> {
    current
        .as_mut()
        .ok_or_else(|| invalid_document(mark, "node event outside a document"))
}

fn charge_yaml_rust2_allocation_envelope(
    input: &str,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    let envelope = yaml_rust2_allocation_envelope(input)?;
    budget.check_bytes(envelope)?;
    budget.consume_bytes(envelope)?;
    Ok(())
}

fn yaml_rust2_allocation_envelope(input: &str) -> Result<u64, YamlAdapterError> {
    let encoded = usize_to_u64(input.len())?;
    let lexical_sites = yaml_lexical_allocation_sites(input)?;
    let lexical_capacity = lexical_sites
        .checked_mul(YAML_RUST2_0_11_BYTES_PER_LEXICAL_SITE)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let variable_capacity = encoded
        .checked_mul(YAML_RUST2_0_11_VARIABLE_BYTES_PER_INPUT_BYTE)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let envelope = lexical_capacity
        .checked_add(variable_capacity)
        .and_then(|value| value.checked_add(YAML_RUST2_0_11_FIXED_CONTAINER_BYTES))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    Ok(envelope)
}

// Each yaml-rust2 scalar scanner starts at the stream beginning or after YAML separation or an
// indicator. Counting every nonblank character at those positions therefore over-approximates
// allocation-producing lexical starts, including malformed input and continuation-line words.
fn yaml_lexical_allocation_sites(input: &str) -> Result<u64, YamlAdapterError> {
    let mut sites = 0_u64;
    let mut previous_is_boundary = true;
    for character in input.chars() {
        let blank_or_break = is_yaml_blank_or_break(character);
        if !blank_or_break && previous_is_boundary {
            sites = sites
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
        }
        previous_is_boundary = blank_or_break || is_yaml_indicator(character);
    }
    Ok(sites)
}

const fn is_yaml_blank_or_break(character: char) -> bool {
    matches!(
        character,
        ' ' | '\t' | '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

const fn is_yaml_indicator(character: char) -> bool {
    matches!(
        character,
        '-' | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    )
}

#[derive(Debug, Clone, Copy)]
struct DocumentHeader<'a> {
    class_id: i32,
    anchor: &'a str,
    line: usize,
    stripped_range: Option<(usize, usize)>,
}

fn scan_headers<'a>(
    input: &'a str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<DocumentHeader<'a>>, YamlAdapterError> {
    let mut headers = Vec::new();
    let mut accounted_capacity = 0;
    let mut line_start = 0;

    for (line_index, raw_line) in input.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        validate_tag_directive(line, line_index + 1)?;
        if let Some(header) = parse_header_line(line, line_start, line_index + 1)? {
            reserve_budgeted_vec(
                &mut headers,
                &mut accounted_capacity,
                1,
                budget,
                "Unity YAML header table",
            )?;
            headers.push(header);
        }
        line_start =
            line_start
                .checked_add(raw_line.len())
                .ok_or(YamlAdapterError::InvalidHeader {
                    line: line_index + 1,
                    column: 1,
                    reason: "input offset overflow",
                })?;
    }

    Ok(headers)
}

fn validate_tag_directive(line: &str, line_number: usize) -> Result<(), YamlAdapterError> {
    if line != "%TAG" && !line.starts_with("%TAG ") && !line.starts_with("%TAG\t") {
        return Ok(());
    }
    let mut tokens = HeaderTokens::new(line);
    let directive = tokens.next().map(|token| token.text);
    let handle = tokens.next().map(|token| token.text);
    let prefix = tokens.next().map(|token| token.text);
    if directive != Some("%TAG")
        || handle != Some("!u!")
        || prefix != Some(UNITY_TAG_PREFIX)
        || tokens.next().is_some()
    {
        return Err(YamlAdapterError::UnexpectedTag {
            line: line_number,
            column: 1,
        });
    }
    Ok(())
}

fn parse_header_line<'a>(
    line: &'a str,
    absolute_start: usize,
    line_number: usize,
) -> Result<Option<DocumentHeader<'a>>, YamlAdapterError> {
    if !line.starts_with("---") {
        return Ok(None);
    }
    let mut tokens = HeaderTokens::new(line);
    let Some(document_start) = tokens.next() else {
        return Ok(None);
    };
    if document_start.text != "---" {
        return Ok(None);
    }
    let Some(tag) = tokens.next() else {
        return Ok(None);
    };
    let Some(class_id_text) = tag.text.strip_prefix("!u!") else {
        return Ok(None);
    };
    let class_id = class_id_text
        .parse::<i32>()
        .map_err(|_| YamlAdapterError::InvalidHeader {
            line: line_number,
            column: tag.start + 1,
            reason: "class id must be a positive i32",
        })?;
    if class_id <= 0 {
        return Err(YamlAdapterError::InvalidHeader {
            line: line_number,
            column: tag.start + 1,
            reason: "class id must be a positive i32",
        });
    }

    let anchor_token = tokens.next().ok_or(YamlAdapterError::InvalidHeader {
        line: line_number,
        column: tag.end + 1,
        reason: "Unity document header requires an anchor",
    })?;
    let anchor = anchor_token
        .text
        .strip_prefix('&')
        .filter(|value| valid_unity_anchor(value))
        .ok_or(YamlAdapterError::InvalidHeader {
            line: line_number,
            column: anchor_token.start + 1,
            reason: "anchor must be a signed decimal identifier",
        })?;

    let extra = tokens.next();
    let stripped_range = match extra {
        None => None,
        Some(token) if token.text == "stripped" => Some((
            absolute_start
                .checked_add(token.start)
                .ok_or(YamlAdapterError::InvalidHeader {
                    line: line_number,
                    column: token.start + 1,
                    reason: "header offset overflow",
                })?,
            absolute_start
                .checked_add(token.end)
                .ok_or(YamlAdapterError::InvalidHeader {
                    line: line_number,
                    column: token.start + 1,
                    reason: "header offset overflow",
                })?,
        )),
        Some(token) => {
            return Err(YamlAdapterError::InvalidHeader {
                line: line_number,
                column: token.start + 1,
                reason: "only the Unity stripped marker is supported after an anchor",
            });
        }
    };
    if let Some(token) = tokens.next() {
        return Err(YamlAdapterError::InvalidHeader {
            line: line_number,
            column: token.start + 1,
            reason: "unexpected data after Unity document header",
        });
    }

    Ok(Some(DocumentHeader {
        class_id,
        anchor,
        line: line_number,
        stripped_range,
    }))
}

fn valid_unity_anchor(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && YamlAnchor::validate(value).is_ok()
}

#[derive(Clone, Copy)]
struct HeaderToken<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

struct HeaderTokens<'a> {
    line: &'a str,
    cursor: usize,
}

impl<'a> HeaderTokens<'a> {
    fn new(line: &'a str) -> Self {
        Self { line, cursor: 0 }
    }

    fn next(&mut self) -> Option<HeaderToken<'a>> {
        let bytes = self.line.as_bytes();
        while self.cursor < bytes.len() && bytes[self.cursor].is_ascii_whitespace() {
            self.cursor += 1;
        }
        if self.cursor == bytes.len() || bytes[self.cursor] == b'#' {
            return None;
        }
        let start = self.cursor;
        while self.cursor < bytes.len() && !bytes[self.cursor].is_ascii_whitespace() {
            self.cursor += 1;
        }
        Some(HeaderToken {
            text: &self.line[start..self.cursor],
            start,
            end: self.cursor,
        })
    }
}

/// A zero-allocation parser view that hides Unity's non-YAML `stripped` header marker.
struct StrippedHeaderChars<'input, 'headers> {
    chars: CharIndices<'input>,
    headers: &'headers [DocumentHeader<'input>],
    header_cursor: usize,
}

impl<'input, 'headers> StrippedHeaderChars<'input, 'headers> {
    fn new(
        input: &'input str,
        headers: &'headers [DocumentHeader<'input>],
    ) -> Result<Self, YamlAdapterError> {
        let mut previous_end = 0;
        for header in headers {
            let Some((start, end)) = header.stripped_range else {
                continue;
            };
            if start < previous_end || input.get(start..end) != Some("stripped") {
                return Err(YamlAdapterError::InvalidHeader {
                    line: header.line,
                    column: 1,
                    reason: "stripped marker range is outside the input",
                });
            }
            previous_end = end;
        }

        Ok(Self {
            chars: input.char_indices(),
            headers,
            header_cursor: 0,
        })
    }
}

impl Iterator for StrippedHeaderChars<'_, '_> {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        let (offset, character) = self.chars.next()?;
        loop {
            let Some(header) = self.headers.get(self.header_cursor) else {
                return Some(character);
            };
            let Some((start, end)) = header.stripped_range else {
                self.header_cursor += 1;
                continue;
            };
            if offset >= end {
                self.header_cursor += 1;
                continue;
            }
            return Some(if offset >= start { ' ' } else { character });
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chars.size_hint()
    }
}

fn take_header_for_line<'a>(
    headers: &[DocumentHeader<'a>],
    cursor: &mut usize,
    line: usize,
) -> Result<Option<DocumentHeader<'a>>, YamlAdapterError> {
    let Some(header) = headers.get(*cursor).copied() else {
        return Ok(None);
    };
    if header.line < line {
        return Err(YamlAdapterError::InvalidHeader {
            line: header.line,
            column: 1,
            reason: "Unity document header did not start a YAML document",
        });
    }
    if header.line != line {
        return Ok(None);
    }
    *cursor += 1;
    Ok(Some(header))
}

struct DocumentBuilder<'a> {
    header: Option<DocumentHeader<'a>>,
    root: Option<UnityValue>,
    frames: Vec<Frame>,
    frame_capacity: usize,
}

impl<'a> DocumentBuilder<'a> {
    fn new(header: Option<DocumentHeader<'a>>) -> Self {
        Self {
            header,
            root: None,
            frames: Vec::new(),
            frame_capacity: 0,
        }
    }

    fn scalar(
        &mut self,
        value: String,
        style: TScalarStyle,
        anchor: usize,
        tag: Option<Tag>,
        budget: &mut AssetLoadBudget,
        mark: Marker,
    ) -> Result<(), YamlAdapterError> {
        if self.mapping_expects_key() {
            validate_node_metadata(None, false, anchor, tag, mark)?;
            observe_yaml_depth(self.frames.len(), budget)?;
            budget.consume_entries(1)?;
            charge_retained_string(&value, budget)?;
            return self.accept_mapping_key(value, mark);
        }

        let is_root = self.frames.is_empty() && self.root.is_none();
        validate_node_metadata(self.header, is_root, anchor, tag, mark)?;
        observe_yaml_depth(self.frames.len(), budget)?;
        budget.consume_entries(1)?;
        let value = parse_scalar(value, style, budget)?;
        self.attach(value, budget, mark)
    }

    fn start_container(
        &mut self,
        kind: ContainerKind,
        anchor: usize,
        tag: Option<Tag>,
        budget: &mut AssetLoadBudget,
        mark: Marker,
    ) -> Result<(), YamlAdapterError> {
        if self.mapping_expects_key() {
            return Err(YamlAdapterError::ComplexKeyUnsupported {
                line: mark.line(),
                column: display_column(mark),
            });
        }
        let is_root = self.frames.is_empty() && self.root.is_none();
        validate_node_metadata(self.header, is_root, anchor, tag, mark)?;
        observe_yaml_depth(self.frames.len(), budget)?;
        budget.consume_entries(1)?;
        reserve_budgeted_vec(
            &mut self.frames,
            &mut self.frame_capacity,
            1,
            budget,
            "Unity YAML parser frame stack",
        )?;
        self.frames.push(Frame::new(kind));
        Ok(())
    }

    fn end_container(
        &mut self,
        expected: ContainerKind,
        budget: &mut AssetLoadBudget,
        mark: Marker,
    ) -> Result<(), YamlAdapterError> {
        let frame = self
            .frames
            .pop()
            .ok_or_else(|| invalid_document(mark, "container end without a start"))?;
        if frame.kind() != expected {
            return Err(invalid_document(mark, "mismatched container end"));
        }
        let value = frame.into_value(mark)?;
        self.attach(value, budget, mark)
    }

    fn mapping_expects_key(&self) -> bool {
        matches!(
            self.frames.last(),
            Some(Frame::Mapping {
                pending_key: None,
                ..
            })
        )
    }

    fn accept_mapping_key(&mut self, key: String, mark: Marker) -> Result<(), YamlAdapterError> {
        if key == "<<" {
            return Err(YamlAdapterError::MergeKeyUnsupported {
                line: mark.line(),
                column: display_column(mark),
            });
        }
        let Some(Frame::Mapping {
            value, pending_key, ..
        }) = self.frames.last_mut()
        else {
            return Err(invalid_document(mark, "mapping key outside a mapping"));
        };
        let UnityValue::Object(map) = value else {
            return Err(invalid_document(mark, "mapping frame lost its object"));
        };
        if map.contains_key(&key) {
            return Err(YamlAdapterError::DuplicateKey {
                key,
                line: mark.line(),
                column: display_column(mark),
            });
        }
        *pending_key = Some(key);
        Ok(())
    }

    fn attach(
        &mut self,
        value: UnityValue,
        budget: &mut AssetLoadBudget,
        mark: Marker,
    ) -> Result<(), YamlAdapterError> {
        let Some(parent) = self.frames.last_mut() else {
            if self.root.replace(value).is_some() {
                return Err(invalid_document(mark, "document contains multiple roots"));
            }
            return Ok(());
        };

        budget.check_members(1)?;
        match parent {
            Frame::Sequence {
                values,
                accounted_capacity,
            } => {
                reserve_budgeted_vec(values, accounted_capacity, 1, budget, "Unity YAML sequence")?;
                budget.consume_members(1)?;
                values.push(value);
            }
            Frame::Mapping {
                value: object,
                pending_key,
                accounted_capacity,
            } => {
                let key = pending_key
                    .take()
                    .ok_or_else(|| invalid_document(mark, "mapping value has no key"))?;
                reserve_budgeted_object(object, *accounted_capacity, accounted_capacity, budget)?;
                let UnityValue::Object(map) = object else {
                    return Err(invalid_document(mark, "mapping frame lost its object"));
                };
                if map.contains_key(&key) {
                    return Err(YamlAdapterError::DuplicateKey {
                        key,
                        line: mark.line(),
                        column: display_column(mark),
                    });
                }
                budget.consume_members(1)?;
                map.insert(key, value);
            }
        }
        Ok(())
    }

    fn finish(
        self,
        ordinal: u64,
        budget: &mut AssetLoadBudget,
        mark: Marker,
    ) -> Result<UnityClass, YamlAdapterError> {
        if !self.frames.is_empty() {
            return Err(invalid_document(mark, "document ended inside a container"));
        }
        let root = self
            .root
            .ok_or_else(|| invalid_document(mark, "document has no root value"))?;

        match self.header {
            Some(header) => finish_unity_document(header, root, budget, mark),
            None => finish_plain_document(ordinal, root, budget, mark),
        }
    }
}

fn observe_yaml_depth(
    frame_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    let depth = u32::try_from(frame_count).map_err(|_| YamlAdapterError::DepthExceeded {
        actual: u32::MAX,
        limit: MAX_YAML_DEPTH,
    })?;
    if depth > MAX_YAML_DEPTH {
        return Err(YamlAdapterError::DepthExceeded {
            actual: depth,
            limit: MAX_YAML_DEPTH,
        });
    }
    budget.observe_depth(depth)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    Sequence,
    Mapping,
}

enum Frame {
    Sequence {
        values: Vec<UnityValue>,
        accounted_capacity: usize,
    },
    Mapping {
        value: UnityValue,
        pending_key: Option<String>,
        accounted_capacity: usize,
    },
}

impl Frame {
    fn new(kind: ContainerKind) -> Self {
        match kind {
            ContainerKind::Sequence => Self::Sequence {
                values: Vec::new(),
                accounted_capacity: 0,
            },
            ContainerKind::Mapping => Self::Mapping {
                value: UnityValue::Object(Default::default()),
                pending_key: None,
                accounted_capacity: 0,
            },
        }
    }

    fn kind(&self) -> ContainerKind {
        match self {
            Self::Sequence { .. } => ContainerKind::Sequence,
            Self::Mapping { .. } => ContainerKind::Mapping,
        }
    }

    fn into_value(self, mark: Marker) -> Result<UnityValue, YamlAdapterError> {
        match self {
            Self::Sequence { values, .. } => Ok(UnityValue::Array(values)),
            Self::Mapping {
                value,
                pending_key: None,
                ..
            } => Ok(value),
            Self::Mapping { .. } => Err(invalid_document(mark, "mapping ended without a value")),
        }
    }
}

fn validate_node_metadata(
    header: Option<DocumentHeader<'_>>,
    is_root: bool,
    anchor: usize,
    tag: Option<Tag>,
    mark: Marker,
) -> Result<(), YamlAdapterError> {
    if !is_root {
        if anchor != 0 {
            return Err(YamlAdapterError::UnexpectedAnchor {
                line: mark.line(),
                column: display_column(mark),
            });
        }
        if tag.is_some() {
            return Err(YamlAdapterError::UnexpectedTag {
                line: mark.line(),
                column: display_column(mark),
            });
        }
        return Ok(());
    }

    match header {
        Some(header) => {
            if anchor == 0 {
                return Err(YamlAdapterError::InvalidHeader {
                    line: header.line,
                    column: 1,
                    reason: "Unity document anchor was not attached to its root",
                });
            }
            let Some(tag) = tag else {
                return Err(YamlAdapterError::InvalidHeader {
                    line: header.line,
                    column: 1,
                    reason: "Unity document tag was not attached to its root",
                });
            };
            if tag.handle != UNITY_TAG_PREFIX
                || tag.suffix.parse::<i32>().ok() != Some(header.class_id)
            {
                return Err(YamlAdapterError::UnexpectedTag {
                    line: mark.line(),
                    column: display_column(mark),
                });
            }
        }
        None => {
            if anchor != 0 {
                return Err(YamlAdapterError::UnexpectedAnchor {
                    line: mark.line(),
                    column: display_column(mark),
                });
            }
            if tag.is_some() {
                return Err(YamlAdapterError::UnexpectedTag {
                    line: mark.line(),
                    column: display_column(mark),
                });
            }
        }
    }
    Ok(())
}

fn parse_scalar(
    value: String,
    style: TScalarStyle,
    budget: &mut AssetLoadBudget,
) -> Result<UnityValue, YamlAdapterError> {
    if style != TScalarStyle::Plain {
        charge_retained_string(&value, budget)?;
        return Ok(UnityValue::String(value));
    }

    let parsed = match value.as_str() {
        "" | "~" | "null" | "Null" | "NULL" => Some(UnityValue::Null),
        "true" | "True" | "TRUE" => Some(UnityValue::Bool(true)),
        "false" | "False" | "FALSE" => Some(UnityValue::Bool(false)),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            Some(UnityValue::Float(f64::INFINITY))
        }
        "-.inf" | "-.Inf" | "-.INF" => Some(UnityValue::Float(f64::NEG_INFINITY)),
        ".nan" | ".NaN" | ".NAN" => Some(UnityValue::Float(f64::NAN)),
        _ => parse_plain_number(&value),
    };
    if let Some(parsed) = parsed {
        return Ok(parsed);
    }

    charge_retained_string(&value, budget)?;
    Ok(UnityValue::String(value))
}

fn parse_plain_number(value: &str) -> Option<UnityValue> {
    if is_multi_digit_zero_prefixed_decimal(value) {
        return None;
    }
    if let Some(hex) = value.strip_prefix("0x") {
        return parse_radix_number(hex, 16);
    }
    if let Some(octal) = value.strip_prefix("0o") {
        return parse_radix_number(octal, 8);
    }
    if let Ok(integer) = value.parse::<i64>() {
        return Some(UnityValue::Integer(integer));
    }
    if let Ok(unsigned) = value.strip_prefix('+').unwrap_or(value).parse::<u64>() {
        return Some(UnityValue::from(unsigned));
    }
    if value.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        return value.parse::<f64>().ok().map(UnityValue::Float);
    }
    None
}

fn is_multi_digit_zero_prefixed_decimal(value: &str) -> bool {
    let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
    unsigned.len() > 1
        && unsigned.starts_with('0')
        && unsigned.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_radix_number(value: &str, radix: u32) -> Option<UnityValue> {
    i64::from_str_radix(value, radix)
        .map(UnityValue::Integer)
        .or_else(|_| u64::from_str_radix(value, radix).map(UnityValue::from))
        .ok()
}

fn finish_unity_document(
    header: DocumentHeader<'_>,
    root: UnityValue,
    budget: &mut AssetLoadBudget,
    mark: Marker,
) -> Result<UnityClass, YamlAdapterError> {
    let UnityValue::Object(root) = root else {
        return Err(invalid_document(
            mark,
            "Unity document root must be a class mapping",
        ));
    };
    if root.len() != 1 {
        return Err(invalid_document(
            mark,
            "Unity document root must contain exactly one class",
        ));
    }
    let Some((class_name, properties)) = root.into_iter().next() else {
        return Err(invalid_document(mark, "Unity class mapping is empty"));
    };
    if class_name.is_empty() {
        return Err(invalid_document(mark, "Unity class name is empty"));
    }
    let UnityValue::Object(properties) = properties else {
        return Err(invalid_document(
            mark,
            "Unity class properties must be a mapping",
        ));
    };

    let anchor = clone_string_budgeted(header.anchor, budget, "Unity YAML object anchor")?;
    let extra_anchor_data = if header.stripped_range.is_some() {
        clone_string_budgeted("stripped", budget, "Unity YAML anchor metadata")?
    } else {
        String::new()
    };
    Ok(UnityClass::from_parts(
        UnityClassHeader::new(header.class_id, class_name, anchor, extra_anchor_data),
        properties,
    ))
}

fn finish_plain_document(
    ordinal: u64,
    root: UnityValue,
    budget: &mut AssetLoadBudget,
    mark: Marker,
) -> Result<UnityClass, YamlAdapterError> {
    let UnityValue::Object(properties) = root else {
        return Err(invalid_document(
            mark,
            "untagged Unity YAML document root must be a mapping",
        ));
    };
    let class_name = clone_string_budgeted(
        UNITY_DOCUMENT_CLASS_NAME,
        budget,
        "untagged Unity YAML class name",
    )?;
    let anchor = document_anchor(ordinal, budget)?;
    Ok(UnityClass::with_properties(
        0, class_name, anchor, properties,
    ))
}

fn document_anchor(ordinal: u64, budget: &mut AssetLoadBudget) -> Result<String, YamlAdapterError> {
    const CAPACITY: usize = "doc_".len() + 20;
    budget.check_bytes(usize_to_u64(CAPACITY)?)?;
    let mut anchor = String::new();
    anchor
        .try_reserve_exact(CAPACITY)
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context: "untagged Unity YAML anchor",
            requested: CAPACITY,
            source,
        })?;
    budget.consume_bytes(usize_to_u64(CAPACITY)?)?;
    write!(&mut anchor, "doc_{ordinal}").map_err(|_| YamlAdapterError::InvalidDocument {
        line: 1,
        column: 1,
        reason: "failed to format the document ordinal",
    })?;
    Ok(anchor)
}

fn push_document_entry(
    entries: &mut Vec<UnityClass>,
    accounted_capacity: &mut usize,
    class: UnityClass,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    budget.check_members(1)?;
    reserve_budgeted_vec(
        entries,
        accounted_capacity,
        1,
        budget,
        "Unity YAML document entries",
    )?;
    budget.consume_members(1)?;
    entries.push(class);
    Ok(())
}

fn reserve_budgeted_object(
    value: &mut UnityValue,
    current_capacity: usize,
    accounted_capacity: &mut usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    let UnityValue::Object(map) = value else {
        return Err(YamlAdapterError::InvalidDocument {
            line: 1,
            column: 1,
            reason: "mapping frame lost its object",
        });
    };
    let required = map
        .len()
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let target = geometric_capacity(current_capacity, required)?;
    if target == current_capacity {
        return Ok(());
    }
    let slots = target - current_capacity;
    let slot_bytes = size_of::<(String, UnityValue)>()
        .checked_add(size_of::<usize>() * 3)
        .and_then(|value| value.checked_add(16))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let bytes = slots
        .checked_mul(slot_bytes)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(usize_to_u64(bytes)?)?;
    map.try_reserve_exact(target - map.len())
        .map_err(|source| YamlAdapterError::IndexMapAllocationFailed {
            context: "Unity YAML mapping",
            requested: bytes,
            source,
        })?;
    budget.consume_bytes(usize_to_u64(bytes)?)?;
    *accounted_capacity = target;
    Ok(())
}

fn reserve_budgeted_vec<T>(
    values: &mut Vec<T>,
    accounted_capacity: &mut usize,
    additional: usize,
    budget: &mut AssetLoadBudget,
    context: &'static str,
) -> Result<(), YamlAdapterError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let target = geometric_capacity(*accounted_capacity, required)?;
    if target == *accounted_capacity {
        return Ok(());
    }
    let slots = target - *accounted_capacity;
    let bytes = slots
        .checked_mul(size_of::<T>().max(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(usize_to_u64(bytes)?)?;
    values
        .try_reserve_exact(target - values.len())
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context,
            requested: bytes,
            source,
        })?;
    budget.consume_bytes(usize_to_u64(bytes)?)?;
    *accounted_capacity = target;
    Ok(())
}

fn geometric_capacity(current: usize, required: usize) -> Result<usize, YamlAdapterError> {
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
    context: &'static str,
) -> Result<String, YamlAdapterError> {
    let bytes = usize_to_u64(value.len())?;
    budget.check_bytes(bytes)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|source| YamlAdapterError::AllocationFailed {
            context,
            requested: value.len(),
            source,
        })?;
    budget.consume_bytes(bytes)?;
    owned.push_str(value);
    Ok(owned)
}

fn charge_retained_string(
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    budget.consume_bytes(usize_to_u64(value.len())?)?;
    Ok(())
}

fn usize_to_u64(value: usize) -> Result<u64, YamlAdapterError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

fn display_column(mark: Marker) -> usize {
    mark.col().saturating_add(1)
}

fn invalid_document(mark: Marker, reason: &'static str) -> YamlAdapterError {
    YamlAdapterError::InvalidDocument {
        line: mark.line(),
        column: display_column(mark),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::FileTimes;
    #[cfg(unix)]
    use std::io::Write as _;
    use unity_asset_core::{AssetLoadLimits, UnityDocument, arc_slice_allocation_bytes};

    fn parse(input: impl AsRef<[u8]>) -> Result<BudgetedYamlSource, BudgetedYamlError> {
        let mut budget = AssetLoadBudget::default();
        parse_budgeted_yaml_source(Arc::from(input.as_ref()), &mut budget)
    }

    fn first_parser_error<T: Iterator<Item = char>>(mut parser: Parser<T>) -> ScanError {
        loop {
            match parser.next_token() {
                Ok((Event::StreamEnd, _)) => panic!("expected malformed YAML to fail"),
                Ok(_) => {}
                Err(error) => return error,
            }
        }
    }

    struct ChangingReader {
        initial: std::io::Cursor<&'static [u8]>,
        changed: std::io::Cursor<&'static [u8]>,
        verifying: bool,
    }

    impl Read for ChangingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.verifying {
                self.changed.read(buffer)
            } else {
                self.initial.read(buffer)
            }
        }
    }

    impl Seek for ChangingReader {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.verifying = true;
            self.changed.seek(position)
        }
    }

    #[cfg(unix)]
    fn replace_file_preserving_observed_metadata(
        path: &Path,
        replacement: &Path,
        replacement_bytes: &[u8],
        before: SourceMetadata,
    ) {
        assert_eq!(
            u64::try_from(replacement_bytes.len()).unwrap(),
            before.length
        );
        let mut replacement_file = File::create(replacement).unwrap();
        replacement_file.write_all(replacement_bytes).unwrap();
        replacement_file
            .set_times(
                FileTimes::new().set_modified(
                    before
                        .modified
                        .expect("Unix file metadata should expose a modification time"),
                ),
            )
            .unwrap();
        drop(replacement_file);
        std::fs::rename(replacement, path).unwrap();

        assert_eq!(
            SourceMetadata::from_metadata(&std::fs::metadata(path).unwrap()),
            before,
            "the replacement must defeat a length-and-mtime-only check"
        );
    }

    #[test]
    fn source_reader_rejects_growth_and_truncation() {
        let path = Path::new("changing.yaml");

        let mut grown = std::io::Cursor::new(b"five!".as_slice());
        let mut grown_budget = AssetLoadBudget::default();
        assert!(matches!(
            read_budgeted_yaml_image(&mut grown, path, 4, &mut grown_budget),
            Err(YamlAdapterError::SourceChanged { .. })
        ));

        let mut truncated = std::io::Cursor::new(b"four".as_slice());
        let mut truncated_budget = AssetLoadBudget::default();
        assert!(matches!(
            read_budgeted_yaml_image(&mut truncated, path, 5, &mut truncated_budget),
            Err(YamlAdapterError::SourceChanged { .. })
        ));
    }

    #[test]
    fn source_reader_rejects_same_length_content_change() {
        let mut reader = ChangingReader {
            initial: std::io::Cursor::new(b"four".as_slice()),
            changed: std::io::Cursor::new(b"five".as_slice()),
            verifying: false,
        };
        let mut budget = AssetLoadBudget::default();

        assert!(matches!(
            read_budgeted_yaml_image(&mut reader, Path::new("changing.yaml"), 4, &mut budget,),
            Err(YamlAdapterError::SourceChanged { .. })
        ));
        assert_eq!(budget.usage().bytes, 8);
    }

    #[cfg(unix)]
    #[test]
    fn synchronous_binding_rejects_same_length_same_mtime_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.yaml");
        let replacement = directory.path().join("replacement.yaml");
        std::fs::write(&path, b"root: old!\n").unwrap();
        let file = File::open(&path).unwrap();
        let before = SourceMetadata::from_metadata(&file.metadata().unwrap());

        replace_file_preserving_observed_metadata(&path, &replacement, b"root: new!\n", before);

        assert!(matches!(
            verify_open_file_binding(file, &path, before),
            Err(YamlAdapterError::SourceChanged { .. })
        ));
    }

    #[cfg(all(unix, feature = "async"))]
    #[tokio::test]
    async fn asynchronous_binding_rejects_same_length_same_mtime_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.yaml");
        let replacement = directory.path().join("replacement.yaml");
        std::fs::write(&path, b"root: old!\n").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let before = SourceMetadata::from_metadata(&file.metadata().await.unwrap());

        replace_file_preserving_observed_metadata(&path, &replacement, b"root: new!\n", before);

        assert!(matches!(
            verify_open_file_binding_async(file, &path, before).await,
            Err(YamlAdapterError::SourceChanged { .. })
        ));
    }

    #[test]
    fn parses_unity_headers_scalars_multiline_values_and_stripped_metadata() {
        let input = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &42
# comments between the header and class are semantic whitespace
GameObject:
  empty:
  signed: -42
  maximum: 18446744073709551615
  oversizedInteger: 22222222222222222222222222222222
  quotedNull: "null"
  literal: |-
    first
    second
  folded: >-
    first
    second
--- !u!114 &9001 stripped
MonoBehaviour:
  enabled: true
"#;

        let parsed = parse(input).unwrap();
        assert_eq!(parsed.encoded().as_ref(), input.as_bytes());
        let entries = parsed.document().entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].class_id(), 1);
        assert_eq!(entries[0].class_name(), "GameObject");
        assert_eq!(entries[0].anchor(), "42");
        assert!(matches!(entries[0].get("empty"), Some(UnityValue::Null)));
        assert!(matches!(
            entries[0].get("signed"),
            Some(UnityValue::Integer(-42))
        ));
        assert!(matches!(
            entries[0].get("maximum"),
            Some(UnityValue::Unsigned(value)) if *value == u64::MAX
        ));
        assert_eq!(
            entries[0]
                .get("oversizedInteger")
                .and_then(UnityValue::as_str),
            Some("22222222222222222222222222222222")
        );
        assert_eq!(
            entries[0].get("quotedNull").and_then(UnityValue::as_str),
            Some("null")
        );
        assert_eq!(
            entries[0].get("literal").and_then(UnityValue::as_str),
            Some("first\nsecond")
        );
        assert_eq!(
            entries[0].get("folded").and_then(UnityValue::as_str),
            Some("first second")
        );
        assert_eq!(entries[1].extra_anchor_data(), "stripped");
        assert_eq!(entries[1].anchor(), "9001");
    }

    #[test]
    fn preserves_zero_prefixed_decimals_without_changing_other_number_forms() {
        let input = r#"root:
  zero: 0
  positiveZero: +0
  negativeZero: -0
  positivePadded: +0012
  negativePadded: -0012
  padded: 0012
  allZeroPadded: 00000000
  float: 0.0
  hexadecimal: 0x10
  octal: 0o10
  hexadecimalPrefix: 0x
  octalPrefix: 0o
  scientific: 0e2
"#;

        let parsed = parse(input).unwrap();
        let root = parsed.document().entries()[0]
            .get("root")
            .and_then(UnityValue::as_object)
            .unwrap();
        assert_eq!(root.get("zero").and_then(UnityValue::as_i64), Some(0));
        assert_eq!(
            root.get("positiveZero").and_then(UnityValue::as_i64),
            Some(0)
        );
        assert_eq!(
            root.get("negativeZero").and_then(UnityValue::as_i64),
            Some(0)
        );
        for (field, expected) in [
            ("positivePadded", "+0012"),
            ("negativePadded", "-0012"),
            ("padded", "0012"),
            ("allZeroPadded", "00000000"),
            ("hexadecimalPrefix", "0x"),
            ("octalPrefix", "0o"),
        ] {
            assert_eq!(root.get(field).and_then(UnityValue::as_str), Some(expected));
        }
        assert!(matches!(root.get("float"), Some(UnityValue::Float(value)) if *value == 0.0));
        assert_eq!(
            root.get("hexadecimal").and_then(UnityValue::as_i64),
            Some(16)
        );
        assert_eq!(root.get("octal").and_then(UnityValue::as_i64), Some(8));
        assert!(matches!(root.get("scientific"), Some(UnityValue::Float(value)) if *value == 0.0));
    }

    #[test]
    fn block_scalar_content_that_looks_like_stream_headers_is_not_prescanned() {
        let input = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &1
MonoBehaviour:
  text: |-
    %TAG !other! tag:example.com,2026:
    --- !u!1 &2
"#;

        let parsed = parse(input).unwrap();
        assert_eq!(
            parsed.document().entries()[0]
                .get("text")
                .and_then(UnityValue::as_str),
            Some("%TAG !other! tag:example.com,2026:\n--- !u!1 &2")
        );
    }

    #[test]
    fn empty_stream_is_allowed_but_empty_or_trailing_documents_are_rejected() {
        assert!(parse("").unwrap().document().entries().is_empty());
        assert!(matches!(
            parse("---\n"),
            Err(YamlAdapterError::InvalidDocument { .. })
        ));
        assert!(matches!(
            parse("---\nroot: value\n---\n"),
            Err(YamlAdapterError::InvalidDocument { .. })
        ));
        assert!(parse("root: value\ntrailing").is_err());
    }

    #[test]
    fn virtual_stripped_header_view_preserves_utf8_parser_locations() {
        let input = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n# Unicode before the header: \u{8d44}\u{6e90}\n--- !u!114 &9001 stripped\nMonoBehaviour:\n  broken: [1,\n";
        let materialized = input.replacen("stripped", "        ", 1);
        assert_eq!(input.len(), materialized.len());

        let mut budget = AssetLoadBudget::default();
        let headers = scan_headers(input, &mut budget).unwrap();
        let virtual_view: String = StrippedHeaderChars::new(input, &headers).unwrap().collect();
        assert_eq!(virtual_view, materialized);

        let virtual_error = first_parser_error(
            Parser::new(StrippedHeaderChars::new(input, &headers).unwrap()).keep_tags(true),
        );
        let materialized_error =
            first_parser_error(Parser::new_from_str(&materialized).keep_tags(true));
        assert_eq!(virtual_error.marker(), materialized_error.marker());
        assert_eq!(virtual_error.info(), materialized_error.info());
    }

    fn assert_dense_parser_budget_boundary(input: &str, minimum_scalar_sites: u64) {
        let lexical_sites = yaml_lexical_allocation_sites(input).unwrap();
        assert!(lexical_sites >= minimum_scalar_sites);
        let envelope = yaml_rust2_allocation_envelope(input).unwrap();
        assert!(envelope >= minimum_scalar_sites * YAML_RUST2_0_11_BYTES_PER_LEXICAL_SITE);
        let mut at_envelope = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: envelope,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        charge_yaml_rust2_allocation_envelope(input, &mut at_envelope).unwrap();
        assert_eq!(at_envelope.usage().bytes, envelope);

        let mut one_short_envelope = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: envelope - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            charge_yaml_rust2_allocation_envelope(input, &mut one_short_envelope),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == envelope - 1 && requested == envelope
        ));
        assert_eq!(one_short_envelope.usage().bytes, 0);

        let source_allocation = arc_slice_allocation_bytes::<u8>(input.len()).unwrap();
        let before_parser = source_allocation.checked_add(envelope).unwrap();
        let mut rejected_before_parser = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: before_parser - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(
                Arc::from(input.as_bytes()),
                &mut rejected_before_parser,
            ),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == before_parser - 1 && requested == before_parser
        ));
        assert_eq!(rejected_before_parser.usage().bytes, source_allocation);

        let encoded: Arc<[u8]> = Arc::from(input.as_bytes());
        let mut probe = AssetLoadBudget::default();
        parse_budgeted_yaml_source(Arc::clone(&encoded), &mut probe).unwrap();
        let reported_boundary = probe.usage().bytes;
        let mut exact_reported_boundary = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: reported_boundary,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        parse_budgeted_yaml_source(Arc::clone(&encoded), &mut exact_reported_boundary).unwrap();
        assert_eq!(exact_reported_boundary.usage().bytes, reported_boundary);

        let mut one_short_reported_boundary = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: reported_boundary - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(encoded, &mut one_short_reported_boundary),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                ..
            })) if limit == reported_boundary - 1
        ));
    }

    #[test]
    fn dense_single_character_sequence_has_a_preconstructed_parser_envelope() {
        let mut input = String::from("root: [");
        for index in 0..4096 {
            if index != 0 {
                input.push(',');
            }
            input.push('a');
        }
        input.push_str("]\n");

        assert_dense_parser_budget_boundary(&input, 4096);
    }

    #[test]
    fn dense_single_character_mappings_have_a_preconstructed_parser_envelope() {
        let mut input = String::from("root: [");
        for index in 0..1024 {
            if index != 0 {
                input.push(',');
            }
            input.push_str("{a: a}");
        }
        input.push_str("]\n");

        assert_dense_parser_budget_boundary(&input, 2048);
    }

    #[test]
    fn large_stripped_yaml_uses_one_backing_and_a_reported_budget_boundary() {
        let payload = "x".repeat(128 * 1024);
        let stripped = format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &9001 stripped\nMonoBehaviour:\n  payload: {payload}\n"
        );
        let plain = stripped.replacen("stripped", "        ", 1);
        assert_eq!(stripped.len(), plain.len());

        let encoded: Arc<[u8]> = Arc::from(stripped.as_bytes());
        let mut stripped_budget = AssetLoadBudget::default();
        let parsed =
            parse_budgeted_yaml_source(Arc::clone(&encoded), &mut stripped_budget).unwrap();
        assert!(Arc::ptr_eq(parsed.encoded(), &encoded));

        let mut plain_budget = AssetLoadBudget::default();
        parse_budgeted_yaml_source(Arc::from(plain.as_bytes()), &mut plain_budget).unwrap();
        let envelope_difference = yaml_rust2_allocation_envelope(&stripped).unwrap()
            - yaml_rust2_allocation_envelope(&plain).unwrap();
        assert_eq!(
            stripped_budget.usage().bytes,
            plain_budget.usage().bytes
                + envelope_difference
                + u64::try_from("stripped".len()).unwrap(),
            "the stripped parser view must not charge a second input-sized backing"
        );

        let required = stripped_budget.usage().bytes;
        let exact_limits = AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        };
        let mut exact_budget = AssetLoadBudget::new(exact_limits).unwrap();
        parse_budgeted_yaml_source(Arc::clone(&encoded), &mut exact_budget).unwrap();
        assert_eq!(exact_budget.usage().bytes, required);

        let one_short_limits = AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        };
        let mut one_short_budget = AssetLoadBudget::new(one_short_limits).unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(encoded, &mut one_short_budget),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn prebudgeted_parser_skips_exactly_one_source_arc_allocation() {
        let encoded: Arc<[u8]> = Arc::from(b"root: value\n".as_slice());
        let source_allocation = arc_slice_allocation_bytes::<u8>(encoded.len()).unwrap();

        let mut unaccounted_budget = AssetLoadBudget::default();
        parse_budgeted_yaml_source(Arc::clone(&encoded), &mut unaccounted_budget).unwrap();

        let mut prebudgeted_budget = AssetLoadBudget::default();
        let source =
            BudgetedSourceBytes::from_arc(Arc::clone(&encoded), &mut prebudgeted_budget).unwrap();
        let before_parser = prebudgeted_budget.usage().bytes;
        let parsed = parse_prebudgeted_yaml_source(source, &mut prebudgeted_budget).unwrap();
        let parser_bytes = prebudgeted_budget.usage().bytes - before_parser;

        assert!(Arc::ptr_eq(parsed.encoded(), &encoded));
        assert_eq!(before_parser, source_allocation);
        assert_eq!(
            unaccounted_budget.usage().bytes - parser_bytes,
            source_allocation
        );
        assert_eq!(unaccounted_budget.usage(), prebudgeted_budget.usage());

        let required = unaccounted_budget.usage().bytes;
        let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let exact_source =
            BudgetedSourceBytes::from_arc(Arc::clone(&encoded), &mut exact_budget).unwrap();
        parse_prebudgeted_yaml_source(exact_source, &mut exact_budget).unwrap();
        assert_eq!(exact_budget.usage().bytes, required);

        let mut one_short_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let one_short_source =
            BudgetedSourceBytes::from_arc(encoded, &mut one_short_budget).unwrap();
        assert!(matches!(
            parse_prebudgeted_yaml_source(one_short_source, &mut one_short_budget),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == required - 1 && requested == required
        ));
    }

    #[test]
    fn prebudgeted_parser_rejects_a_different_budget_domain_without_charge() {
        let mut source_budget = AssetLoadBudget::default();
        let source = BudgetedSourceBytes::from_arc(
            Arc::from(b"root: value\n".as_slice()),
            &mut source_budget,
        )
        .unwrap();
        let mut parser_budget = AssetLoadBudget::default();

        let error = parse_prebudgeted_yaml_source(source, &mut parser_budget).unwrap_err();

        assert!(matches!(
            error,
            YamlAdapterError::Budget(BudgetError::DomainMismatch {
                resource: "source bytes"
            })
        ));
        assert_eq!(parser_budget.usage(), Default::default());
    }

    #[test]
    fn preserves_all_fields_in_untagged_meta_documents() {
        let parsed =
            parse("fileFormatVersion: 2\nguid: abcdef\nPluginImporter:\n  serializedVersion: 3\n")
                .unwrap();
        let entry = &parsed.document().entries()[0];
        assert_eq!(entry.class_id(), 0);
        assert_eq!(entry.class_name(), UNITY_DOCUMENT_CLASS_NAME);
        assert_eq!(entry.anchor(), "doc_0");
        assert_eq!(
            entry.get("fileFormatVersion").and_then(UnityValue::as_i64),
            Some(2)
        );
        assert_eq!(
            entry.get("guid").and_then(UnityValue::as_str),
            Some("abcdef")
        );
        assert!(
            entry
                .get("PluginImporter")
                .and_then(UnityValue::as_object)
                .is_some()
        );
    }

    #[test]
    fn rejects_alias_anchor_tag_merge_duplicate_and_complex_keys_structurally() {
        let cases = [
            (
                "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  self: *1\n",
                "alias",
            ),
            ("root: &nested value\n", "anchor"),
            ("root: !custom value\n", "tag"),
            ("root:\n  <<: {value: 1}\n", "merge"),
            ("root:\n  value: 1\n  value: 2\n", "duplicate"),
            ("? [first, second]\n: value\n", "complex"),
        ];

        for (input, expected) in cases {
            let error = parse(input).unwrap_err();
            let matched = match expected {
                "alias" => matches!(error, YamlAdapterError::AliasUnsupported { .. }),
                "anchor" => matches!(error, YamlAdapterError::UnexpectedAnchor { .. }),
                "tag" => matches!(error, YamlAdapterError::UnexpectedTag { .. }),
                "merge" => matches!(error, YamlAdapterError::MergeKeyUnsupported { .. }),
                "duplicate" => matches!(error, YamlAdapterError::DuplicateKey { .. }),
                "complex" => matches!(error, YamlAdapterError::ComplexKeyUnsupported { .. }),
                _ => false,
            };
            assert!(matched, "expected {expected}, got {error:?}");
        }
    }

    #[test]
    fn rejects_invalid_utf8_and_unity_header_extensions() {
        assert!(matches!(
            parse([0xff]),
            Err(YamlAdapterError::InvalidUtf8 { .. })
        ));
        assert!(matches!(
            parse("--- !u!1 &1 unsupported\nGameObject: {}\n"),
            Err(YamlAdapterError::InvalidHeader { .. })
        ));
        assert!(matches!(
            parse("%TAG !other! tag:example.com,2026:\n---\nvalue: 1\n"),
            Err(YamlAdapterError::UnexpectedTag { .. })
        ));
    }

    #[test]
    fn enforces_hard_depth_and_caller_owned_width_and_byte_budgets() {
        let mut deep = String::from("root: ");
        for _ in 0..60 {
            deep.push('[');
        }
        deep.push('0');
        for _ in 0..60 {
            deep.push(']');
        }
        assert!(matches!(
            parse(deep),
            Err(YamlAdapterError::DepthExceeded {
                actual: 60,
                limit: MAX_YAML_DEPTH
            })
        ));

        let width_limits = AssetLoadLimits {
            max_members: 2,
            ..AssetLoadLimits::default()
        };
        let mut width_budget = AssetLoadBudget::new(width_limits).unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(
                Arc::from("root:\n  first: 1\n  second: 2\n  third: 3\n".as_bytes()),
                &mut width_budget
            ),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "members",
                ..
            }))
        ));

        let byte_limits = AssetLoadLimits {
            max_bytes: 32,
            ..AssetLoadLimits::default()
        };
        let mut byte_budget = AssetLoadBudget::new(byte_limits).unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(Arc::from("root: value\n".as_bytes()), &mut byte_budget),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn depth_budget_is_zero_based_and_composes_with_outer_scopes() {
        let mut root_only = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 0,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        parse_budgeted_yaml_source(Arc::from(b"{}\n".as_slice()), &mut root_only).unwrap();
        assert_eq!(root_only.usage().max_observed_depth, 0);

        let mut rejects_child = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 0,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            parse_budgeted_yaml_source(Arc::from(b"root: value\n".as_slice()), &mut rejects_child),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 0,
                requested: 1,
            }))
        ));

        let mut one_level = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        parse_budgeted_yaml_source(Arc::from(b"root: value\n".as_slice()), &mut one_level).unwrap();
        assert_eq!(one_level.usage().max_observed_depth, 1);

        let mut nested = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 2,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        {
            let mut scoped = nested.enter_depth(1).unwrap();
            parse_budgeted_yaml_source(Arc::from(b"root: value\n".as_slice()), &mut scoped)
                .unwrap();
        }
        assert_eq!(nested.usage().max_observed_depth, 2);

        let mut nested_too_deep = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = {
            let mut scoped = nested_too_deep.enter_depth(1).unwrap();
            parse_budgeted_yaml_source(Arc::from(b"root: value\n".as_slice()), &mut scoped)
                .unwrap_err()
        };
        assert!(matches!(
            error,
            YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 1,
                requested: 2,
            })
        ));
    }
}
