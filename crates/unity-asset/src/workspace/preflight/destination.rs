use std::ffi::OsString;
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, SourceFingerprint, SourceId, SourceKind};
use unity_asset_write::artifact::{
    LogicalArtifactName, OutputSlot, PreparedArtifact, PreparedArtifactSet,
};

use super::super::portable_path::{PortablePathError, native_key};
use super::super::source_catalog::{
    CatalogError, PhysicalFileIdentity, VerifiedPhysicalBinding, VerifiedPhysicalDirectoryBinding,
};

/// What prepare expects to find at one publication destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DestinationExpectation {
    /// Capture the current valid state as the compare-and-swap baseline.
    #[cfg(test)]
    Observe,
    Existing(SourceFingerprint),
    Absent,
}

/// Structured filesystem state used in destination conflict diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestinationState {
    Existing(SourceFingerprint),
    Absent,
    Directory,
    SymbolicLink,
    Other,
}

#[derive(Debug, Clone, Copy)]
enum DestinationLocation<'path> {
    Exact(&'path Path),
    #[cfg(test)]
    UnderRoot(&'path Path),
}

#[derive(Debug, Clone, Copy)]
struct PublicationAuthority {
    source: SourceId,
    output: OutputSlot,
}

/// One caller-declared output-to-filesystem binding.
#[derive(Debug, Clone, Copy)]
pub(super) struct PublicationDestination<'path> {
    authority: PublicationAuthority,
    output: &'path LogicalArtifactName,
    location: DestinationLocation<'path>,
    expectation: DestinationExpectation,
}

impl<'path> PublicationDestination<'path> {
    pub(super) const fn exact(
        source: SourceId,
        output_slot: OutputSlot,
        output: &'path LogicalArtifactName,
        target: &'path Path,
        expectation: DestinationExpectation,
    ) -> Self {
        Self {
            authority: PublicationAuthority {
                source,
                output: output_slot,
            },
            output,
            location: DestinationLocation::Exact(target),
            expectation,
        }
    }

    #[must_use]
    pub(super) const fn source(self) -> SourceId {
        self.authority.source
    }

    #[must_use]
    pub(super) const fn output_name(self) -> &'path LogicalArtifactName {
        self.output
    }

    #[cfg(test)]
    pub(super) const fn under_root(
        source: SourceId,
        output_slot: OutputSlot,
        output: &'path LogicalArtifactName,
        root: &'path Path,
        expectation: DestinationExpectation,
    ) -> Self {
        Self {
            authority: PublicationAuthority {
                source,
                output: output_slot,
            },
            output,
            location: DestinationLocation::UnderRoot(root),
            expectation,
        }
    }
}

/// Complete deterministic destination CAS proof for one prepared artifact set.
#[derive(Debug)]
pub(crate) struct DestinationProofSet {
    bindings: Vec<DestinationProof>,
}

impl DestinationProofSet {
    pub(super) fn observe(
        artifacts: &PreparedArtifactSet,
        destinations: &[PublicationDestination<'_>],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, DestinationProofError> {
        if artifacts.len() != destinations.len() {
            return Err(DestinationProofError::OutputCountMismatch {
                outputs: artifacts.len(),
                destinations: destinations.len(),
            });
        }

        let count = artifacts.len();
        let mut outputs = budgeted_vec::<OutputView<'_>>(count, budget)?;
        for output in artifacts.outputs() {
            outputs.push(OutputView {
                slot: output.slot(),
                name: output.name(),
                artifact: output.artifact(),
            });
        }
        outputs.sort_unstable_by(|left, right| left.name.as_str().cmp(right.name.as_str()));

        let mut destination_order = budgeted_vec::<usize>(count, budget)?;
        destination_order.extend(0..count);
        destination_order.sort_unstable_by(|left, right| {
            destinations[*left]
                .output
                .as_str()
                .cmp(destinations[*right].output.as_str())
                .then_with(|| left.cmp(right))
        });
        for pair in destination_order.windows(2) {
            if destinations[pair[0]].output.as_str() == destinations[pair[1]].output.as_str() {
                return Err(DestinationProofError::DuplicateOutput {
                    first_destination_declaration: pair[0],
                    second_destination_declaration: pair[1],
                });
            }
        }
        for (output, (prepared, destination)) in outputs.iter().zip(&destination_order).enumerate()
        {
            if prepared.name.as_str() != destinations[*destination].output.as_str() {
                return Err(DestinationProofError::OutputNameMismatch {
                    output,
                    destination: *destination,
                });
            }
        }

        let mut bindings = budgeted_vec::<DestinationProof>(count, budget)?;
        for (output, destination) in destination_order.into_iter().enumerate() {
            bindings.push(observe_destination(
                output,
                outputs[output],
                destinations[destination],
                budget,
            )?);
        }

        let mut target_order = budgeted_vec::<usize>(count, budget)?;
        target_order.extend(0..count);
        target_order.sort_unstable_by(|left, right| {
            bindings[*left]
                .target()
                .cmp(bindings[*right].target())
                .then_with(|| left.cmp(right))
        });
        for pair in target_order.windows(2) {
            if bindings[pair[0]].target() == bindings[pair[1]].target() {
                return Err(DestinationProofError::DuplicateTarget {
                    first_output: pair[0],
                    second_output: pair[1],
                });
            }
        }

        let mut portability_order = budgeted_vec::<PortableTargetKey>(count, budget)?;
        for (output, binding) in bindings.iter().enumerate() {
            portability_order.push(PortableTargetKey {
                output,
                key: portable_target_key(output, binding.target(), budget)?,
            });
        }
        portability_order.sort_unstable_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.output.cmp(&right.output))
        });
        for pair in portability_order.windows(2) {
            if pair[0].key == pair[1].key {
                return Err(DestinationProofError::PortableTargetCollision {
                    first_output: pair[0].output,
                    second_output: pair[1].output,
                });
            }
        }

        Ok(Self { bindings })
    }

    #[must_use]
    pub(super) fn bindings(&self) -> &[DestinationProof] {
        &self.bindings
    }

    pub(super) fn revalidate(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        for (output, binding) in self.bindings.iter().enumerate() {
            binding.revalidate(output, budget)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct DestinationProof {
    authority: PublicationAuthority,
    parent: VerifiedPhysicalDirectoryBinding,
    evidence: DestinationEvidence,
}

impl DestinationProof {
    #[must_use]
    pub(super) const fn source(&self) -> SourceId {
        self.authority.source
    }

    #[must_use]
    pub(super) const fn output(&self) -> OutputSlot {
        self.authority.output
    }

    #[must_use]
    pub(crate) fn target(&self) -> &Path {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => binding.path(),
            DestinationEvidence::Absent(binding) => &binding.target,
        }
    }

    #[must_use]
    pub(crate) const fn expected(&self) -> DestinationState {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => {
                DestinationState::Existing(binding.fingerprint())
            }
            DestinationEvidence::Absent(_) => DestinationState::Absent,
        }
    }

    #[must_use]
    pub(crate) const fn existing_file_identity(&self) -> Option<&PhysicalFileIdentity> {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => Some(binding.file_identity()),
            DestinationEvidence::Absent(_) => None,
        }
    }

    #[must_use]
    pub(crate) const fn destination_parent_identity(&self) -> &PhysicalFileIdentity {
        self.parent.identity()
    }

    #[must_use]
    pub(crate) fn filesystem_anchor(&self) -> &Path {
        self.parent.path()
    }

    pub(super) fn revalidate(
        &self,
        output: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => {
                self.parent
                    .revalidate_current_entry(budget)
                    .map_err(|error| map_parent_error(output, error))?;
                let expected = binding.fingerprint();
                match entry_state(binding.path(), output)? {
                    EntryState::File => {}
                    actual => {
                        return Err(DestinationProofError::ObservationMismatch {
                            output,
                            expected: DestinationState::Existing(expected),
                            actual: actual.destination_state(),
                        });
                    }
                }
                let result = match binding.revalidate_current_contents(budget) {
                    Ok(()) => Ok(()),
                    Err(CatalogError::VerifiedFingerprintMismatch { actual, .. }) => {
                        Err(DestinationProofError::ObservationMismatch {
                            output,
                            expected: DestinationState::Existing(expected),
                            actual: DestinationState::Existing(actual),
                        })
                    }
                    Err(CatalogError::VerifiedPhysicalBindingChanged { .. }) => {
                        Err(DestinationProofError::FileIdentityChanged {
                            output,
                            expected_fingerprint: expected,
                        })
                    }
                    Err(error) => Err(map_catalog_error(output, error)),
                };
                result?;
                self.parent
                    .revalidate_current_entry(budget)
                    .map_err(|error| map_parent_error(output, error))
            }
            DestinationEvidence::Absent(binding) => {
                binding.revalidate(&self.parent, output, budget)
            }
        }
    }
}

#[derive(Debug)]
enum DestinationEvidence {
    Existing(VerifiedPhysicalBinding),
    Absent(AbsentDestinationProof),
}

#[derive(Debug)]
struct AbsentDestinationProof {
    first_missing: PathBuf,
    target: PathBuf,
    source_kind: SourceKind,
}

impl AbsentDestinationProof {
    fn revalidate(
        &self,
        parent: &VerifiedPhysicalDirectoryBinding,
        output: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        self.ensure_missing(output, budget)?;
        parent
            .revalidate_current_entry(budget)
            .map_err(|error| map_parent_error(output, error))?;
        self.ensure_missing(output, budget)?;
        Ok(())
    }

    fn ensure_missing(
        &self,
        output: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        let actual = entry_state(&self.first_missing, output)?;
        if actual == EntryState::Missing {
            return Ok(());
        }
        let actual = observed_state(
            actual,
            self.source_kind,
            &self.first_missing,
            output,
            budget,
        )?;
        if self.first_missing == self.target {
            Err(DestinationProofError::ObservationMismatch {
                output,
                expected: DestinationState::Absent,
                actual,
            })
        } else {
            Err(DestinationProofError::PathComponentChanged { output, actual })
        }
    }
}

#[derive(Clone, Copy)]
struct OutputView<'artifact> {
    slot: OutputSlot,
    name: &'artifact LogicalArtifactName,
    artifact: &'artifact PreparedArtifact,
}

struct PortableTargetKey {
    output: usize,
    key: String,
}

fn portable_target_key(
    output: usize,
    target: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<String, DestinationProofError> {
    native_key(target, budget).map_err(|error| match error {
        PortablePathError::Budget(error) => DestinationProofError::Budget(error),
        PortablePathError::UnsupportedEncoding => {
            DestinationProofError::UnsupportedTargetEncoding { output }
        }
        PortablePathError::Allocation { requested, message } => DestinationProofError::Allocation {
            resource: "portable destination path",
            requested,
            message,
        },
    })
}

fn observe_destination(
    output: usize,
    prepared: OutputView<'_>,
    destination: PublicationDestination<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<DestinationProof, DestinationProofError> {
    if destination.authority.output != prepared.slot {
        return Err(DestinationProofError::OutputAuthorityMismatch { output });
    }
    // Destination observation records only the artifact-intrinsic source family. Logical source
    // ownership is sealed later by `PreparedPublicationSet::seal`.
    let source_kind = prepared.artifact.source_kind();
    match destination.location {
        DestinationLocation::Exact(target) => {
            if !target.is_absolute() {
                return Err(DestinationProofError::NonAbsoluteTarget { output });
            }
            let target = budgeted_path_copy(target, budget)?;
            observe_path(
                output,
                destination.authority,
                &target,
                None,
                destination.expectation,
                source_kind,
                budget,
            )
        }
        #[cfg(test)]
        DestinationLocation::UnderRoot(root) => {
            let root = VerifiedPhysicalDirectoryBinding::verify_existing(root, budget)
                .map_err(|error| map_catalog_error(output, error))?;
            let target =
                budgeted_path_join(root.path(), Path::new(prepared.name.as_str()), budget)?;
            let proof = observe_path(
                output,
                destination.authority,
                &target,
                Some(root.path()),
                destination.expectation,
                source_kind,
                budget,
            )?;
            root.revalidate_current_entry(budget)
                .map_err(|error| map_parent_error(output, error))?;
            Ok(proof)
        }
    }
}

fn observe_path(
    output: usize,
    authority: PublicationAuthority,
    requested: &Path,
    containment_root: Option<&Path>,
    expectation: DestinationExpectation,
    source_kind: SourceKind,
    budget: &mut AssetLoadBudget,
) -> Result<DestinationProof, DestinationProofError> {
    let (evidence, parent) = match entry_state(requested, output)? {
        EntryState::Missing => match expectation {
            DestinationExpectation::Existing(expected) => {
                return Err(DestinationProofError::ObservationMismatch {
                    output,
                    expected: DestinationState::Existing(expected),
                    actual: DestinationState::Absent,
                });
            }
            #[cfg(test)]
            DestinationExpectation::Observe => {
                let (parent, proof) =
                    observe_absent(output, requested, containment_root, source_kind, budget)?;
                (DestinationEvidence::Absent(proof), parent)
            }
            DestinationExpectation::Absent => {
                let (parent, proof) =
                    observe_absent(output, requested, containment_root, source_kind, budget)?;
                (DestinationEvidence::Absent(proof), parent)
            }
        },
        EntryState::File => {
            let binding = match expectation {
                #[cfg(test)]
                DestinationExpectation::Observe => {
                    VerifiedPhysicalBinding::observe_existing(source_kind, requested, budget)
                }
                DestinationExpectation::Existing(expected) => {
                    VerifiedPhysicalBinding::verify_existing(
                        source_kind,
                        requested,
                        expected,
                        budget,
                    )
                }
                DestinationExpectation::Absent => {
                    let actual =
                        VerifiedPhysicalBinding::observe_existing(source_kind, requested, budget)
                            .map_err(|error| map_catalog_error(output, error))?
                            .fingerprint();
                    return Err(DestinationProofError::ObservationMismatch {
                        output,
                        expected: DestinationState::Absent,
                        actual: DestinationState::Existing(actual),
                    });
                }
            }
            .map_err(|error| match error {
                CatalogError::VerifiedFingerprintMismatch { expected, actual } => {
                    DestinationProofError::ObservationMismatch {
                        output,
                        expected: DestinationState::Existing(expected),
                        actual: DestinationState::Existing(actual),
                    }
                }
                error => map_catalog_error(output, error),
            })?;
            ensure_contained(output, binding.path(), containment_root)?;
            let parent_path =
                binding
                    .path()
                    .parent()
                    .ok_or(DestinationProofError::InvalidParentState {
                        output,
                        actual: DestinationState::Other,
                    })?;
            let parent = VerifiedPhysicalDirectoryBinding::verify_existing(parent_path, budget)
                .map_err(|error| map_parent_error(output, error))?;
            (DestinationEvidence::Existing(binding), parent)
        }
        actual => {
            let actual = actual.destination_state();
            let expected = match expectation {
                DestinationExpectation::Existing(fingerprint) => {
                    Some(DestinationState::Existing(fingerprint))
                }
                DestinationExpectation::Absent => Some(DestinationState::Absent),
                #[cfg(test)]
                DestinationExpectation::Observe => None,
            };
            return match expected {
                Some(expected) => Err(DestinationProofError::ObservationMismatch {
                    output,
                    expected,
                    actual,
                }),
                None => Err(DestinationProofError::InvalidTargetState { output, actual }),
            };
        }
    };
    Ok(DestinationProof {
        authority,
        parent,
        evidence,
    })
}

fn observe_absent(
    output: usize,
    requested: &Path,
    containment_root: Option<&Path>,
    source_kind: SourceKind,
    budget: &mut AssetLoadBudget,
) -> Result<(VerifiedPhysicalDirectoryBinding, AbsentDestinationProof), DestinationProofError> {
    let parent_path = requested
        .parent()
        .ok_or(DestinationProofError::InvalidAbsentTarget { output })?;
    match entry_state(parent_path, output)? {
        EntryState::Directory => {}
        actual => {
            return Err(DestinationProofError::InvalidParentState {
                output,
                actual: actual.destination_state(),
            });
        }
    }
    let file_name = requested
        .file_name()
        .ok_or(DestinationProofError::InvalidAbsentTarget { output })?;
    if !matches!(
        requested.components().next_back(),
        Some(Component::Normal(_))
    ) {
        return Err(DestinationProofError::InvalidAbsentTarget { output });
    }
    let parent = VerifiedPhysicalDirectoryBinding::verify_existing(parent_path, budget)
        .map_err(|error| map_catalog_error(output, error))?;
    let target = budgeted_path_join(parent.path(), Path::new(file_name), budget)?;
    let first_missing = budgeted_path_copy(&target, budget)?;
    ensure_contained(output, &target, containment_root)?;
    let proof = AbsentDestinationProof {
        first_missing,
        target,
        source_kind,
    };
    proof.ensure_missing(output, budget)?;
    parent
        .revalidate_current_entry(budget)
        .map_err(|error| map_parent_error(output, error))?;
    proof.ensure_missing(output, budget)?;
    Ok((parent, proof))
}

fn ensure_contained(
    output: usize,
    target: &Path,
    containment_root: Option<&Path>,
) -> Result<(), DestinationProofError> {
    if containment_root.is_some_and(|root| !target.starts_with(root)) {
        return Err(DestinationProofError::TargetEscapesRoot { output });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Missing,
    File,
    Directory,
    SymbolicLink,
    Other,
}

impl EntryState {
    const fn destination_state(self) -> DestinationState {
        match self {
            Self::Missing => DestinationState::Absent,
            Self::File => DestinationState::Other,
            Self::Directory => DestinationState::Directory,
            Self::SymbolicLink => DestinationState::SymbolicLink,
            Self::Other => DestinationState::Other,
        }
    }
}

fn entry_state(path: &Path, output: usize) -> Result<EntryState, DestinationProofError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            Ok(if file_type.is_symlink() {
                EntryState::SymbolicLink
            } else if file_type.is_file() {
                EntryState::File
            } else if file_type.is_dir() {
                EntryState::Directory
            } else {
                EntryState::Other
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(EntryState::Missing),
        Err(error) => Err(DestinationProofError::Io {
            output,
            kind: error.kind(),
            message: error.to_string(),
        }),
    }
}

fn observed_state(
    state: EntryState,
    source_kind: SourceKind,
    path: &Path,
    output: usize,
    budget: &mut AssetLoadBudget,
) -> Result<DestinationState, DestinationProofError> {
    if state != EntryState::File {
        return Ok(state.destination_state());
    }
    VerifiedPhysicalBinding::observe_existing(source_kind, path, budget)
        .map(|binding| DestinationState::Existing(binding.fingerprint()))
        .map_err(|error| map_catalog_error(output, error))
}

fn budgeted_vec<T>(
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, DestinationProofError> {
    let entries = checked_usize_to_u64(count)?;
    let requested = checked_allocation_bytes::<T>(count)?;
    budget.check_entries(entries)?;
    budget.check_bytes(requested)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| DestinationProofError::Allocation {
            resource: "destination proof vector",
            requested: count,
            message: error.to_string(),
        })?;
    let actual = checked_allocation_bytes::<T>(values.capacity())?;
    budget.check_bytes(actual)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(actual)?;
    Ok(values)
}

fn budgeted_path_copy(
    value: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, DestinationProofError> {
    budgeted_os_string(value.as_os_str(), None, budget).map(PathBuf::from)
}

fn budgeted_path_join(
    base: &Path,
    tail: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, DestinationProofError> {
    let value = budgeted_os_string(base.as_os_str(), Some(tail), budget)?;
    Ok(PathBuf::from(value))
}

fn budgeted_os_string(
    base: &std::ffi::OsStr,
    tail: Option<&Path>,
    budget: &mut AssetLoadBudget,
) -> Result<OsString, DestinationProofError> {
    let requested = base
        .len()
        .checked_add(tail.map_or(0, |path| path.as_os_str().len().saturating_add(1)))
        .ok_or(DestinationProofError::ArithmeticOverflow {
            resource: "destination path",
        })?;
    budget.check_bytes(checked_usize_to_u64(requested)?)?;
    let mut value = OsString::new();
    value
        .try_reserve_exact(requested)
        .map_err(|error| DestinationProofError::Allocation {
            resource: "destination path",
            requested,
            message: error.to_string(),
        })?;
    value.push(base);
    if let Some(tail) = tail {
        let mut path = PathBuf::from(value);
        path.push(tail);
        value = path.into_os_string();
    }
    budget.consume_bytes(checked_usize_to_u64(value.capacity())?)?;
    Ok(value)
}

fn checked_allocation_bytes<T>(count: usize) -> Result<u64, DestinationProofError> {
    size_of::<T>()
        .checked_mul(count)
        .ok_or(DestinationProofError::ArithmeticOverflow {
            resource: "destination proof allocation",
        })
        .and_then(checked_usize_to_u64)
}

fn checked_usize_to_u64(value: usize) -> Result<u64, DestinationProofError> {
    u64::try_from(value).map_err(|_| DestinationProofError::ArithmeticOverflow {
        resource: "destination proof allocation",
    })
}

fn map_catalog_error(output: usize, error: CatalogError) -> DestinationProofError {
    match error {
        CatalogError::Budget(error) => DestinationProofError::Budget(error),
        error => DestinationProofError::Catalog {
            output,
            source: Box::new(error),
        },
    }
}

fn map_parent_error(output: usize, error: CatalogError) -> DestinationProofError {
    match error {
        CatalogError::Budget(error) => DestinationProofError::Budget(error),
        CatalogError::VerifiedPhysicalBindingChanged { .. }
        | CatalogError::VerifiedPhysicalBindingIo {
            kind: io::ErrorKind::NotFound,
            ..
        } => DestinationProofError::ParentIdentityChanged { output },
        error => DestinationProofError::Catalog {
            output,
            source: Box::new(error),
        },
    }
}

#[derive(Debug, Error)]
/// Destination proof failures use canonical prepared-output ordinals.
///
/// Every singular `output`, `first_output`, and `second_output` field indexes prepared outputs
/// after sorting by [`LogicalArtifactName`]. [`Self::DuplicateOutput`] is deliberately different:
/// its fields index the caller's destination declarations because no output bijection exists.
pub(crate) enum DestinationProofError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("destination proof allocation overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to allocate {requested} elements/bytes for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error(
        "prepared artifacts expose {outputs} outputs, but {destinations} destinations were declared"
    )]
    OutputCountMismatch { outputs: usize, destinations: usize },
    #[error(
        "destination declarations {first_destination_declaration} and {second_destination_declaration} bind the same output"
    )]
    DuplicateOutput {
        first_destination_declaration: usize,
        second_destination_declaration: usize,
    },
    #[error("prepared output {output} does not match destination declaration {destination}")]
    OutputNameMismatch { output: usize, destination: usize },
    #[error("prepared output {output} was bound with a foreign output capability")]
    OutputAuthorityMismatch { output: usize },
    #[error("prepared outputs {first_output} and {second_output} resolve to the same destination")]
    DuplicateTarget {
        first_output: usize,
        second_output: usize,
    },
    #[error(
        "prepared outputs {first_output} and {second_output} collide under portable filesystem rules"
    )]
    PortableTargetCollision {
        first_output: usize,
        second_output: usize,
    },
    #[error("publication destination for output {output} is not valid UTF-8")]
    UnsupportedTargetEncoding { output: usize },
    #[error("publication destination for output {output} must be absolute")]
    NonAbsoluteTarget { output: usize },
    #[error("publication destination for output {output} escapes its declared root")]
    TargetEscapesRoot { output: usize },
    #[error("publication destination for output {output} has unsupported state {actual:?}")]
    InvalidTargetState {
        output: usize,
        actual: DestinationState,
    },
    #[error("publication destination parent for output {output} has unsupported state {actual:?}")]
    InvalidParentState {
        output: usize,
        actual: DestinationState,
    },
    #[error("publication destination for output {output} cannot establish an absent path proof")]
    InvalidAbsentTarget { output: usize },
    #[error("destination output {output} changed from {expected:?} to {actual:?}")]
    ObservationMismatch {
        output: usize,
        expected: DestinationState,
        actual: DestinationState,
    },
    #[error("destination output {output} retained bytes but changed stable file identity")]
    FileIdentityChanged {
        output: usize,
        expected_fingerprint: SourceFingerprint,
    },
    #[error("absent destination output {output} changed its stable parent identity")]
    ParentIdentityChanged { output: usize },
    #[error("an absent path component for destination output {output} appeared as {actual:?}")]
    PathComponentChanged {
        output: usize,
        actual: DestinationState,
    },
    #[error("failed to inspect destination output {output}: {message}")]
    Io {
        output: usize,
        kind: io::ErrorKind,
        message: String,
    },
    #[error("failed to prove destination output {output}: {source}")]
    Catalog {
        output: usize,
        #[source]
        source: Box<CatalogError>,
    },
}

#[cfg(test)]
mod tests;
