use std::path::{Path, PathBuf};
use std::process::Command;

use unity_asset_binary::asset::{ObjectInfo, SerializedFile};
use unity_asset_binary::bundle::AssetBundle;
use unity_asset_binary::reader::ByteOrder;
use unity_asset_binary::typetree::TypeTree;
use unity_asset_core::UnityValue;
use unity_asset_write::bundle::{BundleEdits, BundleWriter};
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};
use unity_asset_write::typetree::{TypeTreeWriteOptions, TypeTreeWriter};
use unity_asset_write::webfile::{WebFileEdits, WebFilePacker, WebFileWriter};
use unity_asset_write::{BinaryWriter, Endian, PackerOptions, UnityPyPacker};

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `.../crates/unity-asset-write`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should be two levels above unity-asset-write crate")
        .to_path_buf()
}

fn unitypy_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UNITYPY_PYTHON") {
        return Some(PathBuf::from(p));
    }

    let venv = repo_root()
        .join(".venv-unitypy")
        .join("Scripts")
        .join("python.exe");
    if venv.exists() {
        return Some(venv);
    }

    None
}

fn unitypy_check(script: &str, args: &[String]) -> anyhow::Result<()> {
    let python = unitypy_python().ok_or_else(|| {
        anyhow::anyhow!(
            "UnityPy E2E is enabled, but no python was found. Set `UNITYPY_PYTHON`, or create a venv at `{}`.",
            repo_root().join(".venv-unitypy").display()
        )
    })?;

    let out = Command::new(python)
        .arg("-c")
        .arg(script)
        .args(args)
        .output()?;

    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "UnityPy check failed (exit={:?}).\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn type_tree_for_object<'a>(file: &'a SerializedFile, info: &ObjectInfo) -> Option<&'a TypeTree> {
    if !file.enable_type_tree {
        return None;
    }

    if info.type_index >= 0 {
        let idx = info.type_index as usize;
        return file.types.get(idx).map(|t| &t.type_tree);
    }

    file.types
        .iter()
        .find(|t| t.class_id == info.type_id)
        .map(|t| &t.type_tree)
}

fn find_first_serialized_node(
    bundle: &AssetBundle,
) -> Option<&unity_asset_binary::bundle::DirectoryNode> {
    bundle
        .nodes
        .iter()
        .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
}

fn build_uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";

    let entry_table_len: usize = entries
        .iter()
        .map(|(name, _)| 12usize.saturating_add(name.len()))
        .sum();
    let header_len: usize = signature
        .len()
        .saturating_add(std::mem::size_of::<i32>())
        .saturating_add(entry_table_len);

    let head_length_i32: i32 = header_len
        .try_into()
        .expect("header_len fits i32 for test webfile");

    let mut out: Vec<u8> = Vec::with_capacity(
        header_len.saturating_add(entries.iter().map(|(_, b)| b.len()).sum::<usize>()),
    );
    out.extend_from_slice(signature);
    out.extend_from_slice(&head_length_i32.to_le_bytes());

    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    let mut cursor = header_len;

    for (name, bytes) in entries {
        let offset_i32: i32 = cursor.try_into().expect("offset fits i32");
        let length_i32: i32 = bytes.len().try_into().expect("length fits i32");
        let name_len_i32: i32 = name.len().try_into().expect("name_len fits i32");

        out.extend_from_slice(&offset_i32.to_le_bytes());
        out.extend_from_slice(&length_i32.to_le_bytes());
        out.extend_from_slice(&name_len_i32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());

        cursor = cursor.saturating_add(bytes.len());
        payloads.push(bytes);
    }

    for payload in payloads {
        out.extend_from_slice(&payload);
    }

    out
}

#[test]
fn unitypy_can_load_saved_unityfs_bundle() -> anyhow::Result<()> {
    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes)?;

    let expected_files: Vec<String> = bundle
        .nodes
        .iter()
        .filter(|n| n.is_file())
        .map(|n| n.name.clone())
        .collect();

    let expected_count = expected_files.len();
    let expected_name = expected_files
        .iter()
        .find(|n| !n.ends_with(".resS") && !n.ends_with(".resource"))
        .cloned()
        .unwrap_or_else(|| expected_files.first().cloned().unwrap_or_default());

    let saved = BundleWriter::save(
        &bundle,
        &BundleEdits::default(),
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
    )?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
expected_count = int(sys.argv[3])
expected_name = sys.argv[4]
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
f = env.file
assert getattr(f, "signature", None) == "UnityFS"
files = getattr(f, "files", None)
assert files is not None
assert len(files) == expected_count, (len(files), expected_count)
assert expected_name in files, expected_name
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
            expected_count.to_string(),
            expected_name,
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_can_load_saved_serialized_file() -> anyhow::Result<()> {
    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes)?;
    let node = find_first_serialized_node(&bundle)
        .expect("expected at least one serialized file node in test sample");

    let node_bytes = bundle.extract_node_data(node)?;
    let serialized = unity_asset_binary::asset::SerializedFileParser::from_bytes(node_bytes)?;

    let saved = SerializedFileWriter::save(&serialized, &SerializedFileEdits::default())?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
assets_path = sys.argv[2]
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(assets_path)
f = env.file
objects = getattr(f, "objects", None)
assert objects is not None
assert len(objects) > 0
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_can_load_saved_webfile() -> anyhow::Result<()> {
    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let entry_name = "char_118_yuki.ab".to_string();
    let bundle_bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let web_bytes = build_uncompressed_webfile(vec![(entry_name.clone(), bundle_bytes)]);

    let web = unity_asset_binary::webfile::WebFile::from_bytes(web_bytes)?;
    let saved = WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePacker::None, None)?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
web_path = sys.argv[2]
entry_name = sys.argv[3]
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(web_path)
f = env.file
assert getattr(f, "signature", "").startswith(("UnityWebData", "TuanjieWebData"))
files = getattr(f, "files", None)
assert files is not None
assert entry_name in files, (entry_name, list(files.keys())[:10])
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
            entry_name,
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_observes_rust_typetree_edit_in_repacked_bundle() -> anyhow::Result<()> {
    if std::env::var("UNITYPY_E2E").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes)?;
    let node = find_first_serialized_node(&bundle)
        .expect("expected at least one serialized file node in test sample");
    let node_name = node.name.clone();

    let node_bytes = bundle.extract_node_data(node)?;
    let serialized = unity_asset_binary::asset::SerializedFileParser::from_bytes(node_bytes)?;

    // Find a named object with a TypeTree so we can patch `m_Name` and roundtrip it.
    let mut chosen: Option<(i64, String)> = None;
    for info in &serialized.objects {
        let handle = unity_asset_binary::object::ObjectHandle::new(&serialized, info);
        if let Ok(Some(name)) = handle.peek_name() {
            if !name.is_empty() {
                chosen = Some((info.path_id, name));
                break;
            }
        }
    }
    let (path_id, old_name) = chosen.expect("expected at least one object with a peekable name");
    let new_name = format!("RUST_E2E_{}", old_name);

    let info = serialized
        .objects
        .iter()
        .find(|o| o.path_id == path_id)
        .expect("chosen object must exist");

    let handle = unity_asset_binary::object::ObjectHandle::new(&serialized, info);
    let mut obj = handle.read()?;

    // Most Unity objects use `m_Name`. Some use `name`.
    if let Some(v) = obj.class.get_mut("m_Name") {
        *v = UnityValue::String(new_name.clone());
    } else if let Some(v) = obj.class.get_mut("name") {
        *v = UnityValue::String(new_name.clone());
    } else {
        anyhow::bail!(
            "Chosen object has peekable name but no writable m_Name/name field: path_id={}",
            path_id
        );
    }

    let type_tree = type_tree_for_object(&serialized, info)
        .ok_or_else(|| anyhow::anyhow!("Missing TypeTree for object path_id={}", path_id))?;

    let endian = match serialized.header.byte_order() {
        ByteOrder::Big => Endian::Big,
        ByteOrder::Little => Endian::Little,
    };
    let mut w = BinaryWriter::new(endian);
    let tt_writer = TypeTreeWriter::with_ref_types(type_tree, &serialized.ref_types);
    tt_writer.write_object(
        &mut w,
        obj.class.properties(),
        TypeTreeWriteOptions {
            allow_missing_fields: false,
        },
    )?;
    let patched_bytes = w.into_bytes();

    let mut sf_edits = SerializedFileEdits::default();
    sf_edits.set_object_bytes(path_id, patched_bytes);
    let saved_serialized = SerializedFileWriter::save(&serialized, &sf_edits)?;

    let mut bundle_edits = BundleEdits::default();
    bundle_edits.replace_file_bytes(node_name.clone(), saved_serialized);
    let saved_bundle = BundleWriter::save(
        &bundle,
        &bundle_edits,
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
    )?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved_bundle)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
node_name = sys.argv[3]
path_id = int(sys.argv[4])
expected_name = sys.argv[5]
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
bf = env.file
sf = bf.files[node_name]
o = sf.objects[path_id]
assert o.peek_name() == expected_name, (o.peek_name(), expected_name)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
            node_name,
            path_id.to_string(),
            new_name,
        ],
    )?;

    Ok(())
}
