//! Read streamed resource bytes (`m_Resource` / `m_StreamData`) using Environment.
//!
//! This is a library-API recipe for `Environment::read_stream_data_source`.
//!
//! Run:
//! `cargo run -p unity-asset --example env_read_stream_data -- <path> [path_id]`
//!
//! - If `path_id` is provided, this example uses that object.
//! - Otherwise, it scans for the first object that contains stream metadata.

use std::path::PathBuf;
use unity_asset::UnityValue;
use unity_asset::environment::{BinaryObjectKey, Environment};

#[derive(Debug, Clone)]
struct StreamDescriptor {
    path: String,
    offset: u64,
    size: u32,
}

fn extract_stream(obj: &unity_asset_binary::object::UnityObject) -> Option<StreamDescriptor> {
    fn as_u64(v: &UnityValue) -> Option<u64> {
        v.as_i64().and_then(|n| u64::try_from(n).ok())
    }
    fn as_u32(v: &UnityValue) -> Option<u32> {
        v.as_i64().and_then(|n| u32::try_from(n).ok())
    }

    if let Some(UnityValue::Object(res)) = obj.get("m_Resource") {
        let path = res
            .get("m_Source")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let offset = res.get("m_Offset").and_then(as_u64).unwrap_or(0);
        let size = res.get("m_Size").and_then(as_u32).unwrap_or(0);
        if !path.is_empty() && size > 0 {
            return Some(StreamDescriptor { path, offset, size });
        }
    }

    if let Some(UnityValue::Object(stream)) = obj.get("m_StreamData") {
        let path = stream
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let offset = stream.get("offset").and_then(as_u64).unwrap_or(0);
        let size = stream.get("size").and_then(as_u32).unwrap_or(0);
        if !path.is_empty() && size > 0 {
            return Some(StreamDescriptor { path, offset, size });
        }
    }

    None
}

fn first_streamed_object_key(env: &Environment) -> Option<BinaryObjectKey> {
    let preferred_class_ids = [83, 28];
    for class_id in preferred_class_ids {
        for obj_ref in env
            .binary_object_infos()
            .filter(|r| r.object.class_id() == class_id)
        {
            let key = obj_ref.key();
            if let Ok(obj) = env.read_binary_object_key(&key)
                && extract_stream(&obj).is_some()
            {
                return Some(key);
            }
        }
    }

    for obj_ref in env.binary_object_infos() {
        let key = obj_ref.key();
        if let Ok(obj) = env.read_binary_object_key(&key)
            && extract_stream(&obj).is_some()
        {
            return Some(key);
        }
    }

    None
}

fn main() -> unity_asset::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples/char_118_yuki.ab"));
    let path_id = std::env::args().nth(2).and_then(|s| s.parse::<i64>().ok());

    let mut env = Environment::new();
    env.load(&path)?;

    let key = if let Some(path_id) = path_id {
        env.find_binary_object_keys(path_id)
            .into_iter()
            .next()
            .ok_or_else(|| {
                unity_asset::UnityAssetError::format(format!("Object not found: path_id={path_id}"))
            })?
    } else {
        first_streamed_object_key(&env).ok_or_else(|| {
            unity_asset::UnityAssetError::format("No streamed objects found in this input")
        })?
    };

    let obj = env.read_binary_object_key(&key)?;
    let Some(stream) = extract_stream(&obj) else {
        return Err(unity_asset::UnityAssetError::format(
            "Object does not contain stream metadata (m_Resource/m_StreamData)",
        ));
    };

    let preview_size: u32 = stream.size.min(64);
    let bytes = env.read_stream_data_source(
        &key.source,
        key.source_kind,
        &stream.path,
        stream.offset,
        preview_size,
    )?;

    print!("preview ({} bytes):", bytes.len());
    for b in &bytes {
        print!(" {:02x}", b);
    }
    println!();

    println!("source: {}", key.source.describe());
    println!("source_kind: {:?}", key.source_kind);
    println!("path_id: {}", key.path_id);
    println!("class_id: {}", obj.class_id());
    println!("name: {:?}", obj.name());
    println!("stream_path: {}", stream.path);
    println!("stream_offset: {}", stream.offset);
    println!("stream_size: {}", stream.size);

    Ok(())
}
