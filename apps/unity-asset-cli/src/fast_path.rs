use anyhow::Result;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use unity_asset::AssetLoadBudget;
use unity_asset_binary::bundle::{AssetBundle, BundleLoadOptions, DirectoryNode};
use unity_asset_binary::error::BinaryError;

const BUNDLE_SNIFF_PREFIX_LEN: usize = 16;
const SERIALIZED_SNIFF_PREFIX_LEN: usize = 64;

pub(crate) fn bundle_list_options() -> BundleLoadOptions {
    BundleLoadOptions::lazy()
}

pub(crate) fn looks_like_unityfs_bundle_prefix(prefix: &[u8]) -> bool {
    unity_asset_binary::file::looks_like_unityfs_bundle_prefix(prefix)
}

pub(crate) fn sniff_unity_file_kind_prefix(
    prefix: &[u8],
) -> Option<unity_asset_binary::file::UnityFileKind> {
    unity_asset_binary::file::sniff_unity_file_kind_prefix(prefix)
}

pub(crate) fn is_unityfs_bundle_path(path: &Path) -> bool {
    let Ok(prefix) = read_prefix(path, BUNDLE_SNIFF_PREFIX_LEN) else {
        return false;
    };
    looks_like_unityfs_bundle_prefix(&prefix)
}

pub(crate) fn is_assetbundle_path(path: &Path) -> bool {
    let Ok(prefix) = read_prefix(path, BUNDLE_SNIFF_PREFIX_LEN) else {
        return false;
    };
    sniff_unity_file_kind_prefix(&prefix)
        == Some(unity_asset_binary::file::UnityFileKind::AssetBundle)
}

pub(crate) fn is_serialized_file_path(path: &Path) -> bool {
    let Ok(prefix) = read_prefix(path, SERIALIZED_SNIFF_PREFIX_LEN) else {
        return false;
    };
    sniff_unity_file_kind_prefix(&prefix)
        == Some(unity_asset_binary::file::UnityFileKind::SerializedFile)
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
            let slot_bytes = usize::from(output.len() == output.capacity())
                .checked_mul(size_of::<PathBuf>())
                .ok_or_else(|| anyhow::anyhow!("candidate path slot allocation overflow"))?;
            let allocation = path
                .as_os_str()
                .len()
                .checked_add(slot_bytes)
                .ok_or_else(|| anyhow::anyhow!("candidate path allocation overflow"))?;
            budget.check_bytes(u64::try_from(allocation)?)?;
            if slot_bytes != 0 {
                output.try_reserve_exact(1)?;
            }
            budget.consume_bytes(u64::try_from(allocation)?)?;
            output.push(path);
        }
    }
    Ok(())
}

pub(crate) fn path_matches_requested(candidate: &Path, requested: &Path) -> bool {
    if candidate == requested {
        return true;
    }
    let candidate_str = candidate.to_string_lossy().replace('\\', "/");
    let requested_str = requested.to_string_lossy().replace('\\', "/");
    if candidate_str.ends_with(&requested_str) || requested_str.ends_with(&candidate_str) {
        return true;
    }
    candidate.file_name() == requested.file_name()
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

pub(crate) fn load_bundle_for_list(path: &Path, options: BundleLoadOptions) -> Result<AssetBundle> {
    Ok(unity_asset_binary::file::load_bundle_file_with_options(
        path, options,
    )?)
}

pub(crate) fn bundle_asset_nodes(bundle: &AssetBundle) -> Vec<DirectoryNode> {
    bundle
        .nodes
        .iter()
        .filter(|n| n.is_file())
        .filter(|n| !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
        .cloned()
        .collect()
}

pub(crate) fn node_range(node: &DirectoryNode) -> Result<(usize, usize)> {
    let end_u64 = node
        .offset
        .checked_add(node.size)
        .ok_or_else(|| anyhow::anyhow!("node offset+size overflow"))?;
    let start = usize::try_from(node.offset).map_err(|_| {
        anyhow::anyhow!(BinaryError::ResourceLimitExceeded(
            "Node offset does not fit in usize".to_string()
        ))
    })?;
    let end = usize::try_from(end_u64).map_err(|_| {
        anyhow::anyhow!(BinaryError::ResourceLimitExceeded(
            "Node end offset does not fit in usize".to_string()
        ))
    })?;
    if start > end {
        anyhow::bail!("node slice start exceeds end");
    }
    Ok((start, end))
}
