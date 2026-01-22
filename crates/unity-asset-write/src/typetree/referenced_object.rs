use super::context::TypeTreeWriteContext;
use super::writer::write_value;
use crate::Result;
use indexmap::IndexMap;
use unity_asset_binary::asset::SerializedType;
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};
use unity_asset_core::{UnityAssetError, UnityValue};

pub(crate) fn write_referenced_object(
    referenced_object: &IndexMap<String, UnityValue>,
    writer: &mut crate::BinaryWriter,
    node: &TypeTreeNode,
    ctx: &mut TypeTreeWriteContext<'_>,
    options: super::writer::TypeTreeWriteOptions,
) -> Result<()> {
    for child in &node.children {
        if child.type_name == "ManagedReferencesRegistry" {
            if ctx.has_managed_registry {
                continue;
            }
            ctx.has_managed_registry = true;
        }

        if child.type_name == "ReferencedObjectData" {
            let Some(value) = referenced_object.get(&child.name) else {
                if options.allow_missing_fields {
                    continue;
                }
                return Err(UnityAssetError::format(format!(
                    "Missing field '{}' for ReferencedObject write",
                    child.name
                )));
            };

            if let Some(ref_types) = ctx.ref_types
                && let Some((class, ns, asm)) = referenced_type_triplet(referenced_object)
                && let Some(tree) = resolve_ref_type_tree_triplet(&class, &ns, &asm, ref_types)
                && let Some(root) = tree.nodes.first()
            {
                let payload = value.as_object().ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "TypeTree write type mismatch: expected object payload for '{}', got {:?}",
                        child.name, value
                    ))
                })?;

                // Write the typed payload layout (UnityPy: get_ref_type_node + write_value on it).
                for field in &root.children {
                    if field.name.is_empty() {
                        continue;
                    }
                    let Some(v) = payload.get(&field.name) else {
                        if options.allow_missing_fields {
                            continue;
                        }
                        return Err(UnityAssetError::format(format!(
                            "Missing referenced field '{}' for ReferencedObjectData write",
                            field.name
                        )));
                    };
                    write_value(writer, field, v, ctx, options)?;
                }

                if child.is_aligned() {
                    writer.align_stream(4);
                }
                continue;
            }

            // Fallback: write bytes according to the placeholder node.
            write_value(writer, child, value, ctx, options)?;
            continue;
        }

        if child.name.is_empty() {
            continue;
        }
        let Some(value) = referenced_object.get(&child.name) else {
            if options.allow_missing_fields {
                continue;
            }
            return Err(UnityAssetError::format(format!(
                "Missing field '{}' for ReferencedObject write",
                child.name
            )));
        };
        write_value(writer, child, value, ctx, options)?;
    }

    if node.is_aligned() {
        writer.align_stream(4);
    }
    Ok(())
}

fn resolve_ref_type_tree_triplet<'a>(
    class: &str,
    ns: &str,
    asm: &str,
    ref_types: &'a [SerializedType],
) -> Option<&'a TypeTree> {
    if class.is_empty() {
        return None;
    }
    ref_types.iter().find_map(|t| {
        if !t.class_name.is_empty()
            && t.class_name == class
            && t.namespace == ns
            && t.assembly_name == asm
            && !t.type_tree.is_empty()
        {
            Some(&t.type_tree)
        } else {
            None
        }
    })
}

fn referenced_type_triplet(
    referenced_object: &IndexMap<String, UnityValue>,
) -> Option<(String, String, String)> {
    // Our parser produces a `type` object with keys `class`, `ns`, `asm`.
    // Also accept UnityPy-like naming variants for robustness.
    let type_obj = referenced_object.get("type")?.as_object()?;
    let class = get_str_ci(type_obj, &["class", "m_ClassName"])?;
    let ns = get_str_ci(type_obj, &["ns", "m_NameSpace"]).unwrap_or_default();
    let asm = get_str_ci(type_obj, &["asm", "m_AssemblyName"]).unwrap_or_default();
    Some((class.to_string(), ns.to_string(), asm.to_string()))
}

fn get_str_ci<'a>(obj: &'a IndexMap<String, UnityValue>, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str()) {
            return Some(v);
        }
    }
    // Also attempt a case-insensitive fallback (Unity data is inconsistent across versions/tools).
    for (k, v) in obj.iter() {
        for want in keys {
            if k.eq_ignore_ascii_case(want) {
                if let Some(s) = v.as_str() {
                    return Some(s);
                }
            }
        }
    }
    None
}
