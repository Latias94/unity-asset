//! Stable content proofs for every physical source consumed by a prepared view.

use std::collections::TryReserveError;
use std::mem::size_of;

use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError, SourceFingerprint, SourceId, SourceKind};

use super::super::source_catalog::{CatalogError, SourceCatalog, VerifiedPhysicalBinding};

/// One unique physical dependency and the exact file identity observed during prepare.
#[derive(Debug)]
pub(crate) struct PhysicalDependencyProof {
    source: SourceId,
    binding: VerifiedPhysicalBinding,
}

/// Complete, source-sorted CAS baseline for the physical inputs behind one prepared view.
#[derive(Debug)]
pub(crate) struct PhysicalDependencyProofSet {
    bindings: Vec<PhysicalDependencyProof>,
}

impl PhysicalDependencyProofSet {
    pub(crate) fn observe(
        catalog: &SourceCatalog,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, PhysicalDependencyProofError> {
        let dependency_count = catalog
            .iter()
            .filter(|(_, descriptor)| {
                matches!(
                    descriptor.kind(),
                    SourceKind::Yaml | SourceKind::SerializedFile
                )
            })
            .count();
        let mut owners =
            budgeted_vec::<SourceId>(dependency_count, "physical dependency owners", budget)?;
        for (source, descriptor) in catalog.iter() {
            if !matches!(
                descriptor.kind(),
                SourceKind::Yaml | SourceKind::SerializedFile
            ) {
                continue;
            }
            let owner = catalog
                .physical_domain_owner(source)
                .map_err(|source_error| PhysicalDependencyProofError::Catalog {
                    source_id: source,
                    expected: None,
                    source: Box::new(source_error),
                })?;
            owners.push(owner);
        }
        owners.sort_unstable();
        owners.dedup();

        let mut bindings = budgeted_vec::<PhysicalDependencyProof>(
            owners.len(),
            "physical dependency proofs",
            budget,
        )?;
        for source in owners {
            let expected = catalog.fingerprint(source).map_err(|source_error| {
                PhysicalDependencyProofError::Catalog {
                    source_id: source,
                    expected: None,
                    source: Box::new(source_error),
                }
            })?;
            let origin = catalog.physical_origin(source).map_err(|source_error| {
                PhysicalDependencyProofError::Catalog {
                    source_id: source,
                    expected: Some(expected),
                    source: Box::new(source_error),
                }
            })?;
            let binding = VerifiedPhysicalBinding::verify_existing(
                source.kind(),
                origin.path(),
                expected,
                budget,
            )
            .map_err(|source_error| PhysicalDependencyProofError::Catalog {
                source_id: source,
                expected: Some(expected),
                source: Box::new(source_error),
            })?;
            bindings.push(PhysicalDependencyProof { source, binding });
        }
        Ok(Self { bindings })
    }

    #[must_use]
    pub(crate) fn bindings(&self) -> &[PhysicalDependencyProof] {
        &self.bindings
    }

    pub(crate) fn revalidate(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), PhysicalDependencyProofError> {
        for proof in &self.bindings {
            let expected = proof.binding.fingerprint();
            if let Err(source_error) = proof.binding.revalidate_current_contents(budget) {
                if let CatalogError::VerifiedFingerprintMismatch { actual, .. } = &source_error {
                    return Err(PhysicalDependencyProofError::ContentChanged {
                        source_id: proof.source,
                        expected,
                        actual: *actual,
                    });
                }
                if !matches!(&source_error, CatalogError::Budget(_))
                    && let Ok(actual) = VerifiedPhysicalBinding::observe_existing(
                        proof.source.kind(),
                        proof.binding.path(),
                        budget,
                    )
                {
                    return Err(PhysicalDependencyProofError::ContentChanged {
                        source_id: proof.source,
                        expected,
                        actual: actual.fingerprint(),
                    });
                }
                return Err(PhysicalDependencyProofError::Catalog {
                    source_id: proof.source,
                    expected: Some(expected),
                    source: Box::new(source_error),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum PhysicalDependencyProofError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("physical dependency allocation overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("failed to allocate {requested} entries for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("physical dependency {source_id:?} changed from {expected} to {actual} during prepare")]
    ContentChanged {
        source_id: SourceId,
        expected: SourceFingerprint,
        actual: SourceFingerprint,
    },
    #[error("physical dependency {source_id:?} failed validation: {source}")]
    Catalog {
        source_id: SourceId,
        expected: Option<SourceFingerprint>,
        #[source]
        source: Box<CatalogError>,
    },
}

impl PhysicalDependencyProofError {
    #[must_use]
    pub(crate) const fn source_id(&self) -> Option<SourceId> {
        match self {
            Self::ContentChanged { source_id, .. } | Self::Catalog { source_id, .. } => {
                Some(*source_id)
            }
            Self::Budget(_) | Self::ArithmeticOverflow { .. } | Self::Allocation { .. } => None,
        }
    }

    #[must_use]
    pub(crate) const fn expected_fingerprint(&self) -> Option<SourceFingerprint> {
        match self {
            Self::ContentChanged { expected, .. } => Some(*expected),
            Self::Catalog { expected, .. } => *expected,
            Self::Budget(_) | Self::ArithmeticOverflow { .. } | Self::Allocation { .. } => None,
        }
    }

    #[must_use]
    pub(crate) fn actual_fingerprint(&self) -> Option<SourceFingerprint> {
        match self {
            Self::ContentChanged { actual, .. } => Some(*actual),
            Self::Catalog { source, .. } => match source.as_ref() {
                CatalogError::VerifiedFingerprintMismatch { actual, .. } => Some(*actual),
                _ => None,
            },
            Self::Budget(_) | Self::ArithmeticOverflow { .. } | Self::Allocation { .. } => None,
        }
    }
}

fn budgeted_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, PhysicalDependencyProofError> {
    let planned_bytes = allocation_bytes::<T>(capacity)?;
    let planned_entries = usize_to_u64(capacity, resource)?;
    budget.check_entries(planned_entries)?;
    budget.check_bytes(planned_bytes)?;

    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|source| {
        PhysicalDependencyProofError::Allocation {
            resource,
            requested: capacity,
            source,
        }
    })?;
    let actual_bytes = allocation_bytes::<T>(values.capacity())?;
    let actual_entries = usize_to_u64(values.capacity(), resource)?;
    budget.check_entries(actual_entries)?;
    budget.check_bytes(actual_bytes)?;
    budget.consume_entries(actual_entries)?;
    budget.consume_bytes(actual_bytes)?;
    Ok(values)
}

fn allocation_bytes<T>(capacity: usize) -> Result<u64, PhysicalDependencyProofError> {
    size_of::<T>()
        .checked_mul(capacity)
        .ok_or(PhysicalDependencyProofError::ArithmeticOverflow {
            resource: "physical dependency vector",
        })
        .and_then(|bytes| usize_to_u64(bytes, "physical dependency vector"))
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, PhysicalDependencyProofError> {
    u64::try_from(value).map_err(|_| PhysicalDependencyProofError::ArithmeticOverflow { resource })
}
