//! Export Texture2D objects from a Unity binary file.
//!
//! Run:
//! `cargo run -p unity-asset-decode --example export_textures --features texture -- <path> <out_dir>`
//!
//! For broader format support, enable `texture-advanced` (or `full`):
//! `cargo run -p unity-asset-decode --example export_textures --features texture-advanced -- <path> <out_dir>`

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use unity_asset_binary::file::{UnityFile, load_unity_file_with_budget};
use unity_asset_binary::object::ObjectHandle;
use unity_asset_binary::{BinaryError, Result};
use unity_asset_core::{AssetLoadBudget, constants::class_ids};
use unity_asset_decode::texture::{Texture2DConverter, TextureExporter};

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| BinaryError::invalid_format("missing <path>"))?;
    let out_dir = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or_else(|| BinaryError::invalid_format("missing <out_dir>"))?;

    std::fs::create_dir_all(&out_dir).map_err(|e| {
        BinaryError::generic(format!(
            "Failed to create output dir {}: {}",
            out_dir.display(),
            e
        ))
    })?;

    let mut budget = AssetLoadBudget::default();
    let file = load_unity_file_with_budget(&path, &mut budget)?;
    let converter = Texture2DConverter::new();
    let mut budget = AssetLoadBudget::default();

    let mut exported = 0usize;
    let mut seen = 0usize;

    let mut process = |handle: ObjectHandle<'_>| -> Result<()> {
        if handle.class_id() != class_ids::TEXTURE_2D {
            return Ok(());
        }
        seen += 1;
        let obj = handle.read(&mut budget)?;
        let tex = converter.from_unity_object(&obj)?;
        let image = converter.decode_to_image(&tex)?;

        let stem = obj
            .name()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pathid_{}", obj.path_id()));
        let file_name = format!("{}_{}.png", stem, obj.path_id());
        let out_path = out_dir.join(file_name);

        let output = File::create(&out_path).map_err(|error| {
            BinaryError::generic(format!("Failed to create {}: {error}", out_path.display()))
        })?;
        let mut output = BufWriter::new(output);
        TextureExporter::write_png(&image, &mut output)?;
        output.flush().map_err(|error| {
            BinaryError::generic(format!("Failed to flush {}: {error}", out_path.display()))
        })?;
        exported += 1;
        Ok(())
    };

    match file {
        UnityFile::SerializedFile(sf) => {
            for h in sf.object_handles() {
                process(h)?;
            }
        }
        UnityFile::AssetBundle(bundle) => {
            for sf in &bundle.assets {
                for h in sf.object_handles() {
                    process(h)?;
                }
            }
        }
        UnityFile::WebFile(_) => {
            return Err(BinaryError::invalid_format(
                "WebFile container: inspect entries through AssetWorkspace or the typed CLI",
            ));
        }
    }

    println!("scanned Texture2D: {}", seen);
    println!("exported: {}", exported);
    println!("output: {}", out_dir.display());

    Ok(())
}
