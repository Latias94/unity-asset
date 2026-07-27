mod support;
#[path = "support/webfile.rs"]
mod webfile_support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use unity_asset_binary::asset::{ObjectInfo, SerializedFile};
use unity_asset_binary::bundle::AssetBundle;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    JsonTypeTreeRegistry, TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions,
    TypeTreeRegistry,
};
use unity_asset_core::{AssetLoadBudget, DigestV1, FieldPath, UnityValue};
use unity_asset_write::object::{
    SerializedFieldGuard, SerializedObjectCandidate, SerializedObjectEncoder,
    SerializedObjectMutation, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
};
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};
use unity_asset_write::webfile::WebFilePackingPolicy;
use unity_asset_write::{BinaryWriter, Endian, PackingPolicy, compress_lzma_unity_with_size};

use support::{OrderedBundleEntry, ordered_bundle_entries, prepare_bundle_bytes};
use webfile_support::{ordered_webfile_members, prepare_webfile_bytes};

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

const PINNED_UNITYPY_COMMIT: &str = "5567c5eddc9dbeaef27b5113f5927226bee4f8ca";

fn unitypy_repo() -> PathBuf {
    repo_root().join("repo-ref").join("UnityPy")
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}

fn run_unitypy_preflight() -> anyhow::Result<()> {
    let python = unitypy_python().ok_or_else(|| {
        anyhow::anyhow!(
            "no UnityPy Python executable was configured; set `UNITYPY_PYTHON`, or create `{}`",
            repo_root().join(".venv-unitypy").display()
        )
    })?;
    anyhow::ensure!(
        python.is_file(),
        "UnityPy Python executable does not exist: {}",
        python.display()
    );

    let unitypy_repo = unitypy_repo();
    anyhow::ensure!(
        unitypy_repo.join("UnityPy").join("__init__.py").is_file(),
        "pinned UnityPy checkout is unavailable: {}",
        unitypy_repo.display()
    );

    let revision = Command::new("git")
        .arg("-C")
        .arg(&unitypy_repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    anyhow::ensure!(
        revision.status.success(),
        "failed to identify pinned UnityPy revision (exit={:?}): {}",
        revision.status.code(),
        String::from_utf8_lossy(&revision.stderr)
    );
    let revision = String::from_utf8(revision.stdout)?;
    anyhow::ensure!(
        revision.trim() == PINNED_UNITYPY_COMMIT,
        "UnityPy revision mismatch: expected {PINNED_UNITYPY_COMMIT}, got {}",
        revision.trim()
    );

    let import = Command::new(&python)
        .arg("-c")
        .arg(
            r#"
import os, sys
from pathlib import Path
repo_root = Path(sys.argv[1]).resolve()
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy
expected = (repo_root / "repo-ref" / "UnityPy" / "UnityPy").resolve()
actual = Path(UnityPy.__file__).resolve()
assert actual.is_relative_to(expected), (actual, expected)
"#,
        )
        .arg(repo_root())
        .output()?;
    anyhow::ensure!(
        import.status.success(),
        "failed to import pinned UnityPy (exit={:?}).\nstdout:\n{}\nstderr:\n{}",
        import.status.code(),
        String::from_utf8_lossy(&import.stdout),
        String::from_utf8_lossy(&import.stderr)
    );
    Ok(())
}

fn unitypy_preflight() -> anyhow::Result<()> {
    static PREFLIGHT: OnceLock<Result<(), String>> = OnceLock::new();
    match PREFLIGHT.get_or_init(|| run_unitypy_preflight().map_err(|error| format!("{error:#}"))) {
        Ok(()) => Ok(()),
        Err(error) => anyhow::bail!("UnityPy E2E prerequisite check failed: {error}"),
    }
}

fn external_e2e_enabled(
    test_name: &str,
    enable_var: &str,
    required_var: &str,
) -> anyhow::Result<bool> {
    let enabled = env_flag(enable_var);
    let required = env_flag(required_var);
    if !enabled && !required {
        eprintln!("UNITYPY_E2E_SKIPPED:{test_name}:{enable_var}_NOT_ENABLED");
        return Ok(false);
    }

    unitypy_preflight().map_err(|error| {
        anyhow::anyhow!(
            "{test_name} cannot run while {enable_var}=1 or {required_var}=1: {error:#}"
        )
    })?;
    Ok(true)
}

fn unitypy_e2e_enabled(test_name: &str) -> anyhow::Result<bool> {
    external_e2e_enabled(test_name, "UNITYPY_E2E", "UNITYPY_E2E_REQUIRED")
}

#[test]
fn unitypy_e2e_prerequisite_gate() -> anyhow::Result<()> {
    let _ = unitypy_e2e_enabled("unitypy_e2e_prerequisite_gate")?;
    Ok(())
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

fn python_run(script_path: &Path, args: &[String]) -> anyhow::Result<()> {
    let python = unitypy_python().ok_or_else(|| {
        anyhow::anyhow!(
            "UnityPy E2E is enabled, but no python was found. Set `UNITYPY_PYTHON`, or create a venv at `{}`.",
            repo_root().join(".venv-unitypy").display()
        )
    })?;

    let out = Command::new(python).arg(script_path).args(args).output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "Python script failed (exit={:?}, script={}).\nstdout:\n{}\nstderr:\n{}",
            out.status.code(),
            script_path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn serialized_type_for_object<'a>(
    file: &'a SerializedFile,
    info: &ObjectInfo,
) -> Option<&'a unity_asset_binary::asset::SerializedType> {
    if let Some(type_index) = info.serialized_type_index() {
        return file.types().get(usize::try_from(type_index).ok()?);
    }
    file.types().iter().find(|t| t.class_id == info.class_id())
}

fn find_first_serialized_node(
    bundle: &AssetBundle,
) -> Option<&unity_asset_binary::bundle::DirectoryNode> {
    bundle
        .nodes
        .iter()
        .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
}

fn apply_guarded_field_replacement(
    candidate: &mut SerializedObjectCandidate<'_>,
    ordinal: u32,
    path: FieldPath,
    replacement: UnityValue,
    budget: &mut AssetLoadBudget,
) -> anyhow::Result<()> {
    let guard = SerializedFieldGuard::from_observed(
        candidate.schema_digest(),
        &path,
        candidate.value_at_path(&path)?,
        budget,
    )?;
    candidate.apply(
        SerializedObjectMutation::replace_field(ordinal, path, guard, replacement),
        budget,
    )?;
    Ok(())
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

fn build_minimal_legacy_bundle(
    signature: &str,
    file_name: &str,
    file_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let version_player = "3.5.0f5";
    let version_engine = "3.5.0f5";

    let mut file_info_header_size: usize = 4; // nodesCount
    file_info_header_size = file_info_header_size
        .saturating_add(file_name.len().saturating_add(1))
        .saturating_add(4 * 2);
    file_info_header_size = (file_info_header_size.saturating_add(3)) & !3;

    let mut directory_info_writer = BinaryWriter::new(Endian::Big);
    directory_info_writer.write_i32(1);
    directory_info_writer.write_string_to_null(file_name);
    directory_info_writer.write_u32(u32::try_from(file_info_header_size)?);
    directory_info_writer.write_u32(u32::try_from(file_bytes.len())?);

    let dir_len = directory_info_writer.position();
    if dir_len > file_info_header_size {
        anyhow::bail!(
            "computed file_info_header_size too small: {} > {}",
            dir_len,
            file_info_header_size
        );
    }
    directory_info_writer.write(&vec![0u8; file_info_header_size - dir_len]);

    let mut blob = directory_info_writer.into_result()?;
    blob.extend_from_slice(file_bytes);

    let uncompressed_size_u32 = u32::try_from(blob.len())?;

    let (compressed_blob, compressed_size_u32) = if signature == "UnityWeb" {
        let compressed = compress_lzma_unity_with_size(&blob)?;
        let compressed_len = u32::try_from(compressed.len())?;
        (compressed, compressed_len)
    } else {
        (blob, uncompressed_size_u32)
    };

    let mut writer = BinaryWriter::new(Endian::Big);
    writer.write_string_to_null(signature);
    writer.write_u32(3); // version
    writer.write_string_to_null(version_player);
    writer.write_string_to_null(version_engine);

    // Matches UnityPy `BundleFile.save_web_raw` header size math (levelCount=1).
    let mut header_size_u32 = u32::try_from(writer.position())?;
    header_size_u32 = header_size_u32.saturating_add(24);
    header_size_u32 = header_size_u32.saturating_add(4); // completeFileSize
    header_size_u32 = header_size_u32.saturating_add(4); // fileInfoHeaderSize
    header_size_u32 = (header_size_u32.saturating_add(3)) & !3;

    let complete_file_size_u32 = header_size_u32
        .checked_add(compressed_size_u32)
        .ok_or_else(|| anyhow::anyhow!("legacy completeFileSize overflow"))?;

    writer.write_u32(complete_file_size_u32); // minimumStreamedBytes
    writer.write_u32(header_size_u32); // headerSize
    writer.write_u32(1); // numberOfLevelsToDownloadBeforeStreaming
    writer.write_i32(1); // levelCount
    writer.write_u32(compressed_size_u32);
    writer.write_u32(uncompressed_size_u32);
    writer.write_u32(complete_file_size_u32); // completeFileSize
    writer.write_u32(u32::try_from(file_info_header_size)?); // fileInfoHeaderSize

    writer.align_stream(4);
    anyhow::ensure!(
        u32::try_from(writer.position())? == header_size_u32,
        "legacy header size mismatch (pos={}, headerSize={})",
        writer.position(),
        header_size_u32
    );

    writer.write(&compressed_blob);
    Ok(writer.into_result()?)
}

#[test]
fn unitypy_can_load_saved_unityfs_bundle() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_unityfs_bundle")? {
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

    let entries = ordered_bundle_entries(&bundle)?;
    let saved = prepare_bundle_bytes(&bundle, &entries, PackingPolicy::Preserve)?;

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
fn unitypy_can_load_saved_legacy_unityraw_bundle() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_legacy_unityraw_bundle")? {
        return Ok(());
    }

    let input = build_minimal_legacy_bundle("UnityRaw", "test.txt", b"abc")?;
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(input)?;

    let mut entries = ordered_bundle_entries(&bundle)?;
    let OrderedBundleEntry::File { bytes, .. } = &mut entries[0] else {
        panic!("legacy fixture entry is a file");
    };
    *bytes = Arc::from(&b"abcd"[..]);
    let saved = prepare_bundle_bytes(&bundle, &entries, PackingPolicy::Uncompressed)?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
expected_sig = sys.argv[3]
file_name = sys.argv[4]
expected = sys.argv[5].encode("utf-8")
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
f = env.file
assert getattr(f, "signature", None) == expected_sig
assert file_name in f.files
got = f.files[file_name].bytes
assert got == expected, (got, expected)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
            "UnityRaw".to_string(),
            "test.txt".to_string(),
            "abcd".to_string(),
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_can_load_saved_legacy_unityweb_bundle() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_legacy_unityweb_bundle")? {
        return Ok(());
    }

    let input = build_minimal_legacy_bundle("UnityWeb", "test.txt", b"abc")?;
    let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(input)?;

    let mut entries = ordered_bundle_entries(&bundle)?;
    let OrderedBundleEntry::File { bytes, .. } = &mut entries[0] else {
        panic!("legacy fixture entry is a file");
    };
    *bytes = Arc::from(&b"abcd"[..]);
    let saved = prepare_bundle_bytes(&bundle, &entries, PackingPolicy::Preserve)?;

    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::write(tmp.path(), &saved)?;

    let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
expected_sig = sys.argv[3]
file_name = sys.argv[4]
expected = sys.argv[5].encode("utf-8")
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
f = env.file
assert getattr(f, "signature", None) == expected_sig
assert file_name in f.files
got = f.files[file_name].bytes
assert got == expected, (got, expected)
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            tmp.path().display().to_string(),
            "UnityWeb".to_string(),
            "test.txt".to_string(),
            "abcd".to_string(),
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_can_load_saved_serialized_file() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_serialized_file")? {
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

fn push_cstring(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn decode_hex_fixture(contents: &str) -> Vec<u8> {
    let digits: Vec<u8> = contents
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert_eq!(digits.len() % 2, 0, "hex fixture must contain byte pairs");
    digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("valid hex digit");
            let low = (pair[1] as char).to_digit(16).expect("valid hex digit");
            u8::try_from((high << 4) | low).expect("hex pair fits u8")
        })
        .collect()
}

fn make_minimal_serialized_file_v8_le() -> Vec<u8> {
    let version: u32 = 8;
    // UnityPy's file type detection skips AssetsFile checks for files < 128 bytes.
    // Keep this synthetic sample comfortably above that threshold.
    let data_offset: u32 = 128;

    let mut meta: Vec<u8> = Vec::new();
    push_cstring(&mut meta, "2.5.0f5");
    meta.extend_from_slice(&0i32.to_le_bytes()); // target_platform
    meta.extend_from_slice(&0i32.to_le_bytes()); // type_count
    meta.extend_from_slice(&0i32.to_le_bytes()); // big_id_enabled (7<=v<14)
    meta.extend_from_slice(&0i32.to_le_bytes()); // object_count
    meta.extend_from_slice(&0i32.to_le_bytes()); // externals_count
    push_cstring(&mut meta, "");

    let metadata_size: u32 = (1u32).saturating_add(meta.len() as u32); // +1 endian boolean
    let file_size: u32 = data_offset.saturating_add(metadata_size);

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&metadata_size.to_be_bytes());
    out.extend_from_slice(&file_size.to_be_bytes());
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&data_offset.to_be_bytes());

    if out.len() < data_offset as usize {
        out.resize(data_offset as usize, 0);
    }

    out.push(0u8); // endian: 0 = little
    out.extend_from_slice(&meta);

    out
}

#[test]
fn unitypy_can_load_saved_legacy_v8_serialized_file() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_legacy_v8_serialized_file")? {
        return Ok(());
    }

    let bytes = make_minimal_serialized_file_v8_le();
    let serialized = unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes)?;
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
assert f.header.version == 8
assert len(f.types) == 0
assert len(f.objects) == 0
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
fn unitypy_loads_independent_serialized_file_wire_goldens() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_loads_independent_serialized_file_wire_goldens")? {
        return Ok(());
    }

    let fixture_dir = repo_root()
        .join("crates")
        .join("unity-asset-write")
        .join("tests")
        .join("fixtures")
        .join("serialized_file_wire");
    let rewritten_dir = tempfile::tempdir()?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_dir.join("manifest.json"))?)?;
    for case in manifest["cases"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("wire manifest cases must be an array"))?
    {
        let version = case["expected"]["version"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("wire case version must be an integer"))?;
        let file_name = case["file"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("wire case file must be a string"))?;
        let serialized = unity_asset_binary::asset::SerializedFileParser::from_bytes(
            std::fs::read(fixture_dir.join(file_name))?,
        )?;
        let no_op = SerializedFileWriter::save(&serialized, &SerializedFileEdits::default())?;
        std::fs::write(
            rewritten_dir
                .path()
                .join(format!("v{version}_noop.assets.bin")),
            no_op,
        )?;

        let object = serialized
            .objects()
            .first()
            .ok_or_else(|| anyhow::anyhow!("wire case v{version} has no object"))?;
        let path_id = object.path_id();
        let original_digest = DigestV1::hash_bytes(
            serialized
                .find_object_handle(path_id)
                .ok_or_else(|| anyhow::anyhow!("wire case v{version} object disappeared"))?
                .raw_data()?,
        );
        let mut edit_budget = AssetLoadBudget::default();
        let encoded = SerializedObjectEncoder::new(&serialized, path_id)?.encode_unsafe_raw(
            UnsafeRawObjectReplacement::new(
                original_digest,
                vec![0xD0, version as u8, 0xAD, 0xBE, 0xEF],
                UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
            ),
            &mut edit_budget,
        )?;
        let mut edits = SerializedFileEdits::default();
        edits.try_insert_encoded_object(encoded, &mut edit_budget)?;
        let edited = SerializedFileWriter::save(&serialized, &edits)?;
        std::fs::write(
            rewritten_dir
                .path()
                .join(format!("v{version}_edited.assets.bin")),
            edited,
        )?;
    }
    let legacy_script = unity_asset_binary::asset::SerializedFileParser::from_bytes(
        std::fs::read(fixture_dir.join("legacy_v15_monobehaviour.assets.bin"))?,
    )?;
    std::fs::write(
        rewritten_dir
            .path()
            .join("legacy_v15_monobehaviour.assets.bin"),
        SerializedFileWriter::save(&legacy_script, &SerializedFileEdits::default())?,
    )?;
    let collision_bytes = decode_hex_fixture(include_str!(
        "fixtures/serialized_file_wire/v16_type_index_collision.assets.hex"
    ));
    std::fs::write(
        rewritten_dir.path().join("collision_input.assets.bin"),
        &collision_bytes,
    )?;
    let collision = unity_asset_binary::asset::SerializedFileParser::from_bytes(collision_bytes)?;
    std::fs::write(
        rewritten_dir.path().join("collision_rewritten.assets.bin"),
        SerializedFileWriter::save(&collision, &SerializedFileEdits::default())?,
    )?;
    let py = r#"
import os, re, sys
from pathlib import Path

repo_root = sys.argv[1]
fixture_dir = Path(sys.argv[2])
rewritten_dir = Path(sys.argv[3])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

paths = sorted(
    fixture_dir.glob("v*.assets.bin"),
    key=lambda path: int(re.fullmatch(r"v(\d+)\.assets\.bin", path.name).group(1)),
)
assert len(paths) == 20, len(paths)
rewritten_paths = sorted(rewritten_dir.glob("v*_*.assets.bin"))
assert len(rewritten_paths) == 40, len(rewritten_paths)

def validate(path, version, expected_payload):
    file = UnityPy.load(str(path)).file
    assert file.header.version == version, (path, file.header.version)
    assert file._enable_type_tree is True, path
    assert len(file.types) == 1, path
    assert len(file.objects) == 1, path

    obj = next(iter(file.objects.values()))
    assert obj.path_id == (0x000000010000002A if version == 8 else 42), path
    expected_type_id = 0 if version >= 16 else (0x13572468 if version == 8 else 28)
    assert obj.type_id == expected_type_id, path
    assert obj.class_id == 28, path
    assert obj.byte_size == len(expected_payload), (path, obj.byte_size, len(expected_payload))
    assert obj.get_raw_data() == expected_payload, (path, obj.get_raw_data(), expected_payload)
    assert obj.is_destroyed == (0x1234 if version < 11 else None), path
    assert obj.is_stripped == (1 if version in (15, 16) else None), path
    if 11 <= version < 17:
        assert obj.serialized_type.script_type_index == -3, path

    assert len(file.externals) == 1, path
    assert file.externals[0].path == "archive:/fixture-dependency.assets", path
    assert len(getattr(file, "ref_types", []) or []) == (1 if version >= 20 else 0), path

for path in paths:
    version = int(re.fullmatch(r"v(\d+)\.assets\.bin", path.name).group(1))
    validate(path, version, bytes((version, 0xAA, 0xBB, 0xCC)))

for path in rewritten_paths:
    match = re.fullmatch(r"v(\d+)_(noop|edited)\.assets\.bin", path.name)
    version = int(match.group(1))
    expected_payload = (
        bytes((version, 0xAA, 0xBB, 0xCC))
        if match.group(2) == "noop"
        else bytes((0xD0, version, 0xAD, 0xBE, 0xEF))
    )
    validate(path, version, expected_payload)

for script_path in (
    fixture_dir / "legacy_v15_monobehaviour.assets.bin",
    rewritten_dir / "legacy_v15_monobehaviour.assets.bin",
):
    script_file = UnityPy.load(str(script_path)).file
    script_obj = next(iter(script_file.objects.values()))
    assert script_obj.type_id == -1, script_path
    assert script_obj.class_id == 114, script_path
    assert script_obj.serialized_type is not None, script_path
    assert script_obj.serialized_type.class_id == -1, script_path
    assert script_obj.serialized_type.script_type_index == 7, script_path

for collision_path in (
    rewritten_dir / "collision_input.assets.bin",
    rewritten_dir / "collision_rewritten.assets.bin",
):
    collision_file = UnityPy.load(str(collision_path)).file
    assert [typ.class_id for typ in collision_file.types] == [1, 28], collision_path
    collision_obj = next(iter(collision_file.objects.values()))
    assert collision_obj.type_id == 1, collision_path
    assert collision_obj.class_id == 28, collision_path
    assert collision_obj.get_raw_data() == bytes((0x10, 0xAA, 0xBB, 0xCC)), collision_path
"#;

    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            fixture_dir.display().to_string(),
            rewritten_dir.path().display().to_string(),
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_can_load_saved_webfile() -> anyhow::Result<()> {
    if !unitypy_e2e_enabled("unitypy_can_load_saved_webfile")? {
        return Ok(());
    }

    let entry_name = "char_118_yuki.ab".to_string();
    let bundle_bytes = include_bytes!("../../../tests/samples/char_118_yuki.ab").to_vec();
    let web_bytes = build_uncompressed_webfile(vec![(entry_name.clone(), bundle_bytes)]);

    let web = unity_asset_binary::webfile::WebFile::from_bytes(web_bytes)?;
    let members = ordered_webfile_members(&web)?;
    let saved = prepare_webfile_bytes(&web, &members, WebFilePackingPolicy::Uncompressed)?;

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
    if !unitypy_e2e_enabled("unitypy_observes_rust_typetree_edit_in_repacked_bundle")? {
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
    let mut budget = AssetLoadBudget::default();
    let mut chosen: Option<(i64, String)> = None;
    for info in serialized.objects() {
        let handle = unity_asset_binary::object::ObjectHandle::new(&serialized, info);
        if let Ok(Some(name)) = handle.peek_name(&mut budget)
            && !name.is_empty()
        {
            chosen = Some((info.path_id(), name));
            break;
        }
    }
    let (path_id, old_name) = chosen.expect("expected at least one object with a peekable name");
    let new_name = format!("RUST_E2E_{}", old_name);

    let mut candidate =
        SerializedObjectEncoder::new(&serialized, path_id)?.begin_semantic(&mut budget)?;
    let name_path = ["m_Name", "name"]
        .into_iter()
        .find_map(|field| {
            let path = FieldPath::root().push_field(field).ok()?;
            matches!(
                candidate.value_at_path(&path),
                Ok(UnityValue::String(name)) if name == &old_name
            )
            .then_some(path)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "chosen object has peekable name but no matching m_Name/name field: path_id={path_id}"
            )
        })?;
    apply_guarded_field_replacement(
        &mut candidate,
        0,
        name_path,
        UnityValue::String(new_name.clone()),
        &mut budget,
    )?;
    let encoded = candidate.finish(&mut budget)?;
    let mut edits = SerializedFileEdits::default();
    edits.try_insert_encoded_object(encoded, &mut budget)?;
    let saved_serialized = SerializedFileWriter::save(&serialized, &edits)?;

    let node_index = bundle
        .nodes
        .iter()
        .position(|node| node.is_file() && node.name == node_name)
        .expect("serialized bundle member still has its directory ordinal");
    let mut entries = ordered_bundle_entries(&bundle)?;
    let OrderedBundleEntry::File { bytes, .. } = &mut entries[node_index] else {
        panic!("serialized bundle member is a file");
    };
    *bytes = Arc::from(saved_serialized);
    let saved_bundle = prepare_bundle_bytes(&bundle, &entries, PackingPolicy::Preserve)?;

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

const AE6_PATH_ID: i64 = 42;
const AE6_OBJECT_SIZE: usize = 156;
const AE6_ORIGINAL_LEAF_WIDE: u64 = 0x8000_0000_0000_1234;
const AE6_ORIGINAL_ROOT_WIDE: u64 = 0xffff_ffff_ffff_fffd;
const AE6_REWRITTEN_LEAF_WIDE: u64 = 0x8000_0000_0000_5678;
const AE6_REWRITTEN_ROOT_WIDE: u64 = 0xfedc_ba98_7654_3210;

struct Ae6RefType {
    class_name: &'static str,
    namespace: &'static str,
    assembly_name: &'static str,
    script_id_byte: u8,
    tree: TypeTree,
}

fn ae6_node(type_name: &str, name: &str) -> TypeTreeNode {
    let byte_size = match type_name {
        "UInt8" => 1,
        "UInt16" => 2,
        "int" => 4,
        "UInt64" => 8,
        _ => -1,
    };
    let mut node = TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), byte_size);
    node.version = 1;
    node
}

fn ae6_record(type_name: &str, name: &str, children: Vec<TypeTreeNode>) -> TypeTreeNode {
    let mut node = ae6_node(type_name, name);
    node.children = children;
    node
}

fn ae6_registry(name: &str) -> TypeTreeNode {
    ae6_record(
        "ManagedReferencesRegistry",
        name,
        vec![ae6_node("int", "m_Version")],
    )
}

fn ae6_reference(name: &str) -> TypeTreeNode {
    let type_node = ae6_record(
        "ReferencedObjectType",
        "type",
        vec![
            ae6_node("string", "class"),
            ae6_node("string", "ns"),
            ae6_node("string", "asm"),
        ],
    );
    ae6_record(
        "ReferencedObject",
        name,
        vec![type_node, ae6_node("ReferencedObjectData", "data")],
    )
}

fn ae6_type_trees() -> (TypeTree, Vec<Ae6RefType>) {
    let mut size = ae6_node("int", "size");
    // UnityPy and the canonical Rust schema both treat this as count metadata, not a value node.
    size.meta_flags = 0x4000;
    let mut array = ae6_record("Array", "Array", vec![size, ae6_node("UInt8", "data")]);
    array.type_flags = 1;
    let size_aligned = ae6_record("vector", "m_SizeAligned", vec![array]);

    let mut pair = ae6_record(
        "pair",
        "m_Pair",
        vec![ae6_node("UInt8", "first"), ae6_node("UInt16", "second")],
    );
    pair.meta_flags = 0x4000;

    let map_pair = ae6_record(
        "pair",
        "data",
        vec![ae6_node("UInt8", "first"), ae6_node("UInt16", "second")],
    );
    let mut map_array = ae6_record("Array", "Array", vec![ae6_node("int", "size"), map_pair]);
    map_array.type_flags = 1;
    let map = ae6_record("map", "m_Map", vec![map_array]);

    let nested = ae6_record(
        "NestedRecord",
        "m_Nested",
        vec![
            ae6_registry("m_NestedRegistry"),
            ae6_node("int", "m_NestedMarker"),
        ],
    );
    let root = ae6_record(
        "Ae6Fixture",
        "Base",
        vec![
            ae6_node("UInt8", "m_Prefix"),
            size_aligned,
            pair,
            map,
            ae6_registry("m_RegistryA"),
            nested,
            ae6_reference("m_Reference"),
            ae6_registry("m_RegistryB"),
            ae6_node("UInt64", "m_Wide"),
            ae6_node("int", "m_RewriteMarker"),
        ],
    );
    let mut root_tree = TypeTree::new();
    root_tree.add_node(root);

    let outer_root = ae6_record(
        "OuterManaged",
        "OuterManaged",
        vec![
            ae6_registry("m_ManagedRegistry"),
            ae6_node("int", "m_ManagedValue"),
            ae6_reference("m_NestedReference"),
        ],
    );
    let mut outer_tree = TypeTree::new();
    outer_tree.add_node(outer_root);

    let leaf_root = ae6_record(
        "LeafManaged",
        "LeafManaged",
        vec![
            ae6_registry("m_LeafRegistry"),
            ae6_node("UInt64", "m_LeafWide"),
            ae6_node("int", "m_LeafMarker"),
        ],
    );
    let mut leaf_tree = TypeTree::new();
    leaf_tree.add_node(leaf_root);

    (
        root_tree,
        vec![
            Ae6RefType {
                class_name: "OuterManaged",
                namespace: "Tests",
                assembly_name: "Assembly-CSharp",
                script_id_byte: 0x41,
                tree: outer_tree,
            },
            Ae6RefType {
                class_name: "LeafManaged",
                namespace: "Tests",
                assembly_name: "Assembly-CSharp",
                script_id_byte: 0x42,
                tree: leaf_tree,
            },
        ],
    )
}

fn flatten_ae6_node<'a>(
    node: &'a TypeTreeNode,
    level: u8,
    flattened: &mut Vec<(u8, &'a TypeTreeNode)>,
) -> anyhow::Result<()> {
    flattened.push((level, node));
    let child_level = level
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("AE6 TypeTree depth overflow"))?;
    for child in &node.children {
        flatten_ae6_node(child, child_level, flattened)?;
    }
    Ok(())
}

fn intern_ae6_string(
    value: &str,
    offsets: &mut HashMap<String, u32>,
    buffer: &mut Vec<u8>,
) -> anyhow::Result<u32> {
    if let Some(offset) = offsets.get(value) {
        return Ok(*offset);
    }
    let offset = u32::try_from(buffer.len())?;
    buffer.extend_from_slice(value.as_bytes());
    buffer.push(0);
    offsets.insert(value.to_owned(), offset);
    Ok(offset)
}

fn write_ae6_type_tree(writer: &mut BinaryWriter, tree: &TypeTree) -> anyhow::Result<()> {
    anyhow::ensure!(tree.nodes.len() == 1, "AE6 fixture needs exactly one root");
    let mut flattened = Vec::new();
    flatten_ae6_node(&tree.nodes[0], 0, &mut flattened)?;

    let mut offsets = HashMap::new();
    let mut string_buffer = Vec::new();
    for (_, node) in &flattened {
        intern_ae6_string(&node.type_name, &mut offsets, &mut string_buffer)?;
        intern_ae6_string(&node.name, &mut offsets, &mut string_buffer)?;
    }

    writer.write_i32(i32::try_from(flattened.len())?);
    writer.write_i32(i32::try_from(string_buffer.len())?);
    for (index, (level, node)) in flattened.iter().enumerate() {
        writer.write_i16(i16::try_from(node.version)?);
        writer.write_u8(*level);
        writer.write_u8(u8::try_from(node.type_flags)?);
        writer.write_u32(offsets[node.type_name.as_str()]);
        writer.write_u32(offsets[node.name.as_str()]);
        writer.write_i32(node.byte_size);
        writer.write_i32(i32::try_from(index)?);
        writer.write_i32(node.meta_flags);
        writer.write_u64(node.ref_type_hash);
    }
    writer.write(&string_buffer);
    writer.ensure_valid()?;
    Ok(())
}

fn write_ae6_serialized_type(
    writer: &mut BinaryWriter,
    tree: &TypeTree,
    ref_type: Option<&Ae6RefType>,
) -> anyhow::Result<()> {
    writer.write_i32(if ref_type.is_some() { 114 } else { 28 });
    writer.write_bool(false);
    writer.write_i16(if ref_type.is_some() { 0 } else { -1 });
    if let Some(ref_type) = ref_type {
        writer.write(&[ref_type.script_id_byte; 16]);
    }
    writer.write(&[if ref_type.is_some() { 0x62 } else { 0x31 }; 16]);
    write_ae6_type_tree(writer, tree)?;
    if let Some(ref_type) = ref_type {
        writer.write_string_to_null(ref_type.class_name);
        writer.write_string_to_null(ref_type.namespace);
        writer.write_string_to_null(ref_type.assembly_name);
    } else {
        writer.write_i32(0);
    }
    Ok(())
}

fn build_ae6_payload() -> anyhow::Result<Vec<u8>> {
    let mut writer = BinaryWriter::new(Endian::Little);
    writer.write_u8(0xA5);
    writer.write_i32(1);
    writer.write_u8(0x7F);

    writer.write_u8(0x11);
    writer.write_u16(0x2233);
    writer.write(&[0xD1, 0xD2, 0xD3]);
    anyhow::ensure!(writer.position() == 12, "AE6 pair alignment phase drifted");

    writer.write_i32(2);
    writer.write_u8(1);
    writer.write_u16(0x0203);
    writer.write_u8(4);
    writer.write_u16(0x0506);
    writer.write_i32(7);
    writer.write_i32(0x1122_3344);

    writer.write_aligned_string("OuterManaged")?;
    writer.write_aligned_string("Tests")?;
    writer.write_aligned_string("Assembly-CSharp")?;
    writer.write_i32(0x5566_7788);
    writer.write_aligned_string("LeafManaged")?;
    writer.write_aligned_string("Tests")?;
    writer.write_aligned_string("Assembly-CSharp")?;
    writer.write_u64(AE6_ORIGINAL_LEAF_WIDE);
    writer.write_i32(0x0102_0304);
    writer.write_u64(AE6_ORIGINAL_ROOT_WIDE);
    writer.write_i32(11);

    let payload = writer.into_result()?;
    anyhow::ensure!(
        payload.len() == AE6_OBJECT_SIZE,
        "AE6 payload size drifted: expected {AE6_OBJECT_SIZE}, got {}",
        payload.len()
    );
    Ok(payload)
}

fn build_ae6_serialized_file() -> anyhow::Result<Vec<u8>> {
    let (root_tree, ref_types) = ae6_type_trees();
    let payload = build_ae6_payload()?;

    let mut metadata = BinaryWriter::new(Endian::Little);
    metadata.write_string_to_null("2022.3.0f1");
    metadata.write_i32(13);
    metadata.write_bool(true);
    metadata.write_i32(1);
    write_ae6_serialized_type(&mut metadata, &root_tree, None)?;
    metadata.write_i32(1);
    metadata.align_stream(4);
    metadata.write_i64(AE6_PATH_ID);
    metadata.write_i64(0);
    metadata.write_u32(u32::try_from(payload.len())?);
    metadata.write_i32(0);
    metadata.write_i32(0);
    metadata.write_i32(0);
    metadata.write_i32(i32::try_from(ref_types.len())?);
    for ref_type in &ref_types {
        write_ae6_serialized_type(&mut metadata, &ref_type.tree, Some(ref_type))?;
    }
    metadata.write_string_to_null("");
    let metadata = metadata.into_result()?;

    let unaligned_data_offset = 48usize
        .checked_add(metadata.len())
        .ok_or_else(|| anyhow::anyhow!("AE6 data offset overflow"))?;
    let data_offset = unaligned_data_offset
        .checked_add(15)
        .ok_or_else(|| anyhow::anyhow!("AE6 data alignment overflow"))?
        & !15;
    let file_size = data_offset
        .checked_add(payload.len())
        .ok_or_else(|| anyhow::anyhow!("AE6 file size overflow"))?;

    let mut header = BinaryWriter::new(Endian::Big);
    header.write_u32(0);
    header.write_u32(0);
    header.write_u32(22);
    header.write_u32(0);
    header.write_u8(0);
    header.write(&[0; 3]);
    header.write_u32(u32::try_from(metadata.len())?);
    header.write_i64(i64::try_from(file_size)?);
    header.write_i64(i64::try_from(data_offset)?);
    header.write_i64(0);
    let mut output = header.into_result()?;
    anyhow::ensure!(output.len() == 48, "AE6 v22 header must be 48 bytes");
    output.extend_from_slice(&metadata);
    output.resize(data_offset, 0);
    output.extend_from_slice(&payload);
    Ok(output)
}

fn ae6_object<'a>(
    properties: &'a IndexMap<String, UnityValue>,
    name: &str,
) -> &'a IndexMap<String, UnityValue> {
    properties
        .get(name)
        .and_then(UnityValue::as_object)
        .unwrap_or_else(|| panic!("AE6 field `{name}` must be an object"))
}

fn assert_ae6_properties(
    properties: &IndexMap<String, UnityValue>,
    size_aligned_value: u8,
    second_map_value: i64,
    managed_value: i64,
    leaf_wide: u64,
    root_wide: u64,
    rewrite_marker: i64,
) {
    assert_eq!(properties.get("m_Prefix"), Some(&UnityValue::Integer(0xA5)));
    assert_eq!(
        properties.get("m_SizeAligned"),
        Some(&UnityValue::Bytes(vec![size_aligned_value]))
    );
    assert_eq!(
        properties.get("m_Pair"),
        Some(&UnityValue::Array(vec![
            UnityValue::Integer(0x11),
            UnityValue::Integer(0x2233),
        ]))
    );
    assert_eq!(
        properties.get("m_Map"),
        Some(&UnityValue::Array(vec![
            UnityValue::Array(vec![UnityValue::Integer(1), UnityValue::Integer(0x0203)]),
            UnityValue::Array(vec![
                UnityValue::Integer(4),
                UnityValue::Integer(second_map_value),
            ]),
        ]))
    );

    let registry = ae6_object(properties, "m_RegistryA");
    assert_eq!(registry.get("m_Version"), Some(&UnityValue::Integer(7)));
    assert!(!properties.contains_key("m_RegistryB"));
    let nested = ae6_object(properties, "m_Nested");
    assert!(!nested.contains_key("m_NestedRegistry"));
    assert_eq!(
        nested.get("m_NestedMarker"),
        Some(&UnityValue::Integer(0x1122_3344))
    );

    let reference = ae6_object(properties, "m_Reference");
    let reference_type = ae6_object(reference, "type");
    assert_eq!(
        reference_type.get("class"),
        Some(&UnityValue::String("OuterManaged".to_owned()))
    );
    let managed = ae6_object(reference, "data");
    assert!(!managed.contains_key("m_ManagedRegistry"));
    assert_eq!(
        managed.get("m_ManagedValue"),
        Some(&UnityValue::Integer(managed_value))
    );

    let nested_reference = ae6_object(managed, "m_NestedReference");
    let nested_type = ae6_object(nested_reference, "type");
    assert_eq!(
        nested_type.get("class"),
        Some(&UnityValue::String("LeafManaged".to_owned()))
    );
    let leaf = ae6_object(nested_reference, "data");
    assert!(!leaf.contains_key("m_LeafRegistry"));
    assert_eq!(
        leaf.get("m_LeafWide"),
        Some(&UnityValue::Unsigned(leaf_wide))
    );
    assert_eq!(
        leaf.get("m_LeafMarker"),
        Some(&UnityValue::Integer(0x0102_0304))
    );
    assert_eq!(
        properties.get("m_Wide"),
        Some(&UnityValue::Unsigned(root_wide))
    );
    assert_eq!(
        properties.get("m_RewriteMarker"),
        Some(&UnityValue::Integer(rewrite_marker))
    );
}

#[test]
fn unitypy_differential_covers_ae6_typetree_semantics() -> anyhow::Result<()> {
    let fixture = build_ae6_serialized_file()?;
    let serialized = unity_asset_binary::asset::SerializedFileParser::from_bytes(fixture.clone())?;
    anyhow::ensure!(
        serialized.header.version == 22,
        "AE6 fixture version drifted"
    );
    anyhow::ensure!(
        serialized.ref_types().len() == 2,
        "AE6 ref_types were not retained"
    );
    let handle = serialized
        .find_object_handle(AE6_PATH_ID)
        .ok_or_else(|| anyhow::anyhow!("AE6 fixture object is missing"))?;
    let raw = handle.raw_data()?;
    assert_eq!(raw.len(), AE6_OBJECT_SIZE);
    assert_eq!(&raw[9..12], &[0xD1, 0xD2, 0xD3]);

    let mut budget = AssetLoadBudget::default();
    let schema = handle
        .schema(&mut budget)?
        .ok_or_else(|| anyhow::anyhow!("AE6 fixture schema is missing"))?;
    let mut reader = BinaryReader::new(raw, ByteOrder::Little);
    let parsed = schema.read_object(
        &mut reader,
        &mut budget,
        TypeTreeParseOptions {
            mode: TypeTreeParseMode::Strict,
        },
    )?;
    assert_eq!(reader.position(), u64::try_from(raw.len())?);
    assert_ae6_properties(
        &parsed.properties,
        0x7F,
        0x0506,
        0x5566_7788,
        AE6_ORIGINAL_LEAF_WIDE,
        AE6_ORIGINAL_ROOT_WIDE,
        11,
    );
    let mut skip_reader = BinaryReader::new(raw, ByteOrder::Little);
    schema.skip_value(&mut skip_reader, &mut budget, schema.root())?;
    assert_eq!(skip_reader.position(), u64::try_from(raw.len())?);

    let mut candidate =
        SerializedObjectEncoder::new(&serialized, AE6_PATH_ID)?.begin_semantic(&mut budget)?;
    apply_guarded_field_replacement(
        &mut candidate,
        0,
        FieldPath::root().push_field("m_SizeAligned")?,
        UnityValue::Bytes(vec![0x80]),
        &mut budget,
    )?;
    apply_guarded_field_replacement(
        &mut candidate,
        1,
        FieldPath::root()
            .push_field("m_Map")?
            .push_index(1)?
            .push_index(1)?,
        UnityValue::Integer(0x0708),
        &mut budget,
    )?;
    apply_guarded_field_replacement(
        &mut candidate,
        2,
        FieldPath::root()
            .push_field("m_Reference")?
            .push_field("data")?
            .push_field("m_ManagedValue")?,
        UnityValue::Integer(0x6677_8899),
        &mut budget,
    )?;
    apply_guarded_field_replacement(
        &mut candidate,
        3,
        FieldPath::root()
            .push_field("m_Reference")?
            .push_field("data")?
            .push_field("m_NestedReference")?
            .push_field("data")?
            .push_field("m_LeafWide")?,
        UnityValue::Unsigned(AE6_REWRITTEN_LEAF_WIDE),
        &mut budget,
    )?;
    apply_guarded_field_replacement(
        &mut candidate,
        4,
        FieldPath::root().push_field("m_Wide")?,
        UnityValue::Unsigned(AE6_REWRITTEN_ROOT_WIDE),
        &mut budget,
    )?;
    apply_guarded_field_replacement(
        &mut candidate,
        5,
        FieldPath::root().push_field("m_RewriteMarker")?,
        UnityValue::Integer(12),
        &mut budget,
    )?;
    let encoded = candidate.finish(&mut budget)?;
    let mut edits = SerializedFileEdits::default();
    edits.try_insert_encoded_object(encoded, &mut budget)?;
    let rewritten_bytes = SerializedFileWriter::save(&serialized, &edits)?;
    let rewritten =
        unity_asset_binary::asset::SerializedFileParser::from_bytes(rewritten_bytes.clone())?;
    let rewritten_handle = rewritten
        .find_object_handle(AE6_PATH_ID)
        .ok_or_else(|| anyhow::anyhow!("rewritten AE6 fixture object is missing"))?;
    let rewritten_raw = rewritten_handle.raw_data()?;
    assert_eq!(rewritten_raw.len(), AE6_OBJECT_SIZE);
    // The Rust template contract preserves untouched non-zero pair padding byte-for-byte.
    assert_eq!(&rewritten_raw[9..12], &[0xD1, 0xD2, 0xD3]);
    let rewritten_schema = rewritten_handle
        .schema(&mut budget)?
        .ok_or_else(|| anyhow::anyhow!("rewritten AE6 fixture schema is missing"))?;
    let mut rewritten_reader = BinaryReader::new(rewritten_raw, ByteOrder::Little);
    let rewritten_values = rewritten_schema.read_object(
        &mut rewritten_reader,
        &mut budget,
        TypeTreeParseOptions {
            mode: TypeTreeParseMode::Strict,
        },
    )?;
    assert_eq!(
        rewritten_reader.position(),
        u64::try_from(rewritten_raw.len())?
    );
    assert_ae6_properties(
        &rewritten_values.properties,
        0x80,
        0x0708,
        0x6677_8899,
        AE6_REWRITTEN_LEAF_WIDE,
        AE6_REWRITTEN_ROOT_WIDE,
        12,
    );

    if !unitypy_e2e_enabled("unitypy_differential_covers_ae6_typetree_semantics")? {
        return Ok(());
    }

    let original = tempfile::NamedTempFile::new()?;
    std::fs::write(original.path(), &fixture)?;
    let rewritten_file = tempfile::NamedTempFile::new()?;
    std::fs::write(rewritten_file.path(), &rewritten_bytes)?;
    let py = r#"
import os, sys
from pathlib import Path

repo_root = Path(sys.argv[1]).resolve()
original_path = Path(sys.argv[2])
rewritten_path = Path(sys.argv[3])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402
from UnityPy.helpers.TypeTreeHelper import TypeTreeConfig, read_value  # noqa: E402
from UnityPy.streams.EndianBinaryReader import EndianBinaryReader  # noqa: E402

expected_package = (repo_root / "repo-ref" / "UnityPy" / "UnityPy").resolve()
assert Path(UnityPy.__file__).resolve().is_relative_to(expected_package)

def load_exact(path):
    serialized = UnityPy.load(str(path)).file
    assert serialized.header.version == 22, path
    assert len(serialized.ref_types) == 2, path
    obj = serialized.objects[42]
    raw = obj.get_raw_data()
    assert obj.byte_size == len(raw) == 156, (path, obj.byte_size, len(raw))

    # The public path uses the pinned accelerator and raises unless its own consumed extent
    # exactly matches byte_size. The pure reader exposes Position, giving an explicit second check.
    accelerated = obj.read_typetree(check_read=True)
    reader = EndianBinaryReader(raw, obj.reader.endian)
    pure = read_value(obj.serialized_type.node, reader, TypeTreeConfig(True, serialized, False))
    assert reader.Position == obj.byte_size, (path, reader.Position, obj.byte_size)
    return raw, accelerated, pure

def assert_values(tree, size_value, map_value, managed_value, leaf_wide, root_wide, marker):
    # Contract split: UnityPy intentionally exposes pairs/maps as tuples/lists and UInt64 as
    # Python int. Rust separately asserts UnityValue::Unsigned and traversal statistics.
    assert tree["m_Prefix"] == 0xA5
    assert list(tree["m_SizeAligned"]) == [size_value]
    assert tuple(tree["m_Pair"]) == (0x11, 0x2233)
    assert [tuple(entry) for entry in tree["m_Map"]] == [(1, 0x0203), (4, map_value)]
    assert tree["m_RegistryA"]["m_Version"] == 7
    assert "m_RegistryB" not in tree
    assert "m_NestedRegistry" not in tree["m_Nested"]
    assert tree["m_Nested"]["m_NestedMarker"] == 0x11223344

    reference = tree["m_Reference"]
    assert reference["type"] == {
        "class": "OuterManaged",
        "ns": "Tests",
        "asm": "Assembly-CSharp",
    }
    managed = reference["data"]
    assert "m_ManagedRegistry" not in managed
    assert managed["m_ManagedValue"] == managed_value
    nested_reference = managed["m_NestedReference"]
    assert nested_reference["type"]["class"] == "LeafManaged"
    leaf = nested_reference["data"]
    assert "m_LeafRegistry" not in leaf
    assert type(leaf["m_LeafWide"]) is int
    assert leaf["m_LeafWide"] == leaf_wide > (1 << 63) - 1
    assert leaf["m_LeafMarker"] == 0x01020304
    assert type(tree["m_Wide"]) is int
    assert tree["m_Wide"] == root_wide > (1 << 63) - 1
    assert tree["m_RewriteMarker"] == marker

original_raw, original_accelerated, original_pure = load_exact(original_path)
assert original_raw[9:12] == bytes((0xD1, 0xD2, 0xD3))
for tree in (original_accelerated, original_pure):
    assert_values(
        tree,
        0x7F,
        0x0506,
        0x55667788,
        0x8000000000001234,
        0xFFFFFFFFFFFFFFFD,
        11,
    )

rewritten_raw, rewritten_accelerated, rewritten_pure = load_exact(rewritten_path)
assert rewritten_raw[9:12] == bytes((0xD1, 0xD2, 0xD3))
for tree in (rewritten_accelerated, rewritten_pure):
    assert_values(
        tree,
        0x80,
        0x0708,
        0x66778899,
        0x8000000000005678,
        0xFEDCBA9876543210,
        12,
    )
"#;
    unitypy_check(
        py,
        &[
            repo_root().display().to_string(),
            original.path().display().to_string(),
            rewritten_file.path().display().to_string(),
        ],
    )?;

    Ok(())
}

#[test]
fn unitypy_script_typetree_registry_enables_monobehaviour_parse() -> anyhow::Result<()> {
    if !external_e2e_enabled(
        "unitypy_script_typetree_registry_enables_monobehaviour_parse",
        "UNITYPY_SCRIPT_TYPETREE_E2E",
        "UNITYPY_SCRIPT_TYPETREE_E2E_REQUIRED",
    )? {
        return Ok(());
    }

    let input = std::env::var("UNITYPY_SCRIPT_TYPETREE_INPUT").map(PathBuf::from)?;
    let game_root = std::env::var("UNITYPY_SCRIPT_TYPETREE_GAME_ROOT")
        .ok()
        .map(PathBuf::from);
    let managed_dir = std::env::var("UNITYPY_SCRIPT_TYPETREE_MANAGED_DIR")
        .ok()
        .map(PathBuf::from);

    if game_root.is_some() == managed_dir.is_some() {
        anyhow::bail!(
            "Set exactly one of `UNITYPY_SCRIPT_TYPETREE_GAME_ROOT` or `UNITYPY_SCRIPT_TYPETREE_MANAGED_DIR`."
        );
    }

    let tmp_registry = tempfile::NamedTempFile::new()?;
    let exporter = repo_root()
        .join("scripts")
        .join("export_unitypy_script_typetrees.py");

    let mut exporter_args: Vec<String> = vec![
        "--input".to_string(),
        input.display().to_string(),
        "--output".to_string(),
        tmp_registry.path().display().to_string(),
    ];
    if let Some(root) = game_root {
        exporter_args.push("--game-root".to_string());
        exporter_args.push(root.display().to_string());
    }
    if let Some(dir) = managed_dir {
        exporter_args.push("--managed-dir".to_string());
        exporter_args.push(dir.display().to_string());
    }
    exporter_args.push("--verbose".to_string());

    python_run(&exporter, &exporter_args)?;

    let mut budget = AssetLoadBudget::default();
    let registry = Arc::new(JsonTypeTreeRegistry::from_path(
        tmp_registry.path(),
        &mut budget,
    )?);
    let registry: Arc<dyn TypeTreeRegistry> = registry;

    let bytes = std::fs::read(&input)?;
    let mut serialized = match unity_asset_binary::bundle::BundleParser::from_bytes(bytes.clone()) {
        Ok(bundle) => {
            let node = find_first_serialized_node(&bundle).ok_or_else(|| {
                anyhow::anyhow!("No serialized node found in bundle: {}", input.display())
            })?;
            let node_bytes = bundle.extract_node_data(node)?;
            unity_asset_binary::asset::SerializedFileParser::from_bytes(node_bytes)?
        }
        Err(_) => unity_asset_binary::asset::SerializedFileParser::from_bytes(bytes)?,
    };

    let mut chosen: Option<usize> = None;
    for (idx, info) in serialized.objects().iter().enumerate() {
        if info.class_id() != 114 {
            continue;
        }

        let Some(st) = serialized_type_for_object(&serialized, info) else {
            continue;
        };
        if st.script_id == [0u8; 16] {
            continue;
        }
        if serialized.type_tree_enabled() && !st.type_tree.is_empty() {
            continue;
        }

        chosen = Some(idx);
        break;
    }

    let idx = chosen.ok_or_else(|| {
        anyhow::anyhow!(
            "No stripped MonoBehaviour with non-zero script_id found in: {}",
            input.display()
        )
    })?;
    {
        let info = &serialized.objects()[idx];
        let before =
            unity_asset_binary::object::ObjectHandle::new(&serialized, info).read(&mut budget)?;
        assert!(
            before.has_property("_raw_data_len"),
            "Expected raw preview before attaching script TypeTree registry"
        );
    }

    serialized = serialized.with_type_tree_registry(Some(registry));
    {
        let info = &serialized.objects()[idx];
        let after =
            unity_asset_binary::object::ObjectHandle::new(&serialized, info).read(&mut budget)?;
        assert!(
            !after.has_property("_raw_data_len"),
            "Expected structured parse after attaching script TypeTree registry"
        );
        assert!(
            after.has_property("m_Script"),
            "Expected MonoBehaviour header field `m_Script` to exist after parse"
        );
    }

    Ok(())
}
