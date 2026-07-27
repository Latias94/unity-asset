use anyhow::Result;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use unity_asset::AssetLoadBudget;
use unity_asset_binary::bundle::{AssetBundle, BundleLoadOptions};

const BUNDLE_SNIFF_PREFIX_LEN: usize = 16;

pub(crate) fn bundle_list_options() -> BundleLoadOptions {
    BundleLoadOptions::lazy()
}

pub(crate) fn sniff_unity_file_kind_prefix(
    prefix: &[u8],
) -> Option<unity_asset_binary::file::UnityFileKind> {
    unity_asset_binary::file::sniff_unity_file_kind_prefix(prefix)
}

pub(crate) fn is_assetbundle_path(path: &Path) -> bool {
    let Ok(prefix) = read_prefix(path, BUNDLE_SNIFF_PREFIX_LEN) else {
        return false;
    };
    sniff_unity_file_kind_prefix(&prefix)
        == Some(unity_asset_binary::file::UnityFileKind::AssetBundle)
}

pub(crate) fn collect_candidate_paths(input: &Path) -> Result<Vec<PathBuf>> {
    collect_candidate_paths_filtered(input, |_| true)
}

pub(crate) fn collect_candidate_paths_filtered(
    input: &Path,
    mut should_descend: impl FnMut(&Path) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    if input.is_dir() {
        collect_files_recursive(input, &mut out, &mut should_descend)?;
        out.sort();
        out.dedup();
    } else {
        out.push(input.to_path_buf());
    }
    Ok(out)
}

pub(crate) fn collect_candidate_paths_filtered_budgeted(
    input: &Path,
    budget: &mut AssetLoadBudget,
    mut should_descend: impl FnMut(&Path) -> bool,
) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    if input.is_dir() {
        collect_files_recursive_budgeted(input, &mut output, &mut should_descend, 0, budget)?;
        output.sort_unstable();
        output.dedup();
    } else {
        budget.consume_entries(1)?;
        let mut path = PathBuf::new();
        let path_bytes = input.as_os_str().len();
        let allocation = path_bytes
            .checked_add(size_of::<PathBuf>())
            .ok_or_else(|| anyhow::anyhow!("candidate path allocation overflow"))?;
        budget.check_bytes(u64::try_from(allocation)?)?;
        path.try_reserve_exact(path_bytes)?;
        output.try_reserve_exact(1)?;
        budget.consume_bytes(u64::try_from(allocation)?)?;
        path.push(input);
        output.push(path);
    }
    Ok(output)
}

fn collect_files_recursive(
    root: &Path,
    out: &mut Vec<PathBuf>,
    should_descend: &mut impl FnMut(&Path) -> bool,
) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && should_descend(&path) {
            collect_files_recursive(&path, out, should_descend)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_files_recursive_budgeted(
    root: &Path,
    output: &mut Vec<PathBuf>,
    should_descend: &mut impl FnMut(&Path) -> bool,
    depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    budget.observe_depth(depth)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        budget.consume_entries(1)?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            budget.consume_bytes(u64::try_from(path.as_os_str().len())?)?;
            if should_descend(&path) {
                let child_depth = depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("candidate directory depth overflow"))?;
                collect_files_recursive_budgeted(
                    &path,
                    output,
                    should_descend,
                    child_depth,
                    budget,
                )?;
            }
        } else if file_type.is_file() {
            let additional = if output.len() == output.capacity() {
                output.capacity().max(1)
            } else {
                0
            };
            let slot_bytes = additional
                .checked_mul(size_of::<PathBuf>())
                .ok_or_else(|| anyhow::anyhow!("candidate path slot allocation overflow"))?;
            let allocation = path
                .as_os_str()
                .len()
                .checked_add(slot_bytes)
                .ok_or_else(|| anyhow::anyhow!("candidate path allocation overflow"))?;
            budget.check_bytes(u64::try_from(allocation)?)?;
            if slot_bytes != 0 {
                output.try_reserve_exact(additional)?;
            }
            budget.consume_bytes(u64::try_from(allocation)?)?;
            output.push(path);
        }
    }
    Ok(())
}

pub(crate) fn read_prefix(path: &Path, max_len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; max_len];
    let n = read_prefix_into(path, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

pub(crate) fn read_prefix_into(path: &Path, output: &mut [u8]) -> Result<usize> {
    use std::io::Read;

    Ok(std::fs::File::open(path)?.read(output)?)
}

pub(crate) fn load_bundle_for_list(
    path: &Path,
    options: BundleLoadOptions,
    budget: &mut AssetLoadBudget,
) -> Result<AssetBundle> {
    Ok(unity_asset_binary::file::load_bundle_file_with_options_and_budget(path, options, budget)?)
}
