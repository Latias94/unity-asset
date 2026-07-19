use super::path::canonicalize_if_exists;
use super::*;

impl Environment {
    fn normalize_stream_path(stream_path: &str) -> String {
        let mut p = stream_path.trim().to_string();
        if let Some(rest) = p.strip_prefix("archive:/") {
            p = rest.to_string();
        }
        p = p.replace('\\', "/");
        while p.starts_with("./") {
            p = p.trim_start_matches("./").to_string();
        }
        p
    }

    fn cab_prefix_from_normalized(normalized: &str) -> Option<String> {
        let needle = "CAB-";
        let start = normalized.find(needle)?;
        let mut hex = String::with_capacity(32);
        for ch in normalized[start + needle.len()..].chars() {
            if ch.is_ascii_hexdigit() && hex.len() < 32 {
                hex.push(ch);
            } else {
                break;
            }
        }
        if hex.len() == 32 {
            Some(format!("CAB-{}", hex))
        } else {
            None
        }
    }

    fn find_exact_bundle_resource_node<'a>(
        bundle: &'a AssetBundle,
        stream_path: &str,
    ) -> Option<&'a unity_asset_binary::bundle::types::DirectoryNode> {
        let normalized = Self::normalize_stream_path(stream_path);
        if normalized.is_empty() {
            return None;
        }

        let file_name = Path::new(&normalized)
            .file_name()
            .and_then(|name| name.to_str());

        let mut nodes: Vec<&unity_asset_binary::bundle::types::DirectoryNode> =
            bundle.nodes.iter().filter(|n| n.is_file()).collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));

        if let Some(node) = nodes
            .iter()
            .find(|node| node.name.replace('\\', "/") == normalized)
        {
            return Some(*node);
        }

        for node in nodes {
            let node_norm = node.name.replace('\\', "/");
            if let Some(file_name) = file_name
                && Path::new(&node_norm).file_name().and_then(|n| n.to_str()) == Some(file_name)
            {
                return Some(node);
            }
        }

        None
    }

    fn find_fuzzy_bundle_resource_node<'a>(
        bundle: &'a AssetBundle,
        stream_path: &str,
    ) -> Option<&'a unity_asset_binary::bundle::types::DirectoryNode> {
        let normalized = Self::normalize_stream_path(stream_path);
        let cab_prefix = Self::cab_prefix_from_normalized(&normalized)?;
        let mut nodes: Vec<_> = bundle.nodes.iter().filter(|node| node.is_file()).collect();
        nodes.sort_by(|left, right| left.name.cmp(&right.name));

        nodes.into_iter().find(|node| {
            let normalized_name = node.name.replace('\\', "/");
            let base = Path::new(&normalized_name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&normalized_name);
            (normalized_name.ends_with(".resS") || normalized_name.ends_with(".resource"))
                && (normalized_name.starts_with(&cab_prefix) || base.starts_with(&cab_prefix))
        })
    }

    fn stream_fs_candidates(source_path: &Path, stream_path: &str) -> Vec<PathBuf> {
        let base_dir = source_path.parent().unwrap_or_else(|| Path::new("."));
        let normalized = Self::normalize_stream_path(stream_path);
        let cab_prefix = Self::cab_prefix_from_normalized(&normalized);

        let mut dirs = vec![base_dir.to_path_buf(), base_dir.join("StreamingAssets")];
        if let Some(cab) = &cab_prefix {
            dirs.push(base_dir.join(cab));
            dirs.push(base_dir.join("StreamingAssets").join(cab));
        }
        dirs.sort();
        dirs.dedup();

        let mut candidates: Vec<PathBuf> = Vec::new();

        // If the path already exists as-is (e.g. absolute path), try it first.
        candidates.push(PathBuf::from(stream_path));

        if !normalized.is_empty() {
            candidates.push(base_dir.join(&normalized));
            if let Some(file_name) = Path::new(&normalized).file_name() {
                candidates.push(base_dir.join(file_name));
                candidates.push(base_dir.join("StreamingAssets").join(file_name));
            }
        }

        // Unity often stores resources as `CAB-<hash><n>.resource` / `.resS` on disk,
        // while the stream path references `CAB-<hash>.resource` (no suffix).
        if let Some(cab) = &cab_prefix {
            for ext in ["resource", "resS"] {
                for dir in &dirs {
                    candidates.push(dir.join(format!("{cab}.{ext}")));
                }
                for suffix in 1..=9 {
                    for dir in &dirs {
                        candidates.push(dir.join(format!("{cab}{suffix}.{ext}")));
                    }
                }
            }

            // Targeted directory scans (non-recursive) to catch suffixes beyond 9.
            for dir in &dirs {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        if !(name.ends_with(".resS") || name.ends_with(".resource")) {
                            continue;
                        }
                        if name.starts_with(cab) {
                            candidates.push(path);
                        }
                    }
                }
            }
        }

        candidates.sort();
        candidates.dedup();
        candidates
    }

    /// Read streamed resource bytes from a loaded bundle.
    ///
    /// This is primarily used for `AudioClip` / `Texture2D` stream data (`m_StreamData`) when the
    /// referenced resource file is contained inside the same bundle (e.g. `.resS` / `.resource`).
    pub fn read_bundle_stream_data<P: AsRef<Path>>(
        &self,
        bundle_path: P,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let bundle_path = canonicalize_if_exists(bundle_path.as_ref());
        let bundle_source = BinarySource::path(&bundle_path);
        self.read_bundle_stream_data_source(&bundle_source, stream_path, offset, size)
    }

    pub fn read_bundle_stream_data_source(
        &self,
        bundle_source: &BinarySource,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let bundle = self.bundles.get(bundle_source).ok_or_else(|| {
            UnityAssetError::format(format!(
                "AssetBundle source not loaded: {}",
                bundle_source.describe()
            ))
        })?;

        let node = Self::find_exact_bundle_resource_node(bundle, stream_path)
            .or_else(|| Self::find_fuzzy_bundle_resource_node(bundle, stream_path))
            .ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Resource node not found in bundle {}: {}",
                    bundle_source.describe(),
                    stream_path
                ))
            })?;
        Self::read_bundle_resource_node(bundle, node, offset, size)
    }

    fn read_bundle_resource_node(
        bundle: &AssetBundle,
        node: &unity_asset_binary::bundle::types::DirectoryNode,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let node_start: usize = node.offset.try_into().map_err(|_| {
            UnityAssetError::format(format!("Resource node offset overflow: {}", node.offset))
        })?;
        let node_size: usize = node.size.try_into().map_err(|_| {
            UnityAssetError::format(format!("Resource node size overflow: {}", node.size))
        })?;
        let data = bundle.data();
        if node_start.saturating_add(node_size) > data.len() {
            return Err(UnityAssetError::format(format!(
                "Resource node out of bounds: name={}, offset={}, size={}, bundle_len={}",
                node.name,
                node.offset,
                node.size,
                data.len()
            )));
        }

        let offset_usize: usize = offset
            .try_into()
            .map_err(|_| UnityAssetError::format(format!("Stream offset overflow: {}", offset)))?;
        let size_usize: usize = size
            .try_into()
            .map_err(|_| UnityAssetError::format(format!("Stream size overflow: {}", size)))?;

        if offset_usize.saturating_add(size_usize) > node_size {
            return Err(UnityAssetError::format(format!(
                "Stream range out of bounds: name={}, stream_offset={}, stream_size={}, node_size={}",
                node.name, offset, size, node.size
            )));
        }

        let start = node_start.saturating_add(offset_usize);
        let end = start.saturating_add(size_usize);
        Ok(data[start..end].to_vec())
    }

    fn find_webfile_resource_entry(web: &WebFile, stream_path: &str) -> Option<String> {
        let normalized = Self::normalize_stream_path(stream_path);
        if normalized.is_empty() {
            return None;
        }

        let file_name = Path::new(&normalized)
            .file_name()
            .and_then(|name| name.to_str());

        let mut names: Vec<&String> = web.files.iter().map(|f| &f.name).collect();
        names.sort();

        if let Some(name) = names
            .iter()
            .find(|name| name.replace('\\', "/") == normalized)
        {
            return Some((**name).clone());
        }

        for name in &names {
            let name_norm = name.replace('\\', "/");
            if let Some(file_name) = file_name
                && Path::new(&name_norm).file_name().and_then(|n| n.to_str()) == Some(file_name)
            {
                return Some((*name).clone());
            }
        }

        let cab_prefix = Self::cab_prefix_from_normalized(&normalized);
        if let Some(cab) = cab_prefix {
            for name in &names {
                let name_norm = name.replace('\\', "/");
                let base = Path::new(&name_norm)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&name_norm);
                if (name_norm.ends_with(".resS") || name_norm.ends_with(".resource"))
                    && (name_norm.starts_with(&cab) || base.starts_with(&cab))
                {
                    return Some((*name).clone());
                }
            }
        }

        None
    }

    fn try_read_webfile_stream_data(
        &self,
        web_path: &PathBuf,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        let web = self.webfiles.get(web_path).ok_or_else(|| {
            UnityAssetError::format(format!("WebFile source not loaded: {:?}", web_path))
        })?;

        let Some(entry_name) = Self::find_webfile_resource_entry(web, stream_path) else {
            return Ok(None);
        };

        let bytes = web.extract_file(&entry_name).map_err(|e| {
            UnityAssetError::format(format!(
                "Failed to extract WebFile entry {:?} from {:?}: {}",
                entry_name, web_path, e
            ))
        })?;

        let offset_usize: usize = offset
            .try_into()
            .map_err(|_| UnityAssetError::format(format!("Stream offset overflow: {}", offset)))?;
        let size_usize: usize = size
            .try_into()
            .map_err(|_| UnityAssetError::format(format!("Stream size overflow: {}", size)))?;

        if offset_usize.saturating_add(size_usize) > bytes.len() {
            return Err(UnityAssetError::format(format!(
                "Stream range out of bounds in WebFile entry {}: offset={}, size={}, entry_len={}",
                entry_name,
                offset,
                size,
                bytes.len()
            )));
        }

        let start = offset_usize;
        let end = start.saturating_add(size_usize);
        Ok(Some(bytes[start..end].to_vec()))
    }

    /// Read streamed resource bytes (best-effort) using the current environment context.
    ///
    /// Resolution strategy:
    /// - If `source_kind` is `AssetBundle`, try to read from resource nodes inside the same bundle.
    /// - Fall back to reading from the filesystem (same directory / `StreamingAssets/`), which
    ///   matches UnityPy's `ResourceReader`-like behavior.
    pub fn read_stream_data<P: AsRef<Path>>(
        &self,
        source_path: P,
        source_kind: BinarySourceKind,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let source_path = source_path.as_ref();
        let canonical = canonicalize_if_exists(source_path);
        let source = BinarySource::path(&canonical);

        let original_path = (source_path != canonical).then_some(source_path);
        self.read_stream_data_source_with_original_path(
            &source,
            source_kind,
            original_path,
            stream_path,
            offset,
            size,
        )
    }

    pub fn read_stream_data_source(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        self.read_stream_data_source_with_original_path(
            source,
            source_kind,
            None,
            stream_path,
            offset,
            size,
        )
    }

    fn read_stream_data_source_with_original_path(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        original_path: Option<&Path>,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        if let Some(bytes) =
            self.try_read_stream_data_source(source, source_kind, stream_path, offset, size)?
        {
            return Ok(bytes);
        }

        // Canonicalization resolves symlinks, while external resources are relative to the path
        // the caller opened. Only a genuine miss may fall back; authoritative read errors return
        // above through `?` without consulting a different resource.
        if let Some(original_path) = original_path
            && let Some(bytes) =
                self.try_read_stream_data_from_fs(original_path, stream_path, offset, size)?
        {
            return Ok(bytes);
        }

        Err(UnityAssetError::format(format!(
            "Stream resource not found for source {}: {}",
            source.describe(),
            stream_path
        )))
    }

    fn try_read_stream_data_source(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        if size == 0 {
            return Ok(Some(Vec::new()));
        }

        match source_kind {
            BinarySourceKind::AssetBundle => {
                let bundle = self.bundles.get(source);
                if let Some((bundle, node)) = bundle.and_then(|bundle| {
                    Self::find_exact_bundle_resource_node(bundle, stream_path)
                        .map(|node| (bundle, node))
                }) {
                    return Self::read_bundle_resource_node(bundle, node, offset, size).map(Some);
                }

                match self.try_read_external_stream_data_source(
                    source,
                    stream_path,
                    offset,
                    size,
                )? {
                    Some(bytes) => Ok(Some(bytes)),
                    None => {
                        if let Some((bundle, node)) = bundle.and_then(|bundle| {
                            Self::find_fuzzy_bundle_resource_node(bundle, stream_path)
                                .map(|node| (bundle, node))
                        }) {
                            return Self::read_bundle_resource_node(bundle, node, offset, size)
                                .map(Some);
                        }
                        Ok(None)
                    }
                }
            }
            BinarySourceKind::SerializedFile => {
                self.try_read_external_stream_data_source(source, stream_path, offset, size)
            }
        }
    }

    fn try_read_external_stream_data_source(
        &self,
        source: &BinarySource,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        match source {
            BinarySource::Path(path) => {
                self.try_read_stream_data_from_fs(path, stream_path, offset, size)
            }
            BinarySource::ArchiveEntry { archive_path, .. } => {
                self.try_read_stream_data_from_fs(archive_path.as_path(), stream_path, offset, size)
            }
            BinarySource::WebEntry { web_path, .. } => {
                self.try_read_webfile_stream_data(web_path, stream_path, offset, size)
            }
        }
    }

    /// Read streamed resource bytes from the filesystem (best-effort).
    ///
    /// This is useful when `StreamedResource.m_Source` points to an external `.resS`/`.resource`
    /// file that is not embedded in the current bundle.
    pub fn read_stream_data_from_fs<P: AsRef<Path>>(
        &self,
        source_path: P,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let source_path = source_path.as_ref();
        self.try_read_stream_data_from_fs(source_path, stream_path, offset, size)?
            .ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Stream resource file not found for source {:?}: {}",
                    source_path, stream_path
                ))
            })
    }

    fn try_read_stream_data_from_fs(
        &self,
        source_path: &Path,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        use std::fs::File;
        use std::io::{Read, Seek, SeekFrom};

        let candidates = Self::stream_fs_candidates(source_path, stream_path);
        for candidate in candidates {
            if !candidate.exists() {
                continue;
            }
            let mut file = File::open(&candidate).map_err(|e| {
                UnityAssetError::with_source(
                    format!("Failed to open stream resource {:?}", candidate),
                    e,
                )
            })?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to seek stream resource {:?} to {}",
                        candidate, offset
                    ),
                    e,
                )
            })?;

            let mut buffer = vec![0u8; size as usize];
            file.read_exact(&mut buffer).map_err(|e| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to read stream resource {:?} (offset={}, size={})",
                        candidate, offset, size
                    ),
                    e,
                )
            })?;
            return Ok(Some(buffer));
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_binary::bundle::{BundleHeader, DirectoryNode};

    #[test]
    fn exact_bundle_resource_matching_requires_a_path_component_boundary() {
        let requested = "CAB-00112233445566778899aabbccddeeff.resource";
        let mut bundle = AssetBundle::new(BundleHeader::default(), vec![0; 8]);
        bundle
            .nodes
            .push(DirectoryNode::new(format!("evil{requested}"), 0, 4, 0));

        assert!(Environment::find_exact_bundle_resource_node(&bundle, requested).is_none());

        bundle.nodes.push(DirectoryNode::new(
            format!("resources/{requested}"),
            4,
            4,
            0,
        ));
        let matched = Environment::find_exact_bundle_resource_node(
            &bundle,
            &format!("archive:/CAB-00112233445566778899aabbccddeeff/{requested}"),
        )
        .expect("the exact basename is a component match");
        assert_eq!(matched.name, format!("resources/{requested}"));
    }

    #[test]
    fn exact_embedded_resource_errors_cannot_fall_back_to_an_original_path() {
        let temporary = tempfile::tempdir().unwrap();
        let original_path = temporary.path().join("opened.bundle");
        let requested = "CAB-00112233445566778899aabbccddeeff.resource";
        std::fs::write(temporary.path().join(requested), b"disk").unwrap();

        let source = BinarySource::path(temporary.path().join("canonical.bundle"));
        let mut bundle = AssetBundle::new(BundleHeader::default(), vec![0; 4]);
        bundle
            .nodes
            .push(DirectoryNode::new(requested.to_owned(), 8, 4, 0));
        let mut environment = Environment::new();
        environment.bundles.insert(source.clone(), bundle);

        let error = environment
            .read_stream_data_source_with_original_path(
                &source,
                BinarySourceKind::AssetBundle,
                Some(&original_path),
                requested,
                0,
                4,
            )
            .unwrap_err();
        assert!(error.to_string().contains("Resource node out of bounds"));
    }
}
