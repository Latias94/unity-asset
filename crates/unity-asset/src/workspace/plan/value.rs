use std::fmt;
use std::fmt::Write as _;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{MutationPlanError, ReferenceTarget};

const MAX_FIELD_NAME_BYTES: usize = 64 * 1024;
const MAX_VALUE_STRING_BYTES: usize = 64 * 1024 * 1024;

/// Exact IEEE-754 binary64 bits used by persisted mutation values.
///
/// Encoding floats by their bits preserves signed zero and every NaN payload without relying on
/// JSON's incomplete floating-point value space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Float64Bits(u64);

impl Float64Bits {
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        Self(value.to_bits())
    }

    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn to_f64(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl Serialize for Float64Bits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&FixedHexU64(self.0))
    }
}

impl<'de> Deserialize<'de> for Float64Bits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FloatBitsVisitor;

        impl Visitor<'_> for FloatBitsVisitor {
            type Value = Float64Bits;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("exactly 16 lowercase hexadecimal digits")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() != 16 || !value.bytes().all(is_lower_hex) {
                    return Err(E::invalid_value(serde::de::Unexpected::Str(value), &self));
                }
                u64::from_str_radix(value, 16)
                    .map(Float64Bits)
                    .map_err(E::custom)
            }
        }

        deserializer.deserialize_str(FloatBitsVisitor)
    }
}

/// Canonically encoded byte string used by mutation values and plan payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanBytes(Vec<u8>);

impl PlanBytes {
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for PlanBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl Serialize for PlanBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&HexBytes(&self.0))
    }
}

impl<'de> Deserialize<'de> for PlanBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BytesVisitor;

        impl Visitor<'_> for BytesVisitor {
            type Value = PlanBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an even-length lowercase hexadecimal byte string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() & 1 != 0 || !value.bytes().all(is_lower_hex) {
                    return Err(E::invalid_value(serde::de::Unexpected::Str(value), &self));
                }

                let byte_len = value.len() / 2;
                let mut decoded = Vec::new();
                decoded
                    .try_reserve_exact(byte_len)
                    .map_err(|error| E::custom(format!("failed to reserve plan bytes: {error}")))?;
                for pair in value.as_bytes().chunks_exact(2) {
                    let high = hex_nibble(pair[0])
                        .ok_or_else(|| E::custom("invalid hexadecimal plan byte"))?;
                    let low = hex_nibble(pair[1])
                        .ok_or_else(|| E::custom("invalid hexadecimal plan byte"))?;
                    decoded.push((high << 4) | low);
                }
                Ok(PlanBytes(decoded))
            }
        }

        deserializer.deserialize_str(BytesVisitor)
    }
}

/// One named field in a canonical mutation object value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MutationField {
    name: String,
    value: MutationValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationFieldWire {
    name: String,
    value: MutationValue,
}

impl<'de> Deserialize<'de> for MutationField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MutationFieldWire::deserialize(deserializer)?;
        Self::new(wire.name, wire.value).map_err(serde::de::Error::custom)
    }
}

impl MutationField {
    pub fn new(name: impl Into<String>, value: MutationValue) -> Result<Self, MutationPlanError> {
        let name = name.into();
        validate_field_name(&name)?;
        Ok(Self { name, value })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn value(&self) -> &MutationValue {
        &self.value
    }

    #[must_use]
    pub(crate) fn into_parts(self) -> (String, MutationValue) {
        (self.name, self.value)
    }
}

/// Typed semantic value stored in a Mutation Plan.
///
/// Every variant has an explicit wire tag. Object fields are sorted by name and duplicate names
/// are rejected, while arrays retain their exact order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationValue {
    kind: MutationValueKind,
    depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MutationValueKind {
    Null,
    Bool { value: bool },
    Signed { value: i64 },
    Unsigned { value: u64 },
    Float64 { bits: Float64Bits },
    String { value: String },
    Bytes { value: PlanBytes },
    Reference { target: ReferenceTarget },
    Array { values: Vec<MutationValue> },
    Object { fields: Vec<MutationField> },
}

/// Borrowed view of a validated mutation value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationValueRef<'value> {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float64(Float64Bits),
    String(&'value str),
    Bytes(&'value PlanBytes),
    Reference(&'value ReferenceTarget),
    Array(&'value [MutationValue]),
    Object(&'value [MutationField]),
}

/// Owned view of a validated mutation value for crate-internal interpreters.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MutationValueOwned {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float64(Float64Bits),
    String(String),
    Bytes(PlanBytes),
    Reference(ReferenceTarget),
    Array(Vec<MutationValue>),
    Object(Vec<MutationField>),
}

impl Serialize for MutationValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.kind.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MutationValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_kind(MutationValueKind::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

impl MutationValue {
    #[must_use]
    pub const fn null() -> Self {
        Self {
            kind: MutationValueKind::Null,
            depth: 1,
        }
    }

    #[must_use]
    pub const fn bool(value: bool) -> Self {
        Self {
            kind: MutationValueKind::Bool { value },
            depth: 1,
        }
    }

    #[must_use]
    pub const fn signed(value: i64) -> Self {
        Self {
            kind: MutationValueKind::Signed { value },
            depth: 1,
        }
    }

    #[must_use]
    pub const fn unsigned(value: u64) -> Self {
        Self {
            kind: MutationValueKind::Unsigned { value },
            depth: 1,
        }
    }

    #[must_use]
    pub fn float64(value: f64) -> Self {
        Self {
            kind: MutationValueKind::Float64 {
                bits: Float64Bits::from_f64(value),
            },
            depth: 1,
        }
    }

    pub fn string(value: impl Into<String>) -> Result<Self, MutationPlanError> {
        let value = value.into();
        validate_string(&value)?;
        Ok(Self {
            kind: MutationValueKind::String { value },
            depth: 1,
        })
    }

    pub(crate) fn validate_string_value(value: &str) -> Result<(), MutationPlanError> {
        validate_string(value)
    }

    #[must_use]
    pub fn bytes(value: impl Into<PlanBytes>) -> Self {
        Self {
            kind: MutationValueKind::Bytes {
                value: value.into(),
            },
            depth: 1,
        }
    }

    /// Stores a logical object reference inside a larger semantic replacement.
    ///
    /// This is used for schema-bound structures such as hierarchy child arrays and UnityEvent
    /// calls. Format adapters resolve the logical target to binary or YAML pointer spelling during
    /// prepare; recipes never persist raw file IDs.
    #[must_use]
    pub const fn reference(target: ReferenceTarget) -> Self {
        Self {
            kind: MutationValueKind::Reference { target },
            depth: 1,
        }
    }

    pub fn array(values: Vec<Self>) -> Result<Self, MutationPlanError> {
        Self::from_kind(MutationValueKind::Array { values })
    }

    pub fn object(fields: Vec<MutationField>) -> Result<Self, MutationPlanError> {
        Self::from_kind(MutationValueKind::Object { fields })
    }

    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    #[must_use]
    pub fn view(&self) -> MutationValueRef<'_> {
        match &self.kind {
            MutationValueKind::Null => MutationValueRef::Null,
            MutationValueKind::Bool { value } => MutationValueRef::Bool(*value),
            MutationValueKind::Signed { value } => MutationValueRef::Signed(*value),
            MutationValueKind::Unsigned { value } => MutationValueRef::Unsigned(*value),
            MutationValueKind::Float64 { bits } => MutationValueRef::Float64(*bits),
            MutationValueKind::String { value } => MutationValueRef::String(value),
            MutationValueKind::Bytes { value } => MutationValueRef::Bytes(value),
            MutationValueKind::Reference { target } => MutationValueRef::Reference(target),
            MutationValueKind::Array { values } => MutationValueRef::Array(values),
            MutationValueKind::Object { fields } => MutationValueRef::Object(fields),
        }
    }

    #[must_use]
    pub(crate) fn into_owned(self) -> MutationValueOwned {
        match self.kind {
            MutationValueKind::Null => MutationValueOwned::Null,
            MutationValueKind::Bool { value } => MutationValueOwned::Bool(value),
            MutationValueKind::Signed { value } => MutationValueOwned::Signed(value),
            MutationValueKind::Unsigned { value } => MutationValueOwned::Unsigned(value),
            MutationValueKind::Float64 { bits } => MutationValueOwned::Float64(bits),
            MutationValueKind::String { value } => MutationValueOwned::String(value),
            MutationValueKind::Bytes { value } => MutationValueOwned::Bytes(value),
            MutationValueKind::Reference { target } => MutationValueOwned::Reference(target),
            MutationValueKind::Array { values } => MutationValueOwned::Array(values),
            MutationValueKind::Object { fields } => MutationValueOwned::Object(fields),
        }
    }

    fn from_kind(mut kind: MutationValueKind) -> Result<Self, MutationPlanError> {
        let depth = match &mut kind {
            MutationValueKind::String { value } => {
                validate_string(value)?;
                1
            }
            MutationValueKind::Array { values } => collection_depth(
                values.iter().map(|value| value.depth),
                super::MAX_PLAN_DEPTH,
            )?,
            MutationValueKind::Object { fields } => {
                for field in fields.iter() {
                    validate_field_name(&field.name)?;
                }
                fields.sort_unstable_by(|left, right| left.name.cmp(&right.name));
                if let Some(duplicate) = fields.windows(2).find(|pair| pair[0].name == pair[1].name)
                {
                    return Err(MutationPlanError::DuplicateObjectField(
                        duplicate[0].name.clone(),
                    ));
                }
                collection_depth(
                    fields.iter().map(|field| field.value.depth),
                    super::MAX_PLAN_DEPTH,
                )?
            }
            MutationValueKind::Null
            | MutationValueKind::Bool { .. }
            | MutationValueKind::Signed { .. }
            | MutationValueKind::Unsigned { .. }
            | MutationValueKind::Float64 { .. }
            | MutationValueKind::Bytes { .. }
            | MutationValueKind::Reference { .. } => 1,
        };
        Ok(Self { kind, depth })
    }
}

fn collection_depth(
    children: impl Iterator<Item = u32>,
    maximum: u32,
) -> Result<u32, MutationPlanError> {
    let child_depth = children.max().unwrap_or(0);
    let actual = child_depth
        .checked_add(1)
        .ok_or(MutationPlanError::ValueDepthOverflow)?;
    if actual > maximum {
        return Err(MutationPlanError::ValueDepthExceeded { maximum, actual });
    }
    Ok(actual)
}

fn validate_field_name(name: &str) -> Result<(), MutationPlanError> {
    if name.is_empty() || name.len() > MAX_FIELD_NAME_BYTES || name.contains('\0') {
        return Err(MutationPlanError::InvalidObjectFieldName(name.to_owned()));
    }
    Ok(())
}

fn validate_string(value: &str) -> Result<(), MutationPlanError> {
    if value.len() > MAX_VALUE_STRING_BYTES {
        return Err(MutationPlanError::ValueStringTooLong {
            actual: value.len(),
            maximum: MAX_VALUE_STRING_BYTES,
        });
    }
    Ok(())
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

struct FixedHexU64(u64);

impl fmt::Display for FixedHexU64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

struct HexBytes<'a>(&'a [u8]);

impl fmt::Display for HexBytes<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            formatter.write_char(char::from(DIGITS[usize::from(byte >> 4)]))?;
            formatter.write_char(char::from(DIGITS[usize::from(byte & 0x0f)]))?;
        }
        Ok(())
    }
}
