mod support;

use std::fs;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use support::{SearchDaemonFixture, TEST_TIMEOUT};
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    DiscoveredLoopbackEndpoint, EndpointNamespaceV1, EndpointStoreError,
};
use unity_asset_search_protocol::{
    ApiErrorCode, BackgroundReindexOrigin, CapabilitiesRequest, DaemonLifecycleState,
    FilesystemReindexIntent, OperationId, ReferenceRequest, ReindexAdmitRequest,
    ReindexCancelRequest, ReindexOperationState, ReindexStatusRequest, ReindexWaitRequest,
    RequestEnvelope, RequestId, RequestOperation, ResponseOperation, ResponseOutcome,
    SearchCapabilities, SearchRequest, ShutdownRequest, StatusRequest, SuggestRequest,
    decode_response_json, encode_request_json, max_response_json_bytes,
};

const OWNER_ONE_GUID: &str = "11111111111111111111111111111111";
const OWNER_TWO_GUID: &str = "22222222222222222222222222222222";
const TARGET_GUID: &str = "0123456789abcdef0123456789abcdef";
const TARGET_TWO_GUID: &str = "fedcba9876543210fedcba9876543210";
const OWNER_ONE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: AgentBeacon
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
  m_SecondaryTarget: {fileID: 200, guid: fedcba9876543210fedcba9876543210, type: 3}
"#;
const OWNER_ONE_UPDATED: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: AgentBeaconUpdated
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
  m_SecondaryTarget: {fileID: 200, guid: fedcba9876543210fedcba9876543210, type: 3}
"#;
const OWNER_TWO: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &2
GameObject:
  m_Name: AgentBeaconSibling
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
"#;
const TARGET: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: TargetBeacon
"#;
const TARGET_TWO: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &200
GameObject:
  m_Name: SecondaryTargetBeacon
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reindex_is_discoverable_observable_and_not_client_cancelable() {
    let fixture = SearchDaemonFixture::new();
    fixture.write_asset("Startup.prefab", TARGET, TARGET_GUID);
    let namespace = fixture.namespace();
    let mut daemon = fixture.spawn_daemon(true);
    let discovered = fixture.wait_for_endpoint(&mut daemon).await;
    let mut client = ProcessClient::new(namespace.clone(), discovered);

    let status = client
        .exchange(RequestOperation::Status(StatusRequest::default()))
        .await;
    let ResponseOperation::Status(status) = status else {
        panic!("real daemon returned a non-status response");
    };
    let startup = status
        .daemon
        .background_reindex_operations
        .iter()
        .find(|operation| operation.origin == BackgroundReindexOrigin::Startup)
        .copied()
        .expect("startup operation remains discoverable after endpoint publication");

    let observed = client
        .exchange(RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id: startup.operation_id,
        }))
        .await;
    let ResponseOperation::ReindexStatus(observed) = observed else {
        panic!("real daemon returned a non-reindex-status response");
    };
    assert_eq!(observed.operation_id, startup.operation_id);
    assert_ne!(observed.state, ReindexOperationState::Lost);

    let completed = client
        .exchange(RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id: startup.operation_id,
            timeout_ms: 20_000,
        }))
        .await;
    let ResponseOperation::ReindexWait(completed) = completed else {
        panic!("real daemon returned a non-reindex-wait response");
    };
    assert_eq!(completed.state, ReindexOperationState::Succeeded);

    let cancellation = client
        .exchange_outcome(RequestOperation::ReindexCancel(ReindexCancelRequest {
            operation_id: startup.operation_id,
        }))
        .await;
    let ResponseOutcome::Error(cancellation) = cancellation else {
        panic!("daemon-owned startup operation was client-cancelable");
    };
    assert_eq!(cancellation.code, ApiErrorCode::OperationControlForbidden);
    assert!(!cancellation.retryable);
    assert_eq!(
        cancellation.details.get("origin").map(String::as_str),
        Some("startup")
    );

    shutdown(&mut client).await;
    drop(client);
    assert!(daemon.wait_for_exit().await.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_daemon_process_exercises_every_operation_and_rejects_stale_state() {
    let fixture = SearchDaemonFixture::new();
    fixture.write_asset("OwnerOne.prefab", OWNER_ONE, OWNER_ONE_GUID);
    fixture.write_asset("OwnerTwo.prefab", OWNER_TWO, OWNER_TWO_GUID);
    fixture.write_asset("Target.prefab", TARGET, TARGET_GUID);
    fixture.write_asset("TargetTwo.prefab", TARGET_TWO, TARGET_TWO_GUID);

    let namespace = fixture.namespace();
    let mut daemon = fixture.spawn_daemon(false);
    let discovered = fixture.wait_for_endpoint(&mut daemon).await;
    let stale_discovered = discovered.clone();
    let original_daemon_instance_id = discovered.descriptor().daemon_instance_id();
    let mut client = ProcessClient::new(namespace.clone(), discovered);

    let capabilities = client
        .exchange(RequestOperation::Capabilities(
            CapabilitiesRequest::default(),
        ))
        .await;
    let ResponseOperation::Capabilities(capabilities) = capabilities else {
        panic!("real daemon returned a non-capabilities response");
    };
    assert_eq!(capabilities.capabilities, SearchCapabilities::current());

    let initial = client
        .exchange(RequestOperation::Status(StatusRequest::default()))
        .await;
    let ResponseOperation::Status(initial) = initial else {
        panic!("real daemon returned a non-status response");
    };
    assert_eq!(initial.daemon.lifecycle, DaemonLifecycleState::Serving);
    assert!(!initial.indexing);

    complete_reindex(&mut client, "process-contract-v1").await;

    let search = client
        .exchange(RequestOperation::Search(SearchRequest {
            query: "AgentBeacon".to_owned(),
            limit: 10,
        }))
        .await;
    let ResponseOperation::Search(search) = search else {
        panic!("real daemon returned a non-search response");
    };
    assert!(
        search.hits.iter().any(|hit| {
            hit.name == "AgentBeacon" && hit.path.as_str() == "Assets/OwnerOne.prefab"
        }),
        "real daemon search did not return the indexed prefab: {:?}",
        search.hits
    );

    let suggest = client
        .exchange(RequestOperation::Suggest(SuggestRequest {
            prefix: "in:Assets/".to_owned(),
            limit: 10,
        }))
        .await;
    let ResponseOperation::Suggest(suggest) = suggest else {
        panic!("real daemon returned a non-suggest response");
    };
    assert_eq!(suggest.suggestions, ["in:Assets/"]);

    let incoming = ReferenceRequest::incoming_guid(TARGET_GUID, Some(100), 1);
    let first_page = client
        .exchange(RequestOperation::References(incoming.clone()))
        .await;
    let ResponseOperation::References(first_page) = first_page else {
        panic!("real daemon returned a non-reference response");
    };
    assert_eq!(first_page.coverage.returned, 1);
    assert_eq!(first_page.coverage.total, Some(2));
    assert!(first_page.coverage.truncated);
    let stale_cursor = first_page
        .coverage
        .next_cursor
        .clone()
        .expect("first reference page has a continuation cursor");

    let second_page = client
        .exchange(RequestOperation::References(
            incoming.clone().with_cursor(stale_cursor.clone()),
        ))
        .await;
    let ResponseOperation::References(second_page) = second_page else {
        panic!("real daemon returned a non-reference response");
    };
    assert_eq!(second_page.coverage.returned, 1);
    assert!(!second_page.coverage.truncated);
    assert!(second_page.coverage.next_cursor.is_none());
    let mut incoming_sources = first_page
        .hits
        .iter()
        .chain(&second_page.hits)
        .map(|hit| hit.source_path.as_str())
        .collect::<Vec<_>>();
    incoming_sources.sort_unstable();
    assert_eq!(
        incoming_sources,
        ["Assets/OwnerOne.prefab", "Assets/OwnerTwo.prefab"]
    );

    let outgoing = ReferenceRequest::outgoing_guid(OWNER_ONE_GUID, Some(1), 1);
    let first_outgoing_page = client
        .exchange(RequestOperation::References(outgoing.clone()))
        .await;
    let ResponseOperation::References(first_outgoing_page) = first_outgoing_page else {
        panic!("real daemon returned a non-reference response");
    };
    assert_eq!(first_outgoing_page.coverage.returned, 1);
    assert_eq!(first_outgoing_page.coverage.total, Some(2));
    assert!(first_outgoing_page.coverage.truncated);
    let outgoing_cursor = first_outgoing_page
        .coverage
        .next_cursor
        .clone()
        .expect("first outgoing reference page has a continuation cursor");
    let second_outgoing_page = client
        .exchange(RequestOperation::References(
            outgoing.with_cursor(outgoing_cursor),
        ))
        .await;
    let ResponseOperation::References(second_outgoing_page) = second_outgoing_page else {
        panic!("real daemon returned a non-reference response");
    };
    assert_eq!(second_outgoing_page.coverage.returned, 1);
    assert!(!second_outgoing_page.coverage.truncated);
    assert!(second_outgoing_page.coverage.next_cursor.is_none());
    let mut outgoing_targets = first_outgoing_page
        .hits
        .iter()
        .chain(&second_outgoing_page.hits)
        .filter_map(|hit| {
            assert_eq!(hit.source_path.as_str(), "Assets/OwnerOne.prefab");
            hit.objects
                .iter()
                .find(|object| {
                    object
                        .field_hints
                        .iter()
                        .any(|hint| hint.starts_with("raw.yaml.file_id="))
                })
                .and_then(|object| object.location.guid.as_deref().zip(object.location.file_id))
        })
        .collect::<Vec<_>>();
    outgoing_targets.sort_unstable();
    assert_eq!(
        outgoing_targets,
        [(TARGET_GUID, 100), (TARGET_TWO_GUID, 200)]
    );

    fs::write(fixture.assets().join("OwnerOne.prefab"), OWNER_ONE_UPDATED)
        .expect("update owner prefab");
    complete_reindex(&mut client, "process-contract-v2").await;
    let stale = client
        .exchange_outcome(RequestOperation::References(
            incoming.with_cursor(stale_cursor),
        ))
        .await;
    let ResponseOutcome::Error(stale) = stale else {
        panic!("old reference cursor was accepted after generation replacement");
    };
    assert_eq!(stale.code, ApiErrorCode::StaleCursor);

    fs::write(fixture.assets().join("OwnerOne.prefab"), OWNER_ONE)
        .expect("prepare a reindex that outlives its admitting connection");
    let disconnected_operation = admit_reindex(&mut client, "process-contract-disconnect").await;
    drop(client);

    let rediscovered = namespace
        .discover_loopback_endpoint()
        .expect("rediscover the still-running daemon");
    let mut client = ProcessClient::new(namespace.clone(), rediscovered);
    let retried_operation = admit_reindex(&mut client, "process-contract-disconnect").await;
    assert_eq!(retried_operation, disconnected_operation);
    let status = client
        .exchange(RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id: disconnected_operation,
        }))
        .await;
    let ResponseOperation::ReindexStatus(status) = status else {
        panic!("reconnected client received a non-reindex-status response");
    };
    assert_eq!(status.operation_id, disconnected_operation);
    assert_ne!(status.state, ReindexOperationState::Lost);
    assert!(status.admission.is_some());
    let completed = client
        .exchange(RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id: disconnected_operation,
            timeout_ms: 20_000,
        }))
        .await;
    let ResponseOperation::ReindexWait(completed) = completed else {
        panic!("reconnected client received a non-reindex-wait response");
    };
    assert_eq!(completed.state, ReindexOperationState::Succeeded);

    shutdown(&mut client).await;
    drop(client);

    let status = daemon.wait_for_exit().await;
    assert!(
        status.success(),
        "real daemon exited unsuccessfully: {status}; stderr: {}",
        daemon.stderr()
    );
    assert!(matches!(
        namespace.discover_loopback_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));

    let mut replacement = fixture.spawn_daemon(false);
    let replacement_discovered = fixture.wait_for_endpoint(&mut replacement).await;
    assert_ne!(
        replacement_discovered.descriptor().daemon_instance_id(),
        original_daemon_instance_id
    );
    assert!(matches!(
        stale_discovered.ensure_unchanged(namespace),
        Err(EndpointStoreError::EndpointChanged)
    ));
    let mut replacement_client = ProcessClient::new(namespace.clone(), replacement_discovered);
    let lost = replacement_client
        .exchange(RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id: disconnected_operation,
        }))
        .await;
    let ResponseOperation::ReindexStatus(lost) = lost else {
        panic!("replacement daemon returned a non-reindex-status response");
    };
    assert_eq!(lost.operation_id, disconnected_operation);
    assert_eq!(lost.state, ReindexOperationState::Lost);
    assert!(lost.admission.is_none());
    assert!(lost.completion.is_none());
    assert!(lost.status.is_none());
    assert!(lost.error.is_none());
    shutdown(&mut replacement_client).await;
    drop(replacement_client);
    assert!(replacement.wait_for_exit().await.success());
    assert!(matches!(
        namespace.discover_loopback_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));
}

async fn complete_reindex(client: &mut ProcessClient, idempotency_key: &str) {
    let operation_id = admit_reindex(client, idempotency_key).await;
    let status = client
        .exchange(RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id,
        }))
        .await;
    let ResponseOperation::ReindexStatus(status) = status else {
        panic!("real daemon returned a non-reindex-status response");
    };
    assert_eq!(status.operation_id, operation_id);
    assert!(matches!(
        status.state,
        ReindexOperationState::Queued
            | ReindexOperationState::Coalesced
            | ReindexOperationState::Running
            | ReindexOperationState::Succeeded
    ));
    assert!(status.admission.is_some());

    let completed = client
        .exchange(RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id,
            timeout_ms: 20_000,
        }))
        .await;
    let ResponseOperation::ReindexWait(completed) = completed else {
        panic!("real daemon returned a non-reindex-wait response");
    };
    assert_eq!(completed.state, ReindexOperationState::Succeeded);
    assert!(completed.completion.is_some());
    assert!(completed.status.is_some());

    let cancelled = client
        .exchange(RequestOperation::ReindexCancel(ReindexCancelRequest {
            operation_id,
        }))
        .await;
    let ResponseOperation::ReindexCancel(cancelled) = cancelled else {
        panic!("real daemon returned a non-reindex-cancel response");
    };
    assert_eq!(cancelled.state, ReindexOperationState::Succeeded);
    assert!(!cancelled.cancelled);
}

async fn admit_reindex(client: &mut ProcessClient, idempotency_key: &str) -> OperationId {
    let admitted = client
        .exchange(RequestOperation::ReindexAdmit(ReindexAdmitRequest {
            intent: FilesystemReindexIntent::full(),
            idempotency_key: Some(idempotency_key.to_owned()),
        }))
        .await;
    let ResponseOperation::ReindexAdmit(admitted) = admitted else {
        panic!("real daemon returned a non-reindex-admission response");
    };
    admitted.operation_id
}

async fn shutdown(client: &mut ProcessClient) {
    let shutdown = client
        .exchange(RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 5_000,
        }))
        .await;
    let ResponseOperation::Shutdown(shutdown) = shutdown else {
        panic!("real daemon returned a non-shutdown response");
    };
    assert!(shutdown.accepted);
}

struct ProcessClient {
    http: reqwest::Client,
    namespace: EndpointNamespaceV1,
    discovered: DiscoveredLoopbackEndpoint,
    next_request_id: u8,
}

impl ProcessClient {
    fn new(namespace: EndpointNamespaceV1, discovered: DiscoveredLoopbackEndpoint) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(TEST_TIMEOUT)
            .timeout(TEST_TIMEOUT)
            .http1_only()
            .pool_max_idle_per_host(0)
            .build()
            .expect("build process-contract HTTP client");
        Self {
            http,
            namespace,
            discovered,
            next_request_id: 1,
        }
    }

    async fn exchange(&mut self, operation: RequestOperation) -> ResponseOperation {
        match self.exchange_outcome(operation).await {
            ResponseOutcome::Success(operation) => *operation,
            ResponseOutcome::Error(error) => panic!("real daemon returned an API error: {error:?}"),
        }
    }

    async fn exchange_outcome(&mut self, operation: RequestOperation) -> ResponseOutcome {
        let operation_kind = operation.kind();
        let request_id = RequestId::from_bytes([self.next_request_id; 16]);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("request ID space");
        let descriptor = self.discovered.descriptor();
        let request = RequestEnvelope::new(
            descriptor.business_protocol_revision(),
            request_id,
            descriptor.project_id(),
            descriptor.daemon_instance_id(),
            descriptor.query_policy_id(),
            operation,
        )
        .expect("valid process-contract request");
        let encoded = encode_request_json(&request).expect("encode business request");
        let capability = descriptor.capability().encode_hex();
        let capability = std::str::from_utf8(&capability).expect("hex capability is UTF-8");
        let response = self
            .http
            .post(format!("http://{}/v1/request", descriptor.socket_addr()))
            .header(HOST, descriptor.socket_addr().to_string())
            .header(AUTHORIZATION, format!("Bearer {capability}"))
            .header(CONTENT_TYPE, "application/json")
            .body(encoded)
            .send()
            .await
            .expect("exchange request with real daemon");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let maximum = max_response_json_bytes(operation_kind);
        if let Some(length) = response.content_length() {
            assert!(length <= maximum as u64, "response exceeds protocol limit");
        }
        let mut response = response;
        let mut response_json = Vec::new();
        while let Some(chunk) = response.chunk().await.expect("read HTTP response body") {
            let requested = response_json
                .len()
                .checked_add(chunk.len())
                .expect("response length fits usize");
            assert!(requested <= maximum, "response exceeds protocol limit");
            response_json.extend_from_slice(&chunk);
        }
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_json(&response_json, &mut budget, &request)
            .expect("decode business response");
        if operation_kind != unity_asset_search_protocol::OperationKind::Shutdown {
            self.discovered
                .ensure_unchanged(&self.namespace)
                .expect("endpoint remains unchanged after HTTP exchange");
        }
        response.into_outcome()
    }
}
