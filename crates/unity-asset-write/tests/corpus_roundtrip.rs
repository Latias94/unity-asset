use std::path::{Path, PathBuf};

use unity_asset_binary::bundle::AssetBundle;
use unity_asset_binary::file::UnityFile;
use unity_asset_write::bundle::{BundleEdits, BundleWriter};
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};
use unity_asset_write::{PackerOptions, UnityPyPacker};

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `.../crates/unity-asset-write`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should be two levels above unity-asset-write crate")
        .to_path_buf()
}

fn samples_dir() -> PathBuf {
    repo_root().join("tests").join("samples")
}

fn list_sample_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let dir = samples_dir();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn roundtrip_bundle_noop(bundle: &AssetBundle) -> anyhow::Result<()> {
    let original_names: Vec<String> = bundle
        .nodes
        .iter()
        .filter(|n| n.is_file())
        .map(|n| n.name.clone())
        .collect();

    let saved = BundleWriter::save(
        bundle,
        &BundleEdits::default(),
        PackerOptions {
            packer: UnityPyPacker::Original,
        },
    )?;

    let reparsed = unity_asset_binary::bundle::BundleParser::from_bytes(saved)?;

    let mut saved_names: Vec<String> = reparsed
        .nodes
        .iter()
        .filter(|n| n.is_file())
        .map(|n| n.name.clone())
        .collect();
    saved_names.sort();
    let mut original_names_sorted = original_names;
    original_names_sorted.sort();
    assert_eq!(saved_names, original_names_sorted);

    // Stronger no-op check: a subset of nodes should preserve extracted bytes.
    // (Avoids being too slow while still catching format regressions.)
    for node in bundle.nodes.iter().filter(|n| n.is_file()).take(3) {
        let a = bundle.extract_node_data(node)?;
        let b = reparsed
            .nodes
            .iter()
            .find(|n| n.is_file() && n.name == node.name)
            .map(|n| reparsed.extract_node_data(n))
            .transpose()?
            .expect("saved bundle should contain the same node name");
        assert_eq!(a, b, "bundle node bytes mismatch for {}", node.name);
    }

    Ok(())
}

fn corpus_max_assets_per_bundle() -> usize {
    std::env::var("UNITY_ASSET_CORPUS_MAX_ASSETS_PER_BUNDLE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(8)
}

#[test]
fn corpus_roundtrip_noop_save_is_loadable() -> anyhow::Result<()> {
    let samples = list_sample_files();
    assert!(
        !samples.is_empty(),
        "no sample files found under {}",
        samples_dir().display()
    );

    let mut tested = 0usize;
    let mut skipped = 0usize;

    for path in samples {
        let bytes = std::fs::read(&path)?;
        let parsed = match unity_asset_binary::file::load_unity_file_from_memory(bytes) {
            Ok(v) => v,
            Err(_) => {
                // `tests/samples` can also contain raw resource blobs (`.resS`/`.resource`) or
                // other non-container artifacts; skip those in this corpus test.
                skipped += 1;
                continue;
            }
        };
        tested += 1;

        match parsed {
            UnityFile::AssetBundle(bundle) => {
                roundtrip_bundle_noop(&bundle).map_err(|e| {
                    anyhow::anyhow!(
                        "bundle noop roundtrip failed for {}: {e}",
                        file_label(&path)
                    )
                })?;

                // Ensure embedded SerializedFiles can be rebuilt and reloaded.
                let max_assets = corpus_max_assets_per_bundle();
                for (asset_index, sf) in bundle.assets.iter().enumerate().take(max_assets) {
                    let out = SerializedFileWriter::save(sf, &SerializedFileEdits::default())?;
                    let reparsed =
                        unity_asset_binary::asset::SerializedFileParser::from_bytes(out)?;
                    assert_eq!(reparsed.header.version, sf.header.version);
                    assert_eq!(reparsed.objects.len(), sf.objects.len());
                    assert_eq!(
                        reparsed.externals.len(),
                        sf.externals.len(),
                        "externals len mismatch for {} asset_index={}",
                        file_label(&path),
                        asset_index
                    );
                }
            }
            UnityFile::SerializedFile(sf) => {
                let out = SerializedFileWriter::save(&sf, &SerializedFileEdits::default())?;
                let reparsed = unity_asset_binary::asset::SerializedFileParser::from_bytes(out)?;
                assert_eq!(reparsed.header.version, sf.header.version);
                assert_eq!(reparsed.objects.len(), sf.objects.len());
            }
            UnityFile::WebFile(web) => {
                // Corpus currently doesn't vendor WebFile samples, but keep this branch for completeness.
                let saved = unity_asset_write::webfile::WebFileWriter::save(
                    &web,
                    &unity_asset_write::webfile::WebFileEdits::default(),
                    unity_asset_write::webfile::WebFilePacker::None,
                    None,
                )?;
                let reparsed = unity_asset_binary::webfile::WebFile::from_bytes(saved)?;
                assert_eq!(reparsed.files().len(), web.files().len());
            }
        }
    }

    assert!(
        tested > 0,
        "no Unity container samples found under {} (skipped={})",
        samples_dir().display(),
        skipped
    );

    Ok(())
}
