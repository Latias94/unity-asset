use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unity_asset_binary::bundle::{AssetBundle, BundleLoadOptions, BundleParser, DirectoryNode};
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::shared_bytes::SharedBytes;

pub(crate) fn bundle_list_options() -> BundleLoadOptions {
    BundleLoadOptions::lazy()
}

pub(crate) fn looks_like_bundle_prefix(prefix: &[u8]) -> bool {
    if prefix.len() < 8 {
        return false;
    }
    if prefix.starts_with(b"UnityFS\0") || prefix.starts_with(b"UnityRaw") {
        return true;
    }
    if prefix.starts_with(b"UnityWeb") {
        if prefix.starts_with(b"UnityWebData") || prefix.starts_with(b"TuanjieWebData") {
            return false;
        }
        return true;
    }
    false
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
    #[cfg(feature = "mmap")]
    {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let shared = SharedBytes::Mmap(Arc::new(mmap));
        let len = shared.len();
        return Ok(BundleParser::from_shared_range_with_options(
            shared,
            0..len,
            options,
        )?);
    }

    #[cfg(not(feature = "mmap"))]
    {
        let bytes = std::fs::read(path)?;
        Ok(BundleParser::from_bytes_with_options(bytes, options)?)
    }
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
