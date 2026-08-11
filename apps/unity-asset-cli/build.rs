use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;

const SOURCE_COMMIT_ENV: &str = "UNITY_ASSET_SOURCE_COMMIT";
const BUILD_TARGET_ENV: &str = "UNITY_ASSET_BUILD_TARGET";

fn main() {
    println!("cargo:rerun-if-env-changed={SOURCE_COMMIT_ENV}");
    let build_target = env::var("TARGET").expect("Cargo must provide the build target");
    assert!(
        !build_target.is_empty(),
        "Cargo provided an empty build target"
    );
    println!("cargo:rustc-env={BUILD_TARGET_ENV}={build_target}");
    let source_commit = env::var(SOURCE_COMMIT_ENV)
        .ok()
        .or_else(package_vcs_commit)
        .or_else(repository_head)
        .unwrap_or_else(|| "unknown".to_owned());
    assert!(
        source_commit == "unknown" || is_full_git_commit(&source_commit),
        "{SOURCE_COMMIT_ENV} must be a full lowercase Git commit ID"
    );
    println!("cargo:rustc-env={SOURCE_COMMIT_ENV}={source_commit}");
}

fn package_vcs_commit() -> Option<String> {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    let path = manifest_dir.join(".cargo_vcs_info.json");
    let encoded = match fs::read(&path) {
        Ok(encoded) => encoded,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => panic!("cannot read {}: {error}", path.display()),
    };
    println!("cargo:rerun-if-changed={}", path.display());
    let document: serde_json::Value = serde_json::from_slice(&encoded)
        .unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()));
    document
        .pointer("/git/sha1")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn repository_head() -> Option<String> {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    let output = Command::new("git")
        .args(["-C"])
        .arg(&manifest_dir)
        .args([
            "rev-parse",
            "HEAD",
            "--git-path",
            "HEAD",
            "--git-path",
            "logs/HEAD",
            "--git-path",
            "packed-refs",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut lines = stdout.lines();
    let commit = lines.next()?.to_owned();
    for raw_path in lines {
        let raw_path = PathBuf::from(raw_path);
        let path = if raw_path.is_absolute() {
            raw_path
        } else {
            manifest_dir.join(raw_path)
        };
        if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Some(commit)
}

fn is_full_git_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
