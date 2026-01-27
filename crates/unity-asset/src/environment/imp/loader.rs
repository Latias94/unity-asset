use super::path::canonicalize_if_exists;
use super::path::find_sensitive_path;
use super::*;
use std::fs::File;
use std::io::Read;
use unity_asset_binary::file::load_unity_file_from_memory;
use unity_asset_binary::file::{UnityFileKind, sniff_unity_file_kind_prefix};
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

impl Environment {
    /// Load assets from a path (file or directory).
    pub fn load<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref();

        if path.is_file() {
            self.load_file(path)?;
        } else if path.is_dir() {
            self.load_directory(path)?;
        }

        Ok(())
    }

    /// Load a single file.
    pub fn load_file<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let mut path = canonicalize_if_exists(path.as_ref());

        // Keep `base_path` aligned with the most recent load entrypoint (UnityPy-like behavior).
        if let Some(parent) = path.parent() {
            self.base_path = parent.to_path_buf();
        }

        if !path.exists() {
            // Unity-style case-insensitive resolution for relative paths.
            if let Some(p) = find_sensitive_path(&self.base_path, &path) {
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
            self.try_load_split_file(&path)?;
            return Ok(());
        }

        // Check file extension to determine type
        if let Some(ext) = path.extension() {
            if ext.to_string_lossy().eq_ignore_ascii_case("zip")
                || ext.to_string_lossy().eq_ignore_ascii_case("apk")
            {
                self.load_zip_archive(&path)?;
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
                            self.yaml_documents.insert(path.clone(), doc);
                        }
                        Err(_) => {
                            // Some Unity projects can store `.asset`-like files in binary form.
                            // If YAML parsing fails, fall back to binary detection.
                            self.try_load_binary(&path)?;
                        }
                    }
                }
                _ => {
                    // Best-effort binary detection for common build outputs.
                    self.try_load_binary(&path)?;
                }
            }
        } else {
            // Some Unity outputs (especially streamed resources and certain build artifacts)
            // can be extension-less. Attempt binary detection anyway.
            self.try_load_binary(&path)?;
        }

        Ok(())
    }

    fn try_load_split_file(&mut self, split_part_path: &Path) -> Result<()> {
        let base = split_part_path.with_extension("");
        let base_key = strip_verbatim_prefix(&base);
        let bytes = load_split_bytes(&base_key)?;
        self.try_load_unity_bytes(BinarySource::path(&base_key), bytes);
        Ok(())
    }

    fn load_zip_archive(&mut self, archive_path: &Path) -> Result<()> {
        let f = File::open(archive_path).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to open zip archive {:?}", archive_path),
                e,
            )
        })?;
        let mut zip = ZipArchive::new(f).map_err(|e| {
            UnityAssetError::with_source(
                format!("Failed to parse zip archive {:?}", archive_path),
                e,
            )
        })?;

        let archive_path = canonicalize_if_exists(archive_path);
        if let Some(parent) = archive_path.parent() {
            self.base_path = parent.to_path_buf();
        }

        for i in 0..zip.len() {
            let mut entry = match zip.by_index(i) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if entry.is_dir() {
                continue;
            }

            let name = entry.name().replace('\\', "/");
            let mut bytes: Vec<u8> = Vec::new();
            if entry.read_to_end(&mut bytes).is_err() {
                continue;
            }
            if bytes.is_empty() {
                continue;
            }

            let prefix_len = bytes.len().min(64);
            let Some(kind) = sniff_unity_file_kind_prefix(&bytes[..prefix_len]) else {
                continue;
            };
            if !matches!(
                kind,
                UnityFileKind::AssetBundle | UnityFileKind::SerializedFile | UnityFileKind::WebFile
            ) {
                continue;
            }

            // Zip/APK entries behave like independent inputs (UnityPy-style): we load each entry
            // as a top-level source, and saving writes the edited entry as a standalone output
            // (we do not repack the zip).
            let source = BinarySource::ArchiveEntry {
                archive_path: archive_path.clone(),
                entry_name: name,
            };
            self.try_load_unity_bytes(source, bytes);
        }

        Ok(())
    }

    fn try_load_unity_bytes(&mut self, source: BinarySource, bytes: Vec<u8>) {
        let Ok(unity_file) = load_unity_file_from_memory(bytes) else {
            return;
        };

        match unity_file {
            UnityFile::AssetBundle(mut bundle) => {
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
                    Err(e) => {
                        let mut cache = e.into_inner();
                        cache.remove(&source);
                    }
                }
            }
            UnityFile::SerializedFile(mut asset) => {
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
                    Err(e) => e.into_inner().clear(),
                }
            }
            UnityFile::WebFile(web) => {
                // Best-effort: store under a synthetic "path" so existing WebFile logic can load entries.
                let web_key = match &source {
                    BinarySource::Path(p) => p.clone(),
                    BinarySource::ArchiveEntry {
                        archive_path,
                        entry_name,
                    } => archive_path.join(entry_name),
                    BinarySource::WebEntry {
                        web_path,
                        entry_name,
                    } => web_path.join(entry_name),
                };
                self.webfiles.insert(web_key.clone(), web);
                let _ = self.load_webfile_entries(&web_key);
            }
        }
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
                    if let Some(dir_name) = entry_path.file_name().and_then(|n| n.to_str()) {
                        if matches!(
                            dir_name,
                            "Library" | "Temp" | "Logs" | ".git" | ".vs" | "obj" | "bin"
                        ) {
                            continue;
                        }
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

    fn try_load_binary(&mut self, path: &Path) -> Result<()> {
        match load_unity_file(path) {
            Ok(UnityFile::AssetBundle(bundle)) => {
                let mut bundle = bundle;
                if let Some(registry) = self.type_tree_registry.clone() {
                    for file in bundle.assets.iter_mut() {
                        file.set_type_tree_registry(Some(registry.clone()));
                    }
                }
                let source = BinarySource::path(path);
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
                    Err(e) => {
                        let mut cache = e.into_inner();
                        cache.remove(&source);
                    }
                }
            }
            Ok(UnityFile::SerializedFile(asset)) => {
                let mut asset = asset;
                if let Some(registry) = self.type_tree_registry.clone() {
                    asset.set_type_tree_registry(Some(registry));
                }
                let source = BinarySource::path(path);
                self.invalidate_dependency_scan_cache_for_source(
                    &source,
                    BinarySourceKind::SerializedFile,
                    None,
                );
                self.binary_assets.insert(source, asset);
                match self.bundle_container_cache.write() {
                    Ok(mut cache) => cache.clear(),
                    Err(e) => e.into_inner().clear(),
                }
            }
            Ok(UnityFile::WebFile(web)) => {
                let web_path = path.to_path_buf();
                self.webfiles.insert(web_path.clone(), web);
                self.load_webfile_entries(&web_path)?;
            }
            Err(_) => {}
        }

        Ok(())
    }

    fn load_webfile_entries(&mut self, web_path: &PathBuf) -> Result<()> {
        let web = self.webfiles.get(web_path).ok_or_else(|| {
            UnityAssetError::format(format!("WebFile not loaded: {:?}", web_path))
        })?;

        let mut entry_names: Vec<String> = web.files.iter().map(|f| f.name.clone()).collect();
        entry_names.sort();
        entry_names.dedup();

        for entry_name in entry_names {
            let view = match web.extract_file_view(&entry_name) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let Ok(parsed) =
                load_unity_file_from_shared_range(view.backing_shared(), view.absolute_range())
            else {
                continue;
            };

            match parsed {
                UnityFile::AssetBundle(bundle) => {
                    let mut bundle = bundle;
                    if let Some(registry) = self.type_tree_registry.clone() {
                        for file in bundle.assets.iter_mut() {
                            file.set_type_tree_registry(Some(registry.clone()));
                        }
                    }
                    let source = BinarySource::WebEntry {
                        web_path: web_path.clone(),
                        entry_name: entry_name.clone(),
                    };
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
                        Err(e) => {
                            let mut cache = e.into_inner();
                            cache.remove(&source);
                        }
                    }
                }
                UnityFile::SerializedFile(asset) => {
                    let mut asset = asset;
                    if let Some(registry) = self.type_tree_registry.clone() {
                        asset.set_type_tree_registry(Some(registry));
                    }
                    let source = BinarySource::WebEntry {
                        web_path: web_path.clone(),
                        entry_name,
                    };
                    self.invalidate_dependency_scan_cache_for_source(
                        &source,
                        BinarySourceKind::SerializedFile,
                        None,
                    );
                    self.binary_assets.insert(source, asset);
                    match self.bundle_container_cache.write() {
                        Ok(mut cache) => cache.clear(),
                        Err(e) => e.into_inner().clear(),
                    }
                }
                UnityFile::WebFile(_) => {
                    // Nested WebFiles are uncommon; ignore for now.
                }
            }
        }

        Ok(())
    }

    /// Load all supported files from a directory.
    pub fn load_directory<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
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
        self.traverse_directory(&path)?;

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
            if let Some(max) = options.max_files {
                if stats.files_visited > max {
                    break;
                }
            }

            let path = canonicalize_if_exists(entry.path());
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            if ext == "meta" {
                stats.meta_files_seen += 1;
                if options.index_meta_guids && self.index_meta_guid_path(&path).is_some() {
                    stats.meta_guids_indexed += 1;
                }
                if options.load_meta_documents {
                    if self.load_file(&path).is_ok() {
                        stats.files_loaded += 1;
                        stats.yaml_loaded += 1;
                    }
                }
                continue;
            }

            if matches!(ext, "asset" | "prefab" | "unity") {
                if options.load_yaml_documents {
                    if self.load_file(&path).is_ok() {
                        stats.files_loaded += 1;
                        stats.yaml_loaded += 1;
                    }
                    continue;
                }
            }

            if !options.load_binary_files {
                continue;
            }

            // Fast sniff: only attempt full binary parsing for likely Unity files.
            let mut prefix = [0u8; 64];
            let prefix_len = File::open(&path)
                .and_then(|mut f| f.read(&mut prefix))
                .unwrap_or(0);
            if prefix_len == 0 {
                continue;
            }

            let Some(kind) = sniff_unity_file_kind_prefix(&prefix[..prefix_len]) else {
                continue;
            };

            if matches!(
                kind,
                UnityFileKind::AssetBundle | UnityFileKind::SerializedFile | UnityFileKind::WebFile
            ) && self.load_file(&path).is_ok()
            {
                stats.files_loaded += 1;
                stats.binary_loaded += 1;
            }
        }

        Ok(stats)
    }

    /// Recursively traverse directory and load Unity files.
    fn traverse_directory(&mut self, dir: &Path) -> Result<()> {
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
                            self.traverse_directory(&path)?;
                        }
                    }
                }
            } else if path.is_file() {
                // Try to load the file
                if let Err(e) = self.load_file(&path) {
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

fn load_split_bytes(base: &Path) -> Result<Vec<u8>> {
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
        let bytes = std::fs::read(&part).map_err(|e| {
            UnityAssetError::with_source(format!("Failed to read split part {:?}", part), e)
        })?;
        out.extend_from_slice(&bytes);
    }

    if !found_any {
        return Err(UnityAssetError::format(format!(
            "No split parts found for base path: {:?}",
            base
        )));
    }

    Ok(out)
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
