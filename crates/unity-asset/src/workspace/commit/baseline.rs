//! Construction of the next immutable workspace baseline.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, BudgetedVerifiedSourceImage,
    SourceFingerprint, SourceId, SourceKind, UnityDocument, vec_allocation_bytes,
};
use unity_asset_write::artifact::{
    ArtifactHandle, ArtifactStreamError, PreparedArtifactFormat, PreparedArtifactSet,
};
use unity_asset_yaml::parse_budgeted_yaml_source;

use super::super::WorkspaceInstallationDigest;
use super::super::adapter::binary::{BinaryPayload, BinaryWorkspaceAdapter};
use super::super::inspection::{
    AssetBundleSummary, SerializedFileSummary, WebFileSummary, WorkspaceSourceFormatInspection,
};
use super::super::interface::{
    map_binary_adapter_error, map_yaml_error, promote_value_to_arc, validate_yaml_identities,
};
use super::super::overlay::PreparedStateCore;
use super::super::preflight::PreparedChange;
use super::super::source_catalog::{CatalogError, PhysicalDomainChange, SourceDescriptor};
use super::super::state::{
    FrozenSourceParse, PreparedWorkspaceState, SourceStoreError, VerifiedSourceContent,
    WorkspaceState, WorkspaceStateError, WorkspaceStateTransaction,
};
use super::super::view::WorkspaceError;
use super::journal::{
    Journal, JournalBaselineImage, JournalBaselineSource, JournalCatalogAction, JournalError,
};
use super::platform::{
    DirectoryIdentity, FileIdentity, observe_directory_identity, open_readonly_regular_in_parent,
    opened_file_identity,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum RecoveryArtifactLocation {
    Target,
    Staging,
}

pub(crate) fn read_artifact_image(
    journal: &Journal,
    index: usize,
    location: RecoveryArtifactLocation,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedVerifiedSourceImage, BaselineBuildError> {
    let artifact = journal.manifest().artifacts().get(index).ok_or_else(|| {
        BaselineBuildError::RecoveryBinding {
            message: "recovery artifact index is out of range".to_owned(),
        }
    })?;
    let (path, parent_identity) = match location {
        RecoveryArtifactLocation::Target => (
            artifact
                .target()
                .join_root_budgeted(
                    journal.layout().parent(),
                    "recovery target image path",
                    budget,
                )
                .map_err(map_journal_path_error)?,
            artifact.destination_parent_identity(),
        ),
        RecoveryArtifactLocation::Staging => (
            artifact
                .staging()
                .join_root_budgeted(
                    journal.layout().directory(),
                    "recovery staging image path",
                    budget,
                )
                .map_err(map_journal_path_error)?,
            journal.manifest().directories().stage(),
        ),
    };
    read_recovery_image(
        &path,
        parent_identity,
        artifact.source(),
        SourceFingerprint::new(artifact.source().kind(), artifact.new_digest()),
        artifact.bytes(),
        Some(artifact.new_identity()),
        budget,
    )
}

/// Fully validated next baseline and the exact state it may replace.
pub(crate) struct PreparedBaseline {
    state: PreparedWorkspaceState,
}

impl PreparedBaseline {
    pub(crate) const fn state(&self) -> &PreparedWorkspaceState {
        &self.state
    }
}

/// Artifact images retained while the publication transaction builds its baseline.
pub(crate) struct MaterializedImages {
    images: Vec<Option<BudgetedSourceBytes>>,
}

impl MaterializedImages {
    pub(crate) fn new(
        set: &PreparedArtifactSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BaselineBuildError> {
        let count = set.proof_image_count();
        let planned = vec_allocation_bytes::<Option<BudgetedSourceBytes>>(count).map_err(|_| {
            BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                resource: "baseline materialized image table",
            })
        })?;
        budget.check_bytes(planned)?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(count)
            .map_err(|error| BaselineBuildError::Allocation {
                resource: "baseline materialized image table",
                requested: count,
                message: error.to_string(),
            })?;
        let actual = vec_allocation_bytes::<Option<BudgetedSourceBytes>>(images.capacity())
            .map_err(|_| {
                BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "baseline materialized image table",
                })
            })?;
        budget.consume_bytes(actual)?;
        images.resize_with(count, || None);
        Ok(Self { images })
    }

    pub(crate) fn insert(&mut self, handle: ArtifactHandle, image: BudgetedSourceBytes) {
        if let Some(slot) = self.images.get_mut(handle.ordinal()) {
            *slot = Some(image);
        }
    }

    pub(crate) fn get(&self, handle: ArtifactHandle) -> Option<&BudgetedSourceBytes> {
        self.images.get(handle.ordinal()).and_then(Option::as_ref)
    }

    pub(crate) fn materialize(
        &mut self,
        set: &PreparedArtifactSet,
        handle: ArtifactHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<BudgetedSourceBytes, BaselineBuildError> {
        if let Some(image) = self.get(handle) {
            return Ok(image.clone());
        }
        self.stream_and_materialize(set, handle, &mut io::sink(), budget)
    }

    pub(crate) fn stream_and_materialize(
        &mut self,
        set: &PreparedArtifactSet,
        handle: ArtifactHandle,
        sink: &mut impl Write,
        budget: &mut AssetLoadBudget,
    ) -> Result<BudgetedSourceBytes, BaselineBuildError> {
        if self.get(handle).is_some() {
            return Err(BaselineBuildError::Artifact(
                "publication artifact was already materialized before staging".to_owned(),
            ));
        }
        let artifact = set
            .artifact(handle)
            .map_err(|error| BaselineBuildError::Artifact(error.to_string()))?;
        let length = usize::try_from(artifact.len()).map_err(|_| {
            BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                resource: "baseline artifact length",
            })
        })?;
        let length_u64 = u64::try_from(length).map_err(|_| {
            BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                resource: "baseline artifact length",
            })
        })?;
        budget.check_bytes(length_u64)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|error| BaselineBuildError::Allocation {
                resource: "baseline artifact image",
                requested: length,
                message: error.to_string(),
            })?;
        let mut tee = TeeWriter {
            first: sink,
            second: &mut bytes,
        };
        let receipt = artifact
            .stream_verified_to(&mut tee)
            .map_err(BaselineBuildError::from)?;
        if receipt.bytes_written() != artifact.len() || receipt.digest() != artifact.digest() {
            return Err(BaselineBuildError::ArtifactStream {
                message: "prepared artifact receipt disagrees with its proof".to_owned(),
            });
        }
        let actual_capacity = u64::try_from(bytes.capacity()).map_err(|_| {
            BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                resource: "baseline artifact image capacity",
            })
        })?;
        budget.consume_bytes(actual_capacity)?;
        let image = BudgetedSourceBytes::from_vec(bytes, budget)?;
        self.insert(handle, image.clone());
        Ok(image)
    }
}

struct TeeWriter<'writer, A, B> {
    first: &'writer mut A,
    second: &'writer mut B,
}

impl<A: Write, B: Write> Write for TeeWriter<'_, A, B> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.first.write_all(bytes)?;
        self.second.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.first.flush()?;
        self.second.flush()
    }
}

pub(crate) fn build(
    prepared: &PreparedChange,
    binary: &BinaryWorkspaceAdapter,
    expected_installation: WorkspaceInstallationDigest,
    images: &mut MaterializedImages,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedBaseline, BaselineBuildError> {
    let state = prepared.state();
    let core: &PreparedStateCore = state.core().as_ref();
    let expected = Arc::clone(core.base().state());
    let base = expected.as_ref();
    let mut transaction = WorkspaceStateTransaction::begin_with_catalog(
        Arc::clone(&expected),
        core.catalog(),
        budget,
    )
    .map_err(BaselineBuildError::state)?;
    let artifacts = core.artifacts();

    for binding in core.source_bindings() {
        let image = images.materialize(artifacts, binding.artifact(), budget)?;
        let verified = image.verify(binding.source().kind());
        if verified.fingerprint() != binding.fingerprint() {
            return Err(BaselineBuildError::Fingerprint {
                source_id: binding.source(),
                expected: binding.fingerprint(),
                actual: verified.fingerprint(),
            });
        }
        if transaction.content_fingerprint(binding.source()) == Some(verified.fingerprint()) {
            continue;
        }
        let artifact = artifacts
            .artifact(binding.artifact())
            .map_err(|error| BaselineBuildError::Artifact(error.to_string()))?;
        let parse = parse_source(
            binding.source(),
            verified.clone_backing(budget)?,
            base,
            binary,
            budget,
        )?;
        let format = format_from_artifact(binding.source(), artifact.format(), base, budget)?;
        let content =
            VerifiedSourceContent::from_budgeted(binding.source(), verified, parse, format);
        transaction
            .replace_verified_content(content, budget)
            .map_err(BaselineBuildError::state)?;
    }

    let prepared_state = transaction
        .commit(budget)
        .map_err(BaselineBuildError::state)?;
    if prepared_state.revision() != core.revision() {
        return Err(BaselineBuildError::Revision {
            expected: core.revision(),
            actual: prepared_state.revision(),
        });
    }
    if prepared_state.installation() != expected_installation {
        return Err(BaselineBuildError::Installation {
            expected: expected_installation,
            actual: prepared_state.installation(),
        });
    }
    Ok(PreparedBaseline {
        state: prepared_state,
    })
}

pub(crate) fn build_from_journal_with_images(
    expected: Arc<WorkspaceState>,
    journal: &Journal,
    binary: &BinaryWorkspaceAdapter,
    published_images: Option<&[Option<BudgetedVerifiedSourceImage>]>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedBaseline, BaselineBuildError> {
    let base = expected.as_ref();
    let manifest = journal.manifest();
    if base.workspace() != manifest.workspace_id() {
        return Err(BaselineBuildError::RecoveryBinding {
            message: "recovery baseline does not match the loaded workspace identity".to_owned(),
        });
    }
    if base.installation() != manifest.base_installation()
        && base.installation() != manifest.committed_installation()
    {
        return Err(BaselineBuildError::RecoveryBinding {
            message: "recovery baseline does not match the journal base or committed installation"
                .to_owned(),
        });
    }

    let sources = manifest.baseline().sources();
    let mut changes = reserve_recovery_vec::<PhysicalDomainChange>(sources.len(), budget)?;
    let mut transaction = WorkspaceStateTransaction::begin(Arc::clone(&expected), budget)
        .map_err(BaselineBuildError::state)?;
    for source in sources {
        match source.catalog() {
            JournalCatalogAction::Existing { base_fingerprint } => {
                let actual = base
                    .catalog()
                    .fingerprint(source.source())
                    .map_err(BaselineBuildError::catalog)?;
                if actual != *base_fingerprint && actual != source.fingerprint() {
                    return Err(BaselineBuildError::RecoveryBinding {
                        message: format!(
                            "source {:?} matches neither its base nor committed fingerprint",
                            source.source()
                        ),
                    });
                }
            }
            JournalCatalogAction::AddCompanion { parent, member } => {
                let descriptor = SourceDescriptor::companion(*parent, member.clone())
                    .map_err(BaselineBuildError::catalog)?;
                let actual = transaction
                    .register_descriptor(descriptor, source.fingerprint(), budget)
                    .map_err(BaselineBuildError::state)?;
                if actual != source.source() {
                    return Err(BaselineBuildError::RecoveryBinding {
                        message: format!(
                            "companion declaration resolved to {:?}, expected {:?}",
                            actual,
                            source.source()
                        ),
                    });
                }
            }
            JournalCatalogAction::AddContainedSidecar { parent, member } => {
                let descriptor = SourceDescriptor::sidecar(*parent, member.clone())
                    .map_err(BaselineBuildError::catalog)?;
                let actual = transaction
                    .register_descriptor(descriptor, source.fingerprint(), budget)
                    .map_err(BaselineBuildError::state)?;
                if actual != source.source() {
                    return Err(BaselineBuildError::RecoveryBinding {
                        message: format!(
                            "sidecar declaration resolved to {:?}, expected {:?}",
                            actual,
                            source.source()
                        ),
                    });
                }
            }
        }
        changes.push(PhysicalDomainChange::new(
            source.source(),
            source.fingerprint(),
        ));
    }
    changes.sort_unstable_by_key(PhysicalDomainChange::source);
    if changes
        .windows(2)
        .any(|pair| pair[0].source() == pair[1].source())
    {
        return Err(BaselineBuildError::RecoveryBinding {
            message: "journal contains duplicate baseline source identities".to_owned(),
        });
    }

    transaction
        .rewrite_physical_domains_from_changes(&changes, budget)
        .map_err(BaselineBuildError::state)?;

    for source in sources {
        validate_in_place_binding(base, journal, source, budget)?;
        let verified = recovery_image(journal, source, published_images, budget)?;
        if verified.fingerprint() != source.fingerprint() {
            return Err(BaselineBuildError::Fingerprint {
                source_id: source.source(),
                expected: source.fingerprint(),
                actual: verified.fingerprint(),
            });
        }
        if transaction.content_fingerprint(source.source()) == Some(verified.fingerprint()) {
            continue;
        }
        let parse = parse_source(
            source.source(),
            verified.clone_backing(budget)?,
            base,
            binary,
            budget,
        )?;
        let format = inspect_recovered_source(
            source.source(),
            verified.backing(budget)?,
            &parse,
            base,
            binary,
            budget,
        )?;
        let content =
            VerifiedSourceContent::from_budgeted(source.source(), verified, parse, format);
        transaction
            .replace_verified_content(content, budget)
            .map_err(BaselineBuildError::state)?;
    }

    let prepared_state = transaction
        .commit(budget)
        .map_err(BaselineBuildError::state)?;
    if prepared_state.revision() != manifest.committed_revision() {
        return Err(BaselineBuildError::Revision {
            expected: manifest.committed_revision(),
            actual: prepared_state.revision(),
        });
    }
    if prepared_state.installation() != manifest.committed_installation() {
        return Err(BaselineBuildError::Installation {
            expected: manifest.committed_installation(),
            actual: prepared_state.installation(),
        });
    }
    Ok(PreparedBaseline {
        state: prepared_state,
    })
}

fn validate_in_place_binding(
    base: &WorkspaceState,
    journal: &Journal,
    source: &JournalBaselineSource,
    budget: &mut AssetLoadBudget,
) -> Result<(), BaselineBuildError> {
    if !matches!(source.catalog(), JournalCatalogAction::Existing { .. }) {
        return Ok(());
    }
    let JournalBaselineImage::Published { artifact } = source.image() else {
        return Ok(());
    };
    let index = usize::try_from(*artifact).map_err(|_| BaselineBuildError::RecoveryBinding {
        message: "published baseline artifact index overflowed".to_owned(),
    })?;
    let artifact = journal.manifest().artifacts().get(index).ok_or_else(|| {
        BaselineBuildError::RecoveryBinding {
            message: "published baseline artifact index is out of range".to_owned(),
        }
    })?;
    let target = artifact
        .target()
        .join_root_budgeted(
            journal.layout().parent(),
            "recovery published baseline target path",
            budget,
        )
        .map_err(map_journal_path_error)?;
    verify_recovery_parent(
        &target,
        artifact.destination_parent_identity(),
        source.source(),
    )?;
    let origin = base
        .catalog()
        .physical_origin(source.source())
        .map_err(BaselineBuildError::catalog)?;
    // Publication roots are canonical and journal paths are validated relative
    // descendants. The parent identity check above rejects symlink traversal,
    // so this lexical comparison is the canonical physical binding without a
    // second, allocator-owned `canonicalize` call.
    if origin.path() != target {
        return Err(BaselineBuildError::RecoveryBinding {
            message: format!(
                "published source {:?} is relocated; the journal has no catalog checkpoint for that path",
                source.source()
            ),
        });
    }
    Ok(())
}

fn recovery_image(
    journal: &Journal,
    source: &JournalBaselineSource,
    published_images: Option<&[Option<BudgetedVerifiedSourceImage>]>,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedVerifiedSourceImage, BaselineBuildError> {
    let (path, expected_bytes, parent_identity, expected_identity) = match source.image() {
        JournalBaselineImage::Published { artifact } => {
            let index =
                usize::try_from(*artifact).map_err(|_| BaselineBuildError::RecoveryBinding {
                    message: "published baseline artifact index overflowed".to_owned(),
                })?;
            let artifact = journal.manifest().artifacts().get(index).ok_or_else(|| {
                BaselineBuildError::RecoveryBinding {
                    message: "published baseline artifact index is out of range".to_owned(),
                }
            })?;
            if let Some(image) = published_images
                .and_then(|images| images.get(index))
                .and_then(Option::as_ref)
            {
                return Ok(image.clone());
            }
            (
                artifact
                    .target()
                    .join_root_budgeted(
                        journal.layout().parent(),
                        "recovery published image path",
                        budget,
                    )
                    .map_err(map_journal_path_error)?,
                artifact.bytes(),
                artifact.destination_parent_identity(),
                Some(artifact.new_identity()),
            )
        }
        JournalBaselineImage::Blob { path, bytes, .. } => (
            path.join_root_budgeted(
                journal.layout().directory(),
                "recovery baseline blob path",
                budget,
            )
            .map_err(map_journal_path_error)?,
            *bytes,
            journal.manifest().directories().baseline(),
            None,
        ),
    };
    read_recovery_image(
        &path,
        parent_identity,
        source.source(),
        source.fingerprint(),
        expected_bytes,
        expected_identity,
        budget,
    )
}

fn map_journal_path_error(error: JournalError) -> BaselineBuildError {
    match error {
        JournalError::Budget(error) => BaselineBuildError::Budget(error),
        JournalError::Allocation {
            resource,
            requested,
            message,
        } => BaselineBuildError::Allocation {
            resource,
            requested,
            message,
        },
        error => BaselineBuildError::RecoveryBinding {
            message: error.to_string(),
        },
    }
}

fn read_recovery_image(
    path: &Path,
    expected_parent: &DirectoryIdentity,
    source: SourceId,
    expected: SourceFingerprint,
    expected_bytes: u64,
    expected_identity: Option<&FileIdentity>,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedVerifiedSourceImage, BaselineBuildError> {
    verify_recovery_parent(path, expected_parent, source)?;
    let mut file = open_readonly_regular_in_parent(path, expected_parent).map_err(|error| {
        BaselineBuildError::RecoveryImage {
            source_id: source,
            message: error.to_string(),
        }
    })?;
    let identity_before =
        opened_file_identity(&file).map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("recovery image identity cannot be captured: {error}"),
        })?;
    if expected_identity.is_some_and(|expected| expected != &identity_before) {
        return Err(BaselineBuildError::RecoveryBinding {
            message: format!("recovery image identity changed for source {source:?}"),
        });
    }
    let metadata = file
        .metadata()
        .map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: error.to_string(),
        })?;
    let length = metadata.len();
    if length != expected_bytes {
        return Err(BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("recovery image length changed from {expected_bytes} to {length}"),
        });
    }
    budget.consume_entries(1)?;
    budget.check_bytes(length)?;
    let length_usize = usize::try_from(length).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "recovery baseline image length",
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length_usize)
        .map_err(|error| BaselineBuildError::Allocation {
            resource: "recovery baseline image",
            requested: length_usize,
            message: error.to_string(),
        })?;
    let actual = u64::try_from(bytes.capacity()).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "recovery baseline image capacity",
    })?;
    budget.consume_bytes(actual)?;
    bytes.resize(length_usize, 0);
    file.read_exact(&mut bytes)
        .map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: error.to_string(),
        })?;
    if file
        .read(&mut [0_u8; 1])
        .map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: error.to_string(),
        })?
        != 0
    {
        return Err(BaselineBuildError::RecoveryImage {
            source_id: source,
            message: "recovery image grew while it was read".to_owned(),
        });
    }
    let identity_after =
        opened_file_identity(&file).map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("recovery image identity cannot be revalidated: {error}"),
        })?;
    let current = open_readonly_regular_in_parent(path, expected_parent).map_err(|error| {
        BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("recovery image path cannot be revalidated: {error}"),
        }
    })?;
    let current_identity =
        opened_file_identity(&current).map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("current recovery image identity cannot be captured: {error}"),
        })?;
    if identity_before != identity_after || identity_after != current_identity {
        return Err(BaselineBuildError::RecoveryBinding {
            message: format!("recovery image binding changed while source {source:?} was read"),
        });
    }
    let verified = BudgetedSourceBytes::from_vec(bytes, budget)?.verify(expected.kind());
    if verified.fingerprint() != expected {
        return Err(BaselineBuildError::Fingerprint {
            source_id: source,
            expected,
            actual: verified.fingerprint(),
        });
    }
    Ok(verified)
}

fn verify_recovery_parent(
    path: &Path,
    expected: &DirectoryIdentity,
    source: SourceId,
) -> Result<(), BaselineBuildError> {
    let parent = path
        .parent()
        .ok_or_else(|| BaselineBuildError::RecoveryBinding {
            message: "recovery image has no parent directory".to_owned(),
        })?;
    let actual =
        observe_directory_identity(parent).map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source,
            message: format!("recovery image parent identity cannot be verified: {error}"),
        })?;
    if &actual != expected {
        return Err(BaselineBuildError::RecoveryBinding {
            message: "recovery image parent directory identity changed".to_owned(),
        });
    }
    Ok(())
}

fn reserve_recovery_vec<T>(
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, BaselineBuildError> {
    let bytes = vec_allocation_bytes::<T>(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "recovery baseline vector",
    })?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| BaselineBuildError::Allocation {
            resource: "recovery baseline vector",
            requested: count,
            message: error.to_string(),
        })?;
    let actual = vec_allocation_bytes::<T>(values.capacity()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "recovery baseline vector",
        }
    })?;
    budget.consume_bytes(actual)?;
    Ok(values)
}

fn parse_source(
    source: SourceId,
    image: Arc<[u8]>,
    base: &WorkspaceState,
    binary: &BinaryWorkspaceAdapter,
    budget: &mut AssetLoadBudget,
) -> Result<FrozenSourceParse, BaselineBuildError> {
    match source.kind() {
        SourceKind::SerializedFile => {
            let payload = binary
                .parse(Arc::clone(&image), budget)
                .map_err(map_binary_adapter_error)
                .map_err(map_workspace_parse_error)?;
            let BinaryPayload::SerializedFile(file) = payload else {
                return Err(BaselineBuildError::Parse {
                    message: "serialized source reparsed as a different binary kind".to_owned(),
                });
            };
            let file = if let Some(previous) = base
                .store()
                .get(source)
                .and_then(|entry| entry.cached_serialized())
            {
                (*file).with_type_tree_registry(previous.type_tree_registry().cloned())
            } else {
                *file
            };
            let file = promote_value_to_arc(file, budget, "baseline serialized parse")
                .map_err(map_workspace_parse_error)?;
            Ok(FrozenSourceParse::Serialized(file))
        }
        SourceKind::Yaml => {
            let parsed = parse_budgeted_yaml_source(image, budget)
                .map_err(|error| map_yaml_error("baseline YAML parsing", error))
                .map_err(map_workspace_parse_error)?;
            validate_yaml_identities(parsed.document(), budget)
                .map_err(map_workspace_parse_error)?;
            Ok(FrozenSourceParse::Yaml(Arc::clone(parsed.document())))
        }
        SourceKind::AssetBundle
        | SourceKind::WebFile
        | SourceKind::Archive
        | SourceKind::StreamedResource => Ok(FrozenSourceParse::None),
    }
}

fn format_from_artifact(
    source: SourceId,
    format: &PreparedArtifactFormat,
    base: &WorkspaceState,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceSourceFormatInspection, BaselineBuildError> {
    match format {
        PreparedArtifactFormat::SerializedFile(proof) => {
            Ok(WorkspaceSourceFormatInspection::SerializedFile(
                SerializedFileSummary::from_proof(proof, budget)
                    .map_err(map_workspace_parse_error)?,
            ))
        }
        PreparedArtifactFormat::AssetBundle(proof) => {
            Ok(WorkspaceSourceFormatInspection::AssetBundle(
                AssetBundleSummary::from_proof(proof, budget).map_err(map_workspace_parse_error)?,
            ))
        }
        PreparedArtifactFormat::WebFile(proof) => Ok(WorkspaceSourceFormatInspection::WebFile(
            WebFileSummary::from_proof(proof, budget).map_err(map_workspace_parse_error)?,
        )),
        PreparedArtifactFormat::StreamedResource(_) => {
            Ok(WorkspaceSourceFormatInspection::StreamedResource)
        }
        PreparedArtifactFormat::Yaml(proof) => Ok(WorkspaceSourceFormatInspection::Yaml {
            document_count: proof.documents(),
        }),
        PreparedArtifactFormat::VerbatimSource(_) => base
            .store()
            .get(source)
            .ok_or_else(|| BaselineBuildError::Parse {
                message: format!("verbatim artifact source {source:?} is absent from the baseline"),
            })?
            .format()
            .try_clone_with_budget(budget)
            .map_err(map_workspace_parse_error),
        _ => Err(BaselineBuildError::Parse {
            message: "prepared artifact uses an unsupported inspection format".to_owned(),
        }),
    }
}

fn inspect_recovered_source(
    source: SourceId,
    image: &Arc<[u8]>,
    parse: &FrozenSourceParse,
    base: &WorkspaceState,
    binary: &BinaryWorkspaceAdapter,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceSourceFormatInspection, BaselineBuildError> {
    match (source.kind(), parse) {
        (SourceKind::SerializedFile, FrozenSourceParse::Serialized(file)) => {
            Ok(WorkspaceSourceFormatInspection::SerializedFile(
                SerializedFileSummary::from_file(file, budget)
                    .map_err(map_workspace_parse_error)?,
            ))
        }
        (SourceKind::Yaml, FrozenSourceParse::Yaml(document)) => {
            let document_count = u64::try_from(document.entries().len()).map_err(|_| {
                BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "recovery_yaml_document_count",
                })
            })?;
            Ok(WorkspaceSourceFormatInspection::Yaml { document_count })
        }
        (SourceKind::AssetBundle, FrozenSourceParse::None) => {
            let payload = binary
                .parse(Arc::clone(image), budget)
                .map_err(map_binary_adapter_error)
                .map_err(map_workspace_parse_error)?;
            let BinaryPayload::AssetBundle(bundle) = payload else {
                return Err(BaselineBuildError::Parse {
                    message: "AssetBundle recovery image reparsed as a different kind".to_owned(),
                });
            };
            Ok(WorkspaceSourceFormatInspection::AssetBundle(
                AssetBundleSummary::from_bundle(&bundle, budget)
                    .map_err(map_workspace_parse_error)?,
            ))
        }
        (SourceKind::WebFile, FrozenSourceParse::None) => {
            let payload = binary
                .parse(Arc::clone(image), budget)
                .map_err(map_binary_adapter_error)
                .map_err(map_workspace_parse_error)?;
            let BinaryPayload::WebFile(web_file) = payload else {
                return Err(BaselineBuildError::Parse {
                    message: "WebFile recovery image reparsed as a different kind".to_owned(),
                });
            };
            Ok(WorkspaceSourceFormatInspection::WebFile(
                WebFileSummary::from_webfile(&web_file, budget)
                    .map_err(map_workspace_parse_error)?,
            ))
        }
        (SourceKind::Archive, FrozenSourceParse::None) => {
            let existing = base
                .store()
                .get(source)
                .ok_or_else(|| BaselineBuildError::Parse {
                    message: format!("archive source {source:?} is absent from the baseline"),
                })?;
            let actual = SourceFingerprint::from_bytes(SourceKind::Archive, image);
            if actual != existing.image().fingerprint() {
                return Err(BaselineBuildError::Parse {
                    message: "changed ZIP archive recovery is not a supported mutation output"
                        .to_owned(),
                });
            }
            existing
                .format()
                .try_clone_with_budget(budget)
                .map_err(map_workspace_parse_error)
        }
        (SourceKind::StreamedResource, FrozenSourceParse::None) => {
            Ok(WorkspaceSourceFormatInspection::StreamedResource)
        }
        _ => Err(BaselineBuildError::Parse {
            message: format!(
                "source {source:?} has a frozen parse incompatible with recovery inspection"
            ),
        }),
    }
}

fn map_workspace_parse_error(error: WorkspaceError) -> BaselineBuildError {
    match error {
        WorkspaceError::Budget(error) => BaselineBuildError::Budget(error),
        WorkspaceError::Allocation {
            resource,
            requested,
            message,
            ..
        } => BaselineBuildError::Allocation {
            resource,
            requested,
            message,
        },
        error => BaselineBuildError::Parse {
            message: error.to_string(),
        },
    }
}

#[derive(Debug, Error)]
pub(crate) enum BaselineBuildError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} bytes for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error("prepared artifact failure: {0}")]
    Artifact(String),
    #[error("prepared artifact stream failure: {message}")]
    ArtifactStream { message: String },
    #[error("prepared artifact stream I/O failure: {message}")]
    ArtifactIo {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("catalog baseline construction failed: {0}")]
    Catalog(#[source] Box<super::super::source_catalog::CatalogError>),
    #[error("workspace baseline validation failed: {0}")]
    State(#[source] Box<WorkspaceStateError>),
    #[error("baseline parse failed: {message}")]
    Parse { message: String },
    #[error("recovery baseline binding is invalid: {message}")]
    RecoveryBinding { message: String },
    #[error("recovery image for {source_id:?} is invalid: {message}")]
    RecoveryImage {
        source_id: SourceId,
        message: String,
    },
    #[error("source {source_id:?} fingerprint changed from {expected} to {actual}")]
    Fingerprint {
        source_id: SourceId,
        expected: unity_asset_core::SourceFingerprint,
        actual: unity_asset_core::SourceFingerprint,
    },
    #[error("prepared baseline revision changed from {expected} to {actual}")]
    Revision {
        expected: unity_asset_core::WorkspaceRevision,
        actual: unity_asset_core::WorkspaceRevision,
    },
    #[error("prepared baseline installation changed from {expected:?} to {actual:?}")]
    Installation {
        expected: super::super::WorkspaceInstallationDigest,
        actual: super::super::WorkspaceInstallationDigest,
    },
}

impl BaselineBuildError {
    fn catalog(error: CatalogError) -> Self {
        Self::Catalog(Box::new(error))
    }

    fn state(error: WorkspaceStateError) -> Self {
        Self::State(Box::new(error))
    }

    pub(crate) fn into_budget(self) -> Result<BudgetError, Self> {
        match self {
            Self::Budget(error) => Ok(error),
            Self::Catalog(error) => match *error {
                CatalogError::Budget(error) => Ok(error),
                error => Err(Self::Catalog(Box::new(error))),
            },
            Self::State(error) => match *error {
                WorkspaceStateError::Budget(error) => Ok(error),
                WorkspaceStateError::Catalog(error) => match *error {
                    CatalogError::Budget(error) => Ok(error),
                    error => Err(Self::state(WorkspaceStateError::Catalog(Box::new(error)))),
                },
                WorkspaceStateError::Store(error) => match *error {
                    SourceStoreError::Budget(error) => Ok(error),
                    error => Err(Self::state(WorkspaceStateError::Store(Box::new(error)))),
                },
                error => Err(Self::state(error)),
            },
            error => Err(error),
        }
    }

    pub(crate) fn is_retryable_prejournal(&self) -> bool {
        match self {
            Self::Allocation { .. } => true,
            Self::ArtifactIo { kind, .. } => !matches!(
                kind,
                io::ErrorKind::AlreadyExists
                    | io::ErrorKind::CrossesDevices
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
            ),
            Self::Catalog(error) => {
                matches!(error.as_ref(), CatalogError::AllocationFailed { .. })
            }
            Self::State(error) => match error.as_ref() {
                WorkspaceStateError::Catalog(error) => {
                    matches!(error.as_ref(), CatalogError::AllocationFailed { .. })
                }
                WorkspaceStateError::Store(error) => {
                    matches!(error.as_ref(), SourceStoreError::AllocationFailed { .. })
                }
                _ => false,
            },
            _ => false,
        }
    }
}

impl From<ArtifactStreamError> for BaselineBuildError {
    fn from(error: ArtifactStreamError) -> Self {
        match error {
            ArtifactStreamError::Io(error) => Self::ArtifactIo {
                kind: error.kind(),
                message: error.to_string(),
            },
            error => Self::ArtifactStream {
                message: error.to_string(),
            },
        }
    }
}

impl From<io::Error> for BaselineBuildError {
    fn from(error: io::Error) -> Self {
        Self::Parse {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_workspace_state_budget_error_is_preserved() {
        let error = BaselineBuildError::state(WorkspaceStateError::Budget(
            BudgetError::ArithmeticOverflow {
                resource: "workspace state",
            },
        ));

        assert!(matches!(
            error.into_budget(),
            Ok(BudgetError::ArithmeticOverflow {
                resource: "workspace state"
            })
        ));
    }
}
