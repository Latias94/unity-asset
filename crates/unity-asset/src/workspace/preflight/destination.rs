use std::ffi::OsString;
use std::fs;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, SourceFingerprint, SourceKind};
use unity_asset_write::artifact::{
    LogicalArtifactName, PreparedArtifact, PreparedArtifactFormat, PreparedArtifactSet,
};

use super::super::source_catalog::{
    CatalogError, VerifiedPhysicalBinding, VerifiedPhysicalDirectoryBinding,
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
pub(super) enum DestinationState {
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

/// One caller-declared output-to-filesystem binding.
#[derive(Debug, Clone, Copy)]
pub(super) struct PublicationDestination<'path> {
    output: &'path LogicalArtifactName,
    location: DestinationLocation<'path>,
    expectation: DestinationExpectation,
}

impl<'path> PublicationDestination<'path> {
    pub(super) const fn exact(
        output: &'path LogicalArtifactName,
        target: &'path Path,
        expectation: DestinationExpectation,
    ) -> Self {
        Self {
            output,
            location: DestinationLocation::Exact(target),
            expectation,
        }
    }

    #[cfg(test)]
    pub(super) const fn under_root(
        output: &'path LogicalArtifactName,
        root: &'path Path,
        expectation: DestinationExpectation,
    ) -> Self {
        Self {
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
pub(super) struct DestinationProof {
    output_name: String,
    evidence: DestinationEvidence,
}

impl DestinationProof {
    #[must_use]
    pub(super) fn output_name(&self) -> &str {
        &self.output_name
    }

    #[must_use]
    pub(super) fn target(&self) -> &Path {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => binding.path(),
            DestinationEvidence::Absent(binding) => &binding.target,
        }
    }

    #[must_use]
    pub(super) const fn expected(&self) -> DestinationState {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => {
                DestinationState::Existing(binding.fingerprint())
            }
            DestinationEvidence::Absent(_) => DestinationState::Absent,
        }
    }

    fn revalidate(
        &self,
        output: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        match &self.evidence {
            DestinationEvidence::Existing(binding) => {
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
                match binding.revalidate_current_contents(budget) {
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
                }
            }
            DestinationEvidence::Absent(binding) => binding.revalidate(output, budget),
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
    parent: VerifiedPhysicalDirectoryBinding,
    first_missing: PathBuf,
    target: PathBuf,
    source_kind: SourceKind,
}

impl AbsentDestinationProof {
    fn revalidate(
        &self,
        output: usize,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), DestinationProofError> {
        self.ensure_missing(output, budget)?;
        self.parent
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
    name: &'artifact LogicalArtifactName,
    artifact: &'artifact PreparedArtifact,
}

fn observe_destination(
    output: usize,
    prepared: OutputView<'_>,
    destination: PublicationDestination<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<DestinationProof, DestinationProofError> {
    let source_kind = artifact_source_kind(prepared.artifact)?;
    match destination.location {
        DestinationLocation::Exact(target) => {
            if !target.is_absolute() {
                return Err(DestinationProofError::NonAbsoluteTarget { output });
            }
            let target = budgeted_path_copy(target, budget)?;
            observe_path(
                output,
                prepared.name,
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
                prepared.name,
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
    output_name: &LogicalArtifactName,
    requested: &Path,
    containment_root: Option<&Path>,
    expectation: DestinationExpectation,
    source_kind: SourceKind,
    budget: &mut AssetLoadBudget,
) -> Result<DestinationProof, DestinationProofError> {
    let evidence = match entry_state(requested, output)? {
        EntryState::Missing => match expectation {
            DestinationExpectation::Existing(expected) => {
                return Err(DestinationProofError::ObservationMismatch {
                    output,
                    expected: DestinationState::Existing(expected),
                    actual: DestinationState::Absent,
                });
            }
            #[cfg(test)]
            DestinationExpectation::Observe => DestinationEvidence::Absent(observe_absent(
                output,
                requested,
                containment_root,
                source_kind,
                budget,
            )?),
            DestinationExpectation::Absent => DestinationEvidence::Absent(observe_absent(
                output,
                requested,
                containment_root,
                source_kind,
                budget,
            )?),
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
            DestinationEvidence::Existing(binding)
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
        output_name: budgeted_string(output_name.as_str(), budget)?,
        evidence,
    })
}

fn observe_absent(
    output: usize,
    requested: &Path,
    containment_root: Option<&Path>,
    source_kind: SourceKind,
    budget: &mut AssetLoadBudget,
) -> Result<AbsentDestinationProof, DestinationProofError> {
    let mut ancestor = budgeted_path_copy(requested, budget)?;
    if !ancestor.pop() {
        return Err(DestinationProofError::InvalidAbsentTarget { output });
    }
    loop {
        match entry_state(&ancestor, output)? {
            EntryState::Directory => break,
            EntryState::Missing => {
                if !ancestor.pop() {
                    return Err(DestinationProofError::InvalidAbsentTarget { output });
                }
            }
            actual => {
                return Err(DestinationProofError::InvalidParentState {
                    output,
                    actual: actual.destination_state(),
                });
            }
        }
    }

    let relative = requested
        .strip_prefix(&ancestor)
        .map_err(|_| DestinationProofError::InvalidAbsentTarget { output })?;
    let mut components = relative.components();
    let Some(Component::Normal(first_component)) = components.next() else {
        return Err(DestinationProofError::InvalidAbsentTarget { output });
    };
    if components.any(|component| !matches!(component, Component::Normal(_))) {
        return Err(DestinationProofError::InvalidAbsentTarget { output });
    }

    let parent = VerifiedPhysicalDirectoryBinding::verify_existing(&ancestor, budget)
        .map_err(|error| map_catalog_error(output, error))?;
    let target = budgeted_path_join(parent.path(), relative, budget)?;
    let first_missing = budgeted_path_join(parent.path(), Path::new(first_component), budget)?;
    ensure_contained(output, &target, containment_root)?;
    let proof = AbsentDestinationProof {
        parent,
        first_missing,
        target,
        source_kind,
    };
    proof.ensure_missing(output, budget)?;
    proof
        .parent
        .revalidate_current_entry(budget)
        .map_err(|error| map_parent_error(output, error))?;
    proof.ensure_missing(output, budget)?;
    Ok(proof)
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

fn artifact_source_kind(artifact: &PreparedArtifact) -> Result<SourceKind, DestinationProofError> {
    match artifact.format() {
        PreparedArtifactFormat::SerializedFile(_) => Ok(SourceKind::SerializedFile),
        PreparedArtifactFormat::AssetBundle(_) => Ok(SourceKind::AssetBundle),
        PreparedArtifactFormat::WebFile(_) => Ok(SourceKind::WebFile),
        PreparedArtifactFormat::StreamedResource(_) => Ok(SourceKind::StreamedResource),
        PreparedArtifactFormat::Yaml(_) => Ok(SourceKind::Yaml),
        PreparedArtifactFormat::VerbatimSource(proof) => Ok(proof.fingerprint().kind()),
        _ => Err(DestinationProofError::UnsupportedArtifactFormat),
    }
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

fn budgeted_string(
    value: &str,
    budget: &mut AssetLoadBudget,
) -> Result<String, DestinationProofError> {
    let requested = checked_usize_to_u64(value.len())?;
    budget.check_bytes(requested)?;
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|error| DestinationProofError::Allocation {
            resource: "destination output name",
            requested: value.len(),
            message: error.to_string(),
        })?;
    copy.push_str(value);
    budget.consume_bytes(checked_usize_to_u64(copy.capacity())?)?;
    Ok(copy)
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
pub(super) enum DestinationProofError {
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
    #[error("prepared outputs {first_output} and {second_output} resolve to the same destination")]
    DuplicateTarget {
        first_output: usize,
        second_output: usize,
    },
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
    #[error("prepared artifact format cannot be mapped to a publication source kind")]
    UnsupportedArtifactFormat,
}

#[cfg(test)]
mod tests;
