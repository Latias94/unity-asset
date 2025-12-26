//! Sniff Unity binary file kind from a small prefix.
//!
//! Run:
//! `cargo run -p unity-asset-binary --example sniff_kind -- <path>`

use std::io::Read;
use std::path::PathBuf;
use unity_asset_binary::file::sniff_unity_file_kind_prefix;

fn main() -> unity_asset_binary::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/samples/char_118_yuki.ab"));

    if path.is_dir() {
        return Err(unity_asset_binary::BinaryError::invalid_format(
            "Please provide a file path (not a directory)",
        ));
    }

    let mut f = std::fs::File::open(&path).map_err(|e| {
        unity_asset_binary::BinaryError::generic(format!(
            "Failed to open file {}: {}",
            path.display(),
            e
        ))
    })?;

    let mut prefix = vec![0u8; 64];
    let n = f.read(&mut prefix).map_err(|e| {
        unity_asset_binary::BinaryError::generic(format!(
            "Failed to read file {}: {}",
            path.display(),
            e
        ))
    })?;
    prefix.truncate(n);

    println!("path: {}", path.display());
    match sniff_unity_file_kind_prefix(&prefix) {
        Some(kind) => println!("sniff: {:?}", kind),
        None => println!("sniff: <unknown>"),
    }
    Ok(())
}
