use super::*;

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
        let path = path.as_ref();

        // Check file extension to determine type
        if let Some(ext) = path.extension() {
            if ext == "meta" {
                // Index meta GUIDs even if YAML parsing fails (best-effort reference resolution).
                self.index_meta_guid_path(path);
            }

            match ext.to_str() {
                Some("asset") | Some("prefab") | Some("unity") | Some("meta") => {
                    match YamlDocument::load_yaml_with_warnings(path, false) {
                        Ok((doc, warnings)) => {
                            for w in warnings {
                                self.push_warning(EnvironmentWarning::YamlDocumentSkipped {
                                    path: path.to_path_buf(),
                                    doc_index: w.doc_index,
                                    error: w.error,
                                });
                            }
                            self.yaml_documents.insert(path.to_path_buf(), doc);
                        }
                        Err(_) => {
                            // Some Unity projects can store `.asset`-like files in binary form.
                            // If YAML parsing fails, fall back to binary detection.
                            self.try_load_binary(path)?;
                        }
                    }
                }
                _ => {
                    // Best-effort binary detection for common build outputs.
                    self.try_load_binary(path)?;
                }
            }
        } else {
            // Some Unity outputs (especially streamed resources and certain build artifacts)
            // can be extension-less. Attempt binary detection anyway.
            self.try_load_binary(path)?;
        }

        Ok(())
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

        // Recursively traverse directory
        self.traverse_directory(path)?;

        Ok(())
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
