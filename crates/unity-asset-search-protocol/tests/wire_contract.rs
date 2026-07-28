use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use unity_asset_search_index::{
    ApiError, ApiErrorCode, GenerationStatus, ReferenceRequest, ReferencesResponse,
    ReindexDisposition, ReindexEvidence, ReindexReceipt, SEARCH_GENERATION_CONTRACT_VERSION,
    SearchCapabilities, SearchResponse, StatusResponse, SuggestResponse,
};
use unity_asset_search_protocol::{
    HEALTH_ENDPOINT, HTTP_API_PREFIX, HTTP_CONTRACT_VERSION, HealthResponse, REFERENCES_ENDPOINT,
    REINDEX_ENDPOINT, ReindexResponse, SEARCH_ENDPOINT, STATUS_ENDPOINT, SUGGEST_ENDPOINT,
    TOKEN_ROTATE_ENDPOINT, ValidateContractVersion,
};

const DIGEST: &str = "blake3-v1:1111111111111111111111111111111111111111111111111111111111111111";
const WORKSPACE: &str = "workspace-v1:00000000000000000000000000000001";

#[test]
fn endpoint_paths_are_one_complete_v2_contract() {
    assert_eq!(HTTP_API_PREFIX, "/v2");
    assert_eq!(
        [
            HEALTH_ENDPOINT,
            STATUS_ENDPOINT,
            SEARCH_ENDPOINT,
            SUGGEST_ENDPOINT,
            REFERENCES_ENDPOINT,
            REINDEX_ENDPOINT,
            TOKEN_ROTATE_ENDPOINT,
        ],
        [
            "/v2/health",
            "/v2/status",
            "/v2/search",
            "/v2/suggest",
            "/v2/references",
            "/v2/reindex",
            "/v2/token/rotate",
        ]
    );
}

#[test]
fn health_response_has_a_strict_golden_wire_shape() {
    let response = HealthResponse::healthy("0.3.0");
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        json!({
            "contract_version": 2,
            "ok": true,
            "version": "0.3.0",
        })
    );
    response.validate_contract_version().unwrap();

    let mut unknown = serde_json::to_value(&response).unwrap();
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<HealthResponse>(unknown).is_err());

    let mut unsupported = response;
    unsupported.contract_version = HTTP_CONTRACT_VERSION + 1;
    let error = unsupported.validate_contract_version().unwrap_err();
    assert_eq!(error.contract(), "health response");
    assert_eq!(error.actual(), HTTP_CONTRACT_VERSION + 1);
    assert_eq!(error.expected(), HTTP_CONTRACT_VERSION);
}

#[test]
fn reindex_envelopes_have_strict_golden_wire_shapes() {
    let admission = receipt(ReindexDisposition::Queued);
    let accepted = ReindexResponse::accepted(admission.clone());
    assert_eq!(
        serde_json::to_value(&accepted).unwrap(),
        json!({
            "contract_version": 2,
            "admission": serde_json::to_value(&admission).unwrap(),
        })
    );
    accepted.validate_contract_version().unwrap();

    let completion = receipt(ReindexDisposition::Applied);
    let status = status();
    let waited = ReindexResponse::waited(admission.clone(), completion.clone(), status.clone());
    assert_eq!(
        serde_json::to_value(&waited).unwrap(),
        json!({
            "contract_version": 2,
            "admission": serde_json::to_value(&admission).unwrap(),
            "completion": serde_json::to_value(&completion).unwrap(),
            "status": serde_json::to_value(&status).unwrap(),
        })
    );
    waited.validate_contract_version().unwrap();

    let mut unknown = serde_json::to_value(&accepted).unwrap();
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<ReindexResponse>(unknown).is_err());
}

#[test]
fn reindex_validation_rejects_every_nested_version_mismatch() {
    let mut response = waited_response();
    response.contract_version += 1;
    assert_contract_error(
        &response,
        "reindex response",
        HTTP_CONTRACT_VERSION + 1,
        HTTP_CONTRACT_VERSION,
    );

    let mut response = waited_response();
    response.admission.contract_version += 1;
    assert_contract_error(
        &response,
        "reindex response admission",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = waited_response();
    response.completion.as_mut().unwrap().contract_version += 1;
    assert_contract_error(
        &response,
        "reindex response completion",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = ReindexResponse::waited(
        receipt(ReindexDisposition::Queued),
        receipt_with_generation(ReindexDisposition::Applied),
        status(),
    );
    response
        .completion
        .as_mut()
        .unwrap()
        .generation
        .as_mut()
        .unwrap()
        .contract_version += 1;
    assert_contract_error(
        &response,
        "reindex receipt generation",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = waited_response();
    response.status.as_mut().unwrap().contract_version += 1;
    assert_contract_error(
        &response,
        "reindex response status",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = waited_response();
    response
        .status
        .as_mut()
        .unwrap()
        .generation
        .contract_version += 1;
    assert_contract_error(
        &response,
        "status response generation status",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = ReindexResponse::waited(
        receipt(ReindexDisposition::Queued),
        receipt(ReindexDisposition::Applied),
        status_with_active_generation(),
    );
    response
        .status
        .as_mut()
        .unwrap()
        .generation
        .active
        .as_mut()
        .unwrap()
        .contract_version += 1;
    assert_contract_error(
        &response,
        "status response active generation",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut response = waited_response();
    response
        .status
        .as_mut()
        .unwrap()
        .capabilities
        .contract_version += 1;
    assert_contract_error(
        &response,
        "status response capabilities",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );
}

#[test]
fn every_shared_search_index_response_rejects_an_unknown_generation_version() {
    let mut search: SearchResponse = response_fixture(json!({
        "contract_version": 1,
        "generation": generation(),
        "query": "",
        "took_ms": 0,
        "match_count": { "value": 0, "relation": "exact" },
        "returned_hits": 0,
        "request_limit_truncated": false,
        "fuzzy_work": { "consumed": 0, "limit": 0, "exhausted": false },
        "hits": [],
        "diagnostics": [],
        "fallback_used": false,
    }));
    search.contract_version += 1;
    assert_contract_error(
        &search,
        "search response",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut status = status();
    status.generation.contract_version += 1;
    assert_contract_error(
        &status,
        "status response generation status",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut suggest: SuggestResponse = response_fixture(json!({
        "contract_version": 1,
        "generation": generation(),
        "prefix": "",
        "took_ms": 0,
        "suggestions": [],
    }));
    suggest.generation.contract_version += 1;
    assert_contract_error(
        &suggest,
        "suggest response generation",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut references: ReferencesResponse = response_fixture(json!({
        "contract_version": 1,
        "generation": generation(),
        "request": serde_json::to_value(ReferenceRequest::incoming_guid(
            "11112222333344445555666677778888",
            Some(-11_500_000),
            8,
        ))
        .unwrap(),
        "took_ms": 0,
        "coverage": {
            "complete": true,
            "truncated": false,
            "returned": 0,
            "total": 0,
        },
        "hits": [],
        "diagnostics": [],
    }));
    references.request.contract_version += 1;
    assert_contract_error(
        &references,
        "references response request",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut api_error = ApiError::new(ApiErrorCode::InvalidRequest, "invalid request", false);
    api_error.contract_version += 1;
    assert_contract_error(
        &api_error,
        "API error",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );

    let mut receipt = receipt(ReindexDisposition::Applied);
    receipt.contract_version += 1;
    assert_contract_error(
        &receipt,
        "reindex receipt",
        SEARCH_GENERATION_CONTRACT_VERSION + 1,
        SEARCH_GENERATION_CONTRACT_VERSION,
    );
}

fn receipt(disposition: ReindexDisposition) -> ReindexReceipt {
    ReindexReceipt {
        contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
        disposition,
        transaction: None,
        target_revision: None,
        generation: None,
        evidence: ReindexEvidence::default(),
    }
}

fn receipt_with_generation(disposition: ReindexDisposition) -> ReindexReceipt {
    let mut value = serde_json::to_value(receipt(disposition)).unwrap();
    value["generation"] = generation();
    response_fixture(value)
}

fn waited_response() -> ReindexResponse {
    ReindexResponse::waited(
        receipt(ReindexDisposition::Queued),
        receipt(ReindexDisposition::Applied),
        status(),
    )
}

fn status() -> StatusResponse {
    StatusResponse {
        contract_version: SEARCH_GENERATION_CONTRACT_VERSION,
        generation: GenerationStatus::default(),
        capabilities: SearchCapabilities::current(),
        project_root: PathBuf::from("project"),
        generation_root: PathBuf::from("index"),
        scan_roots: vec![PathBuf::from("Assets")],
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

fn status_with_active_generation() -> StatusResponse {
    let mut value = serde_json::to_value(status()).unwrap();
    value["generation"]["active"] = generation();
    response_fixture(value)
}

fn generation() -> Value {
    json!({
        "contract_version": 1,
        "generation": DIGEST,
        "workspace": WORKSPACE,
        "actual_revision": DIGEST,
        "desired_revision": DIGEST,
        "stale": false,
    })
}

fn response_fixture<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).unwrap()
}

fn assert_contract_error(
    value: &impl ValidateContractVersion,
    contract: &str,
    actual: u16,
    expected: u16,
) {
    let error = value.validate_contract_version().unwrap_err();
    assert_eq!(error.contract(), contract);
    assert_eq!(error.actual(), actual);
    assert_eq!(error.expected(), expected);
}
