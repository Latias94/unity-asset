use crate::shared::{AppContext, build_environment, load_environment_input, resolve_loaded_source};
use anyhow::Result;
use std::path::PathBuf;
use unity_asset::UnityValue;
use unity_asset::environment::BinarySource;

pub(crate) fn run(
    input: PathBuf,
    key: Option<String>,
    source: Option<PathBuf>,
    kind: String,
    asset_index: Option<usize>,
    path_id: Option<i64>,
    max_depth: usize,
    max_items: usize,
    max_array: usize,
    filter: String,
    ctx: &AppContext,
) -> Result<()> {
    let mut env = build_environment(ctx.strict, ctx.show_warnings, ctx.typetree_registries())?;
    load_environment_input(&mut env, &input)?;

    let mut key = if let Some(key) = key {
        key.parse::<unity_asset::environment::BinaryObjectKey>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        let kind_lc = kind.to_ascii_lowercase();
        let source_kind = match kind_lc.as_str() {
            "bundle" => unity_asset::environment::BinarySourceKind::AssetBundle,
            "serialized" => unity_asset::environment::BinarySourceKind::SerializedFile,
            other => anyhow::bail!("Unknown --kind: {} (expected: bundle|serialized)", other),
        };

        if source_kind == unity_asset::environment::BinarySourceKind::AssetBundle
            && asset_index.is_none()
        {
            anyhow::bail!("--asset-index is required when --kind bundle");
        }

        let path_id = path_id
            .ok_or_else(|| anyhow::anyhow!("--path-id is required unless --key is provided"))?;
        let source = match source {
            Some(source) => source,
            None if input.is_file() => input.clone(),
            None => anyhow::bail!("--source is required unless --key is provided"),
        };

        unity_asset::environment::BinaryObjectKey {
            source: BinarySource::path(&source),
            source_kind,
            asset_index,
            path_id,
        }
    };

    let resolved_source = resolve_loaded_source(&env, key.source_kind, &key.source)?;
    key.source = resolved_source.clone();

    let obj = env.read_binary_object_key(&key)?;

    println!(
        "Object: {} (class_id={}, byte_size={}, byte_start={}, byte_order={:?})",
        obj.describe(),
        obj.class_id(),
        obj.byte_size(),
        obj.byte_start(),
        obj.byte_order()
    );
    println!(
        "Source: {} (kind={:?}, asset_index={:?}, path_id={})",
        resolved_source, key.source_kind, key.asset_index, key.path_id
    );
    println!("Key: {}", key);

    let filter_lc = filter.to_ascii_lowercase();
    let mut printed = 0usize;

    let mut names: Vec<_> = obj.as_unity_class().properties().keys().collect();
    names.sort();
    println!("Properties: {}", names.len());

    for name in names {
        let Some(value) = obj.as_unity_class().get(name.as_str()) else {
            continue;
        };
        print_unity_value_tree(
            name,
            value,
            0,
            max_depth,
            max_items,
            max_array,
            &filter_lc,
            &mut printed,
        );
        if printed >= max_items {
            println!("... (truncated: max_items={})", max_items);
            break;
        }
    }

    Ok(())
}

fn print_unity_value_tree(
    path: &str,
    value: &UnityValue,
    depth: usize,
    max_depth: usize,
    max_items: usize,
    max_array: usize,
    filter_lc: &str,
    printed: &mut usize,
) {
    if *printed >= max_items {
        return;
    }

    let path_lc = path.to_ascii_lowercase();
    if !filter_lc.is_empty() && !path_lc.contains(filter_lc) {
        match value {
            UnityValue::Array(arr) if depth < max_depth => {
                for (i, item) in arr.iter().take(max_array).enumerate() {
                    let child_path = format!("{}[{}]", path, i);
                    print_unity_value_tree(
                        &child_path,
                        item,
                        depth + 1,
                        max_depth,
                        max_items,
                        max_array,
                        filter_lc,
                        printed,
                    );
                    if *printed >= max_items {
                        break;
                    }
                }
            }
            UnityValue::Object(obj) if depth < max_depth => {
                for (k, v) in obj.iter() {
                    let child_path = format!("{}.{}", path, k);
                    print_unity_value_tree(
                        &child_path,
                        v,
                        depth + 1,
                        max_depth,
                        max_items,
                        max_array,
                        filter_lc,
                        printed,
                    );
                    if *printed >= max_items {
                        break;
                    }
                }
            }
            _ => {}
        }
        return;
    }

    let indent = "  ".repeat(depth);
    match value {
        UnityValue::Null => {
            println!("{}{}: Null", indent, path);
            *printed += 1;
        }
        UnityValue::Bool(b) => {
            println!("{}{}: Bool({})", indent, path, b);
            *printed += 1;
        }
        UnityValue::Integer(i) => {
            println!("{}{}: Integer({})", indent, path, i);
            *printed += 1;
        }
        UnityValue::Float(f) => {
            println!("{}{}: Float({})", indent, path, f);
            *printed += 1;
        }
        UnityValue::String(s) => {
            let preview = if s.chars().count() > 200 {
                let head: String = s.chars().take(200).collect();
                format!("{}...(len={})", head, s.len())
            } else {
                s.clone()
            };
            println!("{}{}: String({:?})", indent, path, preview);
            *printed += 1;
        }
        UnityValue::Array(arr) => {
            println!("{}{}: Array(len={})", indent, path, arr.len());
            *printed += 1;
            if depth >= max_depth {
                return;
            }
            for (i, item) in arr.iter().take(max_array).enumerate() {
                let child_path = format!("{}[{}]", path, i);
                print_unity_value_tree(
                    &child_path,
                    item,
                    depth + 1,
                    max_depth,
                    max_items,
                    max_array,
                    filter_lc,
                    printed,
                );
                if *printed >= max_items {
                    return;
                }
            }
            if arr.len() > max_array {
                println!(
                    "{}  {}: ... ({} more items)",
                    indent,
                    path,
                    arr.len().saturating_sub(max_array)
                );
                *printed += 1;
            }
        }
        UnityValue::Bytes(b) => {
            let prefix_len = b.len().min(32);
            let prefix: Vec<String> = b[..prefix_len]
                .iter()
                .map(|v| format!("{:02x}", v))
                .collect();
            println!(
                "{}{}: Bytes(len={}, hex_prefix={})",
                indent,
                path,
                b.len(),
                prefix.join("")
            );
            *printed += 1;
        }
        UnityValue::Object(obj) => {
            println!("{}{}: Object(keys={})", indent, path, obj.len());
            *printed += 1;
            if depth >= max_depth {
                return;
            }
            for (k, v) in obj.iter() {
                let child_path = format!("{}.{}", path, k);
                print_unity_value_tree(
                    &child_path,
                    v,
                    depth + 1,
                    max_depth,
                    max_items,
                    max_array,
                    filter_lc,
                    printed,
                );
                if *printed >= max_items {
                    return;
                }
            }
        }
    }
}
