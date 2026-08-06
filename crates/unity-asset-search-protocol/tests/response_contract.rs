use unity_asset_core::{DigestV1, WorkspaceId, WorkspaceRevision};
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, DaemonInstanceId, FilesystemReindexIntent, FreshnessMaintenance,
    FuzzyWorkUsageV1, GenerationFreshness, GenerationIdV1, GenerationMaintenanceState,
    GenerationStamp, GenerationStatus, MAX_API_ERROR_JSON_BYTES, MAX_ERROR_MESSAGE_BYTES,
    MAX_REINDEX_PUBLISH_WARNING_BYTES, MAX_REINDEX_PUBLISH_WARNINGS, MAX_SEARCH_HITS_JSON_BYTES,
    MatchCountRelationV1, MatchCountV1, OperationId, PortablePath, ProjectId, QueryPolicyId,
    ReferenceCoverage, ReferenceDiagnosticCoverage, ReferenceRequest, ReferencesResponse,
    ReindexAdmitRequest, ReindexDisposition, ReindexEvidence, ReindexOperationState,
    ReindexOperationStatus, ReindexReceipt, RequestEnvelope, RequestId, RequestOperation,
    ResponseEnvelope, ResponseOperation, ResponseOutcome, SEARCH_PROTOCOL_REVISION,
    SearchCapabilities, SearchRequest, SearchResponse, ServingAvailability, ShutdownRequest,
    StatusResponse, SuggestRequest, SuggestResponse, TimerLifecycleState, ValidateContract,
    WatcherLifecycleState, encode_response_frame,
};

const GUID: &str = "0123456789abcdef0123456789abcdef";

fn query_policy(seed: u8) -> QueryPolicyId {
    QueryPolicyId::from_bytes([seed; 32])
}

fn generation(seed: u8) -> GenerationStamp {
    GenerationStamp::current(
        GenerationIdV1::new(DigestV1::from_bytes([seed; 32])),
        WorkspaceId::from_u128(1).unwrap(),
        WorkspaceRevision::new(DigestV1::from_bytes([seed.wrapping_add(1); 32])),
    )
}

fn request(operation: RequestOperation) -> RequestEnvelope {
    RequestEnvelope::new(
        SEARCH_PROTOCOL_REVISION,
        RequestId::from_bytes([1; 16]),
        ProjectId::from_bytes([2; 32]),
        DaemonInstanceId::from_bytes([3; 16]),
        query_policy(4),
        operation,
    )
    .unwrap()
}

fn search_response() -> SearchResponse {
    SearchResponse {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        generation: generation(5),
        query_policy_id: query_policy(4),
        query: "player".to_owned(),
        took_ms: 1,
        match_count: MatchCountV1 {
            value: 0,
            relation: MatchCountRelationV1::Exact,
        },
        returned_hits: 0,
        request_limit_truncated: false,
        fuzzy_work: FuzzyWorkUsageV1 {
            consumed: 0,
            limit: 0,
            exhausted: false,
        },
        hits: Vec::new(),
        diagnostics: Vec::new(),
        fallback_used: false,
    }
}

fn fixture_search_response() -> SearchResponse {
    let envelope: ResponseEnvelope = serde_json::from_str(include_str!(
        "../../../integration/search-protocol/fixtures/responses/search-v3.json"
    ))
    .unwrap();
    let ResponseOutcome::Success(operation) = envelope.into_outcome() else {
        panic!("search fixture must be successful");
    };
    let ResponseOperation::Search(mut response) = *operation else {
        panic!("search fixture must contain a search response");
    };
    response.query_policy_id = query_policy(4);
    response
}

fn fixture_references_response() -> ReferencesResponse {
    let envelope: ResponseEnvelope = serde_json::from_str(include_str!(
        "../../../integration/search-protocol/fixtures/responses/references-v3.json"
    ))
    .unwrap();
    let ResponseOutcome::Success(operation) = envelope.into_outcome() else {
        panic!("references fixture must be successful");
    };
    let ResponseOperation::References(mut response) = *operation else {
        panic!("references fixture must contain a references response");
    };
    response.query_policy_id = query_policy(4);
    response
}

fn status(active: GenerationStamp, policy: QueryPolicyId) -> StatusResponse {
    let generation = GenerationStatus {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        active: Some(active),
        building_revision: None,
        last_failure: None,
    };
    StatusResponse {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        daemon: unity_asset_search_protocol::DaemonLifecycleStatus::unmanaged(&generation, false),
        generation,
        query_policy_id: policy,
        capabilities: SearchCapabilities::current(),
        project_root: PortablePath::new("C:/projects/example").unwrap(),
        generation_root: PortablePath::new("C:/cache/example").unwrap(),
        scan_roots: vec![PortablePath::new("Assets").unwrap()],
        indexed_assets: 0,
        indexed_search_documents: 0,
        indexed_reference_facts: 0,
        incomplete_assets: 0,
        projection_truncations: 0,
        last_build_duration_ms: None,
        last_build_unix_ms: None,
        indexing: false,
    }
}

#[test]
fn search_response_validates_counts_versions_and_query_policy_binding() {
    let request = request(RequestOperation::Search(SearchRequest {
        query: "player".to_owned(),
        limit: 10,
    }));
    let response =
        ResponseEnvelope::success(&request, ResponseOperation::Search(search_response()));
    response.validate_for(&request).unwrap();

    let mut wrong_count = search_response();
    wrong_count.returned_hits = 1;
    assert!(wrong_count.validate().is_err());

    let mut wrong_policy = search_response();
    wrong_policy.query_policy_id = query_policy(9);
    let response = ResponseEnvelope::success(&request, ResponseOperation::Search(wrong_policy));
    assert!(response.validate_for(&request).is_err());
}

#[test]
fn successful_search_and_suggest_responses_are_bound_to_exact_requests() {
    let search = fixture_search_response();
    let matching = request(RequestOperation::Search(SearchRequest {
        query: search.query.clone(),
        limit: 1,
    }));
    ResponseEnvelope::success(&matching, ResponseOperation::Search(search.clone()))
        .validate_for(&matching)
        .unwrap();

    let wrong_query = request(RequestOperation::Search(SearchRequest {
        query: "different".to_owned(),
        limit: 1,
    }));
    assert!(
        ResponseEnvelope::success(&wrong_query, ResponseOperation::Search(search.clone()))
            .validate_for(&wrong_query)
            .is_err()
    );

    let zero_limit = request(RequestOperation::Search(SearchRequest {
        query: search.query.clone(),
        limit: 0,
    }));
    assert!(
        ResponseEnvelope::success(&zero_limit, ResponseOperation::Search(search))
            .validate_for(&zero_limit)
            .is_err()
    );

    let suggest = SuggestResponse {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        generation: generation(5),
        query_policy_id: query_policy(4),
        prefix: "pla".to_owned(),
        took_ms: 1,
        suggestions: vec!["player".to_owned(), "platform".to_owned()],
    };
    let limited = request(RequestOperation::Suggest(SuggestRequest {
        prefix: suggest.prefix.clone(),
        limit: 1,
    }));
    assert!(
        ResponseEnvelope::success(&limited, ResponseOperation::Suggest(suggest.clone()))
            .validate_for(&limited)
            .is_err()
    );

    let wrong_prefix = request(RequestOperation::Suggest(SuggestRequest {
        prefix: "other".to_owned(),
        limit: 2,
    }));
    assert!(
        ResponseEnvelope::success(&wrong_prefix, ResponseOperation::Suggest(suggest))
            .validate_for(&wrong_prefix)
            .is_err()
    );
}

#[test]
fn suggestions_are_individually_and_collectively_frame_bounded() {
    assert!(SuggestResponse::validate_suggestion("").is_err());
    assert!(SuggestResponse::validate_suggestion(&"x".repeat(32 * 1024 + 1)).is_err());

    let oversized_set = (0..8).map(|_| "x".repeat(30 * 1024)).collect::<Vec<_>>();
    assert!(SuggestResponse::validate_suggestions(&oversized_set).is_err());
}

#[test]
fn search_hits_are_bounded_by_their_canonical_json_representation() {
    let mut response = fixture_search_response();
    response.hits[0].name = "x".repeat(MAX_SEARCH_HITS_JSON_BYTES as usize);

    assert!(response.validate().is_err());
}

#[test]
fn search_hit_guids_are_canonical_at_every_location() {
    let mut response = fixture_search_response();
    response.hits[0].guid = Some("not-a-guid".to_owned());
    assert!(response.validate().is_err());

    let mut response = fixture_search_response();
    response.hits[0].location.guid = Some("A".repeat(32));
    assert!(response.validate().is_err());
}

#[test]
fn status_paths_are_collectively_frame_bounded() {
    let mut response = status(generation(5), query_policy(4));
    response.scan_roots = (0..8)
        .map(|ordinal| {
            PortablePath::new(format!("root-{ordinal}/{}", "x".repeat(30 * 1024))).unwrap()
        })
        .collect();

    assert!(response.validate().is_err());
}

#[test]
fn idle_status_cannot_claim_a_building_revision() {
    let mut response = status(generation(5), query_policy(4));
    response.generation.building_revision =
        Some(response.generation.active.as_ref().unwrap().actual_revision);

    assert!(response.validate().is_err());
}

#[test]
fn daemon_status_must_match_generation_availability_and_freshness() {
    let mut response = status(generation(5), query_policy(4));
    response.daemon.serving = ServingAvailability::Unavailable;
    assert!(response.validate().is_err());

    let mut response = status(generation(5), query_policy(4));
    response.daemon.freshness = GenerationFreshness::Stale;
    assert!(response.validate().is_err());
}

#[test]
fn daemon_status_requires_failure_evidence_and_consistent_maintenance() {
    let mut response = status(generation(5), query_policy(4));
    response.daemon.generation_maintenance.state = GenerationMaintenanceState::RecoveryRequired;
    assert!(response.validate().is_err());

    let mut response = status(generation(5), query_policy(4));
    response.daemon.generation_maintenance.last_cleanup_failure =
        Some("staging cleanup failed".to_owned());
    assert!(response.validate().is_err());

    let mut response = status(generation(5), query_policy(4));
    response.daemon.watcher.state = WatcherLifecycleState::Retrying;
    response.daemon.freshness_maintenance = FreshnessMaintenance::Managed;
    assert!(response.validate().is_err());

    let mut response = status(generation(5), query_policy(4));
    response.daemon.timer.state = TimerLifecycleState::Disabled;
    response.daemon.timer.next_run_in_ms = Some(1);
    assert!(response.validate().is_err());
}

#[test]
fn generation_failure_messages_share_the_api_error_bound() {
    let mut response = status(generation(5), query_policy(4));
    response.generation.last_failure = Some(unity_asset_search_protocol::GenerationFailure {
        code: "index_build_failed".to_owned(),
        message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
        retryable: false,
        failed_unix_ms: 1,
        desired_revision: None,
    });

    assert!(response.validate().is_err());
}

#[test]
fn reindex_publish_warnings_are_individually_and_collectively_bounded() {
    let evidence = ReindexEvidence {
        publish_warnings: vec!["warning".to_owned(); MAX_REINDEX_PUBLISH_WARNINGS + 1],
        ..ReindexEvidence::default()
    };
    assert!(evidence.validate().is_err());

    let evidence = ReindexEvidence {
        publish_warnings: vec!["x".repeat(MAX_REINDEX_PUBLISH_WARNING_BYTES + 1)],
        ..ReindexEvidence::default()
    };
    assert!(evidence.validate().is_err());

    let evidence = ReindexEvidence {
        publish_warnings: vec![
            "x".repeat(MAX_REINDEX_PUBLISH_WARNING_BYTES);
            MAX_REINDEX_PUBLISH_WARNINGS
        ],
        ..ReindexEvidence::default()
    };
    assert!(evidence.validate().is_err());
}

#[test]
fn reference_response_cursor_is_bound_to_generation_and_query_policy() {
    let reference_request = ReferenceRequest::incoming_guid(GUID, None, 10);
    let request = request(RequestOperation::References(reference_request.clone()));
    let mut response = ReferencesResponse {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        generation: generation(5),
        query_policy_id: query_policy(4),
        request: reference_request,
        took_ms: 1,
        coverage: ReferenceCoverage {
            complete: true,
            truncated: false,
            returned: 0,
            total: Some(0),
            next_cursor: None,
        },
        hits: Vec::new(),
        diagnostics: Vec::new(),
        diagnostic_coverage: ReferenceDiagnosticCoverage::default(),
    };
    ResponseEnvelope::success(&request, ResponseOperation::References(response.clone()))
        .validate_for(&request)
        .unwrap();

    response.coverage.returned = 1;
    assert!(response.validate().is_err());
}

#[test]
fn reference_coverage_separates_generation_completeness_from_page_truncation() {
    ReferenceCoverage {
        complete: true,
        truncated: true,
        returned: 1,
        total: Some(2),
        next_cursor: None,
    }
    .validate()
    .unwrap();

    for invalid in [
        ReferenceCoverage {
            complete: true,
            truncated: false,
            returned: 0,
            total: None,
            next_cursor: None,
        },
        ReferenceCoverage {
            complete: false,
            truncated: true,
            returned: 0,
            total: Some(0),
            next_cursor: None,
        },
        ReferenceCoverage {
            complete: false,
            truncated: false,
            returned: 0,
            total: None,
            next_cursor: None,
        },
    ] {
        assert!(invalid.validate().is_err());
    }
}

#[test]
fn reference_response_cannot_exceed_the_echoed_request_limit() {
    let reference_request = ReferenceRequest::outgoing_guid(GUID, Some(1_001), 1);
    let request = request(RequestOperation::References(reference_request.clone()));
    let mut response = fixture_references_response();
    response.request = reference_request;
    response.hits.push(response.hits[0].clone());
    response.coverage.returned = 2;
    response.coverage.total = Some(2);
    response.validate().unwrap();

    assert!(
        ResponseEnvelope::success(&request, ResponseOperation::References(response))
            .validate_for(&request)
            .is_err()
    );
}

#[test]
fn reference_diagnostic_coverage_uses_actual_canonical_json_bytes() {
    let mut response = fixture_references_response();
    response.diagnostic_coverage.serialized_bytes = 1;
    assert!(response.validate().is_err());
}

#[test]
fn succeeded_reindex_requires_matching_completion_and_status_generation() {
    let request = request(RequestOperation::ReindexAdmit(ReindexAdmitRequest {
        intent: FilesystemReindexIntent::full(),
        idempotency_key: None,
    }));
    let active = generation(5);
    let completion = ReindexReceipt {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        disposition: ReindexDisposition::Applied,
        transaction: None,
        target_revision: Some(active.actual_revision),
        generation: Some(active.clone()),
        evidence: ReindexEvidence::default(),
    };
    let completed_status = status(active.clone(), query_policy(4));
    let valid = ReindexOperationStatus {
        operation_id: OperationId::from_bytes([7; 16]),
        state: ReindexOperationState::Succeeded,
        admission: None,
        completion: Some(completion.clone()),
        status: Some(completed_status),
        error: None,
    };
    ResponseEnvelope::success(&request, ResponseOperation::ReindexAdmit(valid.clone()))
        .validate_for(&request)
        .unwrap();

    let mismatched = ReindexOperationStatus {
        completion: Some(completion),
        status: Some(status(generation(9), query_policy(4))),
        ..ReindexOperationStatus {
            operation_id: OperationId::from_bytes([7; 16]),
            state: ReindexOperationState::Succeeded,
            admission: None,
            completion: None,
            status: None,
            error: None,
        }
    };
    assert!(mismatched.validate().is_err());

    let mut queued_completion = valid.clone();
    queued_completion.completion.as_mut().unwrap().disposition = ReindexDisposition::Queued;
    assert!(queued_completion.validate().is_err());

    let mut still_indexing = valid.clone();
    still_indexing.status.as_mut().unwrap().indexing = true;
    assert!(still_indexing.validate().is_err());

    let mut still_building = valid;
    still_building
        .status
        .as_mut()
        .unwrap()
        .generation
        .building_revision = Some(active.actual_revision);
    assert!(still_building.validate().is_err());
}

#[test]
fn reindex_receipt_target_can_precede_the_global_desired_revision() {
    let target = generation(5);
    let later_desired = generation(9).actual_revision;
    ReindexReceipt {
        protocol_revision: SEARCH_PROTOCOL_REVISION,
        disposition: ReindexDisposition::AlreadyApplied,
        transaction: None,
        target_revision: Some(target.actual_revision),
        generation: Some(target.with_desired_revision(later_desired)),
        evidence: ReindexEvidence::default(),
    }
    .validate()
    .unwrap();
}

#[test]
fn structured_errors_validate_protocol_and_query_policy_binding() {
    let request = request(RequestOperation::Search(SearchRequest {
        query: "player".to_owned(),
        limit: 10,
    }));
    let mut error = ApiError::new(ApiErrorCode::StaleCursor, "cursor is stale", false);
    error.query_policy_id = Some(query_policy(9));
    let response = ResponseEnvelope::error(&request, error);
    assert!(response.validate_for(&request).is_err());
}

#[test]
fn maximum_error_message_fits_every_response_frame() {
    let request = request(RequestOperation::Shutdown(ShutdownRequest {
        drain_timeout_ms: 0,
    }));
    let error = ApiError::new(
        ApiErrorCode::Internal,
        "x".repeat(MAX_ERROR_MESSAGE_BYTES),
        false,
    );
    let response = ResponseEnvelope::error(&request, error);

    assert!(encode_response_frame(&response, &request).is_ok());
}

#[test]
fn api_error_has_a_collective_json_budget() {
    let mut error = ApiError::new(ApiErrorCode::Internal, "failure", false);
    for ordinal in 0..64 {
        error
            .details
            .insert(format!("detail-{ordinal:02}"), "\u{0001}".repeat(4 * 1024));
    }

    assert!(serde_json::to_vec(&error).unwrap().len() as u64 > MAX_API_ERROR_JSON_BYTES);
    assert!(error.validate().is_err());
}

#[test]
fn required_empty_collections_cannot_disappear_from_the_wire_shape() {
    let mut search = serde_json::to_value(search_response()).unwrap();
    search.as_object_mut().unwrap().remove("diagnostics");
    assert!(serde_json::from_value::<SearchResponse>(search).is_err());

    let error = ApiError::new(ApiErrorCode::NotReady, "booting", true);
    let mut error = serde_json::to_value(error).unwrap();
    error.as_object_mut().unwrap().remove("details");
    assert!(serde_json::from_value::<ApiError>(error).is_err());
}
