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

    fn find_bundle_resource_node<'a>(
        bundle: &'a AssetBundle,
        stream_path: &str,
    ) -> Option<&'a unity_asset_binary::bundle::types::DirectoryNode> {
        let normalized = Self::normalize_stream_path(stream_path);
        if normalized.is_empty() {
            return None;
        }

        let file_name = Path::new(&normalized)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());

        let mut nodes: Vec<&unity_asset_binary::bundle::types::DirectoryNode> =
            bundle.nodes.iter().filter(|n| n.is_file()).collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));

        for node in &nodes {
            let node_norm = node.name.replace('\\', "/");
            if node_norm == normalized
                || node_norm.ends_with(&normalized)
                || normalized.ends_with(&node_norm)
            {
                return Some(*node);
            }

            if let Some(file_name) = &file_name {
                if Path::new(&node_norm).file_name().and_then(|n| n.to_str())
                    == Some(file_name.as_str())
                {
                    return Some(*node);
                }
            }
        }

        // Unity sometimes appends an index suffix to the CAB resource node name
        // (e.g. `CAB-<hash>1.resource`) while the `StreamedResource.m_Source` path
        // points to `CAB-<hash>.resource`. Best-effort: match by CAB prefix.
        let cab_prefix = normalized
            .split('/')
            .find(|s| s.starts_with("CAB-"))
            .and_then(|s| {
                let hash: String = s
                    .trim_start_matches("CAB-")
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect();
                if hash.is_empty() {
                    None
                } else {
                    Some(format!("CAB-{}", hash))
                }
            });

        if let Some(cab_prefix) = cab_prefix {
            for node in &nodes {
                let node_norm = node.name.replace('\\', "/");
                let is_resource = node_norm.ends_with(".resS") || node_norm.ends_with(".resource");
                let base = Path::new(&node_norm)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&node_norm);
                if is_resource && (node_norm.starts_with(&cab_prefix) || base.starts_with(&cab_prefix))
                {
                    return Some(*node);
                }
            }
        }

        None
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
        let bundle_source = BinarySource::path(bundle_path.as_ref());
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

        let node = Self::find_bundle_resource_node(bundle, stream_path).ok_or_else(|| {
            UnityAssetError::format(format!(
                "Resource node not found in bundle {}: {}",
                bundle_source.describe(),
                stream_path
            ))
        })?;

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

        let offset_usize: usize = offset.try_into().map_err(|_| {
            UnityAssetError::format(format!("Stream offset overflow: {}", offset))
        })?;
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
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());

        let mut names: Vec<&String> = web.files.iter().map(|f| &f.name).collect();
        names.sort();

        for name in &names {
            let name_norm = name.replace('\\', "/");
            if name_norm == normalized
                || name_norm.ends_with(&normalized)
                || normalized.ends_with(&name_norm)
            {
                return Some((*name).clone());
            }

            if let Some(file_name) = &file_name {
                if Path::new(&name_norm).file_name().and_then(|n| n.to_str())
                    == Some(file_name.as_str())
                {
                    return Some((*name).clone());
                }
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

    fn read_webfile_stream_data(
        &self,
        web_path: &PathBuf,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let web = self.webfiles.get(web_path).ok_or_else(|| {
            UnityAssetError::format(format!("WebFile source not loaded: {:?}", web_path))
        })?;

        let entry_name =
            Self::find_webfile_resource_entry(web, stream_path).ok_or_else(|| {
                UnityAssetError::format(format!(
                    "Resource entry not found in WebFile {:?}: {}",
                    web_path, stream_path
                ))
            })?;

        let bytes = web.extract_file(&entry_name).map_err(|e| {
            UnityAssetError::format(format!(
                "Failed to extract WebFile entry {:?} from {:?}: {}",
                entry_name, web_path, e
            ))
        })?;

        let offset_usize: usize = offset.try_into().map_err(|_| {
            UnityAssetError::format(format!("Stream offset overflow: {}", offset))
        })?;
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
        Ok(bytes[start..end].to_vec())
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
        let source = BinarySource::path(source_path.as_ref());
        self.read_stream_data_source(&source, source_kind, stream_path, offset, size)
    }

    pub fn read_stream_data_source(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        stream_path: &str,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }

        match source_kind {
            BinarySourceKind::AssetBundle => self
                .read_bundle_stream_data_source(source, stream_path, offset, size)
                .or_else(|_| match source {
                    BinarySource::Path(p) => {
                        self.read_stream_data_from_fs(p, stream_path, offset, size)
                    }
                    BinarySource::WebEntry { web_path, .. } => {
                        self.read_webfile_stream_data(web_path, stream_path, offset, size)
                    }
                }),
            BinarySourceKind::SerializedFile => match source {
                BinarySource::Path(p) => {
                    self.read_stream_data_from_fs(p, stream_path, offset, size)
                }
                BinarySource::WebEntry { web_path, .. } => {
                    self.read_webfile_stream_data(web_path, stream_path, offset, size)
                }
            },
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
        use std::fs::File;
        use std::io::{Read, Seek, SeekFrom};

        let source_path = source_path.as_ref();
        let candidates = Self::stream_fs_candidates(source_path, stream_path);
        for candidate in candidates {
            if !candidate.exists() {
                continue;
            }
            let mut file = File::open(&candidate)
                .map_err(|e| UnityAssetError::with_source(format!("Failed to open stream resource {:?}", candidate), e))?;
            file.seek(SeekFrom::Start(offset)).map_err(|e| {
                UnityAssetError::with_source(
                    format!("Failed to seek stream resource {:?} to {}", candidate, offset),
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
            return Ok(buffer);
        }

        Err(UnityAssetError::format(format!(
            "Stream resource file not found for source {:?}: {}",
            source_path, stream_path
        )))
    }
}
