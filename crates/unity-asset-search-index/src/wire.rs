use std::path::Path;
use std::sync::OnceLock;

use unity_asset_core::{DigestV1, SourceLocator};
use unity_asset_search_core::{MatchKind, SearchPolicy};
use unity_asset_search_protocol::{
    GenerationIdV1, GenerationStamp as WireGenerationStamp, Location, MAX_ERROR_MESSAGE_BYTES,
    MAX_REINDEX_PUBLISH_WARNING_BYTES, PortablePath, QueryPolicyId, ReindexAnalysisEvidence,
    ReindexDiskEstimate, ReindexDisposition, ReindexEvidence, ReindexReceipt,
    SEARCH_PROTOCOL_REVISION, WireProjectionError,
};

use crate::generation::{GenerationStamp, SearchGenerationId};
use crate::pipeline::{PipelineBuildDisposition, PipelineBuildOutput};

const QUERY_POLICY_DOMAIN: &str = "unity-asset:search-query-policy:v1";
const INDEX_QUERY_SEMANTICS: &str = "index-query-projection:v1";
const REFERENCE_QUERY_SEMANTICS: &str = "reference-query-cursor-binding:v2";
const SUGGEST_QUERY_SEMANTICS: &str = "path-and-kind-suggestions:v1";

#[must_use]
pub(crate) fn query_policy_id() -> QueryPolicyId {
    static QUERY_POLICY_ID: OnceLock<QueryPolicyId> = OnceLock::new();
    *QUERY_POLICY_ID.get_or_init(|| {
        let policy = SearchPolicy::default();
        let fuzzy = policy.fuzzy_fallback;
        let limits = policy.limits;
        let identity = format!(
            "{QUERY_POLICY_DOMAIN}\n\
             {INDEX_QUERY_SEMANTICS}\n\
             {REFERENCE_QUERY_SEMANTICS}\n\
             {SUGGEST_QUERY_SEMANTICS}\n\
             max_candidates={}\n\
             candidate_multiplier={}\n\
             filtered_candidate_multiplier={}\n\
             fuzzy.minimum_confident_matches={}\n\
             fuzzy.minimum_confident_kind={}\n\
             fuzzy.minimum_query_chars={}\n\
             fuzzy.maximum_query_chars={}\n\
             fuzzy.maximum_edit_distance={}\n\
             limits.max_query_bytes={}\n\
             limits.max_query_terms={}\n\
             limits.max_retrieval_terms={}\n\
             limits.max_candidate_inputs={}\n\
             limits.max_stable_key_bytes={}\n\
             limits.max_name_bytes={}\n\
             limits.max_path_bytes={}\n\
             limits.max_kind_bytes={}\n\
             limits.max_guid_bytes={}\n\
             limits.max_container_source_path_bytes={}\n\
             limits.max_evidence_items={}\n\
             limits.max_total_candidate_bytes={}\n\
             limits.max_fuzzy_work_units={}",
            policy.max_candidates,
            policy.candidate_multiplier,
            policy.filtered_candidate_multiplier,
            fuzzy.minimum_confident_matches,
            match_kind_name(fuzzy.minimum_confident_kind),
            fuzzy.minimum_query_chars,
            fuzzy.maximum_query_chars,
            fuzzy.maximum_edit_distance,
            limits.max_query_bytes,
            limits.max_query_terms,
            limits.max_retrieval_terms,
            limits.max_candidate_inputs,
            limits.max_stable_key_bytes,
            limits.max_name_bytes,
            limits.max_path_bytes,
            limits.max_kind_bytes,
            limits.max_guid_bytes,
            limits.max_container_source_path_bytes,
            limits.max_evidence_items,
            limits.max_total_candidate_bytes,
            limits.max_fuzzy_work_units,
        );
        let digest = DigestV1::hash_bytes(identity.as_bytes());
        QueryPolicyId::from_bytes(*digest.as_bytes())
    })
}

const fn match_kind_name(kind: MatchKind) -> &'static str {
    match kind {
        MatchKind::Exact => "exact",
        MatchKind::Prefix => "prefix",
        MatchKind::Token => "token",
        MatchKind::Substring => "substring",
        MatchKind::Abbreviation => "abbreviation",
        MatchKind::Fuzzy => "fuzzy",
        MatchKind::None => "none",
    }
}

#[must_use]
pub(crate) const fn generation_id(generation: SearchGenerationId) -> GenerationIdV1 {
    GenerationIdV1::new(generation.digest())
}

#[must_use]
pub(crate) fn generation_stamp(generation: &GenerationStamp) -> WireGenerationStamp {
    WireGenerationStamp {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        generation: generation_id(generation.generation),
        workspace: generation.workspace,
        actual_revision: generation.actual_revision,
        desired_revision: generation.desired_revision,
        semantics_current: generation.semantics_current,
        configuration_current: generation.configuration_current,
        stale: generation.stale,
    }
}

pub(crate) fn portable_path(path: &Path) -> Result<PortablePath, WireProjectionError> {
    PortablePath::try_from(path).map_err(Into::into)
}

pub(crate) fn portable_path_string(path: String) -> Result<PortablePath, WireProjectionError> {
    PortablePath::new(path).map_err(Into::into)
}

pub(crate) fn bounded_error_message(message: String) -> String {
    bounded_utf8(message, MAX_ERROR_MESSAGE_BYTES, "... [truncated]")
}

pub(crate) fn bounded_publish_warning(message: String) -> String {
    bounded_utf8(
        message,
        MAX_REINDEX_PUBLISH_WARNING_BYTES,
        "... [truncated]",
    )
}

fn bounded_utf8(mut value: String, maximum: usize, suffix: &str) -> String {
    if value.len() <= maximum {
        return value;
    }
    let maximum_prefix = maximum - suffix.len();
    let mut boundary = maximum_prefix;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(suffix);
    value
}

#[must_use]
pub(crate) fn locator_path(locator: &SourceLocator) -> String {
    let mut path = locator.root_alias().as_str().to_owned();
    for step in locator.members() {
        path.push_str("::");
        path.push_str(step.container().tag());
        path.push(':');
        path.push_str(step.member().name());
        let occurrence = step.member().same_name_occurrence();
        if occurrence != 0 {
            path.push('@');
            path.push_str(&occurrence.to_string());
        }
    }
    path
}

pub(crate) fn portable_locator(
    locator: &SourceLocator,
) -> Result<PortablePath, WireProjectionError> {
    portable_path_string(locator_path(locator))
}

pub(crate) fn location(
    path: String,
    guid: Option<String>,
    file_id: Option<i64>,
    class_id: Option<i32>,
) -> Result<Location, WireProjectionError> {
    Ok(Location {
        path: portable_path_string(path)?,
        guid,
        file_id,
        class_id,
    })
}

pub(crate) fn locator_location(
    locator: &SourceLocator,
    guid: Option<String>,
    file_id: Option<i64>,
    class_id: Option<i32>,
) -> Result<Location, WireProjectionError> {
    Ok(Location {
        path: portable_locator(locator)?,
        guid,
        file_id,
        class_id,
    })
}

pub(crate) fn fixed_u32(value: usize, field: &'static str) -> Result<u32, WireProjectionError> {
    u32::try_from(value).map_err(|_| WireProjectionError::NumericOverflow { field })
}

pub(crate) fn fixed_u64(value: usize, field: &'static str) -> Result<u64, WireProjectionError> {
    u64::try_from(value).map_err(|_| WireProjectionError::NumericOverflow { field })
}

pub(crate) fn fixed_millis(value: u128, field: &'static str) -> Result<u64, WireProjectionError> {
    u64::try_from(value).map_err(|_| WireProjectionError::NumericOverflow { field })
}

#[must_use]
pub(crate) fn reindex_receipt(output: &PipelineBuildOutput) -> ReindexReceipt {
    ReindexReceipt {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        disposition: match output.disposition {
            PipelineBuildDisposition::AlreadyApplied => ReindexDisposition::AlreadyApplied,
            PipelineBuildDisposition::Published | PipelineBuildDisposition::NoChange => {
                ReindexDisposition::Applied
            }
        },
        transaction: output.transaction,
        target_revision: output.target_revision,
        generation: output
            .active
            .as_ref()
            .map(|active| generation_stamp(active.stamp())),
        evidence: ReindexEvidence {
            forced_full_scan: output.metrics.forced_full_scan,
            forced_full_analysis: output.metrics.forced_full_analysis,
            full_dependency_scan: output.metrics.full_dependency_scan,
            dependency_candidate_assets: output.metrics.dependency_candidate_assets,
            dependency_closure_assets: output.metrics.dependency_closure_assets,
            analysis: ReindexAnalysisEvidence {
                assets_visited: output.metrics.analysis.assets_visited,
                assets_analyzed: output.metrics.analysis.assets_analyzed,
                source_opens: output.metrics.analysis.source_opens,
                source_bytes_read: output.metrics.analysis.source_bytes_read,
                text_sources: output.metrics.analysis.text_sources,
                text_bytes_scanned: output.metrics.analysis.text_bytes_scanned,
                yaml_documents: output.metrics.analysis.yaml_documents,
                binary_objects: output.metrics.analysis.binary_objects,
                unity_values_visited: output.metrics.analysis.unity_values_visited,
                references_emitted: output.metrics.analysis.references_emitted,
                container_entries_emitted: output.metrics.analysis.container_entries_emitted,
                truncations_emitted: output.metrics.analysis.truncations_emitted,
                diagnostics_emitted: output.metrics.analysis.diagnostics_emitted,
            },
            disk_estimate: output.disk_estimate.map(|estimate| ReindexDiskEstimate {
                existing_generation_bytes: estimate.existing_generation_bytes,
                old_active_generation_bytes: estimate.old_active_generation_bytes,
                new_generation_bytes: estimate.new_generation_bytes,
                publish_peak_bytes: estimate.publish_peak_bytes,
                retained_bytes_after_publish: estimate.retained_bytes_after_publish,
                reclaimable_bytes_after_publish: estimate.reclaimable_bytes_after_publish,
            }),
            publish_warnings: output.warnings.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_policy_id_is_stable_and_not_a_placeholder() {
        let policy = query_policy_id();

        assert_eq!(policy, query_policy_id());
        assert_ne!(policy.as_bytes(), &[0; 32]);
    }

    #[test]
    fn millisecond_projection_rejects_values_outside_the_wire_width() {
        let error = fixed_millis(u128::from(u64::MAX) + 1, "test duration").unwrap_err();

        assert_eq!(
            error,
            WireProjectionError::NumericOverflow {
                field: "test duration"
            }
        );
    }
}
