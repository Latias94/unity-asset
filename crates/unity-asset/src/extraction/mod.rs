//! Deterministic, revision-bound artifact extraction.

use unity_asset_core::BudgetError;

mod artifact;
mod container;
mod executor;
mod json_contract;
mod manifest;
mod model;
mod selection;
mod yaml_split;

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

pub use container::{
    BUNDLE_CONTAINER_QUERY_CONTRACT, BUNDLE_CONTAINER_QUERY_VERSION,
    BUNDLE_CONTAINER_RESULT_CONTRACT, BUNDLE_CONTAINER_RESULT_VERSION,
    BundleContainerContractError, BundleContainerOccurrence, BundleContainerQuery,
    BundleContainerRawTarget, BundleContainerResolution, BundleContainerResult,
};
pub use executor::{
    ExistingOutputPolicy, ExtractionExecutionError, ExtractionExecutionLimits,
    ExtractionExecutionOptions, ExtractionExecutor, ExtractionFailurePolicy,
};
pub use manifest::{
    EXTRACTION_MANIFEST_CONTRACT, EXTRACTION_REPORT_CONTRACT, ExtractionArtifactRecord,
    ExtractionArtifactStatus, ExtractionCanonicalError, ExtractionManifest,
    ExtractionManifestArtifact, ExtractionManifestError, ExtractionReport, ExtractionReportCounts,
};
pub use model::{
    EXTRACTION_MANIFEST_VERSION, EXTRACTION_PLAN_CONTRACT, EXTRACTION_PLAN_VERSION,
    EXTRACTION_REPORT_VERSION, EXTRACTION_REQUEST_CONTRACT, EXTRACTION_REQUEST_VERSION,
    ExtractionArtifactKind, ExtractionDiagnostic, ExtractionDiagnosticCode, ExtractionFilter,
    ExtractionModelError, ExtractionPath, ExtractionPlan, ExtractionRepresentationPolicy,
    ExtractionRequest, ExtractionSelection, ExtractionSourceExpectation, ExtractionSourceRange,
    PlannedArtifact,
};
pub use selection::{ExtractionPlanError, ExtractionPlanner};
pub use yaml_split::{
    YAML_SPLIT_REPORT_CONTRACT, YAML_SPLIT_REPORT_VERSION, YamlSplitArtifact, YamlSplitError,
    YamlSplitExecutor, YamlSplitPlan, YamlSplitPlanner, YamlSplitReport,
};
