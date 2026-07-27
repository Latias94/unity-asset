//! Unity YAML serializer
//!
//! This module implements Unity-specific YAML serialization that maintains
//! exact compatibility with Unity's YAML format, including:
//! - Unity tags (!u!classid)
//! - Anchor handling (&anchor)
//! - Extra anchor data (stripped, etc.)
//! - Proper formatting and line endings

use crate::constants::{LineEnding, UNITY_TAG_URI, UNITY_YAML_VERSION};
use std::fmt::{self, Write as FmtWrite};
use std::io::{self, Write as IoWrite};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, Result, UnityAssetError, UnityClass, UnityValue,
};

/// Unity YAML serializer
pub struct UnityYamlSerializer {
    /// Line ending style to use
    line_ending: LineEnding,
    /// Indent size (Unity uses 2 spaces)
    indent_size: usize,
    /// Current indentation level
    indent_level: usize,
}

impl UnityYamlSerializer {
    /// Create a new Unity YAML serializer
    pub fn new() -> Self {
        Self {
            line_ending: LineEnding::default(),
            indent_size: 2,
            indent_level: 0,
        }
    }

    /// Set line ending style
    pub fn with_line_ending(mut self, line_ending: LineEnding) -> Self {
        self.line_ending = line_ending;
        self
    }

    /// Serialize borrowed Unity classes while charging one caller-owned budget.
    pub fn serialize_to_string_with_budget<'class, I>(
        &mut self,
        classes: I,
        budget: &mut AssetLoadBudget,
    ) -> Result<String>
    where
        I: IntoIterator<Item = &'class UnityClass>,
    {
        let mut output = String::new();
        self.serialize_to_fmt_writer(&mut output, classes, budget)?;
        Ok(output)
    }

    /// Stream borrowed Unity classes while charging one caller-owned budget.
    ///
    /// The serializer does not buffer the complete YAML document and does not flush the writer.
    /// Budget usage accumulates across every class, property, and nested value in the stream.
    /// Any writer failure is returned as [`UnityAssetError::Io`] with the original
    /// [`io::Error`] intact.
    pub fn serialize_to_writer_with_budget<'class, W, I>(
        &mut self,
        writer: &mut W,
        classes: I,
        budget: &mut AssetLoadBudget,
    ) -> Result<()>
    where
        W: IoWrite + ?Sized,
        I: IntoIterator<Item = &'class UnityClass>,
    {
        let mut adapter = IoWriterAdapter::new(writer);
        let result = self.serialize_to_fmt_writer(&mut adapter, classes, budget);
        match adapter.take_error() {
            Some(error) => Err(error.into()),
            None => result,
        }
    }

    fn serialize_to_fmt_writer<'class, W, I>(
        &mut self,
        writer: &mut W,
        classes: I,
        budget: &mut AssetLoadBudget,
    ) -> Result<()>
    where
        W: FmtWrite,
        I: IntoIterator<Item = &'class UnityClass>,
    {
        self.indent_level = 0;
        let mut classes = classes.into_iter().peekable();

        // Write YAML header for first document
        if classes.peek().is_some() {
            self.write_yaml_header(writer)?;
        }

        // Serialize each Unity class as a separate document
        for class in classes {
            self.serialize_unity_class(writer, class, budget)?;
        }

        Ok(())
    }

    /// Write YAML header (version and tags)
    fn write_yaml_header<W: FmtWrite>(&self, writer: &mut W) -> Result<()> {
        // Write YAML version
        write!(
            writer,
            "%YAML {}.{}{}",
            UNITY_YAML_VERSION.0,
            UNITY_YAML_VERSION.1,
            self.line_ending.as_str()
        )
        .map_err(|e| UnityAssetError::format(format!("Failed to write YAML version: {}", e)))?;

        // Write Unity tag
        write!(
            writer,
            "%TAG !u! {}{}",
            UNITY_TAG_URI,
            self.line_ending.as_str()
        )
        .map_err(|e| UnityAssetError::format(format!("Failed to write Unity tag: {}", e)))?;

        Ok(())
    }

    /// Serialize a single Unity class
    fn serialize_unity_class<W: FmtWrite>(
        &mut self,
        writer: &mut W,
        class: &UnityClass,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        observe_serialization_entry(budget, 0)?;
        charge_serialization_members(budget, class.properties().len())?;
        charge_serialization_bytes(budget, class.anchor().len())?;
        charge_serialization_bytes(budget, class.extra_anchor_data().len())?;
        charge_serialization_bytes(budget, class.class_name().len())?;

        // Write document separator with Unity tag and anchor
        write!(writer, "--- !u!{} &{}", class.class_id(), class.anchor()).map_err(|e| {
            UnityAssetError::format(format!("Failed to write document header: {}", e))
        })?;

        // Write extra anchor data if present
        if !class.extra_anchor_data().is_empty() {
            write!(writer, " {}", class.extra_anchor_data()).map_err(|e| {
                UnityAssetError::format(format!("Failed to write extra anchor data: {}", e))
            })?;
        }

        write!(writer, "{}", self.line_ending.as_str())
            .map_err(|e| UnityAssetError::format(format!("Failed to write line ending: {}", e)))?;

        // Write class name and properties
        write!(
            writer,
            "{}:{}",
            class.class_name(),
            self.line_ending.as_str()
        )
        .map_err(|e| UnityAssetError::format(format!("Failed to write class name: {}", e)))?;

        // Serialize properties
        self.indent_level = 1;
        for (key, value) in class.properties() {
            self.serialize_property(writer, key, value, budget, 1)?;
        }

        Ok(())
    }

    /// Serialize a property key-value pair
    fn serialize_property<W: FmtWrite>(
        &mut self,
        writer: &mut W,
        key: &str,
        value: &UnityValue,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<()> {
        charge_serialization_bytes(budget, key.len())?;

        // Write indentation
        self.write_indent(writer)?;

        // Write property key
        write!(writer, "{}: ", key)
            .map_err(|e| UnityAssetError::format(format!("Failed to write property key: {}", e)))?;

        // Write property value
        self.serialize_value(writer, value, false, budget, depth)?;

        Ok(())
    }

    /// Serialize a Unity value
    fn serialize_value<W: FmtWrite>(
        &mut self,
        writer: &mut W,
        value: &UnityValue,
        inline: bool,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<()> {
        observe_serialization_entry(budget, depth)?;

        match value {
            UnityValue::Null => {
                write!(writer, "{{fileID: 0}}{}", self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write null value: {}", e))
                })?;
            }
            UnityValue::Bool(b) => {
                write!(
                    writer,
                    "{}{}",
                    if *b { "1" } else { "0" },
                    self.line_ending.as_str()
                )
                .map_err(|e| {
                    UnityAssetError::format(format!("Failed to write bool value: {}", e))
                })?;
            }
            UnityValue::Integer(i) => {
                write!(writer, "{}{}", i, self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write integer value: {}", e))
                })?;
            }
            UnityValue::Unsigned(i) => {
                write!(writer, "{}{}", i, self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write unsigned value: {}", e))
                })?;
            }
            UnityValue::Float(f) => {
                write!(writer, "{}{}", f, self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write float value: {}", e))
                })?;
            }
            UnityValue::String(s) => {
                charge_serialization_bytes(budget, s.len())?;
                self.write_string_inline(writer, s)?;
                write!(writer, "{}", self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write string line ending: {}", e))
                })?;
            }
            UnityValue::Array(arr) => {
                charge_serialization_members(budget, arr.len())?;
                let child_depth = next_serialization_depth(depth)?;
                if arr.is_empty() {
                    write!(writer, "[]{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write empty array: {}", e))
                    })?;
                } else if inline || self.is_simple_array(arr) {
                    // Write inline array
                    write!(writer, "[").map_err(|e| {
                        UnityAssetError::format(format!("Failed to write array start: {}", e))
                    })?;
                    for (i, item) in arr.iter().enumerate() {
                        if i > 0 {
                            write!(writer, ", ").map_err(|e| {
                                UnityAssetError::format(format!(
                                    "Failed to write array separator: {}",
                                    e
                                ))
                            })?;
                        }
                        self.serialize_value_inline(writer, item, budget, child_depth)?;
                    }
                    write!(writer, "]{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write inline array end: {}", e))
                    })?;
                } else {
                    // Write block array
                    write!(writer, "{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write array start: {}", e))
                    })?;
                    self.indent_level += 1;
                    for item in arr {
                        self.write_indent(writer)?;
                        write!(writer, "- ").map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write array item prefix: {}",
                                e
                            ))
                        })?;
                        match item {
                            UnityValue::Array(inner)
                                if !inner.is_empty() && !self.is_simple_array(inner) =>
                            {
                                self.serialize_value(writer, item, false, budget, child_depth)?
                            }
                            UnityValue::Object(inner)
                                if !inner.is_empty() && !self.is_simple_object(inner) =>
                            {
                                self.serialize_value(writer, item, false, budget, child_depth)?
                            }
                            _ => self.serialize_value(writer, item, true, budget, child_depth)?,
                        }
                    }
                    self.indent_level -= 1;
                }
            }
            UnityValue::Bytes(b) => {
                charge_serialization_bytes(budget, b.len())?;
                if b.is_empty() {
                    write!(writer, "[]{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write empty bytes: {}", e))
                    })?;
                } else if inline || b.len() <= 64 {
                    write!(writer, "[").map_err(|e| {
                        UnityAssetError::format(format!("Failed to write bytes start: {}", e))
                    })?;
                    for (i, item) in b.iter().enumerate() {
                        if i > 0 {
                            write!(writer, ", ").map_err(|e| {
                                UnityAssetError::format(format!(
                                    "Failed to write bytes separator: {}",
                                    e
                                ))
                            })?;
                        }
                        write!(writer, "{}", item).map_err(|e| {
                            UnityAssetError::format(format!("Failed to write byte value: {}", e))
                        })?;
                    }
                    write!(writer, "]{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write bytes end: {}", e))
                    })?;
                } else {
                    write!(writer, "{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write bytes start: {}", e))
                    })?;
                    self.indent_level += 1;
                    for item in b {
                        self.write_indent(writer)?;
                        write!(writer, "- {}", item).map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write bytes item prefix: {}",
                                e
                            ))
                        })?;
                        write!(writer, "{}", self.line_ending.as_str()).map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write bytes line ending: {}",
                                e
                            ))
                        })?;
                    }
                    self.indent_level -= 1;
                }
            }
            UnityValue::Object(obj) => {
                charge_serialization_members(budget, obj.len())?;
                let child_depth = next_serialization_depth(depth)?;
                if obj.is_empty() {
                    write!(writer, "{{}}{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write empty object: {}", e))
                    })?;
                } else if inline || self.is_simple_object(obj) {
                    // Write inline object
                    write!(writer, "{{").map_err(|e| {
                        UnityAssetError::format(format!("Failed to write object start: {}", e))
                    })?;
                    for (i, (key, value)) in obj.iter().enumerate() {
                        if i > 0 {
                            write!(writer, ", ").map_err(|e| {
                                UnityAssetError::format(format!(
                                    "Failed to write object separator: {}",
                                    e
                                ))
                            })?;
                        }
                        charge_serialization_bytes(budget, key.len())?;
                        self.write_string_inline(writer, key)?;
                        write!(writer, ": ").map_err(|e| {
                            UnityAssetError::format(format!("Failed to write object key: {}", e))
                        })?;
                        self.serialize_value_inline(writer, value, budget, child_depth)?;
                    }
                    write!(writer, "}}{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write inline object end: {}", e))
                    })?;
                } else {
                    // Write block object
                    write!(writer, "{}", self.line_ending.as_str()).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write object start: {}", e))
                    })?;
                    self.indent_level += 1;
                    for (key, value) in obj {
                        self.serialize_property(writer, key, value, budget, child_depth)?;
                    }
                    self.indent_level -= 1;
                }
            }
        }
        Ok(())
    }

    /// Serialize a value inline (for arrays and objects)
    fn serialize_value_inline<W: FmtWrite>(
        &self,
        writer: &mut W,
        value: &UnityValue,
        budget: &mut AssetLoadBudget,
        depth: u32,
    ) -> Result<()> {
        observe_serialization_entry(budget, depth)?;

        match value {
            UnityValue::Null => {
                write!(writer, "{{fileID: 0}}").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write null value: {}", e))
                })?;
            }
            UnityValue::Bool(b) => {
                write!(writer, "{}", if *b { "1" } else { "0" }).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write bool value: {}", e))
                })?;
            }
            UnityValue::Integer(i) => {
                write!(writer, "{}", i).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write integer value: {}", e))
                })?;
            }
            UnityValue::Unsigned(i) => {
                write!(writer, "{}", i).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write unsigned value: {}", e))
                })?;
            }
            UnityValue::Float(f) => {
                write!(writer, "{}", f).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write float value: {}", e))
                })?;
            }
            UnityValue::String(s) => {
                charge_serialization_bytes(budget, s.len())?;
                self.write_string_inline(writer, s)?;
            }
            UnityValue::Bytes(b) => {
                charge_serialization_bytes(budget, b.len())?;
                write!(writer, "[").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write bytes start: {}", e))
                })?;
                for (index, byte) in b.iter().enumerate() {
                    if index > 0 {
                        write!(writer, ", ").map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write bytes separator: {}",
                                e
                            ))
                        })?;
                    }
                    write!(writer, "{}", byte).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write byte value: {}", e))
                    })?;
                }
                write!(writer, "]").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write bytes end: {}", e))
                })?;
            }
            UnityValue::Array(values) => {
                charge_serialization_members(budget, values.len())?;
                let child_depth = next_serialization_depth(depth)?;
                write!(writer, "[").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write array start: {}", e))
                })?;
                for (index, item) in values.iter().enumerate() {
                    if index > 0 {
                        write!(writer, ", ").map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write array separator: {}",
                                e
                            ))
                        })?;
                    }
                    self.serialize_value_inline(writer, item, budget, child_depth)?;
                }
                write!(writer, "]").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write array end: {}", e))
                })?;
            }
            UnityValue::Object(fields) => {
                charge_serialization_members(budget, fields.len())?;
                let child_depth = next_serialization_depth(depth)?;
                write!(writer, "{{").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write object start: {}", e))
                })?;
                for (index, (key, item)) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(writer, ", ").map_err(|e| {
                            UnityAssetError::format(format!(
                                "Failed to write object separator: {}",
                                e
                            ))
                        })?;
                    }
                    charge_serialization_bytes(budget, key.len())?;
                    self.write_string_inline(writer, key)?;
                    write!(writer, ": ").map_err(|e| {
                        UnityAssetError::format(format!("Failed to write object key: {}", e))
                    })?;
                    self.serialize_value_inline(writer, item, budget, child_depth)?;
                }
                write!(writer, "}}").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write object end: {}", e))
                })?;
            }
        }
        Ok(())
    }

    fn write_string_inline<W: FmtWrite>(&self, writer: &mut W, value: &str) -> Result<()> {
        if !self.needs_quoting(value) {
            write!(writer, "{}", value)
                .map_err(|e| UnityAssetError::format(format!("Failed to write string: {}", e)))?;
            return Ok(());
        }

        writer.write_char('"').map_err(|e| {
            UnityAssetError::format(format!("Failed to write quoted string start: {}", e))
        })?;
        for character in value.chars() {
            let escaped = match character {
                '\\' => "\\\\",
                '"' => "\\\"",
                '\n' => "\\n",
                '\r' => "\\r",
                '\t' => "\\t",
                _ => {
                    writer.write_char(character).map_err(|e| {
                        UnityAssetError::format(format!(
                            "Failed to write quoted string character: {}",
                            e
                        ))
                    })?;
                    continue;
                }
            };
            writer.write_str(escaped).map_err(|e| {
                UnityAssetError::format(format!("Failed to write escaped string character: {}", e))
            })?;
        }
        writer.write_char('"').map_err(|e| {
            UnityAssetError::format(format!("Failed to write quoted string end: {}", e))
        })?;
        Ok(())
    }

    /// Write indentation
    fn write_indent<W: FmtWrite>(&self, writer: &mut W) -> Result<()> {
        for _ in 0..(self.indent_level * self.indent_size) {
            write!(writer, " ").map_err(|e| {
                UnityAssetError::format(format!("Failed to write indentation: {}", e))
            })?;
        }
        Ok(())
    }

    /// Check if a string needs quoting
    fn needs_quoting(&self, s: &str) -> bool {
        s.is_empty()
            || s.contains('\n')
            || s.contains('\r')
            || s.contains('"')
            || s.contains('\'')
            || s.contains(':')
            || s.contains('[')
            || s.contains(']')
            || s.contains('{')
            || s.contains('}')
            || s.contains(',')
            || s.contains('#')
            || s.starts_with(' ')
            || s.ends_with(' ')
            || s.starts_with(|character: char| {
                character.is_ascii_digit()
                    || matches!(
                        character,
                        '-' | '?' | ':' | '&' | '*' | '!' | '|' | '>' | '%' | '@' | '`' | '+' | '.'
                    )
            })
            || ["null", "true", "false", "yes", "no", "on", "off"]
                .iter()
                .any(|keyword| s.eq_ignore_ascii_case(keyword))
            || s == "~"
    }

    /// Check if an array should be written inline
    fn is_simple_array(&self, arr: &[UnityValue]) -> bool {
        arr.len() <= 3
            && arr.iter().all(|v| match v {
                UnityValue::Integer(_)
                | UnityValue::Unsigned(_)
                | UnityValue::Float(_)
                | UnityValue::Bool(_) => true,
                UnityValue::String(s) => s.len() < 20,
                _ => false,
            })
    }

    /// Check if an object should be written inline
    fn is_simple_object(&self, obj: &indexmap::IndexMap<String, UnityValue>) -> bool {
        obj.len() <= 3
            && obj.values().all(|v| match v {
                UnityValue::Integer(_)
                | UnityValue::Unsigned(_)
                | UnityValue::Float(_)
                | UnityValue::Bool(_) => true,
                UnityValue::String(s) => s.len() < 20,
                _ => false,
            })
    }
}

fn observe_serialization_entry(budget: &mut AssetLoadBudget, depth: u32) -> Result<()> {
    budget
        .consume_entries(1)
        .map_err(serialization_budget_error)?;
    budget
        .observe_depth(depth)
        .map_err(serialization_budget_error)
}

fn charge_serialization_bytes(budget: &mut AssetLoadBudget, amount: usize) -> Result<()> {
    let amount = u64::try_from(amount)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" })
        .map_err(serialization_budget_error)?;
    budget
        .consume_bytes(amount)
        .map_err(serialization_budget_error)
}

fn charge_serialization_members(budget: &mut AssetLoadBudget, amount: usize) -> Result<()> {
    let amount = u64::try_from(amount)
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "members",
        })
        .map_err(serialization_budget_error)?;
    budget
        .consume_members(amount)
        .map_err(serialization_budget_error)
}

fn next_serialization_depth(depth: u32) -> Result<u32> {
    depth
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })
        .map_err(serialization_budget_error)
}

fn serialization_budget_error(error: BudgetError) -> UnityAssetError {
    UnityAssetError::with_source("YAML serialization budget exceeded", error)
}

struct IoWriterAdapter<'writer, W: IoWrite + ?Sized> {
    writer: &'writer mut W,
    error: Option<io::Error>,
}

impl<'writer, W: IoWrite + ?Sized> IoWriterAdapter<'writer, W> {
    fn new(writer: &'writer mut W) -> Self {
        Self {
            writer,
            error: None,
        }
    }

    fn take_error(&mut self) -> Option<io::Error> {
        self.error.take()
    }
}

impl<W: IoWrite + ?Sized> FmtWrite for IoWriterAdapter<'_, W> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.error.is_some() {
            return Err(fmt::Error);
        }
        self.writer.write_all(value.as_bytes()).map_err(|error| {
            self.error = Some(error);
            fmt::Error
        })
    }
}

impl Default for UnityYamlSerializer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_budgeted_yaml_source;
    use indexmap::indexmap;
    use std::sync::Arc;
    use unity_asset_core::{AssetLoadLimits, UnityDocument};

    #[test]
    fn complex_inline_values_round_trip_without_placeholders() {
        let value = UnityValue::Object(indexmap! {
            "nested".to_string() => UnityValue::Array(vec![
                UnityValue::Object(indexmap! {
                    "payload".to_string() => UnityValue::Bytes(vec![0, 127, 255]),
                    "metadata".to_string() => UnityValue::Object(indexmap! {
                        "label".to_string() => UnityValue::String("value:quoted".to_string()),
                        "flow,key".to_string() => UnityValue::String("true".to_string()),
                    }),
                }),
                UnityValue::Array(vec![
                    UnityValue::Integer(-1),
                    UnityValue::Unsigned(u64::MAX),
                ]),
            ]),
        });
        let mut encoded = String::new();
        let mut budget = AssetLoadBudget::default();
        UnityYamlSerializer::new()
            .serialize_value_inline(&mut encoded, &value, &mut budget, 0)
            .unwrap();

        assert!(!encoded.contains("{...}"));
        assert!(!encoded.contains("<bytes len="));
        assert!(encoded.contains("payload: [0, 127, 255]"));
        assert!(encoded.contains("metadata: {label: \"value:quoted\","));
        assert!(encoded.contains("\"flow,key\": \"true\""));

        let first_yaml = format!(
            "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &1\nMonoBehaviour:\n  value: {encoded}\n"
        );
        let mut first_budget = AssetLoadBudget::default();
        let first_source =
            parse_budgeted_yaml_source(Arc::from(first_yaml.as_bytes()), &mut first_budget)
                .unwrap();
        let first = first_source.document().entries();
        let mut reserialize_budget = AssetLoadBudget::default();
        let reparsed_yaml = UnityYamlSerializer::new()
            .serialize_to_string_with_budget(first.iter(), &mut reserialize_budget)
            .unwrap();
        let mut second_budget = AssetLoadBudget::default();
        let second_source =
            parse_budgeted_yaml_source(Arc::from(reparsed_yaml.as_bytes()), &mut second_budget)
                .unwrap();
        let second = second_source.document().entries();

        assert_eq!(first[0].get("value"), second[0].get("value"));
    }

    #[test]
    fn complex_inline_values_share_one_depth_budget() {
        let value = UnityValue::Object(indexmap! {
            "first".to_string() => UnityValue::Array(vec![UnityValue::Object(indexmap! {
                "too_deep".to_string() => UnityValue::Integer(1),
            })]),
        });
        let limits = AssetLoadLimits {
            max_depth: 2,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut encoded = String::new();

        let error = UnityYamlSerializer::new()
            .serialize_value_inline(&mut encoded, &value, &mut budget, 1)
            .unwrap_err();

        assert_budget_exceeded(error, "depth", 2, 3);
    }

    #[test]
    fn complex_inline_values_check_member_budget_before_writing_container() {
        let value = UnityValue::Array(vec![
            UnityValue::Integer(1),
            UnityValue::Integer(2),
            UnityValue::Integer(3),
        ]);
        let limits = AssetLoadLimits {
            max_members: 2,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut encoded = String::new();

        let error = UnityYamlSerializer::new()
            .serialize_value_inline(&mut encoded, &value, &mut budget, 0)
            .unwrap_err();

        assert!(encoded.is_empty());
        assert_budget_exceeded(error, "members", 2, 3);
    }

    #[test]
    fn budgeted_writer_accumulates_entries_across_fields() {
        let class = UnityClass::with_properties(
            1,
            "GameObject".into(),
            "1".into(),
            indexmap! {
                "first".into() => UnityValue::Integer(1),
                "second".into() => UnityValue::Integer(2),
            },
        );
        let limits = AssetLoadLimits {
            max_entries: 2,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut encoded = Vec::new();

        let error = UnityYamlSerializer::new()
            .serialize_to_writer_with_budget(&mut encoded, std::iter::once(&class), &mut budget)
            .unwrap_err();

        assert_budget_exceeded(error, "entries", 2, 3);
        assert_eq!(budget.usage().entries, 2);
    }

    #[test]
    fn budgeted_writer_accumulates_entries_across_documents() {
        let first = UnityClass::with_properties(
            1,
            "GameObject".into(),
            "1".into(),
            indexmap! {"value".into() => UnityValue::Integer(1)},
        );
        let second = UnityClass::with_properties(
            1,
            "GameObject".into(),
            "2".into(),
            indexmap! {"value".into() => UnityValue::Integer(2)},
        );
        let limits = AssetLoadLimits {
            max_entries: 3,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut encoded = Vec::new();

        let error = UnityYamlSerializer::new()
            .serialize_to_writer_with_budget(&mut encoded, [&first, &second], &mut budget)
            .unwrap_err();

        assert_budget_exceeded(error, "entries", 3, 4);
        assert_eq!(budget.usage().entries, 3);
    }

    fn assert_budget_exceeded(
        error: UnityAssetError,
        resource: &'static str,
        limit: u64,
        requested: u64,
    ) {
        let UnityAssetError::WithSource { source, .. } = error else {
            panic!("expected a budget source, got {error:?}");
        };
        assert!(matches!(
            source.downcast_ref::<BudgetError>(),
            Some(BudgetError::Exceeded {
                resource: actual_resource,
                limit: actual_limit,
                requested: actual_requested,
            }) if *actual_resource == resource
                && *actual_limit == limit
                && *actual_requested == requested
        ));
    }
}
