//! Unity value types
//!
//! This module defines the UnityValue enum and related functionality
//! for representing Unity asset values in a type-safe manner.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::allocation::{
    index_map_allocation_bytes, string_allocation_bytes, vec_allocation_bytes,
};
use crate::budget::{AssetLoadBudget, BudgetError};
use crate::field_path::{FieldPath, FieldPathSegment};

/// A stable description of a [`UnityValue`] shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnityValueKind {
    Null,
    Bool,
    Integer,
    Unsigned,
    Float,
    String,
    Array,
    Bytes,
    Object,
}

impl UnityValueKind {
    /// Returns the stable lowercase name used in diagnostics and serialized data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Unsigned => "unsigned",
            Self::Float => "float",
            Self::String => "string",
            Self::Array => "array",
            Self::Bytes => "bytes",
            Self::Object => "object",
        }
    }
}

impl fmt::Display for UnityValueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Failure while resolving a [`FieldPath`] against a Unity value tree.
///
/// The error retains only stable scalar diagnostics. Resolution never clones a
/// path or field name and therefore does not allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValuePathError {
    #[error("a UnityClass has no UnityValue at the root field path")]
    ClassRoot,
    #[error("field is missing at path segment {segment}")]
    MissingField { segment: usize },
    #[error("expected an object at path segment {segment}, found {actual}")]
    ExpectedObject {
        segment: usize,
        actual: UnityValueKind,
    },
    #[error("expected an array at path segment {segment}, found {actual}")]
    ExpectedArray {
        segment: usize,
        actual: UnityValueKind,
    },
    #[error("index {index} is outside array length {length} at path segment {segment}")]
    IndexOutOfBounds {
        segment: usize,
        index: u32,
        length: usize,
    },
}

/// Failure while cloning a Unity value into caller-owned budgeted storage.
///
/// Allocation failures deliberately retain only stable scalar diagnostics so
/// reporting an exhausted allocator never requires another allocation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnityValueCloneError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to reserve {requested_bytes} bytes for {allocation}")]
    AllocationFailed {
        allocation: &'static str,
        requested_bytes: u64,
    },
}

impl ValuePathError {
    /// Returns the zero-based failing segment, or `None` for an empty class path.
    #[must_use]
    pub const fn segment(&self) -> Option<usize> {
        match self {
            Self::ClassRoot => None,
            Self::MissingField { segment }
            | Self::ExpectedObject { segment, .. }
            | Self::ExpectedArray { segment, .. }
            | Self::IndexOutOfBounds { segment, .. } => Some(*segment),
        }
    }
}

/// A Unity value that can be stored in a Unity class
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnityValue {
    Null,
    Bool(bool),
    Integer(i64),
    /// An unsigned integer that cannot be represented by [`Self::Integer`].
    Unsigned(u64),
    Float(f64),
    String(String),
    Array(Vec<UnityValue>),
    #[serde(with = "serde_bytes")]
    Bytes(Vec<u8>),
    Object(IndexMap<String, UnityValue>),
}

impl UnityValue {
    /// Deeply clones this value while charging all retained and traversal
    /// storage to `budget` before allocation.
    ///
    /// Object fields and array elements consume the shared member ledger. The
    /// root value has depth zero. Traversal is iterative, so accepted values do
    /// not consume one native call frame per Unity value level.
    pub fn try_clone_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, UnityValueCloneError> {
        CloneEngine::new(budget).clone_value(self, 0)
    }

    /// Returns the stable shape discriminator for this value.
    #[must_use]
    pub const fn kind(&self) -> UnityValueKind {
        match self {
            Self::Null => UnityValueKind::Null,
            Self::Bool(_) => UnityValueKind::Bool,
            Self::Integer(_) => UnityValueKind::Integer,
            Self::Unsigned(_) => UnityValueKind::Unsigned,
            Self::Float(_) => UnityValueKind::Float,
            Self::String(_) => UnityValueKind::String,
            Self::Array(_) => UnityValueKind::Array,
            Self::Bytes(_) => UnityValueKind::Bytes,
            Self::Object(_) => UnityValueKind::Object,
        }
    }

    /// Resolves one field-path segment without cloning the segment or value.
    pub(crate) fn value_at_segment(
        &self,
        segment: &FieldPathSegment,
        segment_ordinal: usize,
    ) -> Result<&Self, ValuePathError> {
        let actual = self.kind();
        match segment {
            FieldPathSegment::Field(name) => {
                let Self::Object(fields) = self else {
                    return Err(ValuePathError::ExpectedObject {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                fields.get(name).ok_or(ValuePathError::MissingField {
                    segment: segment_ordinal,
                })
            }
            FieldPathSegment::Index(index) => {
                let Self::Array(values) = self else {
                    return Err(ValuePathError::ExpectedArray {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                let index_usize =
                    usize::try_from(*index).map_err(|_| ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length: values.len(),
                    })?;
                values
                    .get(index_usize)
                    .ok_or(ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length: values.len(),
                    })
            }
        }
    }

    /// Mutably resolves one field-path segment without cloning the segment or value.
    pub(crate) fn value_at_segment_mut(
        &mut self,
        segment: &FieldPathSegment,
        segment_ordinal: usize,
    ) -> Result<&mut Self, ValuePathError> {
        let actual = self.kind();
        match segment {
            FieldPathSegment::Field(name) => {
                let Self::Object(fields) = self else {
                    return Err(ValuePathError::ExpectedObject {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                fields.get_mut(name).ok_or(ValuePathError::MissingField {
                    segment: segment_ordinal,
                })
            }
            FieldPathSegment::Index(index) => {
                let Self::Array(values) = self else {
                    return Err(ValuePathError::ExpectedArray {
                        segment: segment_ordinal,
                        actual,
                    });
                };
                let length = values.len();
                let index_usize =
                    usize::try_from(*index).map_err(|_| ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length,
                    })?;
                values
                    .get_mut(index_usize)
                    .ok_or(ValuePathError::IndexOutOfBounds {
                        segment: segment_ordinal,
                        index: *index,
                        length,
                    })
            }
        }
    }

    /// Resolves borrowed path segments. An empty segment slice returns this value.
    pub fn value_at_segments(
        &self,
        segments: &[FieldPathSegment],
    ) -> Result<&Self, ValuePathError> {
        let mut current = self;
        for (segment_ordinal, segment) in segments.iter().enumerate() {
            current = current.value_at_segment(segment, segment_ordinal)?;
        }
        Ok(current)
    }

    /// Mutably resolves borrowed path segments. An empty segment slice returns this value.
    pub fn value_at_segments_mut(
        &mut self,
        segments: &[FieldPathSegment],
    ) -> Result<&mut Self, ValuePathError> {
        let mut current = self;
        for (segment_ordinal, segment) in segments.iter().enumerate() {
            current = current.value_at_segment_mut(segment, segment_ordinal)?;
        }
        Ok(current)
    }

    /// Resolves a field path. The root path returns this value.
    pub fn value_at_path(&self, path: &FieldPath) -> Result<&Self, ValuePathError> {
        self.value_at_segments(path.segments())
    }

    /// Mutably resolves a field path. The root path returns this value.
    pub fn value_at_path_mut(&mut self, path: &FieldPath) -> Result<&mut Self, ValuePathError> {
        self.value_at_segments_mut(path.segments())
    }

    /// Check if the value is null
    pub fn is_null(&self) -> bool {
        matches!(self, UnityValue::Null)
    }

    /// Get as boolean
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            UnityValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get as integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            UnityValue::Integer(i) => Some(*i),
            UnityValue::Unsigned(i) => i64::try_from(*i).ok(),
            _ => None,
        }
    }

    /// Get as an unsigned integer without losing range information.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            UnityValue::Integer(i) => u64::try_from(*i).ok(),
            UnityValue::Unsigned(i) => Some(*i),
            _ => None,
        }
    }

    /// Get as float
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            UnityValue::Float(f) => Some(*f),
            UnityValue::Integer(i) => Some(*i as f64),
            UnityValue::Unsigned(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Get as string
    pub fn as_str(&self) -> Option<&str> {
        match self {
            UnityValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get as array
    pub fn as_array(&self) -> Option<&Vec<UnityValue>> {
        match self {
            UnityValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            UnityValue::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Get as object
    pub fn as_object(&self) -> Option<&IndexMap<String, UnityValue>> {
        match self {
            UnityValue::Object(obj) => Some(obj),
            _ => None,
        }
    }

    /// Get mutable reference as object
    pub fn as_object_mut(&mut self) -> Option<&mut IndexMap<String, UnityValue>> {
        match self {
            UnityValue::Object(obj) => Some(obj),
            _ => None,
        }
    }
}

pub(crate) fn try_clone_object_with_budget(
    object: &IndexMap<String, UnityValue>,
    budget: &mut AssetLoadBudget,
) -> Result<IndexMap<String, UnityValue>, UnityValueCloneError> {
    CloneEngine::new(budget).clone_object_root(object)
}

enum CloneFrame<'value> {
    Array {
        remaining: std::slice::Iter<'value, UnityValue>,
        output: Vec<UnityValue>,
        depth: u32,
    },
    Object {
        current_key: &'value str,
        remaining: indexmap::map::Iter<'value, String, UnityValue>,
        output: IndexMap<String, UnityValue>,
        depth: u32,
    },
}

struct CloneEngine<'value, 'budget> {
    frames: Vec<CloneFrame<'value>>,
    accounted_frame_capacity: usize,
    budget: &'budget mut AssetLoadBudget,
}

impl<'value, 'budget> CloneEngine<'value, 'budget> {
    fn new(budget: &'budget mut AssetLoadBudget) -> Self {
        Self {
            frames: Vec::new(),
            accounted_frame_capacity: 0,
            budget,
        }
    }

    fn clone_object_root(
        &mut self,
        object: &'value IndexMap<String, UnityValue>,
    ) -> Result<IndexMap<String, UnityValue>, UnityValueCloneError> {
        self.budget.observe_depth(0)?;
        if object.is_empty() {
            return allocate_object(0, self.budget);
        }
        let child_depth = checked_child_depth(0, self.budget)?;
        let mut output = allocate_object(object.len(), self.budget)?;
        for (key, value) in object {
            let key = clone_string(key, self.budget, "Unity object field name")?;
            let value = self.clone_value(value, child_depth)?;
            output.insert(key, value);
        }
        Ok(output)
    }

    fn clone_value(
        &mut self,
        root: &'value UnityValue,
        root_depth: u32,
    ) -> Result<UnityValue, UnityValueCloneError> {
        let mut current = root;
        let mut depth = root_depth;

        loop {
            self.budget.observe_depth(depth)?;
            let mut completed = match current {
                UnityValue::Null => UnityValue::Null,
                UnityValue::Bool(value) => UnityValue::Bool(*value),
                UnityValue::Integer(value) => UnityValue::Integer(*value),
                UnityValue::Unsigned(value) => UnityValue::Unsigned(*value),
                UnityValue::Float(value) => UnityValue::Float(*value),
                UnityValue::String(value) => {
                    UnityValue::String(clone_string(value, self.budget, "Unity value string")?)
                }
                UnityValue::Bytes(value) => {
                    UnityValue::Bytes(clone_bytes(value, self.budget, "Unity value byte buffer")?)
                }
                UnityValue::Array(values) => {
                    let mut remaining = values.iter();
                    match remaining.next() {
                        None => UnityValue::Array(allocate_array(0, self.budget)?),
                        Some(first) => {
                            let child_depth = checked_child_depth(depth, self.budget)?;
                            self.reserve_frame()?;
                            let output = allocate_array(values.len(), self.budget)?;
                            self.frames.push(CloneFrame::Array {
                                remaining,
                                output,
                                depth,
                            });
                            current = first;
                            depth = child_depth;
                            continue;
                        }
                    }
                }
                UnityValue::Object(object) => {
                    let mut remaining = object.iter();
                    match remaining.next() {
                        None => UnityValue::Object(allocate_object(0, self.budget)?),
                        Some((first_key, first)) => {
                            let child_depth = checked_child_depth(depth, self.budget)?;
                            self.reserve_frame()?;
                            let output = allocate_object(object.len(), self.budget)?;
                            self.frames.push(CloneFrame::Object {
                                current_key: first_key,
                                remaining,
                                output,
                                depth,
                            });
                            current = first;
                            depth = child_depth;
                            continue;
                        }
                    }
                }
            };

            loop {
                let Some(frame) = self.frames.pop() else {
                    return Ok(completed);
                };
                match frame {
                    CloneFrame::Array {
                        mut remaining,
                        mut output,
                        depth: parent_depth,
                    } => {
                        output.push(completed);
                        if let Some(next) = remaining.next() {
                            self.frames.push(CloneFrame::Array {
                                remaining,
                                output,
                                depth: parent_depth,
                            });
                            current = next;
                            depth = checked_child_depth(parent_depth, self.budget)?;
                            break;
                        }
                        completed = UnityValue::Array(output);
                    }
                    CloneFrame::Object {
                        current_key,
                        mut remaining,
                        mut output,
                        depth: parent_depth,
                    } => {
                        let key =
                            clone_string(current_key, self.budget, "Unity object field name")?;
                        output.insert(key, completed);
                        if let Some((next_key, next)) = remaining.next() {
                            self.frames.push(CloneFrame::Object {
                                current_key: next_key,
                                remaining,
                                output,
                                depth: parent_depth,
                            });
                            current = next;
                            depth = checked_child_depth(parent_depth, self.budget)?;
                            break;
                        }
                        completed = UnityValue::Object(output);
                    }
                }
            }
        }
    }

    fn reserve_frame(&mut self) -> Result<(), UnityValueCloneError> {
        reserve_frame(
            &mut self.frames,
            &mut self.accounted_frame_capacity,
            self.budget,
        )
    }
}

fn allocate_array(
    member_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<UnityValue>, UnityValueCloneError> {
    let members = checked_member_count(member_count)?;
    let planned_bytes = allocation_bytes(vec_allocation_bytes::<UnityValue>(member_count))?;
    budget.check_members(members)?;
    budget.check_bytes(planned_bytes)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(member_count)
        .map_err(|_| UnityValueCloneError::AllocationFailed {
            allocation: "Unity value array",
            requested_bytes: planned_bytes,
        })?;
    let actual_bytes = allocation_bytes(vec_allocation_bytes::<UnityValue>(output.capacity()))?;
    charge_container(members, actual_bytes, budget)?;
    Ok(output)
}

fn allocate_object(
    member_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<IndexMap<String, UnityValue>, UnityValueCloneError> {
    let members = checked_member_count(member_count)?;
    let planned_bytes = object_allocation_bytes(member_count)?;
    budget.check_members(members)?;
    budget.check_bytes(planned_bytes)?;
    let mut output = IndexMap::new();
    output
        .try_reserve_exact(member_count)
        .map_err(|_| UnityValueCloneError::AllocationFailed {
            allocation: "Unity object field map",
            requested_bytes: planned_bytes,
        })?;
    let actual_bytes = object_allocation_bytes(output.capacity())?;
    charge_container(members, actual_bytes, budget)?;
    Ok(output)
}

fn charge_container(
    members: u64,
    bytes: u64,
    budget: &mut AssetLoadBudget,
) -> Result<(), UnityValueCloneError> {
    budget.check_members(members)?;
    budget.check_bytes(bytes)?;
    budget.consume_members(members)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn checked_member_count(member_count: usize) -> Result<u64, UnityValueCloneError> {
    u64::try_from(member_count).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "members",
        }
        .into()
    })
}

pub(crate) fn clone_string(
    value: &str,
    budget: &mut AssetLoadBudget,
    allocation: &'static str,
) -> Result<String, UnityValueCloneError> {
    let planned_bytes = allocation_bytes(string_allocation_bytes(value.len()))?;
    budget.check_bytes(planned_bytes)?;
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| UnityValueCloneError::AllocationFailed {
            allocation,
            requested_bytes: planned_bytes,
        })?;
    let actual_bytes = allocation_bytes(string_allocation_bytes(output.capacity()))?;
    budget.consume_bytes(actual_bytes)?;
    output.push_str(value);
    Ok(output)
}

fn clone_bytes(
    value: &[u8],
    budget: &mut AssetLoadBudget,
    allocation: &'static str,
) -> Result<Vec<u8>, UnityValueCloneError> {
    let planned_bytes = allocation_bytes(vec_allocation_bytes::<u8>(value.len()))?;
    budget.check_bytes(planned_bytes)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| UnityValueCloneError::AllocationFailed {
            allocation,
            requested_bytes: planned_bytes,
        })?;
    let actual_bytes = allocation_bytes(vec_allocation_bytes::<u8>(output.capacity()))?;
    budget.consume_bytes(actual_bytes)?;
    output.extend_from_slice(value);
    Ok(output)
}

fn reserve_frame<'value>(
    frames: &mut Vec<CloneFrame<'value>>,
    accounted_capacity: &mut usize,
    budget: &mut AssetLoadBudget,
) -> Result<(), UnityValueCloneError> {
    let required = frames
        .len()
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    if required <= *accounted_capacity {
        return Ok(());
    }
    let target = required
        .max(4)
        .checked_next_power_of_two()
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let additional = target
        .checked_sub(*accounted_capacity)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let planned_bytes = allocation_bytes(vec_allocation_bytes::<CloneFrame<'value>>(additional))?;
    budget.check_bytes(planned_bytes)?;
    frames
        .try_reserve_exact(
            target
                .checked_sub(frames.len())
                .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?,
        )
        .map_err(|_| UnityValueCloneError::AllocationFailed {
            allocation: "Unity value clone traversal stack",
            requested_bytes: planned_bytes,
        })?;
    let actual_capacity = frames.capacity();
    let actual_additional = actual_capacity
        .checked_sub(*accounted_capacity)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "bytes" })?;
    let actual_bytes = allocation_bytes(vec_allocation_bytes::<CloneFrame<'value>>(
        actual_additional,
    ))?;
    budget.consume_bytes(actual_bytes)?;
    *accounted_capacity = actual_capacity;
    Ok(())
}

fn checked_child_depth(depth: u32, budget: &AssetLoadBudget) -> Result<u32, UnityValueCloneError> {
    let child_depth = depth
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
    budget.check_depth(child_depth)?;
    Ok(child_depth)
}

fn object_allocation_bytes(capacity: usize) -> Result<u64, UnityValueCloneError> {
    allocation_bytes(index_map_allocation_bytes::<String, UnityValue>(capacity))
}

fn allocation_bytes(
    bytes: Result<u64, crate::allocation::AllocationSizeError>,
) -> Result<u64, UnityValueCloneError> {
    bytes.map_err(|_| BudgetError::ArithmeticOverflow { resource: "bytes" }.into())
}

impl fmt::Display for UnityValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnityValue::Null => write!(f, "null"),
            UnityValue::Bool(b) => write!(f, "{}", b),
            UnityValue::Integer(i) => write!(f, "{}", i),
            UnityValue::Unsigned(i) => write!(f, "{}", i),
            UnityValue::Float(fl) => write!(f, "{}", fl),
            UnityValue::String(s) => write!(f, "{}", s),
            UnityValue::Array(arr) => {
                write!(f, "[")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            UnityValue::Bytes(b) => write!(f, "<bytes len={}>", b.len()),
            UnityValue::Object(obj) => {
                write!(f, "{{")?;
                for (i, (key, value)) in obj.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", key, value)?;
                }
                write!(f, "}}")
            }
        }
    }
}

// Conversion implementations
impl From<bool> for UnityValue {
    fn from(b: bool) -> Self {
        UnityValue::Bool(b)
    }
}

impl From<i32> for UnityValue {
    fn from(i: i32) -> Self {
        UnityValue::Integer(i as i64)
    }
}

impl From<i64> for UnityValue {
    fn from(i: i64) -> Self {
        UnityValue::Integer(i)
    }
}

impl From<u64> for UnityValue {
    fn from(i: u64) -> Self {
        i64::try_from(i)
            .map(UnityValue::Integer)
            .unwrap_or(UnityValue::Unsigned(i))
    }
}

impl From<f32> for UnityValue {
    fn from(f: f32) -> Self {
        UnityValue::Float(f as f64)
    }
}

impl From<f64> for UnityValue {
    fn from(f: f64) -> Self {
        UnityValue::Float(f)
    }
}

impl From<String> for UnityValue {
    fn from(s: String) -> Self {
        UnityValue::String(s)
    }
}

impl From<&str> for UnityValue {
    fn from(s: &str) -> Self {
        UnityValue::String(s.to_string())
    }
}

impl From<Vec<UnityValue>> for UnityValue {
    fn from(arr: Vec<UnityValue>) -> Self {
        UnityValue::Array(arr)
    }
}

impl From<IndexMap<String, UnityValue>> for UnityValue {
    fn from(obj: IndexMap<String, UnityValue>) -> Self {
        UnityValue::Object(obj)
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::*;
    use crate::budget::AssetLoadLimits;

    fn clone_limits(max_bytes: u64, max_depth: u32, max_members: u64) -> AssetLoadLimits {
        AssetLoadLimits {
            max_bytes,
            max_depth,
            max_members,
            ..AssetLoadLimits::default()
        }
    }

    fn nested_value() -> UnityValue {
        let mut child = IndexMap::new();
        child.insert(
            "items".to_owned(),
            UnityValue::Array(vec![UnityValue::Unsigned(u64::MAX)]),
        );
        let mut root = IndexMap::new();
        root.insert("child".to_owned(), UnityValue::Object(child));
        UnityValue::Object(root)
    }

    #[test]
    fn test_unity_value_creation() {
        let val = UnityValue::String("test".to_string());
        assert_eq!(val.as_str(), Some("test"));
    }

    #[test]
    fn test_unity_value_conversions() {
        // Test various value types
        let bool_val: UnityValue = true.into();
        assert_eq!(bool_val.as_bool(), Some(true));

        let int_val: UnityValue = 42i32.into();
        assert_eq!(int_val.as_i64(), Some(42));

        let float_val: UnityValue = std::f64::consts::PI.into();
        assert_eq!(float_val.as_f64(), Some(std::f64::consts::PI));

        let string_val: UnityValue = "test".into();
        assert_eq!(string_val.as_str(), Some("test"));

        // Test null
        let null_val = UnityValue::Null;
        assert!(null_val.is_null());
    }

    #[test]
    fn test_unity_value_display() {
        let val = UnityValue::String("test".to_string());
        assert_eq!(format!("{}", val), "test");

        let val = UnityValue::Integer(42);
        assert_eq!(format!("{}", val), "42");

        let val = UnityValue::Bool(true);
        assert_eq!(format!("{}", val), "true");
    }

    #[test]
    fn unsigned_values_above_i64_max_round_trip_without_sign_loss() {
        let value = UnityValue::Unsigned(u64::MAX);

        assert_eq!(value.as_u64(), Some(u64::MAX));
        assert_eq!(format!("{value}"), u64::MAX.to_string());

        let encoded = serde_json::to_string(&value).unwrap();
        assert_eq!(encoded, u64::MAX.to_string());
        assert_eq!(serde_json::from_str::<UnityValue>(&encoded).unwrap(), value);
        assert_eq!(UnityValue::from(42_u64), UnityValue::Integer(42));
    }

    #[test]
    fn value_paths_resolve_fields_indices_and_root_without_allocation() {
        let value = nested_value();
        let root = value
            .value_at_path(&FieldPath::root())
            .expect("root path resolves");
        assert!(std::ptr::eq(root, &value));

        let path = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&path),
            Ok(&UnityValue::Unsigned(u64::MAX))
        );
    }

    #[test]
    fn value_path_errors_preserve_segment_and_stable_actual_kind() {
        let value = nested_value();
        let missing = FieldPath::root().push_field("missing").expect("valid path");
        assert_eq!(
            value.value_at_path(&missing),
            Err(ValuePathError::MissingField { segment: 0 })
        );

        let expected_object = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .and_then(|path| path.push_field("invalid"))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&expected_object),
            Err(ValuePathError::ExpectedObject {
                segment: 3,
                actual: UnityValueKind::Unsigned,
            })
        );
        assert_eq!(UnityValueKind::Unsigned.as_str(), "unsigned");
        assert_eq!(
            serde_json::to_string(&UnityValueKind::Unsigned).expect("kind serializes"),
            "\"unsigned\""
        );

        let expected_array = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&expected_array),
            Err(ValuePathError::ExpectedArray {
                segment: 1,
                actual: UnityValueKind::Object,
            })
        );

        let out_of_bounds = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(1))
            .expect("valid path");
        assert_eq!(
            value.value_at_path(&out_of_bounds),
            Err(ValuePathError::IndexOutOfBounds {
                segment: 2,
                index: 1,
                length: 1,
            })
        );
    }

    #[test]
    fn mutable_value_path_updates_the_selected_value() {
        let mut value = nested_value();
        let path = FieldPath::root()
            .push_field("child")
            .and_then(|path| path.push_field("items"))
            .and_then(|path| path.push_index(0))
            .expect("valid path");
        *value.value_at_path_mut(&path).expect("path resolves") = UnityValue::Integer(7);
        assert_eq!(value.value_at_path(&path), Ok(&UnityValue::Integer(7)));
    }

    #[test]
    fn mutable_value_path_errors_match_immutable_resolution() {
        let paths = [
            FieldPath::root().push_field("missing").expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_field("items"))
                .and_then(|path| path.push_index(0))
                .and_then(|path| path.push_field("invalid"))
                .expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_index(0))
                .expect("valid path"),
            FieldPath::root()
                .push_field("child")
                .and_then(|path| path.push_field("items"))
                .and_then(|path| path.push_index(1))
                .expect("valid path"),
        ];

        for path in paths {
            let immutable_error = nested_value()
                .value_at_path(&path)
                .expect_err("path must fail");
            let mutable_error = nested_value()
                .value_at_path_mut(&path)
                .expect_err("path must fail");
            assert_eq!(mutable_error, immutable_error);
        }
    }

    #[test]
    fn budgeted_clone_preserves_order_and_every_value_shape() {
        let mut nested = IndexMap::new();
        nested.insert("null".to_owned(), UnityValue::Null);
        nested.insert("bool".to_owned(), UnityValue::Bool(true));
        nested.insert("integer".to_owned(), UnityValue::Integer(-9));
        nested.insert("unsigned".to_owned(), UnityValue::Unsigned(u64::MAX));
        nested.insert("float".to_owned(), UnityValue::Float(3.25));
        nested.insert("string".to_owned(), UnityValue::String("value".to_owned()));
        nested.insert("bytes".to_owned(), UnityValue::Bytes(vec![0, 1, 255]));
        nested.insert(
            "array".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Array(Vec::new()),
                UnityValue::Object(IndexMap::new()),
            ]),
        );
        let source = UnityValue::Object(nested);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();

        let cloned = source.try_clone_with_budget(&mut budget).unwrap();

        assert_eq!(cloned, source);
        let source_keys = source.as_object().unwrap().keys();
        let cloned_keys = cloned.as_object().unwrap().keys();
        assert!(source_keys.eq(cloned_keys));
        assert_eq!(budget.usage().members, 10);
        assert_eq!(budget.usage().max_observed_depth, 2);
    }

    #[test]
    fn budgeted_clone_charges_scalar_backing_before_allocation() {
        let source = UnityValue::String("eight888".to_owned());
        let mut budget = AssetLoadBudget::new(clone_limits(7, 1, 1)).expect("valid limits");

        assert_eq!(
            source.try_clone_with_budget(&mut budget),
            Err(UnityValueCloneError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 7,
                requested: 8,
            }))
        );
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn budgeted_clone_preserves_float_bits() {
        for bits in [0x8000_0000_0000_0000, 0x7ff8_0000_0000_0042] {
            let source = UnityValue::Float(f64::from_bits(bits));
            let mut budget = AssetLoadBudget::new(AssetLoadLimits::default()).unwrap();

            let UnityValue::Float(cloned) = source.try_clone_with_budget(&mut budget).unwrap()
            else {
                panic!("a float clone must remain a float");
            };

            assert_eq!(cloned.to_bits(), bits);
        }
    }

    #[test]
    fn budgeted_clone_rejects_object_backings_before_reservation() {
        let mut source = IndexMap::new();
        source.insert("field".to_owned(), UnityValue::Null);
        let required = object_allocation_bytes(1).unwrap();
        let mut budget =
            AssetLoadBudget::new(clone_limits(required - 1, 1, 1)).expect("valid limits");

        assert_eq!(
            try_clone_object_with_budget(&source, &mut budget),
            Err(UnityValueCloneError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: required - 1,
                requested: required,
            }))
        );
        assert_eq!(budget.usage().bytes, 0);
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn object_budget_covers_hash_table_growth_thresholds() {
        const FIELD_COUNT: usize = 897;
        const EXPECTED_BUCKETS: usize = 2_048;
        let entry_bytes = size_of::<(usize, String, UnityValue)>() * FIELD_COUNT;
        let minimum_index_bytes = (size_of::<usize>() + 1) * EXPECTED_BUCKETS + 64;

        let accounted = object_allocation_bytes(FIELD_COUNT).unwrap();

        assert!(accounted >= u64::try_from(entry_bytes + minimum_index_bytes).unwrap());
    }

    #[test]
    fn object_allocation_charges_its_actual_reserved_capacity() {
        let mut budget = AssetLoadBudget::default();
        let object = allocate_object(897, &mut budget).unwrap();

        assert_eq!(
            budget.usage().bytes,
            object_allocation_bytes(object.capacity()).unwrap()
        );
        assert_eq!(budget.usage().members, 897);
    }

    #[test]
    fn object_clone_rejects_a_total_retained_byte_shortfall() {
        let source = UnityValue::Object(
            (0..897)
                .map(|index| {
                    (
                        format!("field_{index:04}"),
                        UnityValue::Integer(i64::from(index)),
                    )
                })
                .collect(),
        );
        let mut measured = AssetLoadBudget::default();
        let clone = source.try_clone_with_budget(&mut measured).unwrap();
        assert_eq!(clone, source);
        let required = measured.usage().bytes;
        assert!(required > object_allocation_bytes(897).unwrap());

        let mut short =
            AssetLoadBudget::new(clone_limits(required.checked_sub(1).unwrap(), 1, 897)).unwrap();
        assert!(matches!(
            source.try_clone_with_budget(&mut short),
            Err(UnityValueCloneError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn budgeted_clone_enforces_collection_member_limit() {
        let source = UnityValue::Array(vec![UnityValue::Null, UnityValue::Null]);
        let mut budget = AssetLoadBudget::new(clone_limits(1_000_000, 2, 1)).expect("valid limits");

        assert_eq!(
            source.try_clone_with_budget(&mut budget),
            Err(UnityValueCloneError::Budget(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            }))
        );
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn budgeted_clone_walks_deep_values_iteratively_at_the_exact_limit() {
        const DEPTH: u32 = 256;
        let mut source = UnityValue::Integer(42);
        for _ in 0..DEPTH {
            source = UnityValue::Array(vec![source]);
        }
        let mut budget =
            AssetLoadBudget::new(clone_limits(16 * 1024 * 1024, DEPTH, u64::from(DEPTH)))
                .expect("valid limits");

        let cloned = source.try_clone_with_budget(&mut budget).unwrap();

        let mut current = &cloned;
        for _ in 0..DEPTH {
            let UnityValue::Array(values) = current else {
                panic!("every intermediate value must remain an array");
            };
            assert_eq!(values.len(), 1);
            current = &values[0];
        }
        assert_eq!(current, &UnityValue::Integer(42));
        assert_eq!(budget.usage().members, u64::from(DEPTH));
        assert_eq!(budget.usage().max_observed_depth, DEPTH);

        let mut shallow_budget =
            AssetLoadBudget::new(clone_limits(16 * 1024 * 1024, DEPTH - 1, u64::from(DEPTH)))
                .expect("valid limits");
        assert_eq!(
            source.try_clone_with_budget(&mut shallow_budget),
            Err(UnityValueCloneError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: u64::from(DEPTH - 1),
                requested: u64::from(DEPTH),
            }))
        );
    }
}
