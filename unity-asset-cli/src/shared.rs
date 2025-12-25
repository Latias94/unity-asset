use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unity_asset::environment::{
    BinaryObjectKey, BinarySource, Environment, EnvironmentOptions, EnvironmentReporter,
    EnvironmentWarning,
};
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_binary::typetree::{JsonTypeTreeRegistry, TpkTypeTreeRegistry, TypeTreeRegistry};

#[derive(Debug, Clone)]
pub(crate) struct AppContext {
    pub(crate) strict: bool,
    pub(crate) show_warnings: bool,
    pub(crate) typetree_registry: Option<PathBuf>,
}

impl AppContext {
    pub(crate) fn typetree_registry(&self) -> Option<&PathBuf> {
        self.typetree_registry.as_ref()
    }
}

#[derive(Debug)]
struct CliReporter {
    enabled: bool,
}

impl EnvironmentReporter for CliReporter {
    fn warn(&self, warning: &EnvironmentWarning) {
        if !self.enabled {
            return;
        }
        eprintln!("warning: {}", warning);
    }

    fn typetree_warning(
        &self,
        key: &BinaryObjectKey,
        warning: &unity_asset_binary::typetree::TypeTreeParseWarning,
    ) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "warning: typetree key={} field={} error={}",
            key, warning.field, warning.error
        );
    }
}

pub(crate) fn build_environment(
    strict: bool,
    show_warnings: bool,
    typetree_registry: Option<&PathBuf>,
) -> Result<Environment> {
    let mut env = if strict {
        Environment::with_options(EnvironmentOptions::strict())
    } else {
        Environment::new()
    };

    let reporter: Option<Arc<dyn EnvironmentReporter>> = if show_warnings {
        Some(Arc::new(CliReporter { enabled: true }))
    } else {
        None
    };
    env.set_reporter(reporter);

    if let Some(path) = typetree_registry {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "tpk" {
            let registry = TpkTypeTreeRegistry::from_path(path).map_err(|e| {
                anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e)
            })?;
            env.set_type_tree_registry(Some(Arc::new(registry)));
        } else {
            let registry = JsonTypeTreeRegistry::from_path(path).map_err(|e| {
                anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e)
            })?;
            env.set_type_tree_registry(Some(Arc::new(registry)));
        }
    }

    Ok(env)
}

pub(crate) fn load_typetree_registry(
    typetree_registry: Option<&PathBuf>,
) -> Result<Option<Arc<dyn TypeTreeRegistry>>> {
    let Some(path) = typetree_registry else {
        return Ok(None);
    };
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "tpk" {
        let registry = TpkTypeTreeRegistry::from_path(path)
            .map_err(|e| anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e))?;
        Ok(Some(Arc::new(registry)))
    } else {
        let registry = JsonTypeTreeRegistry::from_path(path)
            .map_err(|e| anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e))?;
        Ok(Some(Arc::new(registry)))
    }
}

pub(crate) fn load_serialized_file_for_scan(
    path: &Path,
) -> Result<unity_asset_binary::asset::SerializedFile> {
    #[cfg(feature = "mmap")]
    {
        use std::sync::Arc;

        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let shared = SharedBytes::Mmap(Arc::new(mmap));
        let len = shared.len();
        return Ok(
            unity_asset_binary::asset::SerializedFileParser::from_shared_range(shared, 0..len)?,
        );
    }

    #[cfg(not(feature = "mmap"))]
    {
        let bytes = std::fs::read(path)?;
        Ok(unity_asset_binary::asset::SerializedFileParser::from_bytes(
            bytes,
        )?)
    }
}

pub(crate) fn resolve_loaded_source(
    env: &Environment,
    kind: unity_asset::environment::BinarySourceKind,
    requested: &BinarySource,
) -> Result<BinarySource> {
    let is_loaded = match kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => {
            env.bundles().contains_key(requested)
        }
        unity_asset::environment::BinarySourceKind::SerializedFile => {
            env.binary_assets().contains_key(requested)
        }
    };
    if is_loaded {
        return Ok(requested.clone());
    }

    let BinarySource::Path(requested_path) = requested else {
        anyhow::bail!("Source not found in loaded environment: {:?}", requested);
    };

    let keys: Vec<&PathBuf> = match kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => env
            .bundles()
            .keys()
            .filter_map(|k| match k {
                BinarySource::Path(p) => Some(p),
                _ => None,
            })
            .collect(),
        unity_asset::environment::BinarySourceKind::SerializedFile => env
            .binary_assets()
            .keys()
            .filter_map(|k| match k {
                BinarySource::Path(p) => Some(p),
                _ => None,
            })
            .collect(),
    };

    let requested_canon = std::fs::canonicalize(requested_path).ok();
    if let Some(requested_canon) = requested_canon {
        let mut matches = Vec::new();
        for k in &keys {
            if let Ok(k_canon) = std::fs::canonicalize(k) {
                if k_canon == requested_canon {
                    matches.push((*k).clone());
                }
            }
        }
        if matches.len() == 1 {
            return Ok(BinarySource::path(matches[0].clone()));
        }
        if matches.len() > 1 {
            anyhow::bail!(
                "Ambiguous source path: {:?} matches multiple loaded sources",
                requested_path
            );
        }
    }

    if let Some(file_name) = requested_path.file_name() {
        let mut matches: Vec<PathBuf> = keys
            .iter()
            .filter(|p| p.file_name() == Some(file_name))
            .map(|p| (*p).clone())
            .collect();
        matches.sort();
        matches.dedup();
        if matches.len() == 1 {
            return Ok(BinarySource::path(matches[0].clone()));
        }
    }

    let mut available: Vec<String> = keys.iter().map(|p| p.display().to_string()).collect();
    available.sort();

    anyhow::bail!(
        "Source not found in loaded environment: {:?} (kind={:?}). Loaded sources:\n- {}",
        requested_path,
        kind,
        available.join("\n- ")
    )
}

pub(crate) fn lookup_object_type_info(env: &Environment, key: &BinaryObjectKey) -> (i32, u32) {
    match key.source_kind {
        unity_asset::environment::BinarySourceKind::AssetBundle => env
            .bundles()
            .get(&key.source)
            .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
        unity_asset::environment::BinarySourceKind::SerializedFile => env
            .binary_assets()
            .get(&key.source)
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
    }
}
