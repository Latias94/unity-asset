use std::collections::TryReserveError;
use std::fmt;
use std::io::{self, Read, Write};
use std::mem::size_of;

#[cfg(test)]
use serde::Deserialize;
use serde::de::{DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::{Marker, Scanner, TScalarStyle, TokenType};

#[cfg(test)]
use super::MutationPlanWire;
use super::{MAX_PLAN_WIRE_DEPTH, MutationPlan};

// The 186-level wire ceiling needs materially more fixed stack/scratch allowance than the generic
// 64-level JSON contract reader. One KiB per level plus the read buffer and diagnostics fits below
// this power-of-two reservation.
const PARSER_FIXED_WORK_BYTES: u64 = 256 * 1024;
// The encoded byte itself is charged separately. Six additional bytes cover power-of-two input
// capacity slack plus YAML scanning, the structure-only JSON pass, and typed JSON parser work.
const PARSER_WORK_BYTES_PER_INPUT_BYTE: u64 = 6;
// Each JSON value pays for the largest retained wire layout plus Serde's internally-tagged enum
// representation and collection capacity slack.
const WIRE_LAYOUT_BYTES_PER_ENTRY: u64 = 512;
// `from_wire` can temporarily own both operation/action buffers; validation can additionally own
// target indexes, and Vec-to-boxed-slice conversion may briefly retain old and new allocations.
const FROM_WIRE_TRANSITION_BYTES_PER_ENTRY: u64 = 1024;
// Covers root layouts and fixed Serde/validation state not proportional to entries or text.
const MATERIALIZATION_FIXED_BYTES: u64 = 64 * 1024;

impl MutationPlan {
    /// Independent hard limit for encoded JSON or YAML Mutation Plan input.
    ///
    /// 128 MiB admits the existing 64 MiB semantic string contract and large hexadecimal
    /// [`super::PlanBytes`] payloads while bounding retained input and both parser passes even when
    /// a caller supplies a larger [`AssetLoadBudget`].
    pub const MAX_ENCODED_INPUT_BYTES: usize = 128 * 1024 * 1024;

    /// Independent hard limit for JSON normalized from YAML before typed materialization.
    ///
    /// YAML escaping can expand during normalization, so this is enforced independently from
    /// [`Self::MAX_ENCODED_INPUT_BYTES`].
    pub const MAX_NORMALIZED_JSON_BYTES: usize = 128 * 1024 * 1024;

    /// Reads an untrusted JSON plan with caller-owned allocation and structure budgets.
    ///
    /// Encoded input is capped by [`Self::MAX_ENCODED_INPUT_BYTES`] independently of the caller
    /// budget. Parser scratch, wire structure, strings, hexadecimal byte decoding, and `from_wire`
    /// transition storage are conservatively charged before typed deserialization starts.
    ///
    /// Only the current wire version is accepted; older plans must be regenerated from a current
    /// workspace snapshot.
    pub fn from_json_reader(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, MutationPlanReadError> {
        let encoded = read_contract_bytes(reader, budget)?;
        let structure = probe_json(&encoded, budget)?;
        let wire = deserialize_after_materialization_reservation(&encoded, structure, budget)?;
        Ok(Self::from_wire(wire)?)
    }

    /// Reads an untrusted JSON plan from memory while consuming the supplied budget.
    pub fn from_json_slice(
        bytes: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, MutationPlanReadError> {
        Self::from_json_reader(bytes, budget)
    }

    /// Reads a strict YAML representation of the JSON plan data model.
    ///
    /// YAML is an input convenience only. Aliases, anchors, tags, complex keys, duplicate keys,
    /// and multiple documents are rejected; persisted identity always uses canonical JSON. The
    /// encoded document and normalized JSON are independently capped by
    /// [`Self::MAX_ENCODED_INPUT_BYTES`] and [`Self::MAX_NORMALIZED_JSON_BYTES`]. Only the current
    /// wire version is accepted.
    pub fn from_yaml_reader(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, MutationPlanReadError> {
        let encoded = read_contract_bytes(reader, budget)?;
        let text =
            std::str::from_utf8(&encoded).map_err(|error| MutationPlanReadError::InvalidUtf8 {
                valid_up_to: error.valid_up_to(),
            })?;
        let node = parse_yaml_node(text, budget)?;
        let json = serialize_yaml_node(node, budget)?;
        let structure = probe_normalized_json(&json, budget)?;
        let wire = deserialize_after_materialization_reservation(&json, structure, budget)?;
        Ok(Self::from_wire(wire)?)
    }

    /// Reads a strict YAML plan from memory while consuming the supplied budget.
    pub fn from_yaml_slice(
        bytes: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, MutationPlanReadError> {
        Self::from_yaml_reader(bytes, budget)
    }
}

fn read_contract_bytes(
    reader: impl Read,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, MutationPlanReadError> {
    read_contract_bytes_with_limit(reader, budget, MutationPlan::MAX_ENCODED_INPUT_BYTES)
}

fn read_contract_bytes_with_limit(
    mut reader: impl Read,
    budget: &mut AssetLoadBudget,
    maximum: usize,
) -> Result<Vec<u8>, MutationPlanReadError> {
    budget.consume_bytes(PARSER_FIXED_WORK_BYTES)?;
    let mut encoded = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let remaining = maximum.saturating_sub(encoded.len());
        let read_limit = chunk.len().min(remaining.saturating_add(1));
        let read = reader.read(&mut chunk[..read_limit])?;
        if read == 0 {
            break;
        }
        let requested =
            encoded
                .len()
                .checked_add(read)
                .ok_or(MutationPlanReadError::CapacityOverflow {
                    resource: "mutation plan encoded input",
                })?;
        if requested > maximum {
            return Err(MutationPlanReadError::EncodedInputLimitExceeded {
                limit: maximum,
                requested,
            });
        }
        let amount = u64::try_from(read).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_bytes",
        })?;
        let parser_work = amount.checked_mul(PARSER_WORK_BYTES_PER_INPUT_BYTE).ok_or(
            BudgetError::ArithmeticOverflow {
                resource: "mutation_plan_bytes",
            },
        )?;
        let total = amount
            .checked_add(parser_work)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "mutation_plan_bytes",
            })?;
        budget.check_bytes(total)?;
        ensure_capacity(&mut encoded, read, maximum, "mutation plan input")?;
        budget.consume_bytes(total)?;
        encoded.extend_from_slice(&chunk[..read]);
    }
    Ok(encoded)
}

fn ensure_capacity<T>(
    values: &mut Vec<T>,
    additional: usize,
    maximum: usize,
    resource: &'static str,
) -> Result<(), MutationPlanReadError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(MutationPlanReadError::CapacityOverflow { resource })?;
    if required <= values.capacity() {
        return Ok(());
    }
    let target = required
        .checked_next_power_of_two()
        .unwrap_or(maximum)
        .min(maximum);
    let additional_capacity = target
        .checked_sub(values.len())
        .ok_or(MutationPlanReadError::CapacityOverflow { resource })?;
    values
        .try_reserve_exact(additional_capacity)
        .map_err(|error| MutationPlanReadError::AllocationFailed {
            resource,
            requested: target,
            error,
        })?;
    debug_assert!(values.capacity() >= required);
    Ok(())
}

fn probe_json(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<JsonStructure, MutationPlanReadError> {
    probe_json_structure(encoded, budget, true)
}

fn probe_normalized_json(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
) -> Result<JsonStructure, MutationPlanReadError> {
    // YAML parsing already charged every scalar, container, and collection member.
    probe_json_structure(encoded, budget, false)
}

fn probe_json_structure(
    encoded: &[u8],
    budget: &mut AssetLoadBudget,
    charge_structure_budget: bool,
) -> Result<JsonStructure, MutationPlanReadError> {
    let encoded_bytes =
        u64::try_from(encoded.len()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })?;
    if charge_structure_budget {
        budget.consume_entries(1)?;
    }
    let mut state = JsonProbeState {
        budget,
        failure: None,
        charge_structure_budget,
        structure: JsonStructure {
            encoded_bytes,
            entries: 1,
            string_bytes: 0,
        },
    };
    let mut deserializer = serde_json::Deserializer::from_slice(encoded);
    deserializer.disable_recursion_limit();
    let result = JsonProbeSeed {
        state: &mut state,
        depth: 0,
        charge_entry: false,
    }
    .deserialize(&mut deserializer);
    if let Some(failure) = state.failure {
        return Err(failure.into_read_error());
    }
    result?;
    deserializer.end()?;
    Ok(state.structure)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonStructure {
    encoded_bytes: u64,
    entries: u64,
    string_bytes: u64,
}

fn materialization_bytes<T>(structure: JsonStructure) -> Result<u64, BudgetError> {
    let root_layout =
        u64::try_from(size_of::<T>()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })?;
    let wire_layout = structure
        .entries
        .checked_mul(WIRE_LAYOUT_BYTES_PER_ENTRY)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })?;
    let transition_layout = structure
        .entries
        .checked_mul(FROM_WIRE_TRANSITION_BYTES_PER_ENTRY)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })?;

    // Two encoded-size copies cover owned strings, enum-content scratch, mapping keys, capacity
    // slack, and the parser pass over normalized YAML. Treating every string as hexadecimal
    // additionally bounds all PlanBytes decoding without having to start typed deserialization to
    // discover its variant.
    let textual_storage =
        structure
            .encoded_bytes
            .checked_mul(2)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "mutation_plan_materialization",
            })?;
    let decoded_plan_bytes = structure
        .string_bytes
        .checked_div(2)
        .and_then(|bytes| bytes.checked_add(structure.string_bytes % 2))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })?;

    MATERIALIZATION_FIXED_BYTES
        .checked_add(root_layout)
        .and_then(|bytes| bytes.checked_add(wire_layout))
        .and_then(|bytes| bytes.checked_add(transition_layout))
        .and_then(|bytes| bytes.checked_add(textual_storage))
        .and_then(|bytes| bytes.checked_add(decoded_plan_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_materialization",
        })
}

fn charge_materialization<T>(
    structure: JsonStructure,
    budget: &mut AssetLoadBudget,
) -> Result<(), MutationPlanReadError> {
    let bytes = materialization_bytes::<T>(structure)?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn deserialize_after_materialization_reservation<T: DeserializeOwned>(
    encoded: &[u8],
    structure: JsonStructure,
    budget: &mut AssetLoadBudget,
) -> Result<T, MutationPlanReadError> {
    charge_materialization::<T>(structure, budget)?;
    let mut deserializer = serde_json::Deserializer::from_slice(encoded);
    deserializer.disable_recursion_limit();
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct JsonProbeState<'budget> {
    budget: &'budget mut AssetLoadBudget,
    failure: Option<JsonProbeFailure>,
    charge_structure_budget: bool,
    structure: JsonStructure,
}

impl JsonProbeState<'_> {
    fn charge_value(&mut self) -> Result<(), BudgetError> {
        let entries =
            self.structure
                .entries
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "mutation_plan_entries",
                })?;
        if self.charge_structure_budget {
            self.budget.check_members(1)?;
            self.budget.check_entries(1)?;
            self.budget.consume_members(1)?;
            self.budget.consume_entries(1)?;
        }
        self.structure.entries = entries;
        Ok(())
    }

    fn observe_string(&mut self, length: usize) -> Result<(), BudgetError> {
        let length = u64::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "mutation_plan_string_bytes",
        })?;
        self.structure.string_bytes = self.structure.string_bytes.checked_add(length).ok_or(
            BudgetError::ArithmeticOverflow {
                resource: "mutation_plan_string_bytes",
            },
        )?;
        Ok(())
    }
}

enum JsonProbeFailure {
    Budget(BudgetError),
    Depth { actual: u32 },
}

impl JsonProbeFailure {
    fn into_read_error(self) -> MutationPlanReadError {
        match self {
            Self::Budget(error) => MutationPlanReadError::Budget(error),
            Self::Depth { actual } => MutationPlanReadError::NestingDepthExceeded {
                format: "JSON",
                maximum: MAX_PLAN_WIRE_DEPTH,
                actual,
            },
        }
    }
}

struct JsonProbeSeed<'state, 'budget> {
    state: &'state mut JsonProbeState<'budget>,
    depth: u32,
    charge_entry: bool,
}

impl<'de> DeserializeSeed<'de> for JsonProbeSeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.charge_entry
            && let Err(error) = self.state.charge_value()
        {
            self.state.failure = Some(JsonProbeFailure::Budget(error));
            return Err(serde::de::Error::custom(
                "mutation plan JSON structure budget exceeded",
            ));
        }
        deserializer.deserialize_any(JsonProbeVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}

struct JsonProbeVisitor<'state, 'budget> {
    state: &'state mut JsonProbeState<'budget>,
    depth: u32,
}

impl<'de> Visitor<'de> for JsonProbeVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if let Err(error) = self.state.observe_string(value.len()) {
            self.state.failure = Some(JsonProbeFailure::Budget(error));
            return Err(E::custom("mutation plan JSON string accounting overflow"));
        }
        Ok(())
    }

    fn visit_seq<A>(mut self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let child_depth = self.enter_container::<A::Error>()?;
        while sequence
            .next_element_seed(JsonProbeSeed {
                state: &mut *self.state,
                depth: child_depth,
                charge_entry: true,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(mut self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let child_depth = self.enter_container::<A::Error>()?;
        while mapping.next_key::<IgnoredAny>()?.is_some() {
            mapping.next_value_seed(JsonProbeSeed {
                state: &mut *self.state,
                depth: child_depth,
                charge_entry: true,
            })?;
        }
        Ok(())
    }
}

impl JsonProbeVisitor<'_, '_> {
    fn enter_container<E>(&mut self) -> Result<u32, E>
    where
        E: serde::de::Error,
    {
        let actual = self.depth.saturating_add(1);
        if actual > MAX_PLAN_WIRE_DEPTH {
            self.state.failure = Some(JsonProbeFailure::Depth { actual });
            return Err(E::custom("mutation plan JSON nesting limit exceeded"));
        }
        if let Err(error) = self.state.budget.observe_depth(actual) {
            self.state.failure = Some(JsonProbeFailure::Budget(error));
            return Err(E::custom("mutation plan JSON depth budget exceeded"));
        }
        Ok(actual)
    }
}

#[derive(Debug)]
enum YamlNode {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
    Sequence(Vec<YamlNode>),
    Mapping(Vec<(String, YamlNode)>),
}

impl Serialize for YamlNode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Signed(value) => serializer.serialize_i64(*value),
            Self::Unsigned(value) => serializer.serialize_u64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Sequence(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Mapping(entries) => {
                let mut mapping = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    mapping.serialize_entry(key, value)?;
                }
                mapping.end()
            }
        }
    }
}

enum YamlFrame {
    Sequence(Vec<YamlNode>),
    Mapping {
        entries: Vec<(String, YamlNode)>,
        pending_key: Option<String>,
    },
}

fn parse_yaml_node(
    input: &str,
    budget: &mut AssetLoadBudget,
) -> Result<YamlNode, MutationPlanReadError> {
    reject_yaml_directives(input)?;
    let mut parser = Parser::new_from_str(input);
    let mut frames = Vec::new();
    let mut root = None;
    let mut documents = 0_u32;
    let mut document_open = false;

    loop {
        let (event, marker) = parser.next_token().map_err(|error| {
            if error.info().contains("unknown anchor") {
                MutationPlanReadError::YamlAliasUnsupported {
                    line: error.marker().line(),
                    column: error.marker().col() + 1,
                }
            } else {
                MutationPlanReadError::Yaml(error)
            }
        })?;
        match event {
            Event::Nothing => {
                return Err(yaml_structure(&marker, "unexpected empty parser event"));
            }
            Event::StreamStart => {}
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents =
                    documents
                        .checked_add(1)
                        .ok_or(MutationPlanReadError::CapacityOverflow {
                            resource: "YAML documents",
                        })?;
                if documents != 1 {
                    return Err(MutationPlanReadError::YamlMultipleDocuments {
                        line: marker.line(),
                        column: marker.col() + 1,
                    });
                }
                document_open = true;
            }
            Event::DocumentEnd => {
                if !frames.is_empty() {
                    return Err(yaml_structure(
                        &marker,
                        "document ended inside a collection",
                    ));
                }
                document_open = false;
            }
            Event::Alias(_) => {
                return Err(MutationPlanReadError::YamlAliasUnsupported {
                    line: marker.line(),
                    column: marker.col() + 1,
                });
            }
            Event::Scalar(value, style, anchor, tag) => {
                reject_yaml_metadata(anchor, tag.is_some(), &marker)?;
                budget.consume_entries(1)?;
                let node = parse_yaml_scalar(value, style, &marker)?;
                attach_yaml_node(node, &marker, &mut frames, &mut root, budget)?;
            }
            Event::SequenceStart(anchor, tag) => {
                reject_yaml_metadata(anchor, tag.is_some(), &marker)?;
                reject_complex_key(&frames, &marker)?;
                charge_yaml_container(frames.len(), budget)?;
                try_push_budgeted(
                    &mut frames,
                    YamlFrame::Sequence(Vec::new()),
                    budget,
                    "YAML collection stack",
                )?;
            }
            Event::SequenceEnd => {
                let Some(YamlFrame::Sequence(values)) = frames.pop() else {
                    return Err(yaml_structure(&marker, "unexpected YAML sequence end"));
                };
                attach_yaml_node(
                    YamlNode::Sequence(values),
                    &marker,
                    &mut frames,
                    &mut root,
                    budget,
                )?;
            }
            Event::MappingStart(anchor, tag) => {
                reject_yaml_metadata(anchor, tag.is_some(), &marker)?;
                reject_complex_key(&frames, &marker)?;
                charge_yaml_container(frames.len(), budget)?;
                try_push_budgeted(
                    &mut frames,
                    YamlFrame::Mapping {
                        entries: Vec::new(),
                        pending_key: None,
                    },
                    budget,
                    "YAML collection stack",
                )?;
            }
            Event::MappingEnd => {
                let Some(YamlFrame::Mapping {
                    mut entries,
                    pending_key,
                }) = frames.pop()
                else {
                    return Err(yaml_structure(&marker, "unexpected YAML mapping end"));
                };
                if pending_key.is_some() {
                    return Err(yaml_structure(&marker, "YAML mapping key has no value"));
                }
                // Unique keys make this deterministic; unstable sort avoids unbudgeted scratch.
                entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
                if let Some(index) = entries.windows(2).position(|pair| pair[0].0 == pair[1].0) {
                    let key = entries.remove(index).0;
                    return Err(MutationPlanReadError::YamlDuplicateKey {
                        key,
                        line: marker.line(),
                        column: marker.col() + 1,
                    });
                }
                attach_yaml_node(
                    YamlNode::Mapping(entries),
                    &marker,
                    &mut frames,
                    &mut root,
                    budget,
                )?;
            }
        }
    }

    if document_open || !frames.is_empty() {
        return Err(MutationPlanReadError::YamlIncompleteDocument);
    }
    if documents != 1 {
        return Err(MutationPlanReadError::YamlDocumentRequired);
    }
    root.ok_or(MutationPlanReadError::YamlDocumentRequired)
}

fn reject_yaml_directives(input: &str) -> Result<(), MutationPlanReadError> {
    let mut scanner = Scanner::new(input.chars());
    while let Some(token) = scanner.next_token()? {
        let directive = match token.1 {
            TokenType::StreamStart(_) => continue,
            TokenType::VersionDirective(..) => "version",
            TokenType::TagDirective(..) => "tag",
            TokenType::DocumentEnd => {
                return Err(yaml_structure(
                    &token.0,
                    "YAML document end precedes its document",
                ));
            }
            _ => return Ok(()),
        };
        return Err(MutationPlanReadError::YamlDirectiveUnsupported {
            directive,
            line: token.0.line(),
            column: token.0.col() + 1,
        });
    }
    Ok(())
}

fn charge_yaml_container(
    parent_depth: usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), MutationPlanReadError> {
    let actual = u32::try_from(parent_depth)
        .ok()
        .and_then(|depth| depth.checked_add(1))
        .ok_or(MutationPlanReadError::CapacityOverflow {
            resource: "YAML nesting depth",
        })?;
    if actual > MAX_PLAN_WIRE_DEPTH {
        return Err(MutationPlanReadError::NestingDepthExceeded {
            format: "YAML",
            maximum: MAX_PLAN_WIRE_DEPTH,
            actual,
        });
    }
    budget.observe_depth(actual)?;
    budget.consume_entries(1)?;
    Ok(())
}

fn reject_yaml_metadata(
    anchor: usize,
    has_tag: bool,
    marker: &Marker,
) -> Result<(), MutationPlanReadError> {
    if anchor != 0 {
        return Err(MutationPlanReadError::YamlAnchorUnsupported {
            line: marker.line(),
            column: marker.col() + 1,
        });
    }
    if has_tag {
        return Err(MutationPlanReadError::YamlTagUnsupported {
            line: marker.line(),
            column: marker.col() + 1,
        });
    }
    Ok(())
}

fn reject_complex_key(frames: &[YamlFrame], marker: &Marker) -> Result<(), MutationPlanReadError> {
    if matches!(
        frames.last(),
        Some(YamlFrame::Mapping {
            pending_key: None,
            ..
        })
    ) {
        return Err(MutationPlanReadError::YamlComplexKeyUnsupported {
            line: marker.line(),
            column: marker.col() + 1,
        });
    }
    Ok(())
}

fn parse_yaml_scalar(
    value: String,
    style: TScalarStyle,
    marker: &Marker,
) -> Result<YamlNode, MutationPlanReadError> {
    if style != TScalarStyle::Plain {
        return Ok(YamlNode::String(value));
    }
    match value.as_str() {
        "" | "~" | "null" | "Null" | "NULL" => Ok(YamlNode::Null),
        "true" | "True" | "TRUE" => Ok(YamlNode::Bool(true)),
        "false" | "False" | "FALSE" => Ok(YamlNode::Bool(false)),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => Err(MutationPlanReadError::YamlNonFiniteNumber {
            line: marker.line(),
            column: marker.col() + 1,
        }),
        _ => {
            if let Ok(signed) = value.parse::<i64>() {
                return Ok(YamlNode::Signed(signed));
            }
            if !value.starts_with('-')
                && let Ok(unsigned) = value.parse::<u64>()
            {
                return Ok(YamlNode::Unsigned(unsigned));
            }
            if let Ok(float) = value.parse::<f64>() {
                if !float.is_finite() {
                    return Err(MutationPlanReadError::YamlNonFiniteNumber {
                        line: marker.line(),
                        column: marker.col() + 1,
                    });
                }
                return Ok(YamlNode::Float(float));
            }
            Ok(YamlNode::String(value))
        }
    }
}

fn attach_yaml_node(
    node: YamlNode,
    marker: &Marker,
    frames: &mut [YamlFrame],
    root: &mut Option<YamlNode>,
    budget: &mut AssetLoadBudget,
) -> Result<(), MutationPlanReadError> {
    let Some(frame) = frames.last_mut() else {
        if root.replace(node).is_some() {
            return Err(yaml_structure(
                marker,
                "document contains more than one root value",
            ));
        }
        return Ok(());
    };

    match frame {
        YamlFrame::Sequence(values) => {
            budget.check_members(1)?;
            try_push_budgeted(values, node, budget, "YAML sequence")?;
            budget.consume_members(1)?;
        }
        YamlFrame::Mapping {
            entries,
            pending_key,
        } => {
            if pending_key.is_none() {
                let YamlNode::String(key) = node else {
                    return Err(MutationPlanReadError::YamlNonStringKey {
                        line: marker.line(),
                        column: marker.col() + 1,
                    });
                };
                *pending_key = Some(key);
            } else {
                budget.check_members(1)?;
                let Some(key) = pending_key.take() else {
                    return Err(yaml_structure(
                        marker,
                        "YAML mapping value is missing its key",
                    ));
                };
                try_push_budgeted(entries, (key, node), budget, "YAML mapping")?;
                budget.consume_members(1)?;
            }
        }
    }
    Ok(())
}

fn try_push_budgeted<T>(
    values: &mut Vec<T>,
    value: T,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), MutationPlanReadError> {
    if values.len() == values.capacity() {
        let target = values
            .len()
            .checked_add(1)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(MutationPlanReadError::CapacityOverflow { resource })?;
        let additional = target.saturating_sub(values.capacity());
        let allocation_bytes = additional
            .checked_mul(size_of::<T>())
            .ok_or(MutationPlanReadError::CapacityOverflow { resource })?;
        let allocation_bytes = u64::try_from(allocation_bytes)
            .map_err(|_| MutationPlanReadError::CapacityOverflow { resource })?;
        budget.check_bytes(allocation_bytes)?;
        values.try_reserve_exact(additional).map_err(|error| {
            MutationPlanReadError::AllocationFailed {
                resource,
                requested: target,
                error,
            }
        })?;
        budget.consume_bytes(allocation_bytes)?;
    }
    values.push(value);
    Ok(())
}

fn serialize_yaml_node(
    node: YamlNode,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, MutationPlanReadError> {
    serialize_yaml_node_with_limit(node, budget, MutationPlan::MAX_NORMALIZED_JSON_BYTES)
}

fn serialize_yaml_node_with_limit(
    node: YamlNode,
    budget: &mut AssetLoadBudget,
    maximum: usize,
) -> Result<Vec<u8>, MutationPlanReadError> {
    let mut output = BudgetedOutput {
        bytes: Vec::new(),
        budget,
        failure: None,
        maximum,
    };
    let result = serde_json::to_writer(&mut output, &node);
    if let Some(failure) = output.failure.take() {
        return Err(failure.into_read_error());
    }
    result?;
    Ok(output.bytes)
}

enum OutputFailure {
    Budget(BudgetError),
    Allocation {
        requested: usize,
        error: TryReserveError,
    },
    Capacity,
    Limit {
        limit: usize,
        requested: usize,
    },
}

impl OutputFailure {
    fn into_read_error(self) -> MutationPlanReadError {
        match self {
            Self::Budget(error) => MutationPlanReadError::Budget(error),
            Self::Allocation { requested, error } => MutationPlanReadError::AllocationFailed {
                resource: "YAML normalized JSON",
                requested,
                error,
            },
            Self::Capacity => MutationPlanReadError::CapacityOverflow {
                resource: "YAML normalized JSON",
            },
            Self::Limit { limit, requested } => {
                MutationPlanReadError::NormalizedJsonLimitExceeded { limit, requested }
            }
        }
    }
}

struct BudgetedOutput<'budget> {
    bytes: Vec<u8>,
    budget: &'budget mut AssetLoadBudget,
    failure: Option<OutputFailure>,
    maximum: usize,
}

impl Write for BudgetedOutput<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() {
            return Err(io::Error::other("budgeted YAML output already failed"));
        }
        let Some(required) = self.bytes.len().checked_add(buffer.len()) else {
            self.failure = Some(OutputFailure::Capacity);
            return Err(io::Error::other("YAML normalized JSON capacity overflow"));
        };
        if required > self.maximum {
            self.failure = Some(OutputFailure::Limit {
                limit: self.maximum,
                requested: required,
            });
            return Err(io::Error::other("YAML normalized JSON hard limit exceeded"));
        }
        let target = if required > self.bytes.capacity() {
            required
                .checked_next_power_of_two()
                .unwrap_or(self.maximum)
                .min(self.maximum)
        } else {
            self.bytes.capacity()
        };
        let capacity_growth = target.saturating_sub(self.bytes.capacity());
        let reserve_additional = target.saturating_sub(self.bytes.len());
        let Some(total_charge) = capacity_growth.checked_add(buffer.len()) else {
            self.failure = Some(OutputFailure::Capacity);
            return Err(io::Error::other("YAML normalized JSON budget overflow"));
        };
        let Ok(total_charge) = u64::try_from(total_charge) else {
            self.failure = Some(OutputFailure::Capacity);
            return Err(io::Error::other("YAML normalized JSON budget overflow"));
        };
        if let Err(error) = self.budget.check_bytes(total_charge) {
            self.failure = Some(OutputFailure::Budget(error));
            return Err(io::Error::other(
                "YAML normalized JSON byte budget exceeded",
            ));
        }
        if capacity_growth != 0
            && let Err(error) = self.bytes.try_reserve_exact(reserve_additional)
        {
            self.failure = Some(OutputFailure::Allocation {
                requested: target,
                error,
            });
            return Err(io::Error::other(
                "failed to reserve YAML normalized JSON output",
            ));
        }
        debug_assert!(self.bytes.capacity() >= required);
        if let Err(error) = self.budget.consume_bytes(total_charge) {
            self.failure = Some(OutputFailure::Budget(error));
            return Err(io::Error::other(
                "YAML normalized JSON byte budget exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn yaml_structure(marker: &Marker, message: &'static str) -> MutationPlanReadError {
    MutationPlanReadError::YamlStructure {
        line: marker.line(),
        column: marker.col() + 1,
        message,
    }
}

/// Failure while reading an untrusted serialized Mutation Plan.
#[derive(Debug, Error)]
pub enum MutationPlanReadError {
    #[error("failed to read mutation plan: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {resource} capacity {requested}: {error}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
        #[source]
        error: TryReserveError,
    },
    #[error("{resource} capacity arithmetic overflow")]
    CapacityOverflow { resource: &'static str },
    #[error(
        "mutation plan encoded input length {requested} exceeds independent hard limit {limit}"
    )]
    EncodedInputLimitExceeded { limit: usize, requested: usize },
    #[error("YAML normalized JSON length {requested} exceeds independent hard limit {limit}")]
    NormalizedJsonLimitExceeded { limit: usize, requested: usize },
    #[error("invalid mutation plan JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Contract(#[from] super::MutationPlanError),
    #[error("invalid mutation plan YAML: {0}")]
    Yaml(#[from] yaml_rust2::ScanError),
    #[error("mutation plan input is not UTF-8; valid prefix ends at byte {valid_up_to}")]
    InvalidUtf8 { valid_up_to: usize },
    #[error("{format} nesting depth {actual} exceeds mutation plan maximum {maximum}")]
    NestingDepthExceeded {
        format: &'static str,
        maximum: u32,
        actual: u32,
    },
    #[error("YAML aliases are unsupported at line {line}, column {column}")]
    YamlAliasUnsupported { line: usize, column: usize },
    #[error("YAML anchors are unsupported at line {line}, column {column}")]
    YamlAnchorUnsupported { line: usize, column: usize },
    #[error("YAML tags are unsupported at line {line}, column {column}")]
    YamlTagUnsupported { line: usize, column: usize },
    #[error("YAML {directive} directives are unsupported at line {line}, column {column}")]
    YamlDirectiveUnsupported {
        directive: &'static str,
        line: usize,
        column: usize,
    },
    #[error(
        "YAML must contain exactly one document; another starts at line {line}, column {column}"
    )]
    YamlMultipleDocuments { line: usize, column: usize },
    #[error("YAML mapping contains duplicate key {key:?} at line {line}, column {column}")]
    YamlDuplicateKey {
        key: String,
        line: usize,
        column: usize,
    },
    #[error("YAML complex mapping keys are unsupported at line {line}, column {column}")]
    YamlComplexKeyUnsupported { line: usize, column: usize },
    #[error("YAML mapping key must be a string at line {line}, column {column}")]
    YamlNonStringKey { line: usize, column: usize },
    #[error("YAML non-finite numbers are unsupported at line {line}, column {column}")]
    YamlNonFiniteNumber { line: usize, column: usize },
    #[error("invalid YAML structure at line {line}, column {column}: {message}")]
    YamlStructure {
        line: usize,
        column: usize,
        message: &'static str,
    },
    #[error("YAML mutation plan document is incomplete")]
    YamlIncompleteDocument,
    #[error("YAML mutation plan must contain exactly one document")]
    YamlDocumentRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn byte_budget(max_bytes: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    struct FragmentedReader {
        bytes: Vec<u8>,
        offset: usize,
        chunks: Vec<usize>,
        next_chunk: usize,
    }

    impl Read for FragmentedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let requested = self.chunks.get(self.next_chunk).copied().unwrap_or(1);
            self.next_chunk += 1;
            let available = self.bytes.len() - self.offset;
            let length = requested.min(available).min(output.len());
            output[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
            self.offset += length;
            Ok(length)
        }
    }

    #[test]
    fn capacity_reservation_uses_length_based_additional_guarantees() {
        let mut values = Vec::with_capacity(32 * 1024);
        values.resize(20 * 1024, 0_u8);

        ensure_capacity(
            &mut values,
            40 * 1024,
            MutationPlan::MAX_ENCODED_INPUT_BYTES,
            "fragmented input",
        )
        .unwrap();

        assert!(values.capacity() >= 60 * 1024);
    }

    #[test]
    fn encoded_input_hard_cap_accepts_exactly_the_limit_and_rejects_one_more_byte() {
        assert_eq!(MutationPlan::MAX_ENCODED_INPUT_BYTES, 128 * 1024 * 1024);

        let encoded =
            read_contract_bytes_with_limit(b"null".as_slice(), &mut AssetLoadBudget::default(), 4)
                .unwrap();
        assert_eq!(encoded, b"null");

        let mut one_over_budget = AssetLoadBudget::default();
        assert!(matches!(
            read_contract_bytes_with_limit(b"null ".as_slice(), &mut one_over_budget, 4,),
            Err(MutationPlanReadError::EncodedInputLimitExceeded {
                limit: 4,
                requested: 5,
            })
        ));
        assert_eq!(one_over_budget.usage().bytes, PARSER_FIXED_WORK_BYTES);
    }

    #[test]
    fn fragmented_reader_never_changes_the_input_budget_contract() {
        let bytes = vec![b' '; 60 * 1024];
        let reader = FragmentedReader {
            bytes: bytes.clone(),
            offset: 0,
            chunks: vec![20 * 1024, 40 * 1024],
            next_chunk: 0,
        };
        let mut budget = AssetLoadBudget::default();

        let encoded = read_contract_bytes(reader, &mut budget).unwrap();

        assert_eq!(encoded, bytes);
        assert_eq!(
            budget.usage().bytes,
            PARSER_FIXED_WORK_BYTES
                + u64::try_from(encoded.len()).unwrap() * (PARSER_WORK_BYTES_PER_INPUT_BYTE + 1)
        );
    }

    #[test]
    fn materialization_reservation_has_an_exact_budget_boundary() {
        let structure = JsonStructure {
            encoded_bytes: 37,
            entries: 9,
            string_bytes: 17,
        };
        let required = materialization_bytes::<MutationPlanWire>(structure).unwrap();

        let mut exact = byte_budget(required);
        charge_materialization::<MutationPlanWire>(structure, &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, required);

        let mut one_short = byte_budget(required - 1);
        assert!(matches!(
            charge_materialization::<MutationPlanWire>(structure, &mut one_short),
            Err(MutationPlanReadError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == required - 1 && requested == required
        ));
        assert_eq!(one_short.usage().bytes, 0);
    }

    #[test]
    fn large_plan_bytes_fail_before_typed_deserialization_starts() {
        struct DeserializationMustNotStart;

        impl<'de> Deserialize<'de> for DeserializationMustNotStart {
            fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                panic!("typed deserialization started before materialization was reserved");
            }
        }

        const PLAN_BYTES: usize = 1024 * 1024;
        let encoded = format!(r#"{{"bytes":"{}"}}"#, "00".repeat(PLAN_BYTES));
        let structure = probe_json(encoded.as_bytes(), &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(
            structure.string_bytes,
            u64::try_from(PLAN_BYTES * 2).unwrap()
        );
        let required = materialization_bytes::<DeserializationMustNotStart>(structure).unwrap();
        let mut one_short = byte_budget(required - 1);

        assert!(matches!(
            deserialize_after_materialization_reservation::<DeserializationMustNotStart>(
                encoded.as_bytes(),
                structure,
                &mut one_short,
            ),
            Err(MutationPlanReadError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == required - 1 && requested == required
        ));
    }

    #[test]
    fn yaml_normalized_json_has_an_independent_exact_hard_cap() {
        assert_eq!(MutationPlan::MAX_NORMALIZED_JSON_BYTES, 128 * 1024 * 1024);

        let normalized = serialize_yaml_node_with_limit(
            YamlNode::String("ab".to_owned()),
            &mut AssetLoadBudget::default(),
            4,
        )
        .unwrap();
        assert_eq!(normalized, br#""ab""#);

        assert!(matches!(
            serialize_yaml_node_with_limit(
                YamlNode::String("ab".to_owned()),
                &mut AssetLoadBudget::default(),
                3,
            ),
            Err(MutationPlanReadError::NormalizedJsonLimitExceeded {
                limit: 3,
                requested: 4,
            })
        ));

        let mut output_budget = AssetLoadBudget::default();
        let mut output = BudgetedOutput {
            bytes: Vec::new(),
            budget: &mut output_budget,
            failure: None,
            maximum: 3,
        };
        Write::write_all(&mut output, b"abc").unwrap();
        let charged_at_limit = output.budget.usage().bytes;
        assert!(Write::write_all(&mut output, b"d").is_err());
        assert_eq!(output.bytes, b"abc");
        assert_eq!(output.budget.usage().bytes, charged_at_limit);
        assert!(matches!(
            output.failure,
            Some(OutputFailure::Limit {
                limit: 3,
                requested: 4,
            })
        ));
    }

    #[test]
    fn per_entry_materialization_model_covers_known_transition_layouts() {
        assert!(
            WIRE_LAYOUT_BYTES_PER_ENTRY
                >= u64::try_from(size_of::<super::super::MutationOperation>()).unwrap()
        );
        assert!(
            FROM_WIRE_TRANSITION_BYTES_PER_ENTRY
                >= u64::try_from(
                    size_of::<super::super::MutationOperation>()
                        + size_of::<super::super::GenericMutation>()
                        + size_of::<usize>() * 2
                )
                .unwrap()
        );
    }

    #[test]
    fn json_probe_accepts_wire_limit_and_rejects_one_more_level() {
        let at_limit = format!(
            "{}0{}",
            "[".repeat(MAX_PLAN_WIRE_DEPTH as usize),
            "]".repeat(MAX_PLAN_WIRE_DEPTH as usize)
        );
        probe_json(at_limit.as_bytes(), &mut AssetLoadBudget::default()).unwrap();

        let over_limit = format!(
            "{}0{}",
            "[".repeat(MAX_PLAN_WIRE_DEPTH as usize + 1),
            "]".repeat(MAX_PLAN_WIRE_DEPTH as usize + 1)
        );
        assert!(matches!(
            probe_json(over_limit.as_bytes(), &mut AssetLoadBudget::default()),
            Err(MutationPlanReadError::NestingDepthExceeded {
                format: "JSON",
                maximum: MAX_PLAN_WIRE_DEPTH,
                actual,
            }) if actual == MAX_PLAN_WIRE_DEPTH + 1
        ));
    }

    #[test]
    fn yaml_event_parser_accepts_wire_limit_and_rejects_one_more_level() {
        let at_limit = format!(
            "---\n{}0{}\n",
            "[".repeat(MAX_PLAN_WIRE_DEPTH as usize),
            "]".repeat(MAX_PLAN_WIRE_DEPTH as usize)
        );
        parse_yaml_node(&at_limit, &mut AssetLoadBudget::default()).unwrap();

        let over_limit = format!(
            "---\n{}0{}\n",
            "[".repeat(MAX_PLAN_WIRE_DEPTH as usize + 1),
            "]".repeat(MAX_PLAN_WIRE_DEPTH as usize + 1)
        );
        assert!(matches!(
            parse_yaml_node(&over_limit, &mut AssetLoadBudget::default()),
            Err(MutationPlanReadError::NestingDepthExceeded {
                format: "YAML",
                maximum: MAX_PLAN_WIRE_DEPTH,
                actual,
            }) if actual == MAX_PLAN_WIRE_DEPTH + 1
        ));
    }
}
