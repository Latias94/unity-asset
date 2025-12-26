use anyhow::Result;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unity_asset::environment::{
    BinaryObjectKey, BinarySource, Environment, EnvironmentOptions, EnvironmentReporter,
    EnvironmentWarning,
};
use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, JsonTypeTreeRegistry, TpkTypeTreeRegistry, TypeTreeRegistry,
};

pub(crate) fn cli_warn(show: bool, msg: impl std::fmt::Display) {
    let msg = msg.to_string();
    tracing::warn!(message = %msg);
    if show {
        eprintln!("warning: {}", msg);
    }
}

fn looks_like_unity_project_root(dir: &Path) -> bool {
    dir.join("Assets").is_dir() && dir.join("ProjectSettings").is_dir()
}

pub(crate) fn load_environment_input(env: &mut Environment, input: &Path) -> Result<()> {
    if input.is_dir() && looks_like_unity_project_root(input) {
        let mut loaded_any = false;
        for root in [input.join("Assets"), input.join("ProjectSettings")] {
            if root.exists() {
                env.load(&root)?;
                loaded_any = true;
            }
        }
        if loaded_any {
            return Ok(());
        }
    }
    env.load(input)?;
    Ok(())
}

pub(crate) fn class_name_for_id(class_id: i32) -> Cow<'static, str> {
    unity_asset::get_class_name_str(class_id)
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(format!("Class_{}", class_id)))
}

#[derive(Debug, Clone)]
pub(crate) struct AppContext {
    pub(crate) strict: bool,
    pub(crate) show_warnings: bool,
    pub(crate) typetree_registries: Vec<PathBuf>,
}

impl AppContext {
    pub(crate) fn typetree_registries(&self) -> &[PathBuf] {
        self.typetree_registries.as_slice()
    }
}

#[derive(Debug)]
struct CliReporter {
    enabled: bool,
}

impl EnvironmentReporter for CliReporter {
    fn warn(&self, warning: &EnvironmentWarning) {
        tracing::warn!(warning = %warning, "environment warning");
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
        tracing::warn!(
            key = %key,
            field = %warning.field,
            error = %warning.error,
            "typetree warning"
        );
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
    typetree_registries: &[PathBuf],
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

    let registry = load_typetree_registry(typetree_registries)?;
    env.set_type_tree_registry(registry);

    Ok(env)
}

pub(crate) fn load_typetree_registry(
    typetree_registries: &[PathBuf],
) -> Result<Option<Arc<dyn TypeTreeRegistry>>> {
    if typetree_registries.is_empty() {
        return Ok(None);
    };

    let mut composite = CompositeTypeTreeRegistry::default();
    for path in typetree_registries {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "tpk" {
            let registry = TpkTypeTreeRegistry::from_path(path).map_err(|e| {
                anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e)
            })?;
            composite.push(Arc::new(registry));
        } else {
            let registry = JsonTypeTreeRegistry::from_path(path).map_err(|e| {
                anyhow::anyhow!("Failed to load --typetree-registry {:?}: {}", path, e)
            })?;
            composite.push(Arc::new(registry));
        }
    }

    if composite.is_empty() {
        return Ok(None);
    }

    Ok(Some(Arc::new(composite)))
}

pub(crate) fn load_serialized_file_for_scan(
    path: &Path,
) -> Result<unity_asset_binary::asset::SerializedFile> {
    Ok(unity_asset_binary::file::load_serialized_file(path, false)?)
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
