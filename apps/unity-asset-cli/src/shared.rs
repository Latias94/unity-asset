use anyhow::{Context, Result};
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unity_asset::environment::{
    BinaryObjectKey, BinarySource, BinarySourceKind, Environment, EnvironmentOptions,
    EnvironmentReporter, EnvironmentWarning,
};
use unity_asset::{
    AssetLoadBudget, BundleMemberId, ContainmentKind, ObjectAddress, ObjectKind, SourceAlias,
    SourceLocator, SourceMemberId,
};
use unity_asset_binary::typetree::{CompositeTypeTreeRegistry, TypeTreeRegistry};

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

pub(crate) fn load_environment_input(
    env: &mut Environment,
    input: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    if input.is_dir() && looks_like_unity_project_root(input) {
        let mut loaded_any = false;
        for root in [input.join("Assets"), input.join("ProjectSettings")] {
            if root.exists() {
                env.load(&root, budget)?;
                loaded_any = true;
            }
        }
        if loaded_any {
            return Ok(());
        }
    }
    env.load(input, budget)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ObjectAddressAdapterError {
    AddressIsNotBinary,
    InvalidRuntimeKey {
        source_kind: BinarySourceKind,
        asset_index: Option<usize>,
    },
    SourceOutsideInput {
        input: PathBuf,
        source: PathBuf,
    },
    SourceCanonicalizationFailed {
        path: PathBuf,
        error: String,
    },
    NonUnicodeSourcePath(PathBuf),
    InvalidAddress(String),
    SourceNotLoaded {
        source_kind: BinarySourceKind,
        source: BinarySource,
    },
    AddressSourceNotLoaded {
        source_kind: BinarySourceKind,
        locator: SourceLocator,
    },
    AmbiguousAddressSource {
        source_kind: BinarySourceKind,
        locator: SourceLocator,
        matches: usize,
    },
    BundleAssetIndexOutOfRange {
        asset_index: usize,
        asset_names: usize,
    },
    BundleAssetMissing {
        asset_index: usize,
        assets: usize,
    },
    BundleMemberMissing {
        name: String,
    },
    BundleMemberOccurrenceOutOfRange {
        name: String,
        occurrence: u32,
        available: usize,
    },
    BundleMemberOccurrenceOverflow {
        name: String,
        occurrence: usize,
    },
}

impl fmt::Display for ObjectAddressAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressIsNotBinary => formatter.write_str("object address is not binary"),
            Self::InvalidRuntimeKey {
                source_kind,
                asset_index,
            } => write!(
                formatter,
                "invalid runtime key shape: source_kind={source_kind:?}, asset_index={asset_index:?}"
            ),
            Self::SourceOutsideInput { input, source } => write!(
                formatter,
                "source {} is outside CLI input {}",
                source.display(),
                input.display()
            ),
            Self::SourceCanonicalizationFailed { path, error } => write!(
                formatter,
                "failed to canonicalize source path {}: {error}",
                path.display()
            ),
            Self::NonUnicodeSourcePath(path) => {
                write!(
                    formatter,
                    "source path is not valid UTF-8: {}",
                    path.display()
                )
            }
            Self::InvalidAddress(error) => write!(formatter, "invalid object address: {error}"),
            Self::SourceNotLoaded {
                source_kind,
                source,
            } => write!(
                formatter,
                "runtime source is not loaded: kind={source_kind:?}, source={source}"
            ),
            Self::AddressSourceNotLoaded {
                source_kind,
                locator,
            } => write!(
                formatter,
                "address source is not loaded: kind={source_kind:?}, locator={locator:?}"
            ),
            Self::AmbiguousAddressSource {
                source_kind,
                locator,
                matches,
            } => write!(
                formatter,
                "address source is ambiguous: kind={source_kind:?}, locator={locator:?}, matches={matches}"
            ),
            Self::BundleAssetIndexOutOfRange {
                asset_index,
                asset_names,
            } => write!(
                formatter,
                "bundle asset index {asset_index} has no exact member name (asset_names={asset_names})"
            ),
            Self::BundleAssetMissing {
                asset_index,
                assets,
            } => write!(
                formatter,
                "bundle member resolves to asset index {asset_index}, but bundle has {assets} assets"
            ),
            Self::BundleMemberMissing { name } => {
                write!(formatter, "bundle member {name:?} is not loaded")
            }
            Self::BundleMemberOccurrenceOutOfRange {
                name,
                occurrence,
                available,
            } => write!(
                formatter,
                "bundle member {name:?} occurrence {occurrence} is out of range (available={available})"
            ),
            Self::BundleMemberOccurrenceOverflow { name, occurrence } => write!(
                formatter,
                "bundle member {name:?} occurrence {occurrence} exceeds the address contract"
            ),
        }
    }
}

impl std::error::Error for ObjectAddressAdapterError {}

pub(crate) fn object_address_for_key(
    env: &Environment,
    input: &Path,
    key: &BinaryObjectKey,
) -> std::result::Result<ObjectAddress, ObjectAddressAdapterError> {
    match key.source_kind {
        BinarySourceKind::SerializedFile => {
            if !env.binary_assets().contains_key(&key.source) {
                return Err(ObjectAddressAdapterError::SourceNotLoaded {
                    source_kind: key.source_kind,
                    source: key.source.clone(),
                });
            }
            object_address_for_key_with_bundle_names(input, key, None)
        }
        BinarySourceKind::AssetBundle => {
            let bundle = env.bundles().get(&key.source).ok_or_else(|| {
                ObjectAddressAdapterError::SourceNotLoaded {
                    source_kind: key.source_kind,
                    source: key.source.clone(),
                }
            })?;
            let asset_index =
                key.asset_index
                    .ok_or(ObjectAddressAdapterError::InvalidRuntimeKey {
                        source_kind: key.source_kind,
                        asset_index: key.asset_index,
                    })?;
            if asset_index >= bundle.assets.len() {
                return Err(ObjectAddressAdapterError::BundleAssetMissing {
                    asset_index,
                    assets: bundle.assets.len(),
                });
            }
            object_address_for_key_with_bundle_names(input, key, Some(&bundle.asset_names))
        }
    }
}

pub(crate) fn object_address_for_key_with_bundle_names(
    input: &Path,
    key: &BinaryObjectKey,
    bundle_asset_names: Option<&[String]>,
) -> std::result::Result<ObjectAddress, ObjectAddressAdapterError> {
    let locator = source_locator_for_binary_source(input, &key.source)?;
    match (key.source_kind, key.asset_index, bundle_asset_names) {
        (BinarySourceKind::SerializedFile, None, None) => {
            ObjectAddress::binary_direct(locator, key.path_id)
                .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))
        }
        (BinarySourceKind::AssetBundle, Some(asset_index), Some(asset_names)) => {
            let member = bundle_member_for_asset_index(asset_names, asset_index)?;
            ObjectAddress::binary_bundle_member(locator, member, key.path_id)
                .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))
        }
        _ => Err(ObjectAddressAdapterError::InvalidRuntimeKey {
            source_kind: key.source_kind,
            asset_index: key.asset_index,
        }),
    }
}

pub(crate) fn binary_object_key_for_address(
    env: &Environment,
    input: &Path,
    address: &ObjectAddress,
) -> std::result::Result<BinaryObjectKey, ObjectAddressAdapterError> {
    if address.kind() != ObjectKind::Binary {
        return Err(ObjectAddressAdapterError::AddressIsNotBinary);
    }
    let path_id = address
        .binary_path_id()
        .ok_or(ObjectAddressAdapterError::AddressIsNotBinary)?;

    if let Some(member) = address.bundle_member() {
        let mut candidates = Vec::new();
        for source in env.bundles().keys() {
            let locator = source_locator_for_binary_source(input, source)?;
            if is_bundle_child_of(address.source_locator(), &locator) {
                candidates.push(source);
            }
        }
        let source = unique_address_source(
            BinarySourceKind::AssetBundle,
            address.source_locator(),
            candidates,
        )?;
        let bundle = env.bundles().get(source).ok_or_else(|| {
            ObjectAddressAdapterError::SourceNotLoaded {
                source_kind: BinarySourceKind::AssetBundle,
                source: source.clone(),
            }
        })?;
        let asset_index = bundle_asset_index_for_member(&bundle.asset_names, member)?;
        if asset_index >= bundle.assets.len() {
            return Err(ObjectAddressAdapterError::BundleAssetMissing {
                asset_index,
                assets: bundle.assets.len(),
            });
        }
        return Ok(BinaryObjectKey {
            source: source.clone(),
            source_kind: BinarySourceKind::AssetBundle,
            asset_index: Some(asset_index),
            path_id,
        });
    }

    let mut candidates = Vec::new();
    for source in env.binary_assets().keys() {
        let locator = source_locator_for_binary_source(input, source)?;
        if locator == *address.source_locator() {
            candidates.push(source);
        }
    }
    let source = unique_address_source(
        BinarySourceKind::SerializedFile,
        address.source_locator(),
        candidates,
    )?;
    Ok(BinaryObjectKey {
        source: source.clone(),
        source_kind: BinarySourceKind::SerializedFile,
        asset_index: None,
        path_id,
    })
}

fn unique_address_source<'a>(
    source_kind: BinarySourceKind,
    locator: &SourceLocator,
    candidates: Vec<&'a BinarySource>,
) -> std::result::Result<&'a BinarySource, ObjectAddressAdapterError> {
    match candidates.as_slice() {
        [source] => Ok(*source),
        [] => Err(ObjectAddressAdapterError::AddressSourceNotLoaded {
            source_kind,
            locator: locator.clone(),
        }),
        candidates => Err(ObjectAddressAdapterError::AmbiguousAddressSource {
            source_kind,
            locator: locator.clone(),
            matches: candidates.len(),
        }),
    }
}

fn source_locator_for_binary_source(
    input: &Path,
    source: &BinarySource,
) -> std::result::Result<SourceLocator, ObjectAddressAdapterError> {
    let (outer_path, containment) = match source {
        BinarySource::Path(path) => (path.as_path(), None),
        BinarySource::ArchiveEntry {
            archive_path,
            entry_name,
        } => (
            archive_path.as_path(),
            Some((ContainmentKind::Archive, entry_name.as_str())),
        ),
        BinarySource::WebEntry {
            web_path,
            entry_name,
        } => (
            web_path.as_path(),
            Some((ContainmentKind::WebFile, entry_name.as_str())),
        ),
    };
    let alias = source_alias_for_path(input, outer_path)?;
    let mut locator = SourceLocator::path(alias.as_str().to_owned())
        .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))?;
    if let Some((container, member)) = containment {
        locator = locator
            .child(
                container,
                SourceMemberId::new(member.to_owned()).map_err(|error| {
                    ObjectAddressAdapterError::InvalidAddress(error.to_string())
                })?,
            )
            .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))?;
    }
    Ok(locator)
}

fn source_alias_for_path(
    input: &Path,
    source: &Path,
) -> std::result::Result<SourceAlias, ObjectAddressAdapterError> {
    let alias_path = if input.is_dir() {
        relative_to_input(input, source)?
    } else {
        if !paths_are_same(input, source)? {
            return Err(ObjectAddressAdapterError::SourceOutsideInput {
                input: input.to_path_buf(),
                source: source.to_path_buf(),
            });
        }
        PathBuf::from(source.file_name().ok_or_else(|| {
            ObjectAddressAdapterError::SourceOutsideInput {
                input: input.to_path_buf(),
                source: source.to_path_buf(),
            }
        })?)
    };
    let alias = alias_path
        .to_str()
        .ok_or_else(|| ObjectAddressAdapterError::NonUnicodeSourcePath(alias_path.clone()))?;
    #[cfg(windows)]
    let alias = alias.replace('\\', "/");
    #[cfg(not(windows))]
    let alias = alias.to_owned();
    SourceAlias::new(alias)
        .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))
}

fn relative_to_input(
    input: &Path,
    source: &Path,
) -> std::result::Result<PathBuf, ObjectAddressAdapterError> {
    let canonical_input = canonicalize_source_path(input)?;
    let canonical_source = canonicalize_source_path(source)?;
    canonical_source
        .strip_prefix(&canonical_input)
        .map(Path::to_path_buf)
        .map_err(|_| ObjectAddressAdapterError::SourceOutsideInput {
            input: canonical_input,
            source: canonical_source,
        })
}

fn paths_are_same(
    left: &Path,
    right: &Path,
) -> std::result::Result<bool, ObjectAddressAdapterError> {
    Ok(canonicalize_source_path(left)? == canonicalize_source_path(right)?)
}

fn canonicalize_source_path(
    path: &Path,
) -> std::result::Result<PathBuf, ObjectAddressAdapterError> {
    std::fs::canonicalize(path).map_err(|error| {
        ObjectAddressAdapterError::SourceCanonicalizationFailed {
            path: path.to_path_buf(),
            error: error.to_string(),
        }
    })
}

fn bundle_member_for_asset_index(
    asset_names: &[String],
    asset_index: usize,
) -> std::result::Result<BundleMemberId, ObjectAddressAdapterError> {
    let name = asset_names.get(asset_index).ok_or(
        ObjectAddressAdapterError::BundleAssetIndexOutOfRange {
            asset_index,
            asset_names: asset_names.len(),
        },
    )?;
    let occurrence = asset_names[..asset_index]
        .iter()
        .filter(|candidate| *candidate == name)
        .count();
    let occurrence = u32::try_from(occurrence).map_err(|_| {
        ObjectAddressAdapterError::BundleMemberOccurrenceOverflow {
            name: name.clone(),
            occurrence,
        }
    })?;
    SourceMemberId::with_occurrence(name.clone(), occurrence)
        .map_err(|error| ObjectAddressAdapterError::InvalidAddress(error.to_string()))
}

fn bundle_asset_index_for_member(
    asset_names: &[String],
    member: &BundleMemberId,
) -> std::result::Result<usize, ObjectAddressAdapterError> {
    let matches = asset_names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| (name == member.name()).then_some(index))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(ObjectAddressAdapterError::BundleMemberMissing {
            name: member.name().to_owned(),
        });
    }
    matches
        .get(member.same_name_occurrence() as usize)
        .copied()
        .ok_or_else(
            || ObjectAddressAdapterError::BundleMemberOccurrenceOutOfRange {
                name: member.name().to_owned(),
                occurrence: member.same_name_occurrence(),
                available: matches.len(),
            },
        )
}

fn is_bundle_child_of(address: &SourceLocator, parent: &SourceLocator) -> bool {
    address.root_alias() == parent.root_alias()
        && address.members().len() == parent.members().len() + 1
        && address.members()[..parent.members().len()] == *parent.members()
        && address
            .members()
            .last()
            .is_some_and(|step| step.container() == ContainmentKind::Bundle)
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
            runtime_key = %key,
            field = %warning.field,
            error = %warning.error,
            "typetree warning"
        );
        if !self.enabled {
            return;
        }
        eprintln!(
            "warning: typetree runtime_key={} field={} error={}",
            key, warning.field, warning.error
        );
    }
}

pub(crate) fn build_environment(
    strict: bool,
    show_warnings: bool,
    typetree_registries: &[PathBuf],
    budget: &mut AssetLoadBudget,
) -> Result<Environment> {
    let registry = load_typetree_registry(typetree_registries, budget)?;
    Ok(build_environment_with_registry(
        strict,
        show_warnings,
        registry,
    ))
}

pub(crate) fn build_environment_with_registry(
    strict: bool,
    show_warnings: bool,
    registry: Option<Arc<dyn TypeTreeRegistry>>,
) -> Environment {
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
    env.set_type_tree_registry(registry);
    env
}

pub(crate) fn load_typetree_registry(
    typetree_registries: &[PathBuf],
    budget: &mut AssetLoadBudget,
) -> Result<Option<Arc<dyn TypeTreeRegistry>>> {
    CompositeTypeTreeRegistry::from_paths(typetree_registries, budget)
        .context("Failed to load --typetree-registry paths")
}

pub(crate) fn load_serialized_file_for_scan(
    path: &Path,
    budget: &mut unity_asset::AssetLoadBudget,
) -> unity_asset_binary::error::Result<unity_asset_binary::asset::SerializedFile> {
    unity_asset_binary::file::load_serialized_file_with_budget(path, false, budget)
}

/// Resolves legacy `--source` flags. Persisted identities must use
/// `binary_object_key_for_address`, which never applies filename fallback.
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
            .map(|info| (info.class_id(), info.byte_size()))
            .unwrap_or((0, 0)),
        unity_asset::environment::BinarySourceKind::SerializedFile => env
            .binary_assets()
            .get(&key.source)
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.class_id(), info.byte_size()))
            .unwrap_or((0, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset::{AssetLoadLimits, BudgetError};

    fn budget_error_in_chain(error: &anyhow::Error) -> Option<&BudgetError> {
        error
            .chain()
            .find_map(|source| source.downcast_ref::<BudgetError>())
    }

    fn bundle_key(source: &Path, asset_index: Option<usize>, path_id: i64) -> BinaryObjectKey {
        BinaryObjectKey {
            source: BinarySource::path(source),
            source_kind: BinarySourceKind::AssetBundle,
            asset_index,
            path_id,
        }
    }

    fn create_source(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"fixture").unwrap();
    }

    #[test]
    fn typetree_registry_table_preserves_member_budget_error() {
        let limits = AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let paths = vec![PathBuf::from("first.json"), PathBuf::from("second.json")];

        let error = load_typetree_registry(&paths, &mut budget).unwrap_err();

        assert!(matches!(
            budget_error_in_chain(&error),
            Some(BudgetError::Exceeded {
                resource: "members",
                limit: 1,
                requested: 2,
            })
        ));
    }

    #[test]
    fn typetree_registry_table_preserves_byte_budget_error() {
        let registry_table_bytes = 2 * std::mem::size_of::<Arc<dyn TypeTreeRegistry>>();
        let limits = AssetLoadLimits {
            max_bytes: u64::try_from(registry_table_bytes - 1).unwrap(),
            ..AssetLoadLimits::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let paths = vec![PathBuf::from("first.json"), PathBuf::from("second.json")];

        let error = load_typetree_registry(&paths, &mut budget).unwrap_err();

        assert!(matches!(
            budget_error_in_chain(&error),
            Some(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if *limit == u64::try_from(registry_table_bytes - 1).unwrap()
                && *requested == u64::try_from(registry_table_bytes).unwrap()
        ));
    }

    #[test]
    fn bundle_member_reorder_keeps_object_address_stable() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("game.ab");
        create_source(&source);
        let original_names = vec!["cab/main".into(), "cab/other".into(), "cab/main".into()];
        let reordered_names = vec!["cab/main".into(), "cab/main".into(), "cab/other".into()];

        let original = object_address_for_key_with_bundle_names(
            root.path(),
            &bundle_key(&source, Some(2), 42),
            Some(&original_names),
        )
        .unwrap();
        let reordered = object_address_for_key_with_bundle_names(
            root.path(),
            &bundle_key(&source, Some(1), 42),
            Some(&reordered_names),
        )
        .unwrap();

        assert_eq!(original, reordered);
        let member = original.bundle_member().unwrap();
        assert_eq!(member.same_name_occurrence(), 1);
        assert_eq!(
            bundle_asset_index_for_member(&original_names, member),
            Ok(2)
        );
        assert_eq!(
            bundle_asset_index_for_member(&reordered_names, member),
            Ok(1)
        );
    }

    #[test]
    fn same_path_id_in_different_bundle_members_has_different_addresses() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("game.ab");
        create_source(&source);
        let names = vec!["cab/left".into(), "cab/right".into()];

        let left = object_address_for_key_with_bundle_names(
            root.path(),
            &bundle_key(&source, Some(0), 7),
            Some(&names),
        )
        .unwrap();
        let right = object_address_for_key_with_bundle_names(
            root.path(),
            &bundle_key(&source, Some(1), 7),
            Some(&names),
        )
        .unwrap();

        assert_ne!(left, right);
    }

    #[test]
    fn webfile_bundle_address_keeps_the_full_containment_chain() {
        let root = tempfile::tempdir().unwrap();
        let web_path = root.path().join("game.web");
        create_source(&web_path);
        let key = BinaryObjectKey {
            source: BinarySource::WebEntry {
                web_path: Arc::new(web_path),
                entry_name: "embedded/game.ab".into(),
            },
            source_kind: BinarySourceKind::AssetBundle,
            asset_index: Some(0),
            path_id: -7,
        };
        let names = vec!["cab/main".into()];

        let address =
            object_address_for_key_with_bundle_names(root.path(), &key, Some(&names)).unwrap();
        let containers = address
            .source_locator()
            .members()
            .iter()
            .map(|step| step.container())
            .collect::<Vec<_>>();

        assert_eq!(
            containers,
            vec![ContainmentKind::WebFile, ContainmentKind::Bundle]
        );
        assert_eq!(
            address
                .to_compact_string()
                .unwrap()
                .parse::<ObjectAddress>(),
            Ok(address)
        );
    }

    #[test]
    fn bundle_member_resolution_rejects_missing_and_out_of_range_occurrence() {
        let names = vec!["cab/main".into(), "cab/main".into()];
        let missing = SourceMemberId::new("cab/missing").unwrap();
        assert!(matches!(
            bundle_asset_index_for_member(&names, &missing),
            Err(ObjectAddressAdapterError::BundleMemberMissing { .. })
        ));

        let out_of_range = SourceMemberId::with_occurrence("cab/main", 2).unwrap();
        assert!(matches!(
            bundle_asset_index_for_member(&names, &out_of_range),
            Err(ObjectAddressAdapterError::BundleMemberOccurrenceOutOfRange { available: 2, .. })
        ));
    }

    #[test]
    fn duplicate_runtime_source_match_is_rejected() {
        let locator = SourceLocator::path("game.ab").unwrap();
        let left = BinarySource::path("left.ab");
        let right = BinarySource::path("right.ab");
        assert!(matches!(
            unique_address_source(BinarySourceKind::AssetBundle, &locator, vec![&left, &right]),
            Err(ObjectAddressAdapterError::AmbiguousAddressSource { matches: 2, .. })
        ));
    }

    #[test]
    fn source_with_the_same_filename_outside_input_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("game.ab");
        create_source(&source);
        let key = bundle_key(&source, Some(0), 1);
        let names = vec!["cab/main".into()];

        assert!(matches!(
            object_address_for_key_with_bundle_names(root.path(), &key, Some(&names)),
            Err(ObjectAddressAdapterError::SourceOutsideInput { .. })
        ));
    }

    #[test]
    fn missing_source_cannot_receive_a_persisted_address() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("missing.ab");
        let key = bundle_key(&source, Some(0), 1);
        let names = vec!["cab/main".into()];

        assert!(matches!(
            object_address_for_key_with_bundle_names(root.path(), &key, Some(&names)),
            Err(ObjectAddressAdapterError::SourceCanonicalizationFailed { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_literal_backslash_is_rejected_instead_of_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("a\\b.assets");
        create_source(&source);
        let key = BinaryObjectKey {
            source: BinarySource::path(&source),
            source_kind: BinarySourceKind::SerializedFile,
            asset_index: None,
            path_id: 1,
        };

        assert!(matches!(
            object_address_for_key_with_bundle_names(root.path(), &key, None),
            Err(ObjectAddressAdapterError::InvalidAddress(_))
        ));
    }

    #[test]
    fn legacy_bok_is_not_an_object_address() {
        let legacy = "bok2|serialized|-|1|8|game.ab|0|";
        assert!(legacy.parse::<ObjectAddress>().is_err());
    }
}
