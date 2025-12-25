use crate::fast_path;
use crate::shared::AppContext;
use anyhow::Result;
use std::path::PathBuf;

pub(crate) fn run(input: PathBuf, filter: String, verbose: bool, _ctx: &AppContext) -> Result<()> {
    let candidate_paths = fast_path::collect_candidate_paths(&input)?;

    let filter_lc = filter.to_ascii_lowercase();
    let mut found_any = false;

    for path in candidate_paths {
        if !fast_path::is_assetbundle_path(&path) {
            continue;
        }

        let options = fast_path::bundle_list_options();
        let bundle = match fast_path::load_bundle_for_list(&path, options) {
            Ok(v) => v,
            Err(_) => continue,
        };

        found_any = true;

        let asset_files = bundle
            .nodes
            .iter()
            .filter(|n| n.is_file())
            .filter(|n| !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
            .count();

        println!(
            "Bundle: {} (nodes={}, asset_files={}, assets_loaded={})",
            path.to_string_lossy(),
            bundle.nodes.len(),
            asset_files,
            bundle.assets.len()
        );

        let mut nodes: Vec<_> = bundle.nodes.iter().filter(|n| n.is_file()).collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        for node in nodes {
            if !filter_lc.is_empty() && !node.name.to_ascii_lowercase().contains(&filter_lc) {
                continue;
            }
            if verbose {
                println!(
                    "  - {} (offset={}, size={}, flags={})",
                    node.name, node.offset, node.size, node.flags
                );
            } else {
                println!("  - {}", node.name);
            }
        }
    }

    if !found_any {
        println!("⚠ No AssetBundles found in {:?}", input);
        return Ok(());
    }

    Ok(())
}
