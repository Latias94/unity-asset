use std::mem::size_of;
use std::path::Path;

use serde::Serialize;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ObjectKind, RevisionedObjectHandle, WorkspaceId,
    WorkspaceRevision,
};
use unity_asset_yaml::UnityYamlSerializer;

use super::artifact::{OutputArtifactError, OutputLayout};
use super::executor::ExistingOutputPolicy;
use super::model::{ExtractionModelError, ExtractionPath};
use crate::workspace::{WorkspaceError, WorkspaceObjectValue, WorkspaceSource, WorkspaceView};

pub const YAML_SPLIT_REPORT_VERSION: u8 = 1;
pub const YAML_SPLIT_REPORT_CONTRACT: &str = "unity_asset.yaml_split_report";

/// One immutable YAML document output chosen before publication begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlSplitArtifact {
    handle: RevisionedObjectHandle,
    path: ExtractionPath,
}

impl YamlSplitArtifact {
    #[must_use]
    pub const fn handle(&self) -> &RevisionedObjectHandle {
        &self.handle
    }

    #[must_use]
    pub const fn path(&self) -> &ExtractionPath {
        &self.path
    }
}

/// A zero-write, revision-bound plan for splitting YAML documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlSplitPlan {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    artifacts: Box<[YamlSplitArtifact]>,
}

impl YamlSplitPlan {
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn artifacts(&self) -> &[YamlSplitArtifact] {
        &self.artifacts
    }
}

/// Deterministic YAML split counts. This is deliberately not an extraction manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct YamlSplitReport {
    contract: &'static str,
    version: u8,
    written: u64,
    skipped_existing: u64,
}

impl YamlSplitReport {
    #[must_use]
    pub const fn written(self) -> u64 {
        self.written
    }

    #[must_use]
    pub const fn skipped_existing(self) -> u64 {
        self.skipped_existing
    }
}

/// Plans one output document per YAML object without touching the filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct YamlSplitPlanner;

impl YamlSplitPlanner {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn plan(
        &self,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<YamlSplitPlan, YamlSplitError> {
        let mut handles = view.objects(budget)?;
        handles.retain(|handle| handle.object().kind() == ObjectKind::Yaml);
        handles.sort_unstable_by(|left, right| left.object().cmp(right.object()));

        let artifact_count =
            u64::try_from(handles.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "yaml_split_artifact_count",
            })?;
        budget.consume_entries(artifact_count)?;
        let artifact_bytes = handles
            .len()
            .checked_mul(size_of::<YamlSplitArtifact>())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "yaml_split_artifact_table",
            })?;
        budget.consume_bytes(u64::try_from(artifact_bytes).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "yaml_split_artifact_table",
            }
        })?)?;

        let mut sources = view.sources(budget)?;
        sources.sort_unstable_by_key(WorkspaceSource::id);
        let mut artifacts = Vec::new();
        artifacts
            .try_reserve_exact(handles.len())
            .map_err(|error| YamlSplitError::Allocation(error.to_string()))?;
        for handle in handles {
            let source = sources
                .binary_search_by_key(&handle.object().source(), WorkspaceSource::id)
                .ok()
                .and_then(|index| sources.get(index))
                .ok_or(YamlSplitError::MissingSource(handle.object().source()))?;
            let path = yaml_output_path(&handle, source, budget)?;
            artifacts.push(YamlSplitArtifact { handle, path });
        }
        artifacts.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        if let Some(pair) = artifacts
            .windows(2)
            .find(|pair| pair[0].path == pair[1].path)
        {
            return Err(YamlSplitError::DuplicatePath(
                pair[0].path.as_str().to_owned(),
            ));
        }

        Ok(YamlSplitPlan {
            workspace_id: view.workspace_id(),
            revision: view.revision(),
            artifacts: artifacts.into_boxed_slice(),
        })
    }
}

/// Publishes a YAML split plan through the safe artifact-set output primitive.
#[derive(Debug, Default, Clone, Copy)]
pub struct YamlSplitExecutor;

impl YamlSplitExecutor {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn execute(
        &self,
        view: &dyn WorkspaceView,
        plan: &YamlSplitPlan,
        output_root: &Path,
        existing_output: ExistingOutputPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<YamlSplitReport, YamlSplitError> {
        if view.workspace_id() != plan.workspace_id || view.revision() != plan.revision {
            return Err(YamlSplitError::ContextMismatch {
                expected_workspace: plan.workspace_id,
                expected_revision: plan.revision,
                actual_workspace: view.workspace_id(),
                actual_revision: view.revision(),
            });
        }
        for artifact in &plan.artifacts {
            artifact
                .handle
                .validate_context(view.workspace_id(), view.revision())?;
        }

        let layout = OutputLayout::prepare(
            output_root,
            plan.artifacts.iter().map(|artifact| artifact.path.as_str()),
        )
        .map_err(YamlSplitError::output)?;
        let mut written = 0_u64;
        let mut skipped_existing = 0_u64;
        for artifact in &plan.artifacts {
            let target = layout
                .path(artifact.path.as_str())
                .map_err(YamlSplitError::output)?;
            let exists = target
                .open_existing()
                .map_err(YamlSplitError::output)?
                .is_some();
            match (exists, existing_output) {
                (true, ExistingOutputPolicy::Error) => {
                    return Err(YamlSplitError::OutputExists(
                        artifact.path.as_str().to_owned(),
                    ));
                }
                (true, ExistingOutputPolicy::Skip) => {
                    skipped_existing =
                        skipped_existing
                            .checked_add(1)
                            .ok_or(BudgetError::ArithmeticOverflow {
                                resource: "yaml_split_skipped_count",
                            })?;
                    continue;
                }
                (true, ExistingOutputPolicy::Replace) | (false, _) => {}
            }

            let object = view.read_object(&artifact.handle, budget)?;
            let WorkspaceObjectValue::Yaml(yaml) = object.value() else {
                return Err(YamlSplitError::ObjectChangedKind(
                    artifact.path.as_str().to_owned(),
                ));
            };
            let mut staging = target.create_staging().map_err(YamlSplitError::output)?;
            UnityYamlSerializer::new()
                .serialize_to_writer_with_budget(
                    staging.writer(),
                    std::iter::once(yaml.class()),
                    budget,
                )
                .map_err(|error| YamlSplitError::Serialization(error.to_string()))?;
            staging
                .finish()
                .map_err(YamlSplitError::output)?
                .publish(existing_output == ExistingOutputPolicy::Replace)
                .map_err(YamlSplitError::output)?;
            written = written
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "yaml_split_written_count",
                })?;
        }

        Ok(YamlSplitReport {
            contract: YAML_SPLIT_REPORT_CONTRACT,
            version: YAML_SPLIT_REPORT_VERSION,
            written,
            skipped_existing,
        })
    }
}

fn yaml_output_path(
    handle: &RevisionedObjectHandle,
    source: &WorkspaceSource,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionPath, YamlSplitError> {
    let alias = source.locator().root_alias().as_str();
    let selector_bytes = handle
        .object()
        .yaml_anchor()
        .map_or(32, |anchor| anchor.len().saturating_add("anchor-".len()));
    let maximum_bytes = "documents//.yaml"
        .len()
        .checked_add(alias.len())
        .and_then(|length| length.checked_add(selector_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "yaml_split_relative_path",
        })?;
    budget.check_bytes(u64::try_from(maximum_bytes).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "yaml_split_relative_path",
        }
    })?)?;
    let selector = match (
        handle.object().yaml_anchor(),
        handle.object().yaml_document_ordinal(),
    ) {
        (Some(anchor), None) => format!("anchor-{anchor}"),
        (None, Some(index)) => format!("ordinal-{index:010}"),
        _ => return Err(YamlSplitError::InvalidYamlIdentity),
    };
    let path = format!("documents/{alias}/{selector}.yaml");
    budget.consume_bytes(u64::try_from(path.len()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "yaml_split_relative_path",
        }
    })?)?;
    ExtractionPath::new(path)
        .map_err(ExtractionModelError::from)
        .map_err(YamlSplitError::from)
}

#[derive(Debug, Error)]
pub enum YamlSplitError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Contract(#[from] unity_asset_core::ContractError),
    #[error(transparent)]
    Model(#[from] ExtractionModelError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("failed to allocate the YAML split plan: {0}")]
    Allocation(String),
    #[error("YAML split object refers to a missing source {0:?}")]
    MissingSource(unity_asset_core::SourceId),
    #[error("YAML split plan contains duplicate output path {0:?}")]
    DuplicatePath(String),
    #[error("YAML split object has no valid anchor or document ordinal")]
    InvalidYamlIdentity,
    #[error(
        "YAML split context mismatch: expected {expected_workspace}/{expected_revision}, actual {actual_workspace}/{actual_revision}"
    )]
    ContextMismatch {
        expected_workspace: WorkspaceId,
        expected_revision: WorkspaceRevision,
        actual_workspace: WorkspaceId,
        actual_revision: WorkspaceRevision,
    },
    #[error("YAML split output already exists: {0:?}")]
    OutputExists(String),
    #[error("YAML split object changed kind before writing {0:?}")]
    ObjectChangedKind(String),
    #[error("failed to encode a YAML split artifact: {0}")]
    Serialization(String),
    #[error("safe YAML split publication failed: {0}")]
    Output(String),
}

impl YamlSplitError {
    fn output(error: OutputArtifactError) -> Self {
        Self::Output(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::workspace::{AssetWorkspace, SourceOpenRequest};
    use unity_asset_core::SourceAlias;

    #[test]
    fn split_plan_is_zero_write_and_publishes_without_an_extraction_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("scene.prefab");
        let output = temporary.path().join("split");
        fs::write(
            &source,
            b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: first\n--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n",
        )
        .unwrap();

        let mut budget = AssetLoadBudget::default();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_source(
                SourceOpenRequest::new(&source, SourceAlias::new("scene.prefab").unwrap()),
                &mut budget,
            )
            .unwrap();
        let snapshot = workspace.snapshot();
        let plan = YamlSplitPlanner::new()
            .plan(&snapshot, &mut budget)
            .unwrap();

        assert_eq!(plan.artifacts().len(), 2);
        assert!(!output.exists());

        let report = YamlSplitExecutor::new()
            .execute(
                &snapshot,
                &plan,
                &output,
                ExistingOutputPolicy::Error,
                &mut budget,
            )
            .unwrap();

        assert_eq!(report.written(), 2);
        assert_eq!(report.skipped_existing(), 0);
        assert!(
            output
                .join("documents/scene.prefab/anchor-1.yaml")
                .is_file()
        );
        assert!(
            output
                .join("documents/scene.prefab/anchor-2.yaml")
                .is_file()
        );
        assert!(!output.join("extraction-manifest.json").exists());
    }
}
