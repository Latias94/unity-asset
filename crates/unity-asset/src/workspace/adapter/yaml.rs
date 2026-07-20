//! Budgeted Unity YAML parsing for workspace-owned source images.

use std::fmt::Write as _;
use std::mem::size_of;
use std::str::{CharIndices, Utf8Error};
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, UnityClass, UnityDocument, UnityValue, YamlAnchor,
    arc_value_allocation_bytes,
};
use yaml_rust2::ScanError;
use yaml_rust2::parser::{Event, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

use crate::YamlDocument;

const MAX_YAML_DEPTH: u32 = 59;
const PARSER_WORK_MULTIPLIER: u64 = 6;
const PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
const UNITY_TAG_PREFIX: &str = "tag:unity3d.com,2011:";
const UNITY_DOCUMENT_CLASS_NAME: &str = "YamlDocument";

/// A parsed YAML document that retains the exact bytes used to fingerprint its source.
#[derive(Debug, Clone)]
pub(crate) struct ParsedYamlSource {
    encoded: Arc<[u8]>,
    document: Arc<YamlDocument>,
}

impl ParsedYamlSource {
    pub(crate) fn encoded(&self) -> &Arc<[u8]> {
        &self.encoded
    }

    pub(crate) fn document(&self) -> &Arc<YamlDocument> {
        &self.document
    }
}

/// Typed failures produced before a YAML source can enter a workspace snapshot.
#[derive(Debug, Error)]
pub(crate) enum YamlAdapterError {
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
    #[error("failed to reserve {requested} bytes for {context}")]
    AllocationFailed {
        context: &'static str,
        requested: usize,
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

/// Parses an owned YAML image without recursive descent or intermediate `serde_yaml::Value`s.
pub(crate) fn parse_yaml_source(
    encoded: Arc<[u8]>,
    budget: &mut AssetLoadBudget,
) -> Result<ParsedYamlSource, YamlAdapterError> {
    charge_parser_preflight(encoded.len(), budget)?;
    let input = std::str::from_utf8(&encoded).map_err(|source| YamlAdapterError::InvalidUtf8 {
        valid_up_to: source.valid_up_to(),
        source,
    })?;

    let headers = scan_headers(input, budget)?;
    let parser_input = StrippedHeaderChars::new(input, &headers)?;
    // Unity declares `!u!` once at the beginning of a multi-document file and reuses it for every
    // following document, despite standard YAML limiting tag directives to one document.
    let mut parser = Parser::new(parser_input).keep_tags(true);
    let mut document = YamlDocument::new();
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
                push_document_entry(&mut document, &mut document_capacity, class, budget)?;
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
    Ok(ParsedYamlSource {
        encoded,
        document: Arc::new(document),
    })
}

fn current_builder<'a, 'input>(
    current: &'a mut Option<DocumentBuilder<'input>>,
    mark: Marker,
) -> Result<&'a mut DocumentBuilder<'input>, YamlAdapterError> {
    current
        .as_mut()
        .ok_or_else(|| invalid_document(mark, "node event outside a document"))
}

fn charge_parser_preflight(
    encoded_len: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    let encoded = usize_to_u64(encoded_len)?;
    let parser_work = encoded
        .checked_mul(PARSER_WORK_MULTIPLIER)
        .and_then(|value| value.checked_add(PARSER_FIXED_WORK_BYTES))
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let total = encoded
        .checked_add(parser_work)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    budget.check_bytes(total)?;
    budget.consume_bytes(encoded)?;
    budget.consume_bytes(parser_work)?;
    Ok(())
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
    let trimmed = line.trim_start_matches([' ', '\t']);
    if !trimmed.starts_with("%TAG") {
        return Ok(());
    }
    let mut tokens = HeaderTokens::new(trimmed);
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
            budget.consume_entries(1)?;
            charge_retained_string(&value, budget)?;
            return self.accept_mapping_key(value, mark);
        }

        let is_root = self.frames.is_empty() && self.root.is_none();
        validate_node_metadata(self.header, is_root, anchor, tag, mark)?;
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
        budget.consume_entries(1)?;
        let depth = u32::try_from(self.frames.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(YamlAdapterError::DepthExceeded {
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
    let mut class = UnityClass::with_properties(header.class_id, class_name, anchor, properties);
    if header.stripped_range.is_some() {
        class.extra_anchor_data =
            clone_string_budgeted("stripped", budget, "Unity YAML anchor metadata")?;
    }
    Ok(class)
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
        .map_err(|_| YamlAdapterError::AllocationFailed {
            context: "untagged Unity YAML anchor",
            requested: CAPACITY,
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
    document: &mut YamlDocument,
    accounted_capacity: &mut usize,
    class: UnityClass,
    budget: &mut AssetLoadBudget,
) -> Result<(), YamlAdapterError> {
    budget.check_members(1)?;
    reserve_budgeted_vec(
        document.entries_mut(),
        accounted_capacity,
        1,
        budget,
        "Unity YAML document entries",
    )?;
    budget.consume_members(1)?;
    document.add_entry(class);
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
        .map_err(|_| YamlAdapterError::AllocationFailed {
            context: "Unity YAML mapping",
            requested: bytes,
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
        .map_err(|_| YamlAdapterError::AllocationFailed {
            context,
            requested: bytes,
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
        .map_err(|_| YamlAdapterError::AllocationFailed {
            context,
            requested: value.len(),
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
    use unity_asset_core::{AssetLoadLimits, UnityDocument};

    fn parse(input: impl AsRef<[u8]>) -> Result<ParsedYamlSource, YamlAdapterError> {
        let mut budget = AssetLoadBudget::default();
        parse_yaml_source(Arc::from(input.as_ref()), &mut budget)
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
        assert_eq!(entries[0].class_id, 1);
        assert_eq!(entries[0].class_name, "GameObject");
        assert_eq!(entries[0].anchor, "42");
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
        assert_eq!(entries[1].extra_anchor_data, "stripped");
        assert_eq!(entries[1].anchor, "9001");
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

    #[test]
    fn large_stripped_yaml_uses_one_backing_and_an_exact_budget() {
        let payload = "x".repeat(128 * 1024);
        let stripped = format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &9001 stripped\nMonoBehaviour:\n  payload: {payload}\n"
        );
        let plain = stripped.replacen("stripped", "        ", 1);
        assert_eq!(stripped.len(), plain.len());

        let encoded: Arc<[u8]> = Arc::from(stripped.as_bytes());
        let mut stripped_budget = AssetLoadBudget::default();
        let parsed = parse_yaml_source(Arc::clone(&encoded), &mut stripped_budget).unwrap();
        assert!(Arc::ptr_eq(parsed.encoded(), &encoded));

        let mut plain_budget = AssetLoadBudget::default();
        parse_yaml_source(Arc::from(plain.as_bytes()), &mut plain_budget).unwrap();
        assert_eq!(
            stripped_budget.usage().bytes,
            plain_budget.usage().bytes + u64::try_from("stripped".len()).unwrap(),
            "the stripped parser view must not charge a second input-sized backing"
        );

        let required = stripped_budget.usage().bytes;
        let exact_limits = AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        };
        let mut exact_budget = AssetLoadBudget::new(exact_limits).unwrap();
        parse_yaml_source(Arc::clone(&encoded), &mut exact_budget).unwrap();
        assert_eq!(exact_budget.usage().bytes, required);

        let one_short_limits = AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        };
        let mut one_short_budget = AssetLoadBudget::new(one_short_limits).unwrap();
        assert!(matches!(
            parse_yaml_source(encoded, &mut one_short_budget),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn preserves_all_fields_in_untagged_meta_documents() {
        let parsed =
            parse("fileFormatVersion: 2\nguid: abcdef\nPluginImporter:\n  serializedVersion: 3\n")
                .unwrap();
        let entry = &parsed.document().entries()[0];
        assert_eq!(entry.class_id, 0);
        assert_eq!(entry.class_name, UNITY_DOCUMENT_CLASS_NAME);
        assert_eq!(entry.anchor, "doc_0");
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
        for _ in 0..59 {
            deep.push('[');
        }
        deep.push('0');
        for _ in 0..59 {
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
            parse_yaml_source(
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
            parse_yaml_source(Arc::from("root: value\n".as_bytes()), &mut byte_budget),
            Err(YamlAdapterError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }
}
