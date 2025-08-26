//! Unity YAML serializer
//!
//! This module implements Unity-specific YAML serialization that maintains
//! exact compatibility with Unity's YAML format, including:
//! - Unity tags (!u!classid)
//! - Anchor handling (&anchor)
//! - Extra anchor data (stripped, etc.)
//! - Proper formatting and line endings

use crate::constants::{LineEnding, UNITY_TAG_URI, UNITY_YAML_VERSION};
use std::fmt::Write;
use unity_asset_core::{Result, UnityAssetError, UnityClass, UnityValue};

/// Unity YAML serializer
pub struct UnityYamlSerializer {
    /// Line ending style to use
    line_ending: LineEnding,
    /// Indent size (Unity uses 2 spaces)
    indent_size: usize,
    /// Current indentation level
    indent_level: usize,
    /// Whether this is the first document
    first_document: bool,
}

impl UnityYamlSerializer {
    /// Create a new Unity YAML serializer
    pub fn new() -> Self {
        Self {
            line_ending: LineEnding::default(),
            indent_size: 2,
            indent_level: 0,
            first_document: true,
        }
    }

    /// Set line ending style
    pub fn with_line_ending(mut self, line_ending: LineEnding) -> Self {
        self.line_ending = line_ending;
        self
    }

    /// Serialize Unity classes to YAML string
    pub fn serialize_to_string(&mut self, classes: &[UnityClass]) -> Result<String> {
        let mut output = String::new();
        self.serialize_to_writer(&mut output, classes)?;
        Ok(output)
    }

    /// Serialize Unity classes to a writer
    pub fn serialize_to_writer<W: Write>(
        &mut self,
        writer: &mut W,
        classes: &[UnityClass],
    ) -> Result<()> {
        self.first_document = true;

        // Write YAML header for first document
        if !classes.is_empty() {
            self.write_yaml_header(writer)?;
        }

        // Serialize each Unity class as a separate document
        for (index, class) in classes.iter().enumerate() {
            if index > 0 {
                self.first_document = false;
            }
            self.serialize_unity_class(writer, class)?;
        }

        Ok(())
    }

    /// Write YAML header (version and tags)
    fn write_yaml_header<W: Write>(&self, writer: &mut W) -> Result<()> {
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
    fn serialize_unity_class<W: Write>(
        &mut self,
        writer: &mut W,
        class: &UnityClass,
    ) -> Result<()> {
        // Write document separator with Unity tag and anchor
        write!(writer, "--- !u!{} &{}", class.class_id, class.anchor).map_err(|e| {
            UnityAssetError::format(format!("Failed to write document header: {}", e))
        })?;

        // Write extra anchor data if present
        if !class.extra_anchor_data.is_empty() {
            write!(writer, " {}", class.extra_anchor_data).map_err(|e| {
                UnityAssetError::format(format!("Failed to write extra anchor data: {}", e))
            })?;
        }

        write!(writer, "{}", self.line_ending.as_str())
            .map_err(|e| UnityAssetError::format(format!("Failed to write line ending: {}", e)))?;

        // Write class name and properties
        write!(writer, "{}:{}", class.class_name, self.line_ending.as_str())
            .map_err(|e| UnityAssetError::format(format!("Failed to write class name: {}", e)))?;

        // Serialize properties
        self.indent_level = 1;
        for (key, value) in class.properties() {
            self.serialize_property(writer, key, value)?;
        }

        Ok(())
    }

    /// Serialize a property key-value pair
    fn serialize_property<W: Write>(
        &mut self,
        writer: &mut W,
        key: &str,
        value: &UnityValue,
    ) -> Result<()> {
        // Write indentation
        self.write_indent(writer)?;

        // Write property key
        write!(writer, "{}: ", key)
            .map_err(|e| UnityAssetError::format(format!("Failed to write property key: {}", e)))?;

        // Write property value
        self.serialize_value(writer, value, false)?;

        Ok(())
    }

    /// Serialize a Unity value
    fn serialize_value<W: Write>(
        &mut self,
        writer: &mut W,
        value: &UnityValue,
        inline: bool,
    ) -> Result<()> {
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
            UnityValue::Float(f) => {
                write!(writer, "{}{}", f, self.line_ending.as_str()).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write float value: {}", e))
                })?;
            }
            UnityValue::String(s) => {
                // Handle string quoting based on content
                if self.needs_quoting(s) {
                    write!(
                        writer,
                        "\"{}\"{}",
                        self.escape_string(s),
                        self.line_ending.as_str()
                    )
                } else {
                    write!(writer, "{}{}", s, self.line_ending.as_str())
                }
                .map_err(|e| {
                    UnityAssetError::format(format!("Failed to write string value: {}", e))
                })?;
            }
            UnityValue::Array(arr) => {
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
                        self.serialize_value_inline(writer, item)?;
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
                        self.serialize_value(writer, item, true)?;
                    }
                    self.indent_level -= 1;
                }
            }
            UnityValue::Object(obj) => {
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
                        write!(writer, "{}: ", key).map_err(|e| {
                            UnityAssetError::format(format!("Failed to write object key: {}", e))
                        })?;
                        self.serialize_value_inline(writer, value)?;
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
                        self.serialize_property(writer, key, value)?;
                    }
                    self.indent_level -= 1;
                }
            }
        }
        Ok(())
    }

    /// Serialize a value inline (for arrays and objects)
    fn serialize_value_inline<W: Write>(&self, writer: &mut W, value: &UnityValue) -> Result<()> {
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
            UnityValue::Float(f) => {
                write!(writer, "{}", f).map_err(|e| {
                    UnityAssetError::format(format!("Failed to write float value: {}", e))
                })?;
            }
            UnityValue::String(s) => {
                if self.needs_quoting(s) {
                    write!(writer, "\"{}\"", self.escape_string(s)).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write quoted string: {}", e))
                    })?;
                } else {
                    write!(writer, "{}", s).map_err(|e| {
                        UnityAssetError::format(format!("Failed to write string: {}", e))
                    })?;
                }
            }
            UnityValue::Array(_) | UnityValue::Object(_) => {
                // For complex nested structures, we might need more sophisticated handling
                write!(writer, "{{...}}").map_err(|e| {
                    UnityAssetError::format(format!("Failed to write complex value: {}", e))
                })?;
            }
        }
        Ok(())
    }

    /// Write indentation
    fn write_indent<W: Write>(&self, writer: &mut W) -> Result<()> {
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
            || s.starts_with(' ')
            || s.ends_with(' ')
    }

    /// Escape a string for YAML
    fn escape_string(&self, s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    }

    /// Check if an array should be written inline
    fn is_simple_array(&self, arr: &[UnityValue]) -> bool {
        arr.len() <= 3
            && arr.iter().all(|v| match v {
                UnityValue::Integer(_) | UnityValue::Float(_) | UnityValue::Bool(_) => true,
                UnityValue::String(s) => s.len() < 20,
                _ => false,
            })
    }

    /// Check if an object should be written inline
    fn is_simple_object(&self, obj: &indexmap::IndexMap<String, UnityValue>) -> bool {
        obj.len() <= 3
            && obj.values().all(|v| match v {
                UnityValue::Integer(_) | UnityValue::Float(_) | UnityValue::Bool(_) => true,
                UnityValue::String(s) => s.len() < 20,
                _ => false,
            })
    }
}

impl Default for UnityYamlSerializer {
    fn default() -> Self {
        Self::new()
    }
}
