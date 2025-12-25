//! Export Texture2D objects from a Unity binary file.
//!
//! Run:
//! `cargo run -p unity-asset-decode --example export_textures --features texture -- <path> <out_dir>`
//!
//! For broader format support, enable `texture-advanced` (or `full`):
//! `cargo run -p unity-asset-decode --example export_textures --features texture-advanced -- <path> <out_dir>`

use std::path::PathBuf;
use unity_asset_core::constants::class_ids;
use unity_asset_decode::file::load_unity_file;
use unity_asset_decode::texture::Texture2DConverter;
use unity_asset_decode::{object::ObjectHandle, unity_version::UnityVersion};

fn main() -> unity_asset_decode::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| unity_asset_decode::BinaryError::invalid_format("missing <path>"))?;
    let out_dir = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or_else(|| unity_asset_decode::BinaryError::invalid_format("missing <out_dir>"))?;

    std::fs::create_dir_all(&out_dir).map_err(|e| {
        unity_asset_decode::BinaryError::generic(format!(
            "Failed to create output dir {}: {}",
            out_dir.display(),
            e
        ))
    })?;

    let file = load_unity_file(&path)?;
    let converter = Texture2DConverter::new(UnityVersion::default());

    let mut exported = 0usize;
    let mut seen = 0usize;

    let mut process = |handle: ObjectHandle<'_>| -> unity_asset_decode::Result<()> {
        if handle.class_id() != class_ids::TEXTURE_2D {
            return Ok(());
        }
        seen += 1;
        let obj = handle.read()?;
        let tex = converter.from_unity_object(&obj)?;
        let image = converter.decode_to_image(&tex)?;

        let stem = obj
            .name()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pathid_{}", obj.path_id()));
        let file_name = format!("{}_{}.png", stem, obj.path_id());
        let out_path = out_dir.join(file_name);

        unity_asset_decode::texture::TextureExporter::export_png(&image, &out_path)?;
        exported += 1;
        Ok(())
    };

    match file {
        unity_asset_decode::file::UnityFile::SerializedFile(sf) => {
            for h in sf.object_handles() {
                process(h)?;
            }
        }
        unity_asset_decode::file::UnityFile::AssetBundle(bundle) => {
            for sf in &bundle.assets {
                for h in sf.object_handles() {
                    process(h)?;
                }
            }
        }
        unity_asset_decode::file::UnityFile::WebFile(_) => {
            return Err(unity_asset_decode::BinaryError::invalid_format(
                "WebFile container: pick an entry and parse it via unity-asset Environment/CLI",
            ));
        }
    }

    println!("scanned Texture2D: {}", seen);
    println!("exported: {}", exported);
    println!("output: {}", out_dir.display());

    Ok(())
}
