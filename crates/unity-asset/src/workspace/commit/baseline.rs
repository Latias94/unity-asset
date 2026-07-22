//! Construction of the next immutable workspace baseline.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, SourceFingerprint, SourceId, SourceKind, VerifiedSourceImage,
    arc_slice_allocation_bytes, arc_value_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_write::artifact::{ArtifactHandle, ArtifactStreamError, PreparedArtifactSet};

use super::super::adapter::binary::{BinaryPayload, BinaryWorkspaceAdapter};
use super::super::adapter::yaml::parse_yaml_source;
use super::super::interface::{
    map_binary_adapter_error, map_yaml_adapter_error, promote_box_to_arc, validate_yaml_identities,
};
use super::super::overlay::PreparedStateCore;
use super::super::preflight::PreparedChange;
use super::super::source_catalog::{CatalogError, PhysicalDomainChange, SourceDescriptor};
use super::super::state::{WorkspaceState, WorkspaceStateError};
use super::super::store::{FrozenSourceParse, SourceStoreError};
use super::super::view::WorkspaceError;
use super::journal::{Journal, JournalBaselineImage, JournalBaselineSource, JournalCatalogAction};
use super::platform::{
    DirectoryIdentity, observe_directory_identity, open_readonly_regular_in_parent,
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
) -> Result<Arc<[u8]>, BaselineBuildError> {
    let artifact = journal.manifest().artifacts().get(index).ok_or_else(|| {
        BaselineBuildError::RecoveryBinding {
            message: "recovery artifact index is out of range".to_owned(),
        }
    })?;
    let (path, parent_identity) = match location {
        RecoveryArtifactLocation::Target => (
            artifact.target().join_root(journal.layout().parent()),
            artifact.destination_parent_identity(),
        ),
        RecoveryArtifactLocation::Staging => (
            artifact.staging().join_root(journal.layout().directory()),
            journal.manifest().directories().stage(),
        ),
    };
    read_recovery_image(
        &path,
        parent_identity,
        artifact.source(),
        SourceFingerprint::new(artifact.source().kind(), artifact.new_digest()),
        artifact.bytes(),
        budget,
    )
}

/// Fully validated next baseline and the exact state it may replace.
pub(crate) struct PreparedBaseline {
    pub(crate) expected: Arc<WorkspaceState>,
    pub(crate) next: Arc<WorkspaceState>,
}

/// Artifact images retained while the publication transaction builds its baseline.
pub(crate) struct MaterializedImages {
    images: Vec<Option<Arc<[u8]>>>,
}

impl MaterializedImages {
    pub(crate) fn new(
        set: &PreparedArtifactSet,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BaselineBuildError> {
        let count = set.proof_image_count();
        let planned = vec_allocation_bytes::<Option<Arc<[u8]>>>(count).map_err(|_| {
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
        let actual =
            vec_allocation_bytes::<Option<Arc<[u8]>>>(images.capacity()).map_err(|_| {
                BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "baseline materialized image table",
                })
            })?;
        budget.consume_bytes(actual)?;
        images.resize_with(count, || None);
        Ok(Self { images })
    }

    pub(crate) fn insert(&mut self, handle: ArtifactHandle, image: Arc<[u8]>) {
        if let Some(slot) = self.images.get_mut(handle.ordinal()) {
            *slot = Some(image);
        }
    }

    pub(crate) fn get(&self, handle: ArtifactHandle) -> Option<&Arc<[u8]>> {
        self.images.get(handle.ordinal()).and_then(Option::as_ref)
    }

    pub(crate) fn materialize(
        &mut self,
        set: &PreparedArtifactSet,
        handle: ArtifactHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<[u8]>, BaselineBuildError> {
        if let Some(image) = self.get(handle) {
            return Ok(Arc::clone(image));
        }
        self.stream_and_materialize(set, handle, &mut io::sink(), budget)
    }

    pub(crate) fn stream_and_materialize(
        &mut self,
        set: &PreparedArtifactSet,
        handle: ArtifactHandle,
        sink: &mut impl Write,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<[u8]>, BaselineBuildError> {
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
        let arc_bytes = arc_slice_allocation_bytes::<u8>(bytes.len()).map_err(|_| {
            BaselineBuildError::Budget(BudgetError::ArithmeticOverflow {
                resource: "baseline artifact backing",
            })
        })?;
        budget.check_bytes(arc_bytes)?;
        let image: Arc<[u8]> = Arc::from(bytes);
        budget.consume_bytes(arc_bytes)?;
        self.insert(handle, Arc::clone(&image));
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
    images: &mut MaterializedImages,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedBaseline, BaselineBuildError> {
    let state = prepared.state();
    let core: &PreparedStateCore = state.core().as_ref();
    let base = core.base().state();
    let catalog = core
        .catalog()
        .begin_transaction(budget)
        .map_err(BaselineBuildError::catalog)?
        .commit(budget)
        .map_err(BaselineBuildError::catalog)?;
    let mut store = base
        .store()
        .clone_for_update(budget)
        .map_err(BaselineBuildError::Store)?;
    let artifacts = core.artifacts();

    for binding in core.source_bindings() {
        let image = images.materialize(artifacts, binding.artifact(), budget)?;
        let parse = parse_source(binding.source(), Arc::clone(&image), base, binary, budget)?;
        let verified = VerifiedSourceImage::verify(binding.source().kind(), image);
        if verified.fingerprint() != binding.fingerprint() {
            return Err(BaselineBuildError::Fingerprint {
                source_id: binding.source(),
                expected: binding.fingerprint(),
                actual: verified.fingerprint(),
            });
        }
        store
            .insert(binding.source(), verified, parse, budget)
            .map_err(BaselineBuildError::Store)?;
    }

    let next = WorkspaceState::new(core.base().workspace_id(), catalog, store, budget)
        .map_err(BaselineBuildError::state)?;
    let retained = arc_value_allocation_bytes::<WorkspaceState>().map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "workspace baseline state",
        }
    })?;
    budget.check_bytes(retained)?;
    let next = Arc::new(next);
    budget.consume_bytes(retained)?;
    if next.revision() != core.revision() {
        return Err(BaselineBuildError::Revision {
            expected: core.revision(),
            actual: next.revision(),
        });
    }
    Ok(PreparedBaseline {
        expected: Arc::clone(core.base().state()),
        next,
    })
}

pub(crate) fn build_from_journal_with_images(
    expected: Arc<WorkspaceState>,
    journal: &Journal,
    binary: &BinaryWorkspaceAdapter,
    published_images: Option<&[Option<Arc<[u8]>>]>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedBaseline, BaselineBuildError> {
    let base = expected.as_ref();
    let manifest = journal.manifest();
    if base.workspace() != manifest.workspace_id() {
        return Err(BaselineBuildError::RecoveryBinding {
            message: "recovery baseline does not match the loaded workspace identity".to_owned(),
        });
    }

    let sources = manifest.baseline().sources();
    let mut changes = reserve_recovery_vec::<PhysicalDomainChange>(sources.len(), budget)?;
    let mut catalog = base
        .catalog()
        .begin_transaction(budget)
        .map_err(BaselineBuildError::catalog)?;
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
                let actual = catalog
                    .register(descriptor, source.fingerprint(), budget)
                    .map_err(BaselineBuildError::catalog)?;
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
                let actual = catalog
                    .register(descriptor, source.fingerprint(), budget)
                    .map_err(BaselineBuildError::catalog)?;
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

    catalog
        .rewrite_physical_domains_from_changes(&changes, budget)
        .map_err(BaselineBuildError::catalog)?;
    let catalog = catalog
        .commit(budget)
        .map_err(BaselineBuildError::catalog)?;

    let mut store = base
        .store()
        .clone_for_update(budget)
        .map_err(BaselineBuildError::Store)?;
    for source in sources {
        validate_in_place_binding(base, journal, source)?;
        let image = recovery_image(journal, source, published_images, budget)?;
        let verified = VerifiedSourceImage::verify(source.source().kind(), image);
        if verified.fingerprint() != source.fingerprint() {
            return Err(BaselineBuildError::Fingerprint {
                source_id: source.source(),
                expected: source.fingerprint(),
                actual: verified.fingerprint(),
            });
        }
        let parse = parse_source(
            source.source(),
            Arc::clone(verified.backing()),
            base,
            binary,
            budget,
        )?;
        store
            .insert(source.source(), verified, parse, budget)
            .map_err(BaselineBuildError::Store)?;
    }

    let next = WorkspaceState::new(base.workspace(), catalog, store, budget)
        .map_err(BaselineBuildError::state)?;
    if next.revision() != manifest.committed_revision() {
        return Err(BaselineBuildError::Revision {
            expected: manifest.committed_revision(),
            actual: next.revision(),
        });
    }
    let retained = arc_value_allocation_bytes::<WorkspaceState>().map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "recovery workspace state",
        }
    })?;
    budget.consume_bytes(retained)?;
    let next = Arc::new(next);
    Ok(PreparedBaseline { expected, next })
}

fn validate_in_place_binding(
    base: &WorkspaceState,
    journal: &Journal,
    source: &JournalBaselineSource,
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
    let target = artifact.target().join_root(journal.layout().parent());
    verify_recovery_parent(
        &target,
        artifact.destination_parent_identity(),
        source.source(),
    )?;
    let target_parent = target
        .parent()
        .ok_or_else(|| BaselineBuildError::RecoveryBinding {
            message: "published source target has no parent directory".to_owned(),
        })?;
    let target_name = target
        .file_name()
        .ok_or_else(|| BaselineBuildError::RecoveryBinding {
            message: "published source target has no file name".to_owned(),
        })?;
    let canonical_parent =
        fs::canonicalize(target_parent).map_err(|error| BaselineBuildError::RecoveryImage {
            source_id: source.source(),
            message: error.to_string(),
        })?;
    let canonical = canonical_parent.join(target_name);
    let origin = base
        .catalog()
        .physical_origin(source.source())
        .map_err(BaselineBuildError::catalog)?;
    if origin.path() != canonical {
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
    published_images: Option<&[Option<Arc<[u8]>>]>,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>, BaselineBuildError> {
    let (path, expected_bytes, parent_identity) = match source.image() {
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
                return Ok(Arc::clone(image));
            }
            (
                artifact.target().join_root(journal.layout().parent()),
                artifact.bytes(),
                artifact.destination_parent_identity(),
            )
        }
        JournalBaselineImage::Blob { path, bytes, .. } => (
            path.join_root(journal.layout().directory()),
            *bytes,
            journal.manifest().directories().baseline(),
        ),
    };
    read_recovery_image(
        &path,
        parent_identity,
        source.source(),
        source.fingerprint(),
        expected_bytes,
        budget,
    )
}

fn read_recovery_image(
    path: &Path,
    expected_parent: &DirectoryIdentity,
    source: SourceId,
    expected: SourceFingerprint,
    expected_bytes: u64,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>, BaselineBuildError> {
    verify_recovery_parent(path, expected_parent, source)?;
    let mut file = open_readonly_regular_in_parent(path, expected_parent).map_err(|error| {
        BaselineBuildError::RecoveryImage {
            source_id: source,
            message: error.to_string(),
        }
    })?;
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
    let arc_bytes = arc_slice_allocation_bytes::<u8>(length_usize).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "recovery baseline image backing",
        }
    })?;
    budget.check_bytes(arc_bytes)?;
    let backing = Arc::<[u8]>::from(bytes);
    budget.consume_bytes(arc_bytes)?;
    let actual = SourceFingerprint::from_bytes(expected.kind(), &backing);
    if actual != expected {
        return Err(BaselineBuildError::Fingerprint {
            source_id: source,
            expected,
            actual,
        });
    }
    Ok(backing)
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
            let BinaryPayload::SerializedFile(mut file) = payload else {
                return Err(BaselineBuildError::Parse {
                    message: "serialized source reparsed as a different binary kind".to_owned(),
                });
            };
            if let Some(previous) = base
                .store()
                .get(source)
                .and_then(|entry| entry.cached_serialized())
            {
                file.set_type_tree_registry(previous.type_tree_registry().cloned());
            }
            let file = promote_box_to_arc(file, budget, "baseline serialized parse")
                .map_err(map_workspace_parse_error)?;
            Ok(FrozenSourceParse::Serialized(file))
        }
        SourceKind::Yaml => {
            let parsed = parse_yaml_source(image, budget)
                .map_err(|error| map_yaml_adapter_error("baseline YAML parsing", error))
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
    #[error("source store baseline construction failed: {0}")]
    Store(#[source] SourceStoreError),
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
            Self::Store(SourceStoreError::Budget(error)) => Ok(error),
            Self::State(error) => match *error {
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
            Self::Store(error) => matches!(error, SourceStoreError::AllocationFailed { .. }),
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
