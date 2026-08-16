use crate::fast_path;
use crate::shared::AppContext;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use unity_asset::workspace::{
    AssetWorkspace, SourceAdmissionBatch, SourceAdmissionDisposition, SourceAdmissionOperation,
    SourceAdmissionPolicy, SourceOpenRequest, WorkspaceOptions, recognize_source,
};
use unity_asset::{AssetLoadBudget, SourceAlias, SourceKind, WorkspaceId};

const INTERNAL_DIRECTORY_NAMES: &[&str] = &[".unity-asset-recovery"];
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    "Library",
    "Temp",
    "Logs",
    ".git",
    ".vs",
    "obj",
    "bin",
    "UserSettings",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    kind_hint: Option<SourceKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    File,
    Directory,
    Other,
}

impl InputKind {
    fn inspect(path: &Path) -> Self {
        if path.is_file() {
            Self::File
        } else if path.is_dir() {
            Self::Directory
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryPolicy {
    Generic,
    UnityProject,
}

pub(crate) fn load_workspace(
    input: &Path,
    excluded_root: Option<&Path>,
    discovery_policy: DiscoveryPolicy,
    workspace_id: Option<WorkspaceId>,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    let input = std::path::absolute(input).context("Failed to normalize the input path")?;
    let input = match std::fs::canonicalize(&input) {
        Ok(input) => input,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("Input does not exist: {}", input.display());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to resolve input path {}", input.display()));
        }
    };
    let input = input.as_path();
    let input_kind = InputKind::inspect(input);

    let workspace_options = if ctx.strict {
        WorkspaceOptions::strict()
    } else {
        WorkspaceOptions::lenient()
    };
    let workspace_options = workspace_options
        .with_type_tree_registry_paths(ctx.typetree_registries(), budget)
        .context("Failed to load --typetree-registry paths")?;
    let mut workspace = match workspace_id {
        Some(workspace_id) => AssetWorkspace::with_workspace_id(workspace_id, workspace_options),
        None => AssetWorkspace::with_options(workspace_options),
    }
    .context("Failed to initialize asset workspace")?;

    let discovery =
        discover_candidates(input, input_kind, excluded_root, discovery_policy, budget)?;
    let discovery_len = discovery.len();
    let mut batch = SourceAdmissionBatch::with_capacity(discovery_len, budget)
        .context("Failed to reserve the workspace source-admission batch")?;
    for (path, candidate) in discovery {
        let alias = source_alias(input, input_kind, &path, budget)?;
        let request = SourceOpenRequest::new(path, alias);
        let request = match candidate.kind_hint {
            Some(kind) => request.with_kind_hint(kind),
            None => request,
        };
        batch
            .try_push(SourceAdmissionOperation::LoadPath(request), budget)
            .context("Failed to retain a workspace source-admission operation")?;
    }
    let report = match workspace.admit_sources(batch, SourceAdmissionPolicy::Strict, budget) {
        Ok(report) => report,
        Err(error) => {
            let context = if let Some(location) = error.operation_location() {
                format!("Failed to load Unity source at {location}")
            } else if let Some(ordinal) = error.operation_ordinal() {
                format!("Failed to load Unity source operation {ordinal}")
            } else if let Some(phase) = error.batch_phase() {
                format!("Failed to load the Unity source batch during {phase}")
            } else {
                "Failed to load the Unity source batch".to_owned()
            };
            return Err(anyhow::Error::new(error).context(context));
        }
    };
    let root_sources_loaded = report
        .outcomes()
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.disposition(),
                SourceAdmissionDisposition::Loaded { .. }
                    | SourceAdmissionDisposition::Unchanged { .. }
            )
        })
        .count();
    debug_assert_eq!(root_sources_loaded, discovery_len);

    if report.state_installed() {
        debug_assert_ne!(report.base_revision(), report.revision());
    }

    if root_sources_loaded == 0 && input_kind == InputKind::File {
        anyhow::bail!("Input is not a supported Unity source: {}", input.display());
    }
    if root_sources_loaded == 0 {
        ctx.warn(format!(
            "no supported Unity sources found under {}",
            input.display()
        ));
    }

    Ok(workspace)
}

pub(crate) fn load_full_workspace(
    input: &Path,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    load_full_workspace_with_id(input, None, None, ctx, budget)
}

pub(crate) fn load_full_workspace_excluding_output(
    input: &Path,
    output: &Path,
    workspace_id: Option<WorkspaceId>,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    load_full_workspace_with_id(input, Some(output), workspace_id, ctx, budget)
}

/// Loads all supported sources into a caller-selected logical workspace namespace.
///
/// CLI resume manifests carry their original workspace ID so a later process can recreate the
/// same object-address namespace before plan validation.
pub(crate) fn load_full_workspace_with_workspace_id(
    input: &Path,
    workspace_id: WorkspaceId,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    load_full_workspace_with_id(input, None, Some(workspace_id), ctx, budget)
}

fn load_full_workspace_with_id(
    input: &Path,
    excluded_root: Option<&Path>,
    workspace_id: Option<WorkspaceId>,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<AssetWorkspace> {
    load_workspace(
        input,
        excluded_root,
        DiscoveryPolicy::Generic,
        workspace_id,
        ctx,
        budget,
    )
}

fn discover_candidates(
    input: &Path,
    input_kind: InputKind,
    excluded_path: Option<&Path>,
    discovery_policy: DiscoveryPolicy,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(PathBuf, Candidate)>> {
    let excluded_path = excluded_path
        .map(canonicalize_output_boundary)
        .transpose()
        .context("Failed to normalize the excluded output path")?;
    let mut discovered =
        fast_path::collect_candidate_paths_filtered_budgeted(input, budget, |directory| {
            !is_internal_workspace_directory(directory)
                && excluded_path
                    .as_ref()
                    .is_none_or(|excluded| !directory.starts_with(excluded))
                && (discovery_policy != DiscoveryPolicy::UnityProject
                    || !is_skipped_root_project_directory(input, directory))
        })
        .with_context(|| format!("Failed to discover input files under {}", input.display()))?;
    discovered.retain(|path| {
        excluded_path
            .as_ref()
            .is_none_or(|excluded| !path.starts_with(excluded))
    });
    let explicit_file = input_kind == InputKind::File;
    let explicit_candidate = if explicit_file {
        classify_candidate(input, true, budget)?
    } else {
        None
    };
    if matches!(
        explicit_candidate,
        Some(Candidate {
            kind_hint: Some(SourceKind::SerializedFile),
        })
    ) {
        let mut companions = collect_serialized_file_companions(input, budget)?;
        companions.retain(|path| {
            excluded_path
                .as_ref()
                .is_none_or(|excluded| !path.starts_with(excluded))
        });
        append_discovered_paths(&mut discovered, &mut companions, budget)?;
        discovered.sort_unstable();
        discovered.dedup();
    }
    let files_discovered = discovered.len();
    let mut candidates = Vec::new();
    let candidate_allocation = files_discovered
        .checked_mul(std::mem::size_of::<(PathBuf, Candidate)>())
        .context("Unity source discovery allocation overflow")?;
    budget.check_bytes(
        u64::try_from(candidate_allocation)
            .context("Unity source discovery allocation does not fit u64")?,
    )?;
    candidates
        .try_reserve_exact(files_discovered)
        .context("Failed to reserve Unity source discovery results")?;
    budget.consume_bytes(
        u64::try_from(candidate_allocation)
            .context("Unity source discovery allocation does not fit u64")?,
    )?;
    for path in discovered {
        let candidate = if explicit_file && path == input {
            explicit_candidate
        } else {
            classify_candidate(&path, false, budget)?
        };
        if let Some(candidate) = candidate {
            candidates.push((path, candidate));
        }
    }
    Ok(candidates)
}

fn collect_serialized_file_companions(
    input: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PathBuf>> {
    let parent = input
        .parent()
        .context("SerializedFile input has no parent directory")?;
    let mut companions = Vec::new();
    for entry in std::fs::read_dir(parent).with_context(|| {
        format!(
            "Failed to inspect SerializedFile companion directory {}",
            parent.display()
        )
    })? {
        budget.check_entries(1)?;
        budget.check_members(1)?;
        budget.consume_entries(1)?;
        budget.consume_members(1)?;
        let entry = entry.with_context(|| {
            format!(
                "Failed to read SerializedFile companion directory {}",
                parent.display()
            )
        })?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if !recognize_source(&path, &[]).is_streamed_resource() {
            continue;
        }
        append_discovered_path(&mut companions, path, budget)?;
    }
    Ok(companions)
}

fn append_discovered_paths(
    discovered: &mut Vec<PathBuf>,
    additions: &mut Vec<PathBuf>,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    if additions.is_empty() {
        return Ok(());
    }
    let required = discovered
        .len()
        .checked_add(additions.len())
        .context("Unity source discovery path allocation overflow")?;
    let additional_capacity = required.saturating_sub(discovered.capacity());
    let minimum_allocation = path_slot_bytes(additional_capacity)?;
    budget.check_bytes(minimum_allocation)?;
    if additional_capacity != 0 {
        let previous_capacity = discovered.capacity();
        discovered
            .try_reserve_exact(additions.len())
            .context("Failed to reserve SerializedFile companion paths")?;
        let actual_allocation = path_slot_bytes(
            discovered
                .capacity()
                .checked_sub(previous_capacity)
                .context("SerializedFile companion path capacity regressed")?,
        )?;
        budget.check_bytes(actual_allocation)?;
        budget.consume_bytes(actual_allocation)?;
    }
    discovered.append(additions);
    Ok(())
}

fn append_discovered_path(
    paths: &mut Vec<PathBuf>,
    path: PathBuf,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let path_bytes = u64::try_from(path.capacity())
        .context("SerializedFile companion path capacity does not fit u64")?;
    let additional_capacity = if paths.len() == paths.capacity() {
        1
    } else {
        0
    };
    let minimum_allocation = path_bytes
        .checked_add(path_slot_bytes(additional_capacity)?)
        .context("Unity source discovery path allocation overflow")?;
    budget.check_bytes(minimum_allocation)?;
    let actual_allocation = if additional_capacity != 0 {
        let previous_capacity = paths.capacity();
        paths
            .try_reserve_exact(1)
            .context("Failed to reserve SerializedFile companion path")?;
        path_slot_bytes(
            paths
                .capacity()
                .checked_sub(previous_capacity)
                .context("SerializedFile companion path capacity regressed")?,
        )?
    } else {
        0
    };
    let retained = path_bytes
        .checked_add(actual_allocation)
        .context("Unity source discovery path allocation overflow")?;
    budget.check_bytes(retained)?;
    budget.consume_bytes(retained)?;
    paths.push(path);
    Ok(())
}

fn path_slot_bytes(capacity: usize) -> Result<u64> {
    capacity
        .checked_mul(std::mem::size_of::<PathBuf>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .context("Unity source discovery path allocation overflow")
}

fn canonicalize_output_boundary(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        return std::fs::canonicalize(absolute);
    }
    let Some(parent) = absolute.parent() else {
        return Ok(absolute);
    };
    let Some(name) = absolute.file_name() else {
        return Ok(absolute);
    };
    if parent.exists() {
        return std::fs::canonicalize(parent).map(|parent| parent.join(name));
    }
    Ok(absolute)
}

fn classify_candidate(
    path: &Path,
    explicit_file: bool,
    budget: &mut AssetLoadBudget,
) -> Result<Option<Candidate>> {
    let mut prefix = [0_u8; unity_asset::workspace::SOURCE_RECOGNITION_PREFIX_LEN];
    let prefix_len = fast_path::read_prefix_into(path, &mut prefix)
        .with_context(|| format!("Failed to probe {}", path.display()))?;
    budget.consume_bytes(
        u64::try_from(prefix_len).context("Unity source probe length does not fit u64")?,
    )?;
    let prefix = &prefix[..prefix_len];
    let recognition = recognize_source(path, prefix);
    if recognition.is_candidate() {
        return Ok(Some(Candidate {
            // Shared Unity extensions carry no hint, so admission keeps its binary-first parser.
            kind_hint: recognition.kind_hint(),
        }));
    }
    if explicit_file {
        return Ok(Some(Candidate { kind_hint: None }));
    }
    Ok(None)
}

fn is_skipped_root_project_directory(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                SKIPPED_DIRECTORY_NAMES
                    .iter()
                    .any(|skipped| name.eq_ignore_ascii_case(skipped))
            })
}

fn is_internal_workspace_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            INTERNAL_DIRECTORY_NAMES
                .iter()
                .any(|internal| name.eq_ignore_ascii_case(internal))
        })
}

fn source_alias(
    root: &Path,
    input_kind: InputKind,
    path: &Path,
    budget: &AssetLoadBudget,
) -> Result<SourceAlias> {
    let relative = if input_kind == InputKind::Directory {
        path.strip_prefix(root).with_context(|| {
            format!(
                "Discovered source {} is outside input root {}",
                path.display(),
                root.display()
            )
        })?
    } else {
        path.file_name()
            .map(Path::new)
            .context("Input source has no file name")?
    };
    let relative = relative
        .to_str()
        .with_context(|| format!("Source path is not UTF-8: {}", relative.display()))?;
    let requested =
        u64::try_from(relative.len()).context("Source alias length does not fit u64")?;
    budget.check_bytes(requested)?;
    let mut portable = String::new();
    portable
        .try_reserve_exact(relative.len())
        .context("Failed to reserve a portable source alias")?;
    let retained =
        u64::try_from(portable.capacity()).context("Source alias capacity does not fit u64")?;
    budget.check_bytes(retained)?;
    portable.extend(
        relative
            .chars()
            .map(|character| if character == '\\' { '/' } else { character }),
    );
    SourceAlias::new(portable).context("Source path cannot be represented as a portable alias")
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset::AssetLoadLimits;

    #[test]
    fn companion_scan_charges_each_directory_member() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("main.assets");
        std::fs::write(&input, b"serialized").unwrap();
        std::fs::write(directory.path().join("CAB-main.resS"), b"resource").unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(
            collect_serialized_file_companions(&input, &mut budget).is_err(),
            "the second parent-directory entry must exceed the shared scan limits"
        );
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().members, 1);
    }

    #[test]
    fn companion_path_retention_charges_actual_pathbuf_capacity() {
        let mut path = PathBuf::with_capacity(4_096);
        path.push("CAB-main.resS");
        let mut paths = Vec::new();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(path.capacity()).unwrap() - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(append_discovered_path(&mut paths, path, &mut budget).is_err());
        assert!(paths.is_empty());
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn explicit_serialized_discovery_excludes_streamed_resource_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("main.assets");
        let mut serialized = [0_u8; 17];
        serialized[0..4].copy_from_slice(&1_u32.to_be_bytes());
        serialized[4..8].copy_from_slice(&17_u32.to_be_bytes());
        serialized[8..12].copy_from_slice(&8_u32.to_be_bytes());
        serialized[12..16].copy_from_slice(&16_u32.to_be_bytes());
        std::fs::write(&input, serialized).unwrap();

        let retained = directory.path().join("CAB-main.resS");
        let excluded = directory.path().join("generated.resource");
        std::fs::write(&retained, b"retained companion").unwrap();
        std::fs::write(&excluded, b"excluded companion").unwrap();
        let input = std::fs::canonicalize(input).unwrap();
        let retained = std::fs::canonicalize(retained).unwrap();
        let excluded = std::fs::canonicalize(excluded).unwrap();

        let discovery = discover_candidates(
            &input,
            InputKind::File,
            Some(&excluded),
            DiscoveryPolicy::Generic,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.len(), 2, "unexpected discovery: {discovery:#?}");
        assert!(discovery.iter().any(|(path, _)| path == &input));
        assert!(discovery.iter().any(|(path, candidate)| {
            path == &retained && candidate.kind_hint == Some(SourceKind::StreamedResource)
        }));
        assert!(discovery.iter().all(|(path, _)| path != &excluded));
    }

    #[test]
    fn nested_sources_receive_portable_root_relative_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let source = root.join("Assets").join("Shared").join("thing.asset");
        assert_eq!(
            source_alias(
                root,
                InputKind::Directory,
                &source,
                &AssetLoadBudget::default(),
            )
            .unwrap()
            .as_str(),
            "Assets/Shared/thing.asset"
        );
    }

    #[test]
    fn every_workspace_yaml_extension_is_selected_by_discovery() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "clip.anim",
            "data.asset",
            "state.controller",
            "material.mat",
            "asset.meta",
            "object.prefab",
            "scene.unity",
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, []).unwrap();
            let candidate =
                classify_candidate(&path, false, &mut AssetLoadBudget::default()).unwrap();
            assert_eq!(
                candidate,
                Some(Candidate { kind_hint: None }),
                "extension was not selected: {name}"
            );
        }
    }

    #[test]
    fn discovery_prunes_generated_directories_before_selecting_supported_sources() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("Assets");
        let library = directory.path().join("Library");
        let nested_temp = assets.join("Temp");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&nested_temp).unwrap();
        let target = assets.join("000-target.prefab");
        std::fs::write(
            &target,
            b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Target\n",
        )
        .unwrap();
        std::fs::write(assets.join("000-unrelated.txt"), b"not a Unity source").unwrap();
        std::fs::write(library.join("000-generated.prefab"), b"generated").unwrap();
        let nested = nested_temp.join("nested.prefab");
        std::fs::write(&nested, b"nested Unity YAML").unwrap();
        let project_root = std::fs::canonicalize(directory.path()).unwrap();
        let assets = project_root.join("Assets");
        let target = assets.join("000-target.prefab");
        let nested = assets.join("Temp").join("nested.prefab");

        let discovery = discover_candidates(
            &project_root,
            InputKind::Directory,
            None,
            DiscoveryPolicy::UnityProject,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.len(), 2, "unexpected discovery: {discovery:#?}");
        assert_eq!(discovery[0].0, target);
        assert_eq!(discovery[1].0, nested);

        let generic = discover_candidates(
            &project_root,
            InputKind::Directory,
            None,
            DiscoveryPolicy::Generic,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(generic.len(), 3);

        let output = assets.join("graph.asset");
        std::fs::write(&output, b"stale output").unwrap();
        let discovery = discover_candidates(
            &project_root,
            InputKind::Directory,
            Some(&output),
            DiscoveryPolicy::UnityProject,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.len(), 2);
        assert_eq!(discovery[0].0, target);

        let output_directory = assets.join("generated");
        std::fs::create_dir_all(&output_directory).unwrap();
        std::fs::write(
            output_directory.join("artifact.asset"),
            b"%YAML 1.1\n--- !u!1 &1\nGameObject:\n  m_Name: Generated\n",
        )
        .unwrap();
        let discovery = discover_candidates(
            &project_root,
            InputKind::Directory,
            Some(&output_directory),
            DiscoveryPolicy::UnityProject,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.len(), 3);
        assert!(
            discovery
                .iter()
                .all(|(path, _)| !path.starts_with(&output_directory))
        );

        let recovery_directory = project_root.join(".unity-asset-recovery");
        std::fs::create_dir_all(&recovery_directory).unwrap();
        std::fs::write(
            recovery_directory.join("transaction.asset"),
            b"%YAML 1.1\n--- !u!1 &1\nGameObject:\n  m_Name: RecoveryEvidence\n",
        )
        .unwrap();
        let discovery = discover_candidates(
            &project_root,
            InputKind::Directory,
            None,
            DiscoveryPolicy::Generic,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(
            discovery
                .iter()
                .all(|(path, _)| !path.starts_with(&recovery_directory))
        );
    }
}
