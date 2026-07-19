use std::fmt;

use unity_asset_binary::typetree::{PrimitiveKind, SchemaNode};
use unity_asset_core::{Result, UnityAssetError, UnityValue};

use super::output::TypeTreeSink;
use crate::binary_writer::Endian;

const BULK_STACK_BYTES: usize = 4 * 1024;

pub(crate) struct UnityValueSummary<'value>(&'value UnityValue);

impl fmt::Display for UnityValueSummary<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            UnityValue::Null => formatter.write_str("Null"),
            UnityValue::Bool(_) => formatter.write_str("Bool"),
            UnityValue::Integer(_) => formatter.write_str("Integer"),
            UnityValue::Unsigned(_) => formatter.write_str("Unsigned"),
            UnityValue::Float(_) => formatter.write_str("Float"),
            UnityValue::String(value) => write!(formatter, "String(bytes={})", value.len()),
            UnityValue::Array(value) => write!(formatter, "Array(len={})", value.len()),
            UnityValue::Bytes(value) => write!(formatter, "Bytes(len={})", value.len()),
            UnityValue::Object(value) => write!(formatter, "Object(fields={})", value.len()),
        }
    }
}

pub(crate) const fn summarize_value(value: &UnityValue) -> UnityValueSummary<'_> {
    UnityValueSummary(value)
}

pub(crate) fn checked_i32_length(length: usize, label: &str) -> Result<i32> {
    i32::try_from(length)
        .map_err(|_| UnityAssetError::format(format!("{label} length exceeds i32: {length}")))
}

pub(crate) fn usize_to_u64(value: usize, label: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| UnityAssetError::format(format!("{label} does not fit u64: {value}")))
}

pub(crate) fn expect_pair<'value>(
    node: SchemaNode<'_>,
    value: &'value UnityValue,
) -> Result<&'value [UnityValue]> {
    match value {
        UnityValue::Array(values) if values.len() == 2 => Ok(values),
        _ => Err(UnityAssetError::format(format!(
            "TypeTree pair '{}' requires an Array with exactly two values, got {}",
            node.name(),
            summarize_value(value)
        ))),
    }
}

pub(crate) fn write_primitive<S: TypeTreeSink + ?Sized>(
    output: &mut S,
    kind: PrimitiveKind,
    value: &UnityValue,
    endian: Endian,
) -> Result<()> {
    let (bytes, width) = encode_primitive(kind, value, endian)?;
    output.write_scalar_bytes(&bytes[..width])
}

pub(crate) fn write_primitive_run<S: TypeTreeSink + ?Sized>(
    output: &mut S,
    kind: PrimitiveKind,
    values: &[UnityValue],
    endian: Endian,
) -> Result<()> {
    let width = usize::from(kind.width());
    let values_per_chunk = BULK_STACK_BYTES / width;
    let mut chunk = [0_u8; BULK_STACK_BYTES];

    for values in values.chunks(values_per_chunk) {
        let mut used = 0;
        for value in values {
            let (encoded, encoded_width) = encode_primitive(kind, value, endian)?;
            debug_assert_eq!(encoded_width, width);
            let end = used + encoded_width;
            chunk[used..end].copy_from_slice(&encoded[..encoded_width]);
            used = end;
        }
        output.write_bulk_bytes(&chunk[..used])?;
    }

    Ok(())
}

fn encode_primitive(
    kind: PrimitiveKind,
    value: &UnityValue,
    endian: Endian,
) -> Result<([u8; 8], usize)> {
    let mut encoded = [0_u8; 8];
    let width = usize::from(kind.width());

    match kind {
        PrimitiveKind::Bool => {
            let value = match value {
                UnityValue::Bool(value) => *value,
                _ => return Err(type_mismatch(kind, "bool", value)),
            };
            encoded[0] = u8::from(value);
        }
        PrimitiveKind::I8 => {
            let value =
                i8::try_from(as_i64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
            encoded[0] = value.to_ne_bytes()[0];
        }
        PrimitiveKind::U8 => {
            encoded[0] =
                u8::try_from(as_u64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
        }
        PrimitiveKind::I16 => {
            let value =
                i16::try_from(as_i64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::U16 => {
            let value =
                u16::try_from(as_u64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::I32 => {
            let value =
                i32::try_from(as_i64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::U32 => {
            let value =
                u32::try_from(as_u64(kind, value)?).map_err(|_| out_of_range(kind, value))?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::I64 => {
            let value = as_i64(kind, value)?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::U64 => {
            let value = as_u64(kind, value)?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
        PrimitiveKind::F32 => {
            let source = as_f64(kind, value)?;
            let converted = source as f32;
            if source.is_finite() && !converted.is_finite() {
                return Err(out_of_range(kind, value));
            }
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => converted.to_le_bytes(),
                Endian::Big => converted.to_be_bytes(),
            });
        }
        PrimitiveKind::F64 => {
            let value = as_f64(kind, value)?;
            encoded[..width].copy_from_slice(&match endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            });
        }
    }

    Ok((encoded, width))
}

fn as_i64(kind: PrimitiveKind, input: &UnityValue) -> Result<i64> {
    if !matches!(input, UnityValue::Integer(_) | UnityValue::Unsigned(_)) {
        return Err(type_mismatch(kind, "integer", input));
    }
    input.as_i64().ok_or_else(|| out_of_range(kind, input))
}

fn as_u64(kind: PrimitiveKind, input: &UnityValue) -> Result<u64> {
    if !matches!(input, UnityValue::Integer(_) | UnityValue::Unsigned(_)) {
        return Err(type_mismatch(kind, "unsigned integer", input));
    }
    input.as_u64().ok_or_else(|| out_of_range(kind, input))
}

fn as_f64(kind: PrimitiveKind, value: &UnityValue) -> Result<f64> {
    if !matches!(
        value,
        UnityValue::Float(_) | UnityValue::Integer(_) | UnityValue::Unsigned(_)
    ) {
        return Err(type_mismatch(kind, "number", value));
    }
    value.as_f64().ok_or_else(|| out_of_range(kind, value))
}

fn type_mismatch(
    kind: PrimitiveKind,
    expected: &'static str,
    value: &UnityValue,
) -> UnityAssetError {
    UnityAssetError::format(format!(
        "TypeTree write expected {expected} for {kind:?}, got {}",
        summarize_value(value)
    ))
}

fn out_of_range(kind: PrimitiveKind, value: &UnityValue) -> UnityAssetError {
    UnityAssetError::format(format!(
        "TypeTree write {} is out of range for {kind:?}",
        summarize_value(value)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_accessors_distinguish_type_mismatch_from_range_failure() {
        let text = UnityValue::String("1".to_owned());
        assert!(
            as_i64(PrimitiveKind::I64, &text)
                .unwrap_err()
                .to_string()
                .contains("expected integer")
        );
        assert!(
            as_u64(PrimitiveKind::U64, &text)
                .unwrap_err()
                .to_string()
                .contains("expected unsigned integer")
        );
        assert!(
            as_f64(PrimitiveKind::F64, &text)
                .unwrap_err()
                .to_string()
                .contains("expected number")
        );

        assert!(
            as_i64(PrimitiveKind::I64, &UnityValue::Unsigned(u64::MAX))
                .unwrap_err()
                .to_string()
                .contains("out of range")
        );
        assert!(
            as_u64(PrimitiveKind::U64, &UnityValue::Integer(-1))
                .unwrap_err()
                .to_string()
                .contains("out of range")
        );
    }
}
