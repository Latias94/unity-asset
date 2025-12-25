use anyhow::Result;
use std::path::{Path, PathBuf};
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
    let mut out: Vec<PathBuf> = Vec::new();
    if input.is_dir() {
        collect_files_recursive(input, &mut out)?;
        out.sort();
        out.dedup();
    } else {
        out.push(input.to_path_buf());
    }
    Ok(out)
}

fn collect_files_recursive(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            collect_files_recursive(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
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
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max_len];
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
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
