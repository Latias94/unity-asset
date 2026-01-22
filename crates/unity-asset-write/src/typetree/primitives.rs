use crate::Result;
use crate::binary_writer::BinaryWriter;
use unity_asset_core::{UnityAssetError, UnityValue};

pub fn write_primitive(
    writer: &mut BinaryWriter,
    type_name: &str,
    value: &UnityValue,
) -> Result<bool> {
    match type_name {
        "SInt8" => {
            writer.write_i8(as_i64(value, type_name)? as i8);
            Ok(true)
        }
        "UInt8" | "char" => {
            writer.write_u8(as_i64(value, type_name)? as u8);
            Ok(true)
        }
        "short" | "SInt16" => {
            writer.write_i16(as_i64(value, type_name)? as i16);
            Ok(true)
        }
        "unsigned short" | "UInt16" => {
            writer.write_u16(as_i64(value, type_name)? as u16);
            Ok(true)
        }
        "int" | "SInt32" => {
            writer.write_i32(as_i64(value, type_name)? as i32);
            Ok(true)
        }
        "unsigned int" | "UInt32" | "Type*" => {
            writer.write_u32(as_i64(value, type_name)? as u32);
            Ok(true)
        }
        "long long" | "SInt64" => {
            writer.write_i64(as_i64(value, type_name)?);
            Ok(true)
        }
        "unsigned long long" | "UInt64" | "FileSize" => {
            writer.write_u64(as_u64(value, type_name)?);
            Ok(true)
        }
        "float" => {
            let f = as_f64(value, type_name)? as f32;
            writer.write_f32(f);
            Ok(true)
        }
        "double" => {
            let f = as_f64(value, type_name)?;
            writer.write_f64(f);
            Ok(true)
        }
        "bool" => {
            let b = match value {
                UnityValue::Bool(v) => *v,
                UnityValue::Integer(v) => *v != 0,
                _ => {
                    return Err(UnityAssetError::format(format!(
                        "TypeTree write type mismatch: expected bool-like for {}, got {:?}",
                        type_name, value
                    )));
                }
            };
            writer.write_bool(b);
            Ok(true)
        }
        "string" => {
            let s = match value {
                UnityValue::String(v) => v.as_str(),
                _ => {
                    return Err(UnityAssetError::format(format!(
                        "TypeTree write type mismatch: expected string for {}, got {:?}",
                        type_name, value
                    )));
                }
            };
            writer.write_aligned_string(s)?;
            Ok(true)
        }
        "TypelessData" => {
            let bytes = match value {
                UnityValue::Bytes(v) => v.as_slice(),
                _ => {
                    return Err(UnityAssetError::format(format!(
                        "TypeTree write type mismatch: expected bytes for {}, got {:?}",
                        type_name, value
                    )));
                }
            };
            writer.write_byte_array(bytes)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn as_i64(v: &UnityValue, type_name: &str) -> Result<i64> {
    match v {
        UnityValue::Integer(n) => Ok(*n),
        UnityValue::Bool(b) => Ok(if *b { 1 } else { 0 }),
        _ => Err(UnityAssetError::format(format!(
            "TypeTree write type mismatch: expected integer-like for {}, got {:?}",
            type_name, v
        ))),
    }
}

fn as_u64(v: &UnityValue, type_name: &str) -> Result<u64> {
    let n = as_i64(v, type_name)?;
    u64::try_from(n).map_err(|_| {
        UnityAssetError::format(format!(
            "TypeTree write out of range: expected unsigned for {}, got {}",
            type_name, n
        ))
    })
}

fn as_f64(v: &UnityValue, type_name: &str) -> Result<f64> {
    match v {
        UnityValue::Float(f) => Ok(*f),
        UnityValue::Integer(n) => Ok(*n as f64),
        _ => Err(UnityAssetError::format(format!(
            "TypeTree write type mismatch: expected float-like for {}, got {:?}",
            type_name, v
        ))),
    }
}
