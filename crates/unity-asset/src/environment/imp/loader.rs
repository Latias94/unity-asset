use super::path::canonicalize_if_exists;
use super::path::find_sensitive_path;
use super::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use unity_asset_binary::file::{
    UnityFileLoadOutcome, load_unity_file_from_memory_with_budget,
    load_unity_file_from_shared_range_with_budget, load_unity_file_with_budget,
    sniff_unity_file_kind_prefix, try_load_unity_file_from_memory_with_budget,
    try_load_unity_file_with_budget,
};
use zip::ZipArchive;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetaGuidIndexStats {
    pub dirs_visited: usize,
    pub files_visited: usize,
    pub meta_files_seen: usize,
    pub meta_guids_indexed: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ProjectLoadOptions {
    /// Index `.meta` GUIDs under the project root for best-effort external reference resolution.
    pub index_meta_guids: bool,
    /// Load YAML documents (`.asset`, `.prefab`, `.unity`).
    ///
    /// For large Unity projects, this can be expensive; consider starting with `binaries_only()`.
    pub load_yaml_documents: bool,
    /// Load `.meta` files as YAML documents.
    ///
    /// Most workflows only need `.meta` GUIDs, not full parsed `.meta` documents.
    pub load_meta_documents: bool,
    /// Load binary Unity files (AssetBundles / SerializedFiles / WebFiles) discovered during scan.
    pub load_binary_files: bool,
    /// Stop after visiting this many files (best-effort).
    pub max_files: Option<usize>,
    /// Respect `.gitignore` / `.ignore` / global ignores via the `ignore` crate.
    pub respect_ignores: bool,
    /// Follow filesystem symlinks during the project walk.
    pub follow_symlinks: bool,
}

impl ProjectLoadOptions {
    pub fn binaries_only() -> Self {
        Self {
            index_meta_guids: true,
            load_yaml_documents: false,
            load_meta_documents: false,
            load_binary_files: true,
            max_files: None,
            respect_ignores: true,
            follow_symlinks: false,
        }
    }

    pub fn everything() -> Self {
        Self {
            index_meta_guids: true,
            load_yaml_documents: true,
            load_meta_documents: false,
            load_binary_files: true,
            max_files: None,
            respect_ignores: true,
            follow_symlinks: false,
        }
    }
}

impl Default for ProjectLoadOptions {
    fn default() -> Self {
        Self::binaries_only()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectLoadStats {
    pub files_visited: usize,
    pub files_loaded: usize,
    pub yaml_loaded: usize,
    pub binary_loaded: usize,
    pub meta_files_seen: usize,
    pub meta_guids_indexed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryLoadOutcome {
    Loaded,
    Unrecognized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileLoadMode {
    Explicit,
    ConservativeScan,
}

#[allow(clippy::large_enum_variant)]
enum PreparedUnityFile {
    AssetBundle {
        source: BinarySource,
        bundle: AssetBundle,
    },
    SerializedFile {
        source: BinarySource,
        asset: SerializedFile,
    },
    WebFile {
        path: PathBuf,
        web: WebFile,
    },
}

impl PreparedUnityFile {
    fn logical_identity(&self, budget: &mut AssetLoadBudget) -> Result<PathBuf> {
        match self {
            Self::AssetBundle { source, .. } | Self::SerializedFile { source, .. } => {
                match source {
                    BinarySource::Path(path) => {
                        budgeted_clone_path(path, budget, "prepared Unity source identity path")
                    }
                    BinarySource::ArchiveEntry {
                        archive_path,
                        entry_name,
                    } => budgeted_join_path(
                        archive_path,
                        Path::new(entry_name),
                        budget,
                        "prepared archive source identity path",
                    ),
                    BinarySource::WebEntry {
                        web_path,
                        entry_name,
                    } => budgeted_join_path(
                        web_path,
                        Path::new(entry_name),
                        budget,
                        "prepared WebFile source identity path",
                    ),
                }
            }
            Self::WebFile { path, .. } => {
                budgeted_clone_path(path, budget, "prepared WebFile identity path")
            }
        }
    }
}

#[derive(Default)]
struct PreparedUnityFiles {
    files: Vec<PreparedUnityFile>,
    accounted_capacity: usize,
    identities: HashSet<PathBuf>,
    identity_capacity: usize,
}

impl PreparedUnityFiles {
    fn push(&mut self, file: PreparedUnityFile, budget: &mut AssetLoadBudget) -> Result<()> {
        let identity = file.logical_identity(budget)?;
        if self.identities.contains(&identity) {
            return Err(UnityAssetError::format(format!(
                "Prepared Unity source identity collision: {:?}",
                identity
            )));
        }
        reserve_budgeted_hash_set(
            &mut self.identities,
            &mut self.identity_capacity,
            1,
            budget,
            "prepared Unity source identity table",
        )?;
        reserve_budgeted_vec(
            &mut self.files,
            &mut self.accounted_capacity,
            1,
            budget,
            "prepared Unity file table",
        )?;
        self.identities.insert(identity);
        self.files.push(file);
        Ok(())
    }
}

impl Environment {
    /// Load assets from a path (file or directory).
    pub fn load<P: AsRef<Path>>(&mut self, path: P, budget: &mut AssetLoadBudget) -> Result<()> {
        let path = path.as_ref();

        if path.is_file() {
            self.load_file(path, budget)?;
        } else if path.is_dir() {
            self.load_directory(path, budget)?;
        }

        Ok(())
    }

    /// Load a single file through a caller-owned cumulative budget.
    ///
    /// Explicit loading is strict: a file that is not a supported Unity format is an error. Bulk
    /// directory and project scans use a conservative probe so unrelated compressed files remain
    /// ignorable candidates.
    pub fn load_file<P: AsRef<Path>>(
        &mut self,
        path: P,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        self.load_file_impl(path.as_ref(), budget, FileLoadMode::Explicit)
    }

    fn load_file_impl(
        &mut self,
        input_path: &Path,
        budget: &mut AssetLoadBudget,
        mode: FileLoadMode,
    ) -> Result<()> {
        let mut path = canonicalize_if_exists(input_path);
        let next_base_path = path.parent().map(Path::to_path_buf);

        if !path.exists() {
            // Unity-style case-insensitive resolution for relative paths.
            let resolution_base = next_base_path.as_deref().unwrap_or(&self.base_path);
            if let Some(p) = find_sensitive_path(resolution_base, &path) {
                path = canonicalize_if_exists(&p);
            }
        }

        // UnityPy split-file convention: `<base>.split0/.split1/...`.
        if !path.exists() {
            // If a base path is provided, attempt loading `<path>.split0`.
            let split0 = append_suffix(&path, ".split0");
            if split0.exists() {
                path = split0;
            }
        }

        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && ext.starts_with("split")
            && ext[5..].parse::<usize>().is_ok()
        {
            self.try_load_split_file(&path, budget)?;
            if let Some(base_path) = next_base_path {
                self.base_path = base_path;
            }
            return Ok(());
        }

        // Check file extension to determine type
        if let Some(ext) = path.extension() {
            if ext.to_string_lossy().eq_ignore_ascii_case("zip")
                || ext.to_string_lossy().eq_ignore_ascii_case("apk")
            {
                self.load_zip_archive(&path, budget)?;
                if let Some(base_path) = next_base_path {
                    self.base_path = base_path;
                }
                return Ok(());
            }

            if ext == "meta" {
                // Index meta GUIDs even if YAML parsing fails (best-effort reference resolution).
                let _ = self.index_meta_guid_path(&path);
            }

            match ext.to_str() {
                Some("asset") | Some("prefab") | Some("unity") | Some("meta") => {
                    match YamlDocument::load_yaml_with_warnings(&path, false) {
                        Ok((doc, warnings)) => {
                            for w in warnings {
                                self.push_warning(EnvironmentWarning::YamlDocumentSkipped {
                                    path: path.clone(),
                                    doc_index: w.doc_index,
                                    error: w.error,
                                });
                            }
                            self.remove_loaded_source_root(&path);
                            self.yaml_documents.insert(path.clone(), doc);
                        }
                        Err(_) => {
                            // Some Unity projects can store `.asset`-like files in binary form.
                            // If YAML parsing fails, fall back to binary detection.
                            self.load_binary_for_mode(&path, budget, mode)?;
                        }
                    }
                }
                _ => {
                    self.load_binary_for_mode(&path, budget, mode)?;
                }
            }
        } else {
            // Some Unity outputs (especially streamed resources and certain build artifacts)
            // can be extension-less. Attempt binary detection anyway.
            self.load_binary_for_mode(&path, budget, mode)?;
        }

        if let Some(base_path) = next_base_path {
            self.base_path = base_path;
        }
        Ok(())
    }

    fn try_load_split_file(
        &mut self,
        split_part_path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let base = split_part_path.with_extension("");
        let base_key = strip_verbatim_prefix(&base);
        let bytes = load_split_bytes(&base_key, budget)?;
        let unity_file =
            load_unity_file_from_memory_with_budget(bytes, budget).map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to load split Unity source {:?}", base_key),
                    error,
                )
            })?;
        let mut prepared = PreparedUnityFiles::default();
        Self::prepare_unity_file(
            budgeted_path_source(&base_key, budget, "split Unity source path")?,
            unity_file,
            budget,
            &mut prepared,
            1,
        )?;
        self.commit_prepared_unity_files(&base_key, prepared);
        Ok(())
    }

    fn load_zip_archive(
        &mut self,
        archive_path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let mut file = File::open(archive_path).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to open zip archive {:?}", archive_path),
                e,
            )
        })?;
        let archive_len = file
            .metadata()
            .map_err(|e| {
                UnityAssetError::with_source(
                    format!("Failed to inspect zip archive {:?}", archive_path),
                    e,
                )
            })?
            .len();
        budget.check_bytes(archive_len).map_err(|e| {
            UnityAssetError::with_source(
                format!(
                    "Zip archive {:?} exceeds the asset load budget",
                    archive_path
                ),
                e,
            )
        })?;
        let preflight = preflight_zip_directory(&mut file, archive_len).map_err(|error| {
            UnityAssetError::with_source(
                format!("Failed to preflight zip archive {:?}", archive_path),
                error,
            )
        })?;
        budget
            .check_members(preflight.member_count)
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!("Zip archive {:?} exceeds the member budget", archive_path),
                    error,
                )
            })?;
        validate_zip_central_directory(
            &mut file,
            archive_len,
            preflight.directory_start,
            preflight.directory_size,
            preflight.member_count,
        )
        .map_err(|error| {
            UnityAssetError::with_source(
                format!("Invalid zip central directory in {:?}", archive_path),
                error,
            )
        })?;
        budget
            .consume_members(preflight.member_count)
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!("Zip archive {:?} exceeds the member budget", archive_path),
                    error,
                )
            })?;
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            UnityAssetError::with_source(
                format!("Failed to rewind zip archive {:?}", archive_path),
                error,
            )
        })?;
        let mut zip = ZipArchive::new(file).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to parse zip archive {:?}", archive_path),
                e,
            )
        })?;
        let parsed_count = u64::try_from(zip.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "Zip archive {:?} member count does not fit in u64",
                archive_path
            ))
        })?;
        if parsed_count != preflight.member_count {
            return Err(UnityAssetError::format(format!(
                "Zip archive {:?} changed or has inconsistent member counts: preflight={}, parsed={}",
                archive_path, preflight.member_count, parsed_count
            )));
        }

        let archive_path = canonicalize_if_exists(archive_path);
        let shared_archive_path = Arc::new(budgeted_clone_path(
            &archive_path,
            budget,
            "shared ZIP archive source path",
        )?);
        let mut prepared = PreparedUnityFiles::default();

        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to open zip entry {i} from {:?}", archive_path),
                    error,
                )
            })?;
            if entry.is_dir() {
                continue;
            }

            let name = normalize_zip_entry_name(entry.name(), budget)?;
            let compressed_size = entry.compressed_size();
            let decompressed_size = entry.size();
            // Preflight the source buffer here; the decompressor and unified parser charge their
            // respective output and source-traversal ledgers before this allocation is used.
            budget.check_bytes(decompressed_size).map_err(|e| {
                UnityAssetError::with_source(
                    format!("Zip entry {name:?} exceeds the asset load budget"),
                    e,
                )
            })?;
            budget
                .check_decompression(compressed_size, decompressed_size)
                .map_err(|e| {
                    UnityAssetError::with_source(
                        format!("Zip entry {name:?} exceeds the decompression budget"),
                        e,
                    )
                })?;

            let capacity = usize::try_from(decompressed_size).map_err(|_| {
                UnityAssetError::with_source(
                    format!("Zip entry {name:?} is too large to allocate"),
                    unity_asset_binary::error::BinaryError::memory_error(
                        "Zip entry size does not fit in usize",
                    ),
                )
            })?;
            let mut bytes: Vec<u8> = Vec::new();
            bytes.try_reserve_exact(capacity).map_err(|e| {
                UnityAssetError::with_source(
                    format!("Failed to allocate zip entry {name:?}"),
                    unity_asset_binary::error::BinaryError::memory_error(e.to_string()),
                )
            })?;

            budget
                .begin_decompression()
                .consume(compressed_size, decompressed_size)
                .map_err(|e| {
                    UnityAssetError::with_source(
                        format!("Zip entry {name:?} exceeds the decompression budget"),
                        e,
                    )
                })?;
            let mut chunk = [0_u8; 64 * 1024];
            let read_result = loop {
                let read = match entry.read(&mut chunk) {
                    Ok(read) => read,
                    Err(error) => break Err(error),
                };
                if read == 0 {
                    break Ok(());
                }
                let Some(new_len) = bytes.len().checked_add(read) else {
                    break Err(std::io::Error::other("Zip entry length overflow"));
                };
                if new_len > capacity {
                    break Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Zip entry exceeded its declared decompressed size",
                    ));
                }
                bytes.extend_from_slice(&chunk[..read]);
            };
            read_result.map_err(|error| {
                UnityAssetError::with_source(
                    format!("Failed to read zip entry {name:?} from {:?}", archive_path),
                    error,
                )
            })?;
            if bytes.is_empty() {
                continue;
            }

            // Zip/APK entries behave like independent inputs (UnityPy-style): we load each entry
            // as a top-level source, and saving writes the edited entry as a standalone output
            // (we do not repack the zip).
            let source = BinarySource::ArchiveEntry {
                archive_path: Arc::clone(&shared_archive_path),
                entry_name: name,
            };
            Self::prepare_unity_bytes(source, bytes, budget, &mut prepared)?;
        }

        self.commit_prepared_unity_files(&archive_path, prepared);
        Ok(())
    }

    fn prepare_unity_bytes(
        source: BinarySource,
        bytes: Vec<u8>,
        budget: &mut AssetLoadBudget,
        prepared: &mut PreparedUnityFiles,
    ) -> Result<()> {
        let outcome =
            try_load_unity_file_from_memory_with_budget(bytes, budget).map_err(|error| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to load recognized Unity source {}",
                        source.describe()
                    ),
                    error,
                )
            })?;
        let UnityFileLoadOutcome::Recognized(unity_file) = outcome else {
            return Ok(());
        };

        Self::prepare_unity_file(source, unity_file, budget, prepared, 1)
    }

    fn prepare_unity_file(
        source: BinarySource,
        unity_file: UnityFile,
        budget: &mut AssetLoadBudget,
        prepared: &mut PreparedUnityFiles,
        depth: u32,
    ) -> Result<()> {
        budget.observe_depth(depth).map_err(|error| {
            UnityAssetError::with_source(
                format!(
                    "Unity container {} exceeds the recursion budget",
                    source.describe()
                ),
                error,
            )
        })?;

        match unity_file {
            UnityFile::AssetBundle(bundle) => {
                prepared.push(PreparedUnityFile::AssetBundle { source, bundle }, budget)?
            }
            UnityFile::SerializedFile(asset) => {
                prepared.push(PreparedUnityFile::SerializedFile { source, asset }, budget)?
            }
            UnityFile::WebFile(web) => {
                let web_key = match source {
                    BinarySource::Path(path) => path,
                    BinarySource::ArchiveEntry {
                        archive_path,
                        entry_name,
                    } => budgeted_join_path(
                        archive_path.as_path(),
                        Path::new(&entry_name),
                        budget,
                        "shared archive WebFile source path",
                    )?,
                    BinarySource::WebEntry {
                        web_path,
                        entry_name,
                    } => budgeted_join_path(
                        web_path.as_path(),
                        Path::new(&entry_name),
                        budget,
                        "shared nested WebFile source path",
                    )?,
                };
                let web_key = Arc::new(web_key);
                let child_depth = depth.checked_add(1).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "WebFile recursion depth overflow for {:?}",
                        web_key
                    ))
                })?;
                let entries = Self::parse_webfile_entries(&web, &web_key, child_depth, budget)?;
                let published_path =
                    budgeted_clone_path(&web_key, budget, "published WebFile container path")?;
                prepared.push(
                    PreparedUnityFile::WebFile {
                        path: published_path,
                        web,
                    },
                    budget,
                )?;
                for (entry_name, parsed) in entries {
                    let entry_source = BinarySource::WebEntry {
                        web_path: Arc::clone(&web_key),
                        entry_name,
                    };
                    Self::prepare_unity_file(entry_source, parsed, budget, prepared, child_depth)?;
                }
            }
        }

        Ok(())
    }

    fn commit_prepared_unity_files(&mut self, source_root: &Path, prepared: PreparedUnityFiles) {
        self.remove_loaded_source_root(source_root);
        for file in prepared.files {
            match file {
                PreparedUnityFile::AssetBundle { source, mut bundle } => {
                    if let Some(registry) = self.type_tree_registry.clone() {
                        for file in bundle.assets.iter_mut() {
                            file.set_type_tree_registry(Some(registry.clone()));
                        }
                    }
                    self.invalidate_dependency_scan_cache_for_source(
                        &source,
                        BinarySourceKind::AssetBundle,
                        None,
                    );
                    self.bundles.insert(source.clone(), bundle);
                    match self.bundle_container_cache.write() {
                        Ok(mut cache) => {
                            cache.remove(&source);
                        }
                        Err(error) => {
                            error.into_inner().remove(&source);
                        }
                    }
                }
                PreparedUnityFile::SerializedFile { source, mut asset } => {
                    if let Some(registry) = self.type_tree_registry.clone() {
                        asset.set_type_tree_registry(Some(registry));
                    }
                    self.invalidate_dependency_scan_cache_for_source(
                        &source,
                        BinarySourceKind::SerializedFile,
                        None,
                    );
                    self.binary_assets.insert(source, asset);
                    match self.bundle_container_cache.write() {
                        Ok(mut cache) => cache.clear(),
                        Err(error) => error.into_inner().clear(),
                    }
                }
                PreparedUnityFile::WebFile { path, web } => {
                    self.webfiles.insert(path, web);
                }
            }
        }
    }

    fn remove_loaded_source_root(&mut self, source_root: &Path) {
        let belongs_to_root = |source: &BinarySource| match source {
            BinarySource::Path(path) => path == source_root,
            BinarySource::ArchiveEntry { archive_path, .. } => {
                archive_path.as_path() == source_root
            }
            BinarySource::WebEntry { web_path, .. } => {
                web_path.as_path() == source_root || web_path.starts_with(source_root)
            }
        };
        let web_belongs_to_root =
            |path: &PathBuf| path.as_path() == source_root || path.starts_with(source_root);

        self.binary_assets
            .retain(|source, _| !belongs_to_root(source));
        self.bundles.retain(|source, _| !belongs_to_root(source));
        self.webfiles.retain(|path, _| !web_belongs_to_root(path));
        self.yaml_documents.remove(source_root);
        self.write_state
            .standalone
            .retain(|source, _| !belongs_to_root(source));
        self.write_state
            .bundles
            .retain(|source, _| !belongs_to_root(source));
        self.write_state
            .webfiles
            .retain(|path, _| !web_belongs_to_root(path));
        self.write_state.yaml_documents.remove(source_root);

        match self.bundle_container_cache.write() {
            Ok(mut cache) => cache.clear(),
            Err(error) => error.into_inner().clear(),
        }
        self.invalidate_dependency_scan_cache();
    }

    /// Recursively index `.meta` GUIDs under a directory (without loading YAML/binary assets).
    ///
    /// This is useful to improve best-effort external reference resolution (GUID -> asset path),
    /// while keeping the main loading path focused (e.g. only load bundles / serialized files).
    pub fn index_meta_guids_in_directory<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<MetaGuidIndexStats> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(UnityAssetError::format(format!(
                "Directory does not exist: {:?}",
                path
            )));
        }
        if !path.is_dir() {
            return Err(UnityAssetError::format(format!(
                "Path is not a directory: {:?}",
                path
            )));
        }

        let path = canonicalize_if_exists(path);

        let mut stats = MetaGuidIndexStats::default();
        let mut stack: Vec<PathBuf> = vec![path];

        while let Some(dir) = stack.pop() {
            stats.dirs_visited += 1;

            let entries = std::fs::read_dir(&dir).map_err(|e| {
                UnityAssetError::with_source(format!("Failed to read directory {:?}", dir), e)
            })?;

            for entry in entries {
                let entry = entry.map_err(|e| {
                    UnityAssetError::with_source("Failed to read directory entry", e)
                })?;
                let entry_path = entry.path();

                if entry_path.is_dir() {
                    if let Some(dir_name) = entry_path.file_name().and_then(|n| n.to_str())
                        && matches!(
                            dir_name,
                            "Library" | "Temp" | "Logs" | ".git" | ".vs" | "obj" | "bin"
                        )
                    {
                        continue;
                    }
                    stack.push(entry_path);
                    continue;
                }

                if !entry_path.is_file() {
                    continue;
                }

                stats.files_visited += 1;
                if entry_path.extension().and_then(|e| e.to_str()) != Some("meta") {
                    continue;
                }

                stats.meta_files_seen += 1;
                if self.index_meta_guid_path(&entry_path).is_some() {
                    stats.meta_guids_indexed += 1;
                }
            }
        }

        Ok(stats)
    }

    fn try_load_binary(
        &mut self,
        path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryLoadOutcome> {
        let outcome = try_load_unity_file_with_budget(path, budget).map_err(|error| {
            UnityAssetError::with_source(
                format!("Failed to probe or load Unity binary file {:?}", path),
                error,
            )
        })?;
        let UnityFileLoadOutcome::Recognized(unity_file) = outcome else {
            return Ok(BinaryLoadOutcome::Unrecognized);
        };

        let source = budgeted_path_source(path, budget, "probed Unity source path")?;
        let mut prepared = PreparedUnityFiles::default();
        Self::prepare_unity_file(source, unity_file, budget, &mut prepared, 1)?;
        self.commit_prepared_unity_files(path, prepared);
        Ok(BinaryLoadOutcome::Loaded)
    }

    fn load_binary_for_mode(
        &mut self,
        path: &Path,
        budget: &mut AssetLoadBudget,
        mode: FileLoadMode,
    ) -> Result<BinaryLoadOutcome> {
        match mode {
            FileLoadMode::Explicit => {
                let unity_file = load_unity_file_with_budget(path, budget).map_err(|error| {
                    UnityAssetError::with_source(
                        format!("Failed to load Unity binary file {:?}", path),
                        error,
                    )
                })?;
                let mut prepared = PreparedUnityFiles::default();
                Self::prepare_unity_file(
                    budgeted_path_source(path, budget, "explicit Unity source path")?,
                    unity_file,
                    budget,
                    &mut prepared,
                    1,
                )?;
                self.commit_prepared_unity_files(path, prepared);
                Ok(BinaryLoadOutcome::Loaded)
            }
            FileLoadMode::ConservativeScan => self.try_load_binary(path, budget),
        }
    }

    fn parse_webfile_entries(
        web: &WebFile,
        web_path: &Path,
        child_depth: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<(String, UnityFile)>> {
        let mut entry_indices = Vec::new();
        let mut entry_index_capacity = 0;
        reserve_budgeted_vec(
            &mut entry_indices,
            &mut entry_index_capacity,
            web.files().len(),
            budget,
            "WebFile entry-index table",
        )?;
        entry_indices.extend(0..web.files().len());
        entry_indices
            .sort_unstable_by(|left, right| web.files()[*left].name.cmp(&web.files()[*right].name));
        if let Some(duplicate) = entry_indices
            .windows(2)
            .find(|indices| web.files()[indices[0]].name == web.files()[indices[1]].name)
            .map(|indices| &web.files()[indices[0]].name)
        {
            return Err(UnityAssetError::format(format!(
                "WebFile {:?} contains duplicate entry name {:?}",
                web_path, duplicate
            )));
        }
        let mut entries = Vec::new();
        let mut entry_capacity = 0;

        for entry_index in entry_indices {
            let file_info = &web.files()[entry_index];
            let entry_name = file_info.name.as_str();
            let view = web.extract_file_view_by_info(file_info).map_err(|error| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to extract WebFile entry {entry_name:?} from {:?}",
                        web_path
                    ),
                    error,
                )
            })?;
            let prefix_len = view.len().min(64);
            if sniff_unity_file_kind_prefix(&view.as_bytes()[..prefix_len]).is_none() {
                continue;
            }
            budget.observe_depth(child_depth).map_err(|error| {
                UnityAssetError::with_source(
                    format!(
                        "WebFile entry {entry_name:?} from {:?} exceeds the recursion budget",
                        web_path
                    ),
                    error,
                )
            })?;

            let parsed = load_unity_file_from_shared_range_with_budget(
                view.backing_shared(),
                view.absolute_range(),
                budget,
            )
            .map_err(|error| {
                UnityAssetError::with_source(
                    format!(
                        "Failed to load recognized WebFile entry {entry_name:?} from {:?}",
                        web_path
                    ),
                    error,
                )
            })?;

            reserve_budgeted_vec(
                &mut entries,
                &mut entry_capacity,
                1,
                budget,
                "parsed WebFile entry table",
            )?;
            let entry_name =
                clone_string_with_budget(entry_name, budget, "recognized WebFile entry identity")?;
            entries.push((entry_name, parsed));
        }

        Ok(entries)
    }

    /// Load all supported files from a directory.
    pub fn load_directory<P: AsRef<Path>>(
        &mut self,
        path: P,
        budget: &mut AssetLoadBudget,
    ) -> Result<()> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(UnityAssetError::format(format!(
                "Directory does not exist: {:?}",
                path
            )));
        }

        if !path.is_dir() {
            return Err(UnityAssetError::format(format!(
                "Path is not a directory: {:?}",
                path
            )));
        }

        let path = canonicalize_if_exists(path);

        // Recursively traverse directory
        self.traverse_directory(&path, budget)?;

        Ok(())
    }

    /// Load a Unity project directory (best-effort).
    ///
    /// Unlike `load_directory`, this API is designed for real Unity project roots:
    /// - can index `.meta` GUIDs without loading `.meta` documents
    /// - can respect ignore files (`.gitignore`, `.ignore`)
    /// - can avoid attempting to parse every non-Unity file (fast prefix sniffing)
    pub fn load_project<P: AsRef<Path>>(
        &mut self,
        root: P,
        options: ProjectLoadOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<ProjectLoadStats> {
        use ignore::WalkBuilder;

        let root = root.as_ref();
        if !root.exists() {
            return Err(UnityAssetError::format(format!(
                "Directory does not exist: {:?}",
                root
            )));
        }
        if !root.is_dir() {
            return Err(UnityAssetError::format(format!(
                "Path is not a directory: {:?}",
                root
            )));
        }

        let root = canonicalize_if_exists(root);
        let mut stats = ProjectLoadStats::default();

        let mut builder = WalkBuilder::new(&root);
        builder.follow_links(options.follow_symlinks);
        builder.hidden(false);

        if options.respect_ignores {
            builder
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .ignore(true);
        } else {
            builder
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .ignore(false);
        }

        let skip_dir_names = [
            "Library",
            "Temp",
            "Logs",
            ".git",
            ".vs",
            "obj",
            "bin",
            "UserSettings",
        ];

        let walker = builder.filter_entry(move |entry| {
            let Some(name) = entry.file_name().to_str() else {
                return false;
            };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                return !skip_dir_names.iter().any(|d| d == &name);
            }
            true
        });

        for result in walker.build() {
            let entry = match result {
                Ok(v) => v,
                Err(_) => continue,
            };
            if entry.file_type().is_none_or(|t| !t.is_file()) {
                continue;
            }

            stats.files_visited += 1;
            if let Some(max) = options.max_files
                && stats.files_visited > max
            {
                break;
            }

            let path = canonicalize_if_exists(entry.path());
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            if ext == "meta" {
                stats.meta_files_seen += 1;
                if options.index_meta_guids && self.index_meta_guid_path(&path).is_some() {
                    stats.meta_guids_indexed += 1;
                }
                if options.load_meta_documents {
                    match self.load_file_impl(&path, budget, FileLoadMode::ConservativeScan) {
                        Ok(()) => {
                            stats.files_loaded += 1;
                            stats.yaml_loaded += 1;
                        }
                        Err(error) if super::pptr::is_resource_error(&error) => return Err(error),
                        Err(_) => {}
                    }
                }
                continue;
            }

            if matches!(ext, "asset" | "prefab" | "unity") && options.load_yaml_documents {
                match self.load_file_impl(&path, budget, FileLoadMode::ConservativeScan) {
                    Ok(()) => {
                        stats.files_loaded += 1;
                        stats.yaml_loaded += 1;
                    }
                    Err(error) if super::pptr::is_resource_error(&error) => return Err(error),
                    Err(_) => {}
                }
                continue;
            }

            if !options.load_binary_files {
                continue;
            }

            match self.try_load_binary(&path, budget) {
                Ok(BinaryLoadOutcome::Loaded) => {
                    if let Some(base_path) = path.parent() {
                        self.base_path = base_path.to_path_buf();
                    }
                    stats.files_loaded += 1;
                    stats.binary_loaded += 1;
                }
                Ok(BinaryLoadOutcome::Unrecognized) => {}
                Err(error) if super::pptr::is_resource_error(&error) => return Err(error),
                Err(_) => {}
            }
        }

        Ok(stats)
    }

    /// Recursively traverse directory and load Unity files.
    fn traverse_directory(&mut self, dir: &Path, budget: &mut AssetLoadBudget) -> Result<()> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            UnityAssetError::with_source(format!("Failed to read directory {:?}", dir), e)
        })?;

        for entry in entries {
            let entry = entry
                .map_err(|e| UnityAssetError::with_source("Failed to read directory entry", e))?;
            let path = entry.path();

            if path.is_dir() {
                // Skip common Unity directories that don't contain assets
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    match dir_name {
                        "Library" | "Temp" | "Logs" | ".git" | ".vs" | "obj" | "bin" => {
                            continue; // Skip these directories
                        }
                        _ => {
                            // Recursively process subdirectory
                            self.traverse_directory(&path, budget)?;
                        }
                    }
                }
            } else if path.is_file() {
                // Try to load the file
                if let Err(e) = self.load_file_impl(&path, budget, FileLoadMode::ConservativeScan) {
                    if super::pptr::is_resource_error(&e) {
                        return Err(e);
                    }
                    // Record warning but continue processing other files
                    self.push_warning(EnvironmentWarning::LoadFailed {
                        path,
                        error: e.to_string(),
                    });
                }
            }
        }

        Ok(())
    }
}

const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP_CENTRAL_HEADER_SIGNATURE: u32 = 0x0201_4b50;
const ZIP_CENTRAL_DIGITAL_SIGNATURE: u32 = 0x0505_4b50;
const ZIP_EOCD_FIXED_LEN: usize = 22;
const ZIP_EOCD_SEARCH_LEN: usize = ZIP_EOCD_FIXED_LEN + u16::MAX as usize;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_EOCD_MIN_LEN: usize = 56;
const ZIP64_RECORD_SEARCH_LEN: usize = ZIP_EOCD_SEARCH_LEN;
const ZIP_PREFLIGHT_TAIL_LEN: usize =
    ZIP_EOCD_SEARCH_LEN + ZIP64_LOCATOR_LEN + ZIP64_RECORD_SEARCH_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ZipDirectoryPreflight {
    member_count: u64,
    directory_start: u64,
    directory_size: u64,
}

fn preflight_zip_directory(
    file: &mut File,
    file_len: u64,
) -> std::io::Result<ZipDirectoryPreflight> {
    if file_len < ZIP_EOCD_FIXED_LEN as u64 {
        return Err(invalid_zip("archive is shorter than the ZIP end record"));
    }

    let tail_len = usize::try_from(file_len.min(ZIP_PREFLIGHT_TAIL_LEN as u64))
        .map_err(|_| invalid_zip("ZIP tail length does not fit in usize"))?;
    let tail_start = file_len
        .checked_sub(tail_len as u64)
        .ok_or_else(|| invalid_zip("ZIP tail range underflow"))?;
    let mut tail = [0_u8; ZIP_PREFLIGHT_TAIL_LEN];
    file.seek(SeekFrom::Start(tail_start))?;
    file.read_exact(&mut tail[..tail_len])?;
    let tail = &tail[..tail_len];
    let eocd_search_start = tail_len.saturating_sub(ZIP_EOCD_SEARCH_LEN);
    let Some(last_candidate) = tail_len.checked_sub(ZIP_EOCD_FIXED_LEN) else {
        return Err(invalid_zip("archive is shorter than the ZIP end record"));
    };

    let mut last_error = None;
    let mut valid_candidate = None;
    for candidate in (eocd_search_start..=last_candidate).rev() {
        if zip_u32(tail, candidate) != Some(ZIP_EOCD_SIGNATURE) {
            continue;
        }
        let Some(comment_len) = zip_u16(tail, candidate + 20).map(usize::from) else {
            continue;
        };
        let Some(candidate_end) = candidate
            .checked_add(ZIP_EOCD_FIXED_LEN)
            .and_then(|offset| offset.checked_add(comment_len))
        else {
            continue;
        };
        if candidate_end != tail_len {
            continue;
        }

        match validate_zip_end_candidate(file_len, tail, tail_start, candidate) {
            Ok(preflight) => {
                if valid_candidate.is_some() {
                    return Err(invalid_zip("archive has ambiguous valid ZIP end records"));
                }
                valid_candidate = Some(preflight);
            }
            Err(error) => last_error = Some(error),
        }
    }

    valid_candidate.ok_or_else(|| {
        last_error.unwrap_or_else(|| invalid_zip("could not find a valid ZIP end record"))
    })
}

fn validate_zip_end_candidate(
    file_len: u64,
    tail: &[u8],
    tail_start: u64,
    eocd: usize,
) -> std::io::Result<ZipDirectoryPreflight> {
    let eocd_position = tail_start
        .checked_add(eocd as u64)
        .ok_or_else(|| invalid_zip("ZIP end position overflow"))?;
    let disk_number = required_zip_u16(tail, eocd + 4, "ZIP disk number")?;
    let directory_disk = required_zip_u16(tail, eocd + 6, "ZIP directory disk")?;
    let entries_on_disk = required_zip_u16(tail, eocd + 8, "ZIP disk entry count")?;
    let entries = required_zip_u16(tail, eocd + 10, "ZIP entry count")?;
    let directory_size = required_zip_u32(tail, eocd + 12, "ZIP directory size")?;
    let directory_offset = required_zip_u32(tail, eocd + 16, "ZIP directory offset")?;

    let locator = eocd
        .checked_sub(ZIP64_LOCATOR_LEN)
        .filter(|offset| zip_u32(tail, *offset) == Some(ZIP64_LOCATOR_SIGNATURE));
    if let Some(locator) = locator {
        return validate_zip64_directory(file_len, tail, tail_start, locator, eocd_position);
    }

    let needs_zip64 = disk_number == u16::MAX
        || directory_disk == u16::MAX
        || entries_on_disk == u16::MAX
        || entries == u16::MAX
        || directory_size == u32::MAX
        || directory_offset == u32::MAX;
    if needs_zip64 {
        return Err(invalid_zip("ZIP64 end locator is missing"));
    }
    if disk_number != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(invalid_zip("multi-disk ZIP archives are not supported"));
    }

    let directory_size = u64::from(directory_size);
    let directory_offset = u64::from(directory_offset);
    let directory_start = eocd_position
        .checked_sub(directory_size)
        .ok_or_else(|| invalid_zip("ZIP central directory starts before the archive"))?;
    if directory_offset > directory_start {
        return Err(invalid_zip(
            "ZIP central directory offset is outside the archive",
        ));
    }
    let directory_end = directory_start
        .checked_add(directory_size)
        .ok_or_else(|| invalid_zip("ZIP central directory range overflow"))?;
    if directory_end > file_len {
        return Err(invalid_zip(
            "ZIP central directory extends beyond the archive",
        ));
    }
    Ok(ZipDirectoryPreflight {
        member_count: u64::from(entries),
        directory_start,
        directory_size,
    })
}

fn validate_zip64_directory(
    file_len: u64,
    tail: &[u8],
    tail_start: u64,
    locator: usize,
    eocd_position: u64,
) -> std::io::Result<ZipDirectoryPreflight> {
    let locator_position = tail_start
        .checked_add(locator as u64)
        .ok_or_else(|| invalid_zip("ZIP64 locator position overflow"))?;
    if locator_position.checked_add(ZIP64_LOCATOR_LEN as u64) != Some(eocd_position) {
        return Err(invalid_zip(
            "ZIP64 locator is not adjacent to the ZIP end record",
        ));
    }
    let locator_disk = required_zip_u32(tail, locator + 4, "ZIP64 locator disk")?;
    let nominal_record = required_zip_u64(tail, locator + 8, "ZIP64 record offset")?;
    let disk_count = required_zip_u32(tail, locator + 16, "ZIP64 disk count")?;
    if locator_disk != 0 || disk_count != 1 {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }

    let record_search_start = locator.saturating_sub(ZIP64_RECORD_SEARCH_LEN);
    let Some(last_record) = locator.checked_sub(ZIP64_EOCD_MIN_LEN) else {
        return Err(invalid_zip("ZIP64 end record is truncated"));
    };
    let mut record = None;
    for candidate in (record_search_start..=last_record).rev() {
        if zip_u32(tail, candidate) != Some(ZIP64_EOCD_SIGNATURE) {
            continue;
        }
        let Some(record_size) = zip_u64(tail, candidate + 4) else {
            continue;
        };
        if record_size < 44 {
            continue;
        }
        let Some(record_len) = record_size.checked_add(12) else {
            continue;
        };
        let Ok(record_len) = usize::try_from(record_len) else {
            continue;
        };
        if candidate.checked_add(record_len) == Some(locator) {
            record = Some(candidate);
            break;
        }
    }
    let record = record.ok_or_else(|| {
        invalid_zip("ZIP64 end record is invalid or exceeds the bounded preflight window")
    })?;
    let record_position = tail_start
        .checked_add(record as u64)
        .ok_or_else(|| invalid_zip("ZIP64 record position overflow"))?;
    if nominal_record > record_position {
        return Err(invalid_zip(
            "ZIP64 nominal record offset is outside the archive",
        ));
    }
    let archive_offset = record_position - nominal_record;

    let disk_number = required_zip_u32(tail, record + 16, "ZIP64 disk number")?;
    let directory_disk = required_zip_u32(tail, record + 20, "ZIP64 directory disk")?;
    let entries_on_disk = required_zip_u64(tail, record + 24, "ZIP64 disk entry count")?;
    let entries = required_zip_u64(tail, record + 32, "ZIP64 entry count")?;
    let directory_size = required_zip_u64(tail, record + 40, "ZIP64 directory size")?;
    let nominal_directory = required_zip_u64(tail, record + 48, "ZIP64 directory offset")?;
    if disk_number != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }
    let directory_start = nominal_directory
        .checked_add(archive_offset)
        .ok_or_else(|| invalid_zip("ZIP64 central directory position overflow"))?;
    let directory_end = directory_start
        .checked_add(directory_size)
        .ok_or_else(|| invalid_zip("ZIP64 central directory range overflow"))?;
    if directory_end > record_position {
        return Err(invalid_zip(
            "ZIP64 central directory overlaps its end record",
        ));
    }
    if directory_end > file_len {
        return Err(invalid_zip(
            "ZIP64 central directory extends beyond the archive",
        ));
    }
    Ok(ZipDirectoryPreflight {
        member_count: entries,
        directory_start,
        directory_size,
    })
}

fn validate_zip_central_directory(
    file: &mut File,
    file_len: u64,
    directory_start: u64,
    directory_size: u64,
    expected_entries: u64,
) -> std::io::Result<()> {
    let directory_end = directory_start
        .checked_add(directory_size)
        .ok_or_else(|| invalid_zip("ZIP central directory range overflow"))?;
    if directory_end > file_len {
        return Err(invalid_zip(
            "ZIP central directory extends beyond the archive",
        ));
    }
    let minimum_size = expected_entries
        .checked_mul(46)
        .ok_or_else(|| invalid_zip("ZIP central directory entry count overflow"))?;
    if minimum_size > directory_size {
        return Err(invalid_zip(
            "ZIP central directory is too small for its entry count",
        ));
    }

    file.seek(SeekFrom::Start(directory_start))?;
    let mut position = directory_start;
    let mut header = [0_u8; 46];
    for _ in 0..expected_entries {
        file.read_exact(&mut header)?;
        if zip_u32(&header, 0) != Some(ZIP_CENTRAL_HEADER_SIGNATURE) {
            return Err(invalid_zip("invalid ZIP central directory entry signature"));
        }
        let name_len = u64::from(required_zip_u16(&header, 28, "ZIP entry name length")?);
        let extra_len = u64::from(required_zip_u16(&header, 30, "ZIP extra field length")?);
        let comment_len = u64::from(required_zip_u16(&header, 32, "ZIP entry comment length")?);
        let entry_len = 46_u64
            .checked_add(name_len)
            .and_then(|length| length.checked_add(extra_len))
            .and_then(|length| length.checked_add(comment_len))
            .ok_or_else(|| invalid_zip("ZIP central directory entry range overflow"))?;
        position = position
            .checked_add(entry_len)
            .ok_or_else(|| invalid_zip("ZIP central directory position overflow"))?;
        if position > directory_end {
            return Err(invalid_zip("ZIP central directory entry is truncated"));
        }
        file.seek(SeekFrom::Start(position))?;
    }

    if position == directory_end {
        return Ok(());
    }

    let remaining = directory_end - position;
    if remaining < 6 {
        return Err(invalid_zip("ZIP central directory has trailing bytes"));
    }
    let mut signature_header = [0_u8; 6];
    file.read_exact(&mut signature_header)?;
    if zip_u32(&signature_header, 0) != Some(ZIP_CENTRAL_DIGITAL_SIGNATURE) {
        return Err(invalid_zip(
            "ZIP central directory contains uncounted entries",
        ));
    }
    let signature_len = u64::from(required_zip_u16(
        &signature_header,
        4,
        "ZIP central directory signature length",
    )?);
    if signature_len.checked_add(6) != Some(remaining) {
        return Err(invalid_zip(
            "ZIP central directory digital signature is truncated",
        ));
    }
    Ok(())
}

fn zip_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

fn zip_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

fn zip_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(value))
}

fn required_zip_u16(bytes: &[u8], offset: usize, field: &str) -> std::io::Result<u16> {
    zip_u16(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn required_zip_u32(bytes: &[u8], offset: usize, field: &str) -> std::io::Result<u32> {
    zip_u32(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn required_zip_u64(bytes: &[u8], offset: usize, field: &str) -> std::io::Result<u64> {
    zip_u64(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn invalid_zip(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn load_split_bytes(base: &Path, budget: &mut AssetLoadBudget) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut found_any = false;

    for i in 0..999usize {
        let part = append_suffix(base, &format!(".split{i}"));
        if !part.exists() {
            if found_any {
                break;
            }
            continue;
        }
        found_any = true;
        let mut file = File::open(&part).map_err(|e| {
            UnityAssetError::with_source(format!("Failed to open split part {:?}", part), e)
        })?;
        let declared_len = file
            .metadata()
            .map_err(|e| {
                UnityAssetError::with_source(format!("Failed to inspect split part {:?}", part), e)
            })?
            .len();
        let declared_len_usize = usize::try_from(declared_len).map_err(|_| {
            UnityAssetError::with_source(
                format!("Split part {:?} is too large to allocate", part),
                unity_asset_binary::error::BinaryError::memory_error(
                    "Split part size does not fit in usize",
                ),
            )
        })?;
        let planned_len = out.len().checked_add(declared_len_usize).ok_or_else(|| {
            UnityAssetError::with_source(
                "Combined split file length overflow",
                unity_asset_binary::error::BinaryError::memory_error(
                    "Combined split file length overflow",
                ),
            )
        })?;
        // Match the binary path loader: preflight owned source backing here, then let the unified
        // parser charge the bytes it traverses so mmap and owned inputs have one source charge.
        budget
            .check_bytes(u64::try_from(planned_len).map_err(|_| {
                UnityAssetError::with_source(
                    "Combined split file length does not fit in u64",
                    unity_asset_binary::error::BinaryError::memory_error(
                        "Combined split file length does not fit in u64",
                    ),
                )
            })?)
            .map_err(|e| {
                UnityAssetError::with_source(
                    format!("Split part {:?} exceeds the asset load budget", part),
                    e,
                )
            })?;
        out.try_reserve_exact(declared_len_usize).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to allocate split part {:?}", part),
                unity_asset_binary::error::BinaryError::memory_error(e.to_string()),
            )
        })?;

        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut chunk).map_err(|e| {
                UnityAssetError::with_source(format!("Failed to read split part {:?}", part), e)
            })?;
            if read == 0 {
                break;
            }
            let new_len = out.len().checked_add(read).ok_or_else(|| {
                UnityAssetError::with_source(
                    "Combined split file length overflow",
                    unity_asset_binary::error::BinaryError::memory_error(
                        "Combined split file length overflow",
                    ),
                )
            })?;
            budget
                .check_bytes(u64::try_from(new_len).map_err(|_| {
                    UnityAssetError::with_source(
                        "Combined split file length does not fit in u64",
                        unity_asset_binary::error::BinaryError::memory_error(
                            "Combined split file length does not fit in u64",
                        ),
                    )
                })?)
                .map_err(|e| {
                    UnityAssetError::with_source(
                        format!("Split part {:?} exceeds the asset load budget", part),
                        e,
                    )
                })?;
            out.try_reserve(read).map_err(|e| {
                UnityAssetError::with_source(
                    format!("Failed to grow split file buffer for {:?}", part),
                    unity_asset_binary::error::BinaryError::memory_error(e.to_string()),
                )
            })?;
            out.extend_from_slice(&chunk[..read]);
        }
    }

    if !found_any {
        return Err(UnityAssetError::format(format!(
            "No split parts found for base path: {:?}",
            base
        )));
    }

    Ok(out)
}

fn path_component_owned_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn path_separator_owned_bytes() -> usize {
    Path::new(std::path::MAIN_SEPARATOR_STR)
        .as_os_str()
        .as_encoded_bytes()
        .len()
}

fn reserve_budgeted_path(
    allocation: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<PathBuf> {
    let allocation_u64 = u64::try_from(allocation)
        .map_err(|_| budgeted_allocation_error(label, "allocation does not fit in u64"))?;
    budget.check_bytes(allocation_u64).map_err(|error| {
        UnityAssetError::with_source(format!("{label} exceeds the asset load budget"), error)
    })?;
    let mut path = PathBuf::new();
    path.try_reserve(allocation).map_err(|error| {
        budgeted_allocation_error(label, format!("failed to reserve storage: {error}"))
    })?;
    budget.consume_bytes(allocation_u64).map_err(|error| {
        UnityAssetError::with_source(format!("Failed to charge {label}"), error)
    })?;
    Ok(path)
}

fn budgeted_clone_path(
    source: &Path,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<PathBuf> {
    let allocation = path_component_owned_bytes(source);
    let mut path = reserve_budgeted_path(allocation, budget, label)?;
    path.push(source);
    Ok(path)
}

fn budgeted_path_source(
    path: &Path,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<BinarySource> {
    Ok(BinarySource::Path(budgeted_clone_path(
        path, budget, label,
    )?))
}

fn budgeted_join_path(
    base: &Path,
    child: &Path,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<PathBuf> {
    let allocation = path_component_owned_bytes(base)
        .checked_add(path_component_owned_bytes(child))
        .and_then(|bytes| bytes.checked_add(path_separator_owned_bytes()))
        .ok_or_else(|| budgeted_allocation_error(label, "joined path byte length overflow"))?;
    let mut path = reserve_budgeted_path(allocation, budget, label)?;
    path.push(base);
    path.push(child);
    Ok(path)
}

fn reserve_budgeted_hash_set<T: Eq + std::hash::Hash>(
    values: &mut HashSet<T>,
    accounted_capacity: &mut usize,
    additional: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<()> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or_else(|| budgeted_allocation_error(label, "capacity arithmetic overflow"))?;
    if required <= *accounted_capacity {
        return Ok(());
    }

    let target_capacity = if *accounted_capacity == 0 {
        required
    } else {
        accounted_capacity
            .checked_mul(2)
            .ok_or_else(|| budgeted_allocation_error(label, "geometric capacity overflow"))?
            .max(required)
    };
    let additional_capacity = target_capacity - *accounted_capacity;
    let allocation = additional_capacity
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| budgeted_allocation_error(label, "allocation size overflow"))?;
    let allocation = u64::try_from(allocation)
        .map_err(|_| budgeted_allocation_error(label, "allocation does not fit in u64"))?;
    budget.check_bytes(allocation).map_err(|error| {
        UnityAssetError::with_source(format!("{label} exceeds the asset load budget"), error)
    })?;
    values.try_reserve(additional_capacity).map_err(|error| {
        budgeted_allocation_error(label, format!("failed to reserve storage: {error}"))
    })?;
    budget.consume_bytes(allocation).map_err(|error| {
        UnityAssetError::with_source(format!("Failed to charge {label}"), error)
    })?;
    *accounted_capacity = target_capacity;
    Ok(())
}

fn reserve_budgeted_vec<T>(
    values: &mut Vec<T>,
    accounted_capacity: &mut usize,
    additional: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<()> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or_else(|| budgeted_allocation_error(label, "capacity arithmetic overflow"))?;
    if required <= *accounted_capacity {
        return Ok(());
    }

    let target_capacity = if *accounted_capacity == 0 {
        required
    } else {
        accounted_capacity
            .checked_mul(2)
            .ok_or_else(|| budgeted_allocation_error(label, "geometric capacity overflow"))?
            .max(required)
    };
    let additional_capacity = target_capacity - *accounted_capacity;
    let allocation = additional_capacity
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| budgeted_allocation_error(label, "allocation size overflow"))?;
    let allocation = u64::try_from(allocation)
        .map_err(|_| budgeted_allocation_error(label, "allocation does not fit in u64"))?;
    budget.check_bytes(allocation).map_err(|error| {
        UnityAssetError::with_source(format!("{label} exceeds the asset load budget"), error)
    })?;
    let reserve = target_capacity - values.len();
    values.try_reserve_exact(reserve).map_err(|error| {
        budgeted_allocation_error(label, format!("failed to reserve storage: {error}"))
    })?;
    budget.consume_bytes(allocation).map_err(|error| {
        UnityAssetError::with_source(format!("Failed to charge {label}"), error)
    })?;
    *accounted_capacity = target_capacity;
    Ok(())
}

fn clone_string_with_budget(
    value: &str,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let mut owned = reserve_budgeted_string(value.len(), budget, label)?;
    owned.push_str(value);
    Ok(owned)
}

fn normalize_zip_entry_name(value: &str, budget: &mut AssetLoadBudget) -> Result<String> {
    let mut normalized = reserve_budgeted_string(value.len(), budget, "ZIP entry identity")?;
    for character in value.chars() {
        normalized.push(if character == '\\' { '/' } else { character });
    }
    Ok(normalized)
}

fn reserve_budgeted_string(
    allocation: usize,
    budget: &mut AssetLoadBudget,
    label: &str,
) -> Result<String> {
    let allocation_u64 = u64::try_from(allocation)
        .map_err(|_| budgeted_allocation_error(label, "length does not fit in u64"))?;
    budget.check_bytes(allocation_u64).map_err(|error| {
        UnityAssetError::with_source(format!("{label} exceeds the asset load budget"), error)
    })?;
    let mut owned = String::new();
    owned.try_reserve_exact(allocation).map_err(|error| {
        budgeted_allocation_error(label, format!("failed to reserve storage: {error}"))
    })?;
    budget.consume_bytes(allocation_u64).map_err(|error| {
        UnityAssetError::with_source(format!("Failed to charge {label}"), error)
    })?;
    Ok(owned)
}

fn budgeted_allocation_error(label: &str, message: impl Into<String>) -> UnityAssetError {
    UnityAssetError::with_source(
        format!("Failed to allocate {label}"),
        unity_asset_binary::error::BinaryError::memory_error(message),
    )
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{}", rest));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_binary::file::load_unity_file_from_memory_with_budget;
    use unity_asset_core::AssetLoadLimits;

    fn byte_budget(max_bytes: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    fn uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        let signature = b"UnityWebData1.0\0";
        let entry_table_len = entries
            .iter()
            .map(|(name, _)| 12_usize.checked_add(name.len()).unwrap())
            .sum::<usize>();
        let header_len = signature
            .len()
            .checked_add(std::mem::size_of::<i32>())
            .and_then(|len| len.checked_add(entry_table_len))
            .unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(signature);
        bytes.extend_from_slice(&i32::try_from(header_len).unwrap().to_le_bytes());
        let mut payload_offset = header_len;
        for (name, payload) in &entries {
            bytes.extend_from_slice(&i32::try_from(payload_offset).unwrap().to_le_bytes());
            bytes.extend_from_slice(&i32::try_from(payload.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(&i32::try_from(name.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            payload_offset = payload_offset.checked_add(payload.len()).unwrap();
        }
        for (_, payload) in entries {
            bytes.extend_from_slice(&payload);
        }
        bytes
    }

    fn empty_webfile() -> Vec<u8> {
        uncompressed_webfile(Vec::new())
    }

    #[test]
    fn zip_preflight_accepts_eocd_comments_with_signature_bytes() {
        use std::io::Write;
        use zip::write::FileOptions;

        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(temp.reopen().unwrap());
        zip.start_file("entry.bin", FileOptions::default()).unwrap();
        zip.write_all(b"payload").unwrap();
        zip.set_raw_comment(b"comment-PK\x05\x06-not-an-end-record".to_vec());
        let file = zip.finish().unwrap();
        let file_len = file.metadata().unwrap().len();
        drop(file);
        let mut file = temp.reopen().unwrap();

        let preflight = preflight_zip_directory(&mut file, file_len).unwrap();

        assert_eq!(preflight.member_count, 1);
        validate_zip_central_directory(
            &mut file,
            file_len,
            preflight.directory_start,
            preflight.directory_size,
            preflight.member_count,
        )
        .unwrap();
    }

    #[test]
    fn zip_preflight_rejects_ambiguous_fake_eocd_in_comment() {
        use std::io::Write;
        use zip::write::FileOptions;

        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(temp.reopen().unwrap());
        zip.start_file("entry.bin", FileOptions::default()).unwrap();
        zip.write_all(b"payload").unwrap();
        let mut fake_end = ZIP_EOCD_SIGNATURE.to_le_bytes().to_vec();
        fake_end.extend_from_slice(&[0_u8; ZIP_EOCD_FIXED_LEN - 4]);
        zip.set_raw_comment(fake_end);
        let file = zip.finish().unwrap();
        let file_len = file.metadata().unwrap().len();
        drop(file);
        let mut file = temp.reopen().unwrap();

        let error = preflight_zip_directory(&mut file, file_len)
            .expect_err("a second valid-looking end record must be rejected as ambiguous");

        assert!(error.to_string().contains("ambiguous"), "{error:?}");
    }

    #[test]
    fn zip64_preflight_validates_locator_and_record_ranges() {
        use std::io::Write;

        fn empty_zip64() -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&ZIP64_EOCD_SIGNATURE.to_le_bytes());
            bytes.extend_from_slice(&44_u64.to_le_bytes());
            bytes.extend_from_slice(&45_u16.to_le_bytes());
            bytes.extend_from_slice(&45_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&ZIP64_LOCATOR_SIGNATURE.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&ZIP_EOCD_SIGNATURE.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes.extend_from_slice(&u16::MAX.to_le_bytes());
            bytes.extend_from_slice(&u16::MAX.to_le_bytes());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            bytes.extend_from_slice(&0_u16.to_le_bytes());
            bytes
        }

        let valid = empty_zip64();
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        temp.as_file_mut().write_all(&valid).unwrap();
        let mut file = temp.reopen().unwrap();
        assert_eq!(
            preflight_zip_directory(&mut file, valid.len() as u64)
                .unwrap()
                .member_count,
            0
        );

        let mut invalid_locator = valid.clone();
        invalid_locator[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut invalid = tempfile::NamedTempFile::new().unwrap();
        invalid.as_file_mut().write_all(&invalid_locator).unwrap();
        let mut file = invalid.reopen().unwrap();
        let error = preflight_zip_directory(&mut file, invalid_locator.len() as u64)
            .expect_err("the ZIP64 locator cannot point beyond its record");
        assert!(error.to_string().contains("offset"), "{error:?}");

        let mut overflowing_directory = valid;
        overflowing_directory[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        overflowing_directory[48..56].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut invalid = tempfile::NamedTempFile::new().unwrap();
        invalid
            .as_file_mut()
            .write_all(&overflowing_directory)
            .unwrap();
        let mut file = invalid.reopen().unwrap();
        preflight_zip_directory(&mut file, overflowing_directory.len() as u64)
            .expect_err("overflowing ZIP64 central directory ranges must be rejected");
    }

    #[test]
    fn webfile_staging_indexes_borrowed_names_without_cloning_unrecognized_names() {
        let first_name = format!("a-{}", "x".repeat(2_048));
        let second_name = format!("z-{}", "y".repeat(2_048));
        let web = WebFile::from_bytes(uncompressed_webfile(vec![
            (second_name, b"ordinary second payload".to_vec()),
            (first_name, b"ordinary first payload".to_vec()),
        ]))
        .unwrap();
        let index_bytes = u64::try_from(web.files().len() * std::mem::size_of::<usize>()).unwrap();
        let mut budget = byte_budget(index_bytes);

        let entries =
            Environment::parse_webfile_entries(&web, Path::new("outer.web"), 2, &mut budget)
                .unwrap();

        assert!(entries.is_empty());
        assert_eq!(budget.usage().bytes, index_bytes);
        assert_eq!(budget.usage().members, 0);
    }

    #[test]
    fn webfile_staging_keeps_deterministic_name_order_with_direct_entry_views() {
        let child = empty_webfile();
        let web = WebFile::from_bytes(uncompressed_webfile(vec![
            ("z.web".to_string(), child.clone()),
            ("a.web".to_string(), child),
        ]))
        .unwrap();
        let mut budget = AssetLoadBudget::default();

        let entries =
            Environment::parse_webfile_entries(&web, Path::new("outer.web"), 2, &mut budget)
                .unwrap();
        let names = entries
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, ["a.web", "z.web"]);
    }

    #[test]
    fn webfile_members_share_one_budgeted_parent_path_allocation() {
        fn prepare(path: &Path) -> (unity_asset_core::AssetLoadUsage, Vec<Arc<PathBuf>>) {
            let bundle = include_bytes!("../../../../../tests/samples/char_118_yuki.ab");
            let entries = (0..4)
                .map(|index| (format!("member-{index}.ab"), bundle.to_vec()))
                .collect();
            let web = WebFile::from_bytes(uncompressed_webfile(entries)).unwrap();
            let mut budget = AssetLoadBudget::default();
            let mut prepared = PreparedUnityFiles::default();
            let source =
                budgeted_path_source(path, &mut budget, "test WebFile source path").unwrap();

            Environment::prepare_unity_file(
                source,
                UnityFile::WebFile(web),
                &mut budget,
                &mut prepared,
                1,
            )
            .unwrap();

            let retained_paths = prepared
                .files
                .iter()
                .filter_map(|file| match file {
                    PreparedUnityFile::AssetBundle {
                        source: BinarySource::WebEntry { web_path, .. },
                        ..
                    } => Some(Arc::clone(web_path)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            (budget.usage(), retained_paths)
        }

        let short = PathBuf::from("a.web");
        let long = PathBuf::from(format!("{}.web", "a".repeat(2_048)));
        let (short_usage, _) = prepare(&short);
        let (long_usage, retained_paths) = prepare(&long);
        let path_growth = u64::try_from(
            long.as_os_str()
                .as_encoded_bytes()
                .len()
                .checked_sub(short.as_os_str().as_encoded_bytes().len())
                .unwrap(),
        )
        .unwrap();

        assert_eq!(retained_paths.len(), 4);
        assert!(
            retained_paths
                .windows(2)
                .all(|paths| Arc::ptr_eq(&paths[0], &paths[1]))
        );
        assert_eq!(
            long_usage.bytes - short_usage.bytes,
            path_growth * 7,
            "one source path, one published path, one WebFile identity, and four member identities"
        );
    }

    #[test]
    fn budgeted_vec_grows_geometrically_and_charges_logical_capacity() {
        let slot_bytes = std::mem::size_of::<u32>() as u64;
        let mut budget = byte_budget(8 * slot_bytes);
        let mut values = Vec::new();
        let mut accounted_capacity = 0;

        for (value, expected_capacity) in (0_u32..5).zip([1_usize, 2, 4, 4, 8]) {
            reserve_budgeted_vec(
                &mut values,
                &mut accounted_capacity,
                1,
                &mut budget,
                "test vector",
            )
            .unwrap();
            values.push(value);
            assert_eq!(accounted_capacity, expected_capacity);
            assert!(values.capacity() >= accounted_capacity);
        }

        assert_eq!(values, [0, 1, 2, 3, 4]);
        assert_eq!(budget.usage().bytes, 8 * slot_bytes);
    }

    #[test]
    fn budgeted_vec_preflight_failure_is_atomic() {
        let slot_bytes = std::mem::size_of::<u32>() as u64;
        let mut budget = byte_budget(6 * slot_bytes);
        let mut values = Vec::new();
        let mut accounted_capacity = 0;

        for value in 0_u32..4 {
            reserve_budgeted_vec(
                &mut values,
                &mut accounted_capacity,
                1,
                &mut budget,
                "test vector",
            )
            .unwrap();
            values.push(value);
        }
        let usage_before = budget.usage();

        assert!(
            reserve_budgeted_vec(
                &mut values,
                &mut accounted_capacity,
                1,
                &mut budget,
                "test vector",
            )
            .is_err()
        );
        assert_eq!(values, [0, 1, 2, 3]);
        assert_eq!(accounted_capacity, 4);
        assert_eq!(budget.usage(), usage_before);
    }

    #[test]
    fn budgeted_hash_set_preflight_failure_is_atomic() {
        let slot_bytes = std::mem::size_of::<u32>() as u64;
        let mut budget = byte_budget(slot_bytes);
        let mut values = HashSet::new();
        let mut accounted_capacity = 0;

        reserve_budgeted_hash_set(
            &mut values,
            &mut accounted_capacity,
            1,
            &mut budget,
            "test set",
        )
        .unwrap();
        values.insert(1_u32);
        let usage_before = budget.usage();

        assert!(
            reserve_budgeted_hash_set(
                &mut values,
                &mut accounted_capacity,
                1,
                &mut budget,
                "test set",
            )
            .is_err()
        );
        assert_eq!(values, HashSet::from([1_u32]));
        assert_eq!(accounted_capacity, 1);
        assert_eq!(budget.usage(), usage_before);
    }

    #[test]
    fn budgeted_join_path_accepts_ascii_identity_at_exact_byte_limit() {
        let base = Path::new("archive");
        let child = Path::new("entry");
        let expected = base.join(child);
        let identity_bytes = expected.as_os_str().as_encoded_bytes().len() as u64;
        let mut budget = byte_budget(identity_bytes);

        let identity = budgeted_join_path(base, child, &mut budget, "test identity").unwrap();

        assert_eq!(identity, expected);
        assert_eq!(budget.usage().bytes, identity_bytes);
    }

    #[test]
    fn budgeted_clone_path_rejects_cjk_identity_one_byte_below_encoded_length() {
        let source = Path::new("路径");
        let identity_bytes = source.as_os_str().as_encoded_bytes().len() as u64;
        let mut budget = byte_budget(identity_bytes - 1);

        budgeted_clone_path(source, &mut budget, "test identity")
            .expect_err("encoded path storage must be charged before allocation");

        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn budgeted_path_source_rejects_retained_clone_before_allocation() {
        let source = Path::new("retained-source.assets");
        let source_bytes = u64::try_from(source.as_os_str().as_encoded_bytes().len()).unwrap();
        let mut budget = byte_budget(source_bytes - 1);

        budgeted_path_source(source, &mut budget, "test source")
            .expect_err("retained source paths must be charged before allocation");

        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn zip_entry_normalization_is_budgeted_exactly_once() {
        let raw = "nested\\entry.ab";
        let raw_bytes = u64::try_from(raw.len()).unwrap();
        let mut exact = byte_budget(raw_bytes);

        let normalized = normalize_zip_entry_name(raw, &mut exact).unwrap();

        assert_eq!(normalized, "nested/entry.ab");
        assert_eq!(exact.usage().bytes, raw_bytes);

        let mut insufficient = byte_budget(raw_bytes - 1);
        normalize_zip_entry_name(raw, &mut insufficient)
            .expect_err("ZIP entry identities must be charged before normalization");
        assert_eq!(insufficient.usage(), Default::default());
    }

    #[test]
    fn prepared_webfile_identity_clone_is_budgeted_before_slot_allocation() {
        let signature = b"UnityWebData1.0\0";
        let mut bytes = signature.to_vec();
        bytes.extend_from_slice(&i32::try_from(signature.len() + 4).unwrap().to_le_bytes());
        let web = WebFile::from_bytes(bytes).unwrap();
        let path = PathBuf::from("x".repeat(128));
        let identity_bytes = path.as_os_str().as_encoded_bytes().len() as u64;
        let mut budget = byte_budget(identity_bytes - 1);
        let mut prepared = PreparedUnityFiles::default();

        prepared
            .push(PreparedUnityFile::WebFile { path, web }, &mut budget)
            .expect_err("identity clone must exceed the byte budget before table allocation");

        assert!(prepared.files.is_empty());
        assert!(prepared.identities.is_empty());
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn prepared_archive_identity_join_is_budgeted_before_slot_allocation() {
        let bytes = include_bytes!("../../../../../tests/samples/char_118_yuki.ab").to_vec();
        let UnityFile::AssetBundle(bundle) =
            load_unity_file_from_memory_with_budget(bytes, &mut AssetLoadBudget::default())
                .unwrap()
        else {
            panic!("sample must parse as an AssetBundle");
        };
        let archive_path = PathBuf::from("a".repeat(96));
        let entry_name = "b".repeat(96);
        let identity_bytes = archive_path
            .as_os_str()
            .as_encoded_bytes()
            .len()
            .checked_add(Path::new(&entry_name).as_os_str().as_encoded_bytes().len())
            .and_then(|bytes| bytes.checked_add(std::path::MAIN_SEPARATOR.len_utf8()))
            .unwrap() as u64;
        let source = BinarySource::ArchiveEntry {
            archive_path: Arc::new(archive_path),
            entry_name,
        };
        let mut budget = byte_budget(identity_bytes - 1);
        let mut prepared = PreparedUnityFiles::default();

        prepared
            .push(
                PreparedUnityFile::AssetBundle { source, bundle },
                &mut budget,
            )
            .expect_err("identity join must exceed the byte budget before table allocation");

        assert!(prepared.files.is_empty());
        assert!(prepared.identities.is_empty());
        assert_eq!(budget.usage(), Default::default());
    }
}
