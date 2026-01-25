use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;

use unity_asset_binary::bundle::{BundleLoadOptions, BundleParser};
use unity_asset_write::bundle::{BundleEdits, BundleWriter};
use unity_asset_write::{PackerOptions, UnityPyPacker};

fn repo_root() -> PathBuf {
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

fn parse_env_usize(name: &str) -> anyhow::Result<Option<usize>> {
    let Ok(v) = std::env::var(name) else {
        return Ok(None);
    };
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    Ok(Some(v.parse::<usize>()?))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}

fn parse_packer_env() -> anyhow::Result<UnityPyPacker> {
    let Ok(raw) = std::env::var("UNITY_ASSET_EXTERNAL_CORPUS_PACKER") else {
        return Ok(UnityPyPacker::Original);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(UnityPyPacker::Original);
    }
    UnityPyPacker::from_unitypy_str(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid UNITY_ASSET_EXTERNAL_CORPUS_PACKER={raw:?}. Expected one of: none, lz4, lzma, original."
        )
    })
}

fn read_prefix(path: &Path, max: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn sniff_bundle_signature(path: &Path) -> anyhow::Result<Option<String>> {
    let prefix = read_prefix(path, 16)?;
    if prefix.starts_with(b"UnityFS") {
        return Ok(Some("UnityFS".to_string()));
    }
    if prefix.starts_with(b"UnityWeb") {
        return Ok(Some("UnityWeb".to_string()));
    }
    if prefix.starts_with(b"UnityRaw") {
        return Ok(Some("UnityRaw".to_string()));
    }
    Ok(None)
}

fn is_ignored_dir_name(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".svn" | ".hg" | "Library" | "Temp" | "obj" | "Logs"
    )
}

fn walk_files_limited(
    root: &Path,
    mut on_file: impl FnMut(&Path) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());

    while let Some(p) = queue.pop_front() {
        let meta = std::fs::metadata(&p)?;
        if meta.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if is_ignored_dir_name(name) {
                continue;
            }

            for entry in std::fs::read_dir(&p)? {
                let entry = entry?;
                queue.push_back(entry.path());
            }
            continue;
        }

        if meta.is_file() {
            let keep_going = on_file(&p)?;
            if !keep_going {
                break;
            }
        }
    }

    Ok(())
}

#[test]
fn external_corpus_bundle_roundtrip_and_optional_unitypy_validation() -> anyhow::Result<()> {
    let Ok(root) = std::env::var("UNITY_ASSET_EXTERNAL_CORPUS") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    if !root.exists() {
        anyhow::bail!(
            "UNITY_ASSET_EXTERNAL_CORPUS does not exist: {}",
            root.display()
        );
    }

    let limit = parse_env_usize("UNITY_ASSET_EXTERNAL_CORPUS_LIMIT")?.unwrap_or(20);
    let max_bytes =
        parse_env_usize("UNITY_ASSET_EXTERNAL_CORPUS_MAX_BYTES")?.unwrap_or(200_000_000);
    let unitypy_limit = parse_env_usize("UNITY_ASSET_EXTERNAL_CORPUS_UNITYPY_LIMIT")?
        .unwrap_or(3)
        .min(limit);

    let unitypy_enabled = std::env::var("UNITYPY_E2E").ok().as_deref() == Some("1");
    let verbose = env_flag("UNITY_ASSET_EXTERNAL_CORPUS_VERBOSE");
    let packer = parse_packer_env()?;

    let mut attempted = 0usize;
    let mut skipped_too_large = 0usize;
    let mut skipped_not_bundle = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    let mut unitypy_checked = 0usize;

    walk_files_limited(&root, |path| {
        if attempted >= limit {
            return Ok(false);
        }

        let Ok(sig) = sniff_bundle_signature(path) else {
            skipped_not_bundle += 1;
            return Ok(true);
        };
        let Some(sig) = sig else {
            skipped_not_bundle += 1;
            return Ok(true);
        };

        let meta = std::fs::metadata(path)?;
        if meta.len() as usize > max_bytes {
            skipped_too_large += 1;
            return Ok(true);
        }

        attempted += 1;
        if verbose {
            eprintln!(
                "[external-corpus] ({}/{}) {} ({} bytes, sig={})",
                attempted,
                limit,
                path.display(),
                meta.len(),
                sig
            );
        }

        let bytes = std::fs::read(path)?;
        let bundle = match BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy()) {
            Ok(b) => b,
            Err(e) => {
                failures.push((path.to_path_buf(), format!("parse failed: {e}")));
                return Ok(true);
            }
        };

        if bundle.header.signature != sig {
            failures.push((
                path.to_path_buf(),
                format!(
                    "signature mismatch: sniffed={sig} parsed={}",
                    bundle.header.signature
                ),
            ));
            return Ok(true);
        }

        let expected_files: Vec<String> = bundle
            .nodes
            .iter()
            .filter(|n| n.is_file())
            .map(|n| n.name.clone())
            .collect();

        let saved =
            match BundleWriter::save(&bundle, &BundleEdits::default(), PackerOptions { packer }) {
                Ok(b) => b,
                Err(e) => {
                    failures.push((path.to_path_buf(), format!("save failed: {e}")));
                    return Ok(true);
                }
            };

        let reparsed =
            match BundleParser::from_bytes_with_options(saved.clone(), BundleLoadOptions::lazy()) {
                Ok(b) => b,
                Err(e) => {
                    failures.push((path.to_path_buf(), format!("reparse failed: {e}")));
                    return Ok(true);
                }
            };

        if reparsed.header.signature != sig {
            failures.push((
                path.to_path_buf(),
                format!(
                    "reparsed signature mismatch: expected={sig} got={}",
                    reparsed.header.signature
                ),
            ));
            return Ok(true);
        }

        let mut got_files: Vec<String> = reparsed
            .nodes
            .iter()
            .filter(|n| n.is_file())
            .map(|n| n.name.clone())
            .collect();
        got_files.sort();
        let mut expected_files_sorted = expected_files;
        expected_files_sorted.sort();
        if got_files != expected_files_sorted {
            failures.push((
                path.to_path_buf(),
                "directory listing changed after save".to_string(),
            ));
            return Ok(true);
        }

        if unitypy_enabled && unitypy_checked < unitypy_limit {
            let smallest = bundle
                .nodes
                .iter()
                .filter(|n| {
                    n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource")
                })
                .min_by_key(|n| n.size)
                .cloned()
                .or_else(|| bundle.nodes.iter().find(|n| n.is_file()).cloned());

            if let Some(node) = smallest {
                let expected_len = node.size;
                let tmp = tempfile::NamedTempFile::new()?;
                std::fs::write(tmp.path(), &saved)?;

                let py = r#"
import os, sys
repo_root = sys.argv[1]
bundle_path = sys.argv[2]
expected_sig = sys.argv[3]
file_name = sys.argv[4]
expected_len = int(sys.argv[5])
sys.path.insert(0, os.path.join(repo_root, "repo-ref", "UnityPy"))
import UnityPy  # noqa: E402

env = UnityPy.load(bundle_path)
f = env.file
assert getattr(f, "signature", None) == expected_sig
files = getattr(f, "files", None)
assert files is not None
assert file_name in files, (file_name, list(files.keys())[:10])
item = files[file_name]
if hasattr(item, "bytes"):
    got = item.bytes
elif hasattr(item, "reader") and hasattr(item.reader, "bytes"):
    got = item.reader.bytes
else:
    got = None
assert got is not None, type(item)
assert len(got) == expected_len, (len(got), expected_len, type(item))
"#;

                unitypy_check(
                    py,
                    &[
                        repo_root().display().to_string(),
                        tmp.path().display().to_string(),
                        sig.clone(),
                        node.name.clone(),
                        expected_len.to_string(),
                    ],
                )?;
                unitypy_checked += 1;
            }
        }

        Ok(true)
    })?;

    if attempted == 0 {
        anyhow::bail!(
            "No candidate bundles processed under {} (skipped_not_bundle={}, skipped_too_large={}).",
            root.display(),
            skipped_not_bundle,
            skipped_too_large
        );
    }

    if !failures.is_empty() {
        let mut msg = format!(
            "External corpus failures: {} (attempted={}, skipped_not_bundle={}, skipped_too_large={}).\n",
            failures.len(),
            attempted,
            skipped_not_bundle,
            skipped_too_large
        );
        for (path, err) in failures.iter().take(10) {
            msg.push_str(&format!("- {}: {}\n", path.display(), err));
        }
        anyhow::bail!(msg);
    }

    eprintln!(
        "External corpus OK: attempted={}, skipped_not_bundle={}, skipped_too_large={}, unitypy_checked={}",
        attempted, skipped_not_bundle, skipped_too_large, unitypy_checked
    );

    Ok(())
}
