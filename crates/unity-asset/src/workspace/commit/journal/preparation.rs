//! Durable transaction ownership installed before private scratch state exists.

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, ChangeSet, DigestV1, SourceId, TransactionId, WorkspaceId, WorkspaceRevision,
};

use super::super::platform::{
    DirectoryIdentity, FileIdentity, JournalAccess, open_journal_regular, opened_file_identity,
    remove_journal_regular, sync_journal_access,
};
#[cfg(test)]
use super::super::platform::{observe_directory_identity, open_readonly_regular_in_parent};
#[cfg(test)]
use super::write_encoded_atomic_with_temporary_path_tracked;
use super::{
    BoundedSequence, JOURNAL_VERSION, JournalArtifact, JournalBaseline, JournalError,
    JournalExpectedDestination, JournalLayout, JournalManifest, JournalTransactionOutputSeed,
    JournalTransactionSeed, MAX_ARTIFACT_COUNT, MAX_MANIFEST_BYTES, budgeted_journal_string,
    encode_json_bounded, journal_budgeted_vec, read_json_bounded_from_file,
    transaction_id_from_seed, validate_event_capacity,
    write_encoded_atomic_in_journal_access_tracked,
};
use crate::workspace::commit::{CommitAtomicity, CommitReport};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalPreparationOutput {
    ordinal: u32,
    logical_name: String,
    source: SourceId,
    target: super::JournalPath,
    expected: JournalExpectedDestination,
    expected_digest: Option<DigestV1>,
    expected_identity: Option<FileIdentity>,
    destination_parent_identity: DirectoryIdentity,
    digest: DigestV1,
    bytes: u64,
}

impl JournalPreparationOutput {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ordinal: u32,
        logical_name: &str,
        source: SourceId,
        target: super::JournalPath,
        expected: JournalExpectedDestination,
        expected_digest: Option<DigestV1>,
        expected_identity: Option<FileIdentity>,
        destination_parent_identity: DirectoryIdentity,
        digest: DigestV1,
        bytes: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        let output = Self {
            ordinal,
            logical_name: budgeted_journal_string(
                logical_name,
                "journal preparation logical name",
                budget,
            )?,
            source,
            target,
            expected,
            expected_digest,
            expected_identity,
            destination_parent_identity,
            digest,
            bytes,
        };
        output.validate(usize::try_from(ordinal).ok())?;
        Ok(output)
    }

    fn validate(&self, expected_ordinal: Option<usize>) -> Result<(), JournalError> {
        if expected_ordinal != usize::try_from(self.ordinal).ok() {
            return Err(JournalError::InvalidManifest(
                "preparation output ordinal is not canonical".to_owned(),
            ));
        }
        if self.logical_name.is_empty() || self.logical_name.len() > 1024 {
            return Err(JournalError::InvalidManifest(
                "preparation output logical name is empty or too long".to_owned(),
            ));
        }
        let has_expected = self.expected_digest.is_some() && self.expected_identity.is_some();
        let expectation_matches = match self.expected {
            JournalExpectedDestination::Existing => has_expected,
            JournalExpectedDestination::Absent => {
                self.expected_digest.is_none() && self.expected_identity.is_none()
            }
        };
        if !expectation_matches {
            return Err(JournalError::InvalidManifest(
                "preparation destination expectation is internally inconsistent".to_owned(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalPreparation {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    containment_root_identity: DirectoryIdentity,
    outputs: Vec<JournalPreparationOutput>,
    baseline: JournalBaseline,
    changes: ChangeSet,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalPreparationWire {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    containment_root_identity: DirectoryIdentity,
    outputs: BoundedSequence<JournalPreparationOutput, MAX_ARTIFACT_COUNT>,
    baseline: JournalBaseline,
    changes: ChangeSet,
}

impl<'de> Deserialize<'de> for JournalPreparation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JournalPreparationWire::deserialize(deserializer)?;
        Ok(Self {
            version: wire.version,
            transaction: wire.transaction,
            workspace_id: wire.workspace_id,
            base_revision: wire.base_revision,
            committed_revision: wire.committed_revision,
            plan_digest: wire.plan_digest,
            atomicity: wire.atomicity,
            containment_root_identity: wire.containment_root_identity,
            outputs: wire.outputs.0,
            baseline: wire.baseline,
            changes: wire.changes,
        })
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalPreparationRef<'a> {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    containment_root_identity: &'a DirectoryIdentity,
    outputs: &'a [JournalPreparationOutput],
    baseline: &'a JournalBaseline,
    changes: &'a ChangeSet,
}

impl JournalPreparation {
    pub(crate) fn install_in_access(
        layout: &JournalLayout,
        report: &CommitReport,
        outputs: &[JournalPreparationOutput],
        baseline: &JournalBaseline,
        temporary: &Path,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalPreparationInstallError> {
        let mut preparation_installed = false;
        let result = (|| {
            if report.recovery().root_identity() != layout.root_identity() {
                return Err(JournalError::InvalidManifest(
                    "commit report recovery locator does not match the preparation containment root"
                        .to_owned(),
                ));
            }
            let preparation = JournalPreparationRef {
                version: JOURNAL_VERSION,
                transaction: report.transaction(),
                workspace_id: report.workspace_id(),
                base_revision: report.base_revision(),
                committed_revision: report.committed_revision(),
                plan_digest: report.plan_digest(),
                atomicity: report.atomicity(),
                containment_root_identity: layout.root_identity(),
                outputs,
                baseline,
                changes: report.changes(),
            };
            validate_parts(preparation, layout.parent(), layout.root_identity(), budget)?;
            if preparation.transaction != layout.transaction() {
                return Err(JournalError::TransactionMismatch {
                    expected: layout.transaction(),
                    actual: preparation.transaction,
                });
            }
            let path = layout.preparation_path();
            let bytes = encode_json_bounded(path, &preparation, MAX_MANIFEST_BYTES, budget)?;
            write_encoded_atomic_in_journal_access_tracked(
                access,
                path,
                &bytes,
                false,
                temporary,
                &mut preparation_installed,
            )?;
            Self::open_in_access(layout, access, budget)
        })();
        result.map_err(|source| JournalPreparationInstallError {
            source,
            preparation_installed,
        })
    }

    #[cfg(test)]
    pub(crate) fn install(
        layout: &JournalLayout,
        report: &CommitReport,
        outputs: &[JournalPreparationOutput],
        baseline: &JournalBaseline,
        temporary: &Path,
        expected_parent: &DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalPreparationInstallError> {
        let mut preparation_installed = false;
        let result = (|| {
            layout.verify_root_path_binding()?;
            if report.recovery().root_identity() != layout.root_identity() {
                return Err(JournalError::InvalidManifest(
                    "commit report recovery locator does not match the preparation containment root"
                        .to_owned(),
                ));
            }
            let preparation = JournalPreparationRef {
                version: JOURNAL_VERSION,
                transaction: report.transaction(),
                workspace_id: report.workspace_id(),
                base_revision: report.base_revision(),
                committed_revision: report.committed_revision(),
                plan_digest: report.plan_digest(),
                atomicity: report.atomicity(),
                containment_root_identity: layout.root_identity(),
                outputs,
                baseline,
                changes: report.changes(),
            };
            validate_parts(preparation, layout.parent(), layout.root_identity(), budget)?;
            if preparation.transaction != layout.transaction() {
                return Err(JournalError::TransactionMismatch {
                    expected: layout.transaction(),
                    actual: preparation.transaction,
                });
            }
            let path = layout.preparation_path();
            let bytes = encode_json_bounded(path, &preparation, MAX_MANIFEST_BYTES, budget)?;
            write_encoded_atomic_with_temporary_path_tracked(
                path,
                &bytes,
                false,
                temporary,
                expected_parent,
                &mut preparation_installed,
            )?;
            Self::open_path_with_parent(layout, path, expected_parent, budget)
        })();
        result.map_err(|source| JournalPreparationInstallError {
            source,
            preparation_installed,
        })
    }

    #[cfg(test)]
    pub(crate) fn open(
        layout: &JournalLayout,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        Self::open_path(layout, layout.preparation_path(), budget)
    }

    pub(crate) fn open_in_access(
        layout: &JournalLayout,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        Self::open_in_access_path(layout, access, layout.preparation_path(), budget)
    }

    pub(crate) fn open_rollback_in_access(
        layout: &JournalLayout,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        Self::open_in_access_path(layout, access, layout.rollback_path(), budget)
    }

    #[cfg(test)]
    fn open_path(
        layout: &JournalLayout,
        path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        layout.verify_root_path_binding()?;
        let parent = path.parent().ok_or_else(|| JournalError::InvalidPath {
            path: path.display().to_string(),
            reason: "preparation state has no version directory",
        })?;
        let parent_identity = observe_directory_identity(parent)?;
        Self::open_path_with_parent(layout, path, &parent_identity, budget)
    }

    #[cfg(test)]
    fn open_path_with_parent(
        layout: &JournalLayout,
        path: &Path,
        parent_identity: &DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        let file = open_readonly_regular_in_parent(path, parent_identity)?;
        let identity = opened_file_identity(&file)?;
        let document = read_json_bounded_from_file::<Self>(path, file, MAX_MANIFEST_BYTES, budget)?;
        document.validate(layout.parent(), layout.root_identity(), budget)?;
        if document.transaction != layout.transaction() {
            return Err(JournalError::TransactionMismatch {
                expected: layout.transaction(),
                actual: document.transaction,
            });
        }
        Ok(OpenedJournalPreparation { document, identity })
    }

    fn open_in_access_path(
        layout: &JournalLayout,
        access: &JournalAccess<'_>,
        path: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedJournalPreparation, JournalError> {
        let file = open_journal_regular(access, path)?;
        let identity = opened_file_identity(&file)?;
        let document = read_json_bounded_from_file::<Self>(path, file, MAX_MANIFEST_BYTES, budget)?;
        document.validate(layout.parent(), layout.root_identity(), budget)?;
        if document.transaction != layout.transaction() {
            return Err(JournalError::TransactionMismatch {
                expected: layout.transaction(),
                actual: document.transaction,
            });
        }
        Ok(OpenedJournalPreparation { document, identity })
    }

    pub(crate) fn validate(
        &self,
        containment_root: &Path,
        containment_root_identity: &DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), JournalError> {
        validate_parts(
            JournalPreparationRef {
                version: self.version,
                transaction: self.transaction,
                workspace_id: self.workspace_id,
                base_revision: self.base_revision,
                committed_revision: self.committed_revision,
                plan_digest: self.plan_digest,
                atomicity: self.atomicity,
                containment_root_identity: &self.containment_root_identity,
                outputs: &self.outputs,
                baseline: &self.baseline,
                changes: &self.changes,
            },
            containment_root,
            containment_root_identity,
            budget,
        )
    }

    pub(crate) fn validate_manifest(&self, manifest: &JournalManifest) -> Result<(), JournalError> {
        if self.transaction != manifest.transaction
            || self.workspace_id != manifest.workspace_id
            || self.base_revision != manifest.base_revision
            || self.committed_revision != manifest.committed_revision
            || self.plan_digest != manifest.plan_digest
            || self.atomicity != manifest.atomicity
            || self.containment_root_identity != manifest.containment_root_identity
            || self.baseline != manifest.baseline
            || self.changes != manifest.result.changes
            || self.outputs.len() != manifest.artifacts.len()
        {
            return Err(JournalError::InvalidManifest(
                "final manifest disagrees with its durable preparation record".to_owned(),
            ));
        }
        for (output, artifact) in self.outputs.iter().zip(&manifest.artifacts) {
            if !output.matches_artifact(artifact) {
                return Err(JournalError::InvalidManifest(
                    "final manifest artifact disagrees with its durable preparation record"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn acknowledge_matching_rollback_in_access(
        layout: &JournalLayout,
        report: &CommitReport,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), JournalError> {
        let rollback = Self::open_rollback_in_access(layout, access, budget)?;
        let document = rollback.document();
        let report_matches = document.transaction == report.transaction()
            && document.workspace_id == report.workspace_id()
            && document.base_revision == report.base_revision()
            && document.committed_revision == report.committed_revision()
            && document.plan_digest == report.plan_digest()
            && document.atomicity == report.atomicity()
            && document.containment_root_identity == *report.recovery().root_identity()
            && document.changes == *report.changes()
            && document.outputs.len() == report.artifacts().len()
            && document
                .outputs
                .iter()
                .zip(report.artifacts())
                .all(|(output, artifact)| {
                    output.logical_name == artifact.logical_name()
                        && output.source == artifact.source()
                        && output.digest == artifact.digest()
                        && output.bytes == artifact.bytes()
                });
        if !report_matches {
            return Err(JournalError::InvalidManifest(
                "terminal rollback does not match the retried commit report".to_owned(),
            ));
        }
        remove_journal_regular(access, layout.rollback_path(), rollback.identity())?;
        sync_journal_access(access).map_err(JournalError::Io)
    }

    #[must_use]
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub(crate) const fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    #[must_use]
    pub(crate) fn outputs(&self) -> &[JournalPreparationOutput] {
        &self.outputs
    }

    #[must_use]
    pub(crate) const fn baseline(&self) -> &JournalBaseline {
        &self.baseline
    }
}

impl JournalPreparationOutput {
    fn matches_artifact(&self, artifact: &JournalArtifact) -> bool {
        let expected_matches = match self.expected {
            JournalExpectedDestination::Existing => {
                artifact.backup().is_some()
                    && artifact.old_digest() == self.expected_digest
                    && artifact.old_identity() == self.expected_identity.as_ref()
            }
            JournalExpectedDestination::Absent => {
                artifact.backup().is_none()
                    && artifact.old_digest().is_none()
                    && artifact.old_identity().is_none()
            }
        };
        expected_matches
            && artifact.logical_name() == self.logical_name
            && artifact.source() == self.source
            && artifact.target() == &self.target
            && artifact.destination_parent_identity() == &self.destination_parent_identity
            && artifact.new_digest() == self.digest
            && artifact.bytes() == self.bytes
    }
}

fn validate_parts(
    preparation: JournalPreparationRef<'_>,
    containment_root: &Path,
    containment_root_identity: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), JournalError> {
    let JournalPreparationRef {
        version,
        transaction,
        workspace_id,
        base_revision,
        committed_revision,
        plan_digest,
        atomicity,
        containment_root_identity: preparation_root_identity,
        outputs,
        baseline,
        changes,
    } = preparation;
    if version != JOURNAL_VERSION {
        return Err(JournalError::UnsupportedVersion(version));
    }
    if preparation_root_identity != containment_root_identity {
        return Err(JournalError::InvalidManifest(
            "preparation containment root identity does not match its trusted locator".to_owned(),
        ));
    }
    if outputs.is_empty() || outputs.len() > MAX_ARTIFACT_COUNT {
        return Err(JournalError::InvalidManifest(
            "preparation output count is outside the allowed range".to_owned(),
        ));
    }
    for (ordinal, output) in outputs.iter().enumerate() {
        output.validate(Some(ordinal))?;
    }
    if outputs
        .windows(2)
        .any(|pair| pair[0].logical_name >= pair[1].logical_name)
    {
        return Err(JournalError::InvalidManifest(
            "preparation outputs are not in strict logical-name order".to_owned(),
        ));
    }
    let existing = outputs
        .iter()
        .filter(|output| output.expected == JournalExpectedDestination::Existing)
        .count();
    validate_event_capacity(existing, outputs.len() - existing)?;
    baseline.validate(workspace_id)?;
    if changes.transaction() != transaction
        || changes.workspace() != workspace_id
        || changes.from_revision() != base_revision
        || changes.to_revision() != committed_revision
    {
        return Err(JournalError::InvalidManifest(
            "preparation change set disagrees with its transaction header".to_owned(),
        ));
    }
    let containment_root = containment_root.to_str().ok_or_else(|| {
        JournalError::InvalidManifest("transaction containment root is not valid UTF-8".to_owned())
    })?;
    let mut seeds = journal_budgeted_vec(
        outputs.len(),
        "journal preparation transaction outputs",
        budget,
    )?;
    for output in outputs {
        seeds.push(JournalTransactionOutputSeed {
            ordinal: output.ordinal,
            logical_name: &output.logical_name,
            source: output.source,
            relative_target: output.target.as_str(),
            expected: output.expected,
            expected_digest: output.expected_digest,
            expected_identity: output.expected_identity.as_ref(),
            destination_parent_identity: &output.destination_parent_identity,
            digest: output.digest,
            bytes: output.bytes,
        });
    }
    let actual = transaction_id_from_seed(
        &JournalTransactionSeed {
            version: 1,
            workspace: workspace_id,
            base_revision,
            committed_revision,
            plan_digest,
            atomicity,
            containment_root,
            containment_root_identity: preparation_root_identity,
            outputs: &seeds,
            changed_sources: changes.changed_sources(),
            changed_objects: changes.changed_objects(),
            identity_remaps: changes.identity_remaps(),
            baseline,
        },
        budget,
    )?;
    if actual != transaction {
        return Err(JournalError::TransactionMismatch {
            expected: transaction,
            actual,
        });
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct OpenedJournalPreparation {
    document: JournalPreparation,
    identity: FileIdentity,
}

impl OpenedJournalPreparation {
    #[must_use]
    pub(crate) const fn document(&self) -> &JournalPreparation {
        &self.document
    }

    #[must_use]
    pub(crate) const fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    pub(crate) fn revalidate_in_access(
        &self,
        layout: &JournalLayout,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), JournalError> {
        let current = JournalPreparation::open_in_access(layout, access, budget)?;
        if current.identity != self.identity || current.document != self.document {
            return Err(JournalError::InvalidManifest(
                "durable preparation record changed during recovery".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("{source}")]
pub(crate) struct JournalPreparationInstallError {
    #[source]
    source: JournalError,
    preparation_installed: bool,
}

impl JournalPreparationInstallError {
    #[must_use]
    pub(crate) const fn preparation_installed(&self) -> bool {
        self.preparation_installed
    }

    pub(crate) fn into_source(self) -> JournalError {
        self.source
    }
}
