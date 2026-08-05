//! Deterministic, revision-bound artifact extraction.

use std::io::{self, Write};

use unity_asset_core::BudgetError;

mod artifact;
mod container;
mod contract;
mod executor;
mod json_contract;
mod manifest;
mod model;
mod planning_contract;
mod publication;
mod representation;
mod selection;
#[cfg(all(test, feature = "decode"))]
mod test_probe;
mod yaml_split;

struct CheckedByteCounter {
    bytes: u64,
    overflow_message: &'static str,
}

impl CheckedByteCounter {
    const fn new(overflow_message: &'static str) -> Self {
        Self {
            bytes: 0,
            overflow_message,
        }
    }

    const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Write for CheckedByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let amount =
            u64::try_from(buffer.len()).map_err(|_| io::Error::other(self.overflow_message))?;
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| io::Error::other(self.overflow_message))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn source_budget_error<'error>(
    error: &'error (dyn std::error::Error + 'static),
) -> Option<&'error BudgetError> {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(error) = candidate.downcast_ref::<BudgetError>() {
            return Some(error);
        }
        current = candidate.source();
    }
    None
}

pub use artifact::ExtractionOutputErrorKind;
pub use container::{
    BUNDLE_CONTAINER_QUERY_CONTRACT, BUNDLE_CONTAINER_QUERY_VERSION,
    BUNDLE_CONTAINER_RESULT_CONTRACT, BUNDLE_CONTAINER_RESULT_VERSION,
    BundleContainerContractError, BundleContainerOccurrence, BundleContainerQuery,
    BundleContainerRawTarget, BundleContainerResolution, BundleContainerResult,
};
pub use contract::{
    ExtractionAllocationUnit, ExtractionArtifactKind, ExtractionDiagnostic,
    ExtractionDiagnosticCode, ExtractionPath, ExtractionRepresentationPolicy,
    ExtractionSourceExpectation,
};
pub use executor::{
    ExistingOutputPolicy, ExtractionExecutionError, ExtractionExecutionLimits,
    ExtractionExecutionOptions, ExtractionExecutor, ExtractionFailurePolicy, ExtractionRunOptions,
};
pub use manifest::{
    EXTRACTION_MANIFEST_CONTRACT, EXTRACTION_REPORT_CONTRACT, ExtractionArtifactRecord,
    ExtractionArtifactStatus, ExtractionCanonicalError, ExtractionManifest,
    ExtractionManifestArtifact, ExtractionManifestError, ExtractionReport, ExtractionReportCounts,
};
pub use model::{
    EXTRACTION_MANIFEST_VERSION, EXTRACTION_PLAN_CONTRACT, EXTRACTION_PLAN_VERSION,
    EXTRACTION_REPORT_VERSION, EXTRACTION_REQUEST_CONTRACT, EXTRACTION_REQUEST_VERSION,
    ExtractionFilter, ExtractionModelError, ExtractionPlan, ExtractionReferenceDirection,
    ExtractionReferenceTraversalLimits, ExtractionRequest, ExtractionSelection,
    ExtractionSelectionWitness, PlannedArtifact,
};
pub use planning_contract::{ExtractionPlanError, ExtractionPlanMismatchKind};
pub use selection::ExtractionPlanner;
#[cfg(feature = "decode")]
pub use unity_asset_decode::media::MediaInspectionError;
pub use yaml_split::{
    YAML_SPLIT_REPORT_CONTRACT, YAML_SPLIT_REPORT_VERSION, YamlSplitArtifact, YamlSplitError,
    YamlSplitExecutor, YamlSplitPlan, YamlSplitPlanner, YamlSplitReport,
};
