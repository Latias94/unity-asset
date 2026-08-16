#[path = "../../unity-asset-search-daemon/tests/support/mod.rs"]
mod support;

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use support::{SearchDaemonFixture, TEST_TIMEOUT};
use unity_asset_search_local::EndpointStoreError;
use unity_asset_search_protocol::{
    DaemonLifecycleState, FilesystemReindexIntent, FreshnessMaintenance, GenerationFreshness,
    GenerationMaintenanceState, GenerationStamp, ReconcileLifecycle, ReferenceRequest,
    ReferencesResponse, ReindexAdmitRequest, ReindexOperationState, ReindexStatusRequest,
    ReindexWaitRequest, RequestOperation, ResponseOperation, SearchCapabilities,
    ServingAvailability, ShutdownRequest, StatusResponse,
};

const CLI_CONTRACT_VERSION: u16 = 2;
const DAEMON_BINARY_ENV: &str = "UNITY_ASSET_SEARCH_DAEMON";
const DAEMON_BUILD_IDENTITY_ENV: &str = "UNITY_ASSET_SEARCH_DAEMON_BUILD_IDENTITY";
const DAEMON_BINARY_NAME: &str = "unity-asset-search-daemon";
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
#[ignore = "run scripts/run_real_daemon_agent.py for the explicit cross-package harness"]
async fn json_agent_drives_a_real_daemon_across_generation_and_process_replacement() {
    let daemon_build_identity = assert_runner_daemon_identity();
    let fixture = SearchDaemonFixture::new();
    fixture.write_asset("OwnerOne.prefab", OWNER_ONE, OWNER_ONE_GUID);
    fixture.write_asset("OwnerTwo.prefab", OWNER_TWO, OWNER_TWO_GUID);
    fixture.write_asset("Target.prefab", TARGET, TARGET_GUID);
    fixture.write_asset("TargetTwo.prefab", TARGET_TWO, TARGET_TWO_GUID);

    let project_id = fixture.namespace().project_id().to_string();
    let namespace = fixture.namespace();
    let mut daemon = fixture.spawn_daemon(false);
    let discovered = fixture.wait_for_endpoint(&mut daemon).await;
    let original_instance = discovered.descriptor().daemon_instance_id();

    let (capabilities_document, capabilities) =
        successful_operation(run_convenience_capabilities(&fixture));
    let ResponseOperation::Capabilities(capabilities) = capabilities else {
        panic!("JSON agent received a non-capabilities response");
    };
    assert_eq!(capabilities.daemon_version, daemon_build_identity);
    assert_eq!(capabilities.capabilities, SearchCapabilities::current());
    assert_eq!(capabilities_document["project_id"], project_id);
    assert_eq!(
        capabilities_document["daemon_instance_id"],
        original_instance.to_string()
    );

    let (_, status) = successful_operation(run_agent_request(
        &fixture,
        json!({
            "kind": "status",
            "request": {},
        }),
    ));
    let ResponseOperation::Status(status) = status else {
        panic!("JSON agent received a non-status response");
    };
    assert_absent_status(&status);

    complete_reindex(&fixture, "json-agent-v1");

    let (_, search) = successful_operation(run_agent_request(
        &fixture,
        json!({
            "kind": "search",
            "request": {
                "query": "AgentBeacon",
                "limit": 10,
            },
        }),
    ));
    let ResponseOperation::Search(search) = search else {
        panic!("JSON agent received a non-search response");
    };
    assert!(
        search.hits.iter().any(|hit| {
            hit.name == "AgentBeacon" && hit.path.as_str() == "Assets/OwnerOne.prefab"
        })
    );

    let incoming = ReferenceRequest::incoming_guid(TARGET_GUID, Some(100), 1);
    let first_incoming = references(&fixture, incoming.clone());
    let stale_cursor = first_incoming
        .coverage
        .next_cursor
        .clone()
        .expect("first incoming page has a cursor");
    let second_incoming = references(&fixture, incoming.clone().with_cursor(stale_cursor.clone()));
    assert_two_page_coverage(&first_incoming, &second_incoming);
    let mut incoming_sources = first_incoming
        .hits
        .iter()
        .chain(&second_incoming.hits)
        .map(|hit| hit.source_path.as_str())
        .collect::<Vec<_>>();
    incoming_sources.sort_unstable();
    assert_eq!(
        incoming_sources,
        ["Assets/OwnerOne.prefab", "Assets/OwnerTwo.prefab"]
    );

    let outgoing = ReferenceRequest::outgoing_guid(OWNER_ONE_GUID, Some(1), 1);
    let first_outgoing = references(&fixture, outgoing.clone());
    let outgoing_cursor = first_outgoing
        .coverage
        .next_cursor
        .clone()
        .expect("first outgoing page has a cursor");
    let second_outgoing = references(&fixture, outgoing.with_cursor(outgoing_cursor));
    assert_two_page_coverage(&first_outgoing, &second_outgoing);
    let mut outgoing_targets = first_outgoing
        .hits
        .iter()
        .chain(&second_outgoing.hits)
        .filter_map(|hit| {
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
    complete_reindex(&fixture, "json-agent-v2");
    let stale = run_agent_request(
        &fixture,
        RequestOperation::References(incoming.with_cursor(stale_cursor)),
    );
    assert_daemon_error(stale, "stale_cursor");

    shutdown(&fixture);
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
    let replacement_instance = replacement_discovered.descriptor().daemon_instance_id();
    assert_ne!(replacement_instance, original_instance);

    let (replacement_document, replacement_capabilities) = successful_operation(run_agent_request(
        &fixture,
        json!({
            "kind": "capabilities",
            "request": {},
        }),
    ));
    let ResponseOperation::Capabilities(replacement_capabilities) = replacement_capabilities else {
        panic!("replacement daemon returned a non-capabilities response");
    };
    assert_eq!(
        replacement_capabilities.daemon_version,
        daemon_build_identity
    );
    assert_eq!(
        replacement_document["daemon_instance_id"],
        replacement_instance.to_string()
    );
    let (_, replacement_status) = successful_operation(run_agent_request(
        &fixture,
        json!({
            "kind": "status",
            "request": {},
        }),
    ));
    let ResponseOperation::Status(replacement_status) = replacement_status else {
        panic!("replacement daemon returned a non-status response");
    };
    assert_current_status(&replacement_status, None);
    let (_, replacement_search) = successful_operation(run_agent_request(
        &fixture,
        json!({
            "kind": "search",
            "request": {
                "query": "AgentBeaconUpdated",
                "limit": 10,
            },
        }),
    ));
    let ResponseOperation::Search(replacement_search) = replacement_search else {
        panic!("replacement daemon returned a non-search response");
    };
    assert!(
        replacement_search
            .hits
            .iter()
            .any(|hit| hit.name == "AgentBeaconUpdated")
    );

    shutdown(&fixture);
    let replacement_status = replacement.wait_for_exit().await;
    assert!(
        replacement_status.success(),
        "replacement daemon exited unsuccessfully: {replacement_status}; stderr: {}",
        replacement.stderr()
    );
    assert!(matches!(
        namespace.discover_loopback_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));
}

fn complete_reindex(fixture: &SearchDaemonFixture, idempotency_key: &str) {
    let (_, admitted) = successful_operation(run_agent_request(
        fixture,
        RequestOperation::ReindexAdmit(ReindexAdmitRequest {
            intent: FilesystemReindexIntent::full(),
            idempotency_key: Some(idempotency_key.to_owned()),
        }),
    ));
    let ResponseOperation::ReindexAdmit(admitted) = admitted else {
        panic!("JSON agent received a non-reindex-admission response");
    };

    let (_, status) = successful_operation(run_agent_request(
        fixture,
        RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id: admitted.operation_id,
        }),
    ));
    let ResponseOperation::ReindexStatus(status) = status else {
        panic!("JSON agent received a non-reindex-status response");
    };
    assert!(matches!(
        status.state,
        ReindexOperationState::Queued
            | ReindexOperationState::Coalesced
            | ReindexOperationState::Running
            | ReindexOperationState::Succeeded
    ));

    let (_, completed) = successful_operation(run_agent_request(
        fixture,
        RequestOperation::ReindexWait(ReindexWaitRequest {
            operation_id: admitted.operation_id,
            timeout_ms: 20_000,
        }),
    ));
    let ResponseOperation::ReindexWait(completed) = completed else {
        panic!("JSON agent received a non-reindex-wait response");
    };
    assert_eq!(completed.state, ReindexOperationState::Succeeded);
    let generation = completed
        .completion
        .as_ref()
        .and_then(|completion| completion.generation.as_ref())
        .expect("successful reindex returns its published generation");
    let status = completed
        .status
        .as_ref()
        .expect("successful reindex returns terminal daemon status");
    assert_current_status(status, Some(generation));
}

fn assert_absent_status(status: &StatusResponse) {
    assert_eq!(status.daemon.lifecycle, DaemonLifecycleState::Serving);
    assert_eq!(status.daemon.serving, ServingAvailability::Unavailable);
    assert_eq!(status.daemon.freshness, GenerationFreshness::Absent);
    assert_eq!(
        status.daemon.generation_maintenance.state,
        GenerationMaintenanceState::Clean
    );
    assert!(status.generation.active.is_none());
    assert!(status.generation.building_revision.is_none());
    assert!(!status.indexing);
}

fn assert_current_status(status: &StatusResponse, expected: Option<&GenerationStamp>) {
    let active = status
        .generation
        .active
        .as_ref()
        .expect("current daemon status has an active generation");
    if let Some(expected) = expected {
        assert_eq!(active, expected);
    }
    assert_eq!(active.actual_revision, active.desired_revision);
    assert!(active.semantics_current);
    assert!(active.configuration_current);
    assert!(!active.stale);
    assert_eq!(status.daemon.lifecycle, DaemonLifecycleState::Serving);
    assert_eq!(status.daemon.serving, ServingAvailability::Queryable);
    assert_eq!(status.daemon.freshness, GenerationFreshness::Current);
    assert_eq!(
        status.daemon.freshness_maintenance,
        FreshnessMaintenance::Unmanaged
    );
    assert_eq!(status.daemon.reconcile, ReconcileLifecycle::Idle);
    assert_eq!(
        status.daemon.generation_maintenance.state,
        GenerationMaintenanceState::Clean
    );
    assert!(status.generation.building_revision.is_none());
    assert!(status.generation.last_failure.is_none());
    assert!(!status.indexing);
}

fn references(fixture: &SearchDaemonFixture, request: ReferenceRequest) -> ReferencesResponse {
    let (_, response) = successful_operation(run_agent_request(
        fixture,
        RequestOperation::References(request),
    ));
    let ResponseOperation::References(response) = response else {
        panic!("JSON agent received a non-reference response");
    };
    response
}

fn assert_two_page_coverage(first: &ReferencesResponse, second: &ReferencesResponse) {
    assert_eq!(first.coverage.returned, 1);
    assert_eq!(first.coverage.total, Some(2));
    assert!(first.coverage.truncated);
    assert_eq!(second.coverage.returned, 1);
    assert!(!second.coverage.truncated);
    assert!(second.coverage.next_cursor.is_none());
}

fn shutdown(fixture: &SearchDaemonFixture) {
    let (_, response) = successful_operation(run_agent_request(
        fixture,
        RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 5_000,
        }),
    ));
    let ResponseOperation::Shutdown(response) = response else {
        panic!("JSON agent received a non-shutdown response");
    };
    assert!(response.accepted);
}

fn assert_runner_daemon_identity() -> String {
    let daemon = PathBuf::from(
        std::env::var_os(DAEMON_BINARY_ENV)
            .expect("explicit harness must set UNITY_ASSET_SEARCH_DAEMON"),
    );
    assert!(
        daemon.is_absolute(),
        "explicit harness daemon path must be absolute: {}",
        daemon.display()
    );
    assert!(
        daemon.is_file(),
        "explicit harness daemon path is not a file: {}",
        daemon.display()
    );
    let expected = std::env::var(DAEMON_BUILD_IDENTITY_ENV)
        .expect("explicit harness must set UNITY_ASSET_SEARCH_DAEMON_BUILD_IDENTITY");
    assert!(
        !expected.is_empty() && !expected.contains(['\r', '\n']),
        "explicit harness daemon build identity must be one non-empty line"
    );

    let output = Command::new(&daemon)
        .arg("--version")
        .output()
        .expect("run the explicit harness daemon --version");
    assert!(
        output.status.success(),
        "explicit harness daemon --version failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "explicit harness daemon --version wrote stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual = String::from_utf8(output.stdout).expect("daemon --version is UTF-8");
    let actual = actual.trim_end_matches(['\r', '\n']);
    assert_eq!(
        actual,
        format!("{DAEMON_BINARY_NAME} {expected}"),
        "runner-bound daemon identity does not match the executable at {}",
        daemon.display()
    );
    expected
}

fn runner_daemon_binary() -> PathBuf {
    PathBuf::from(
        std::env::var_os(DAEMON_BINARY_ENV)
            .expect("explicit harness must set UNITY_ASSET_SEARCH_DAEMON"),
    )
}

fn run_convenience_capabilities(fixture: &SearchDaemonFixture) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .arg("--project-root")
        .arg(fixture.project_root())
        .arg("--index-dir")
        .arg(fixture.index_directory())
        .arg("--connect-timeout-ms")
        .arg(TEST_TIMEOUT.as_millis().to_string())
        .arg("--request-timeout-ms")
        .arg(TEST_TIMEOUT.as_millis().to_string())
        .arg("--daemon-binary")
        .arg(runner_daemon_binary())
        .arg("capabilities")
        .output()
        .expect("run convenience capabilities command")
}

fn run_agent_request<T>(fixture: &SearchDaemonFixture, operation: T) -> Output
where
    T: serde::Serialize,
{
    let request = serde_json::to_vec(&json!({
        "cli_contract_version": CLI_CONTRACT_VERSION,
        "operation": operation,
    }))
    .expect("serialize bounded agent request");
    let mut child = Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .arg("--project-root")
        .arg(fixture.project_root())
        .arg("--index-dir")
        .arg(fixture.index_directory())
        .arg("--connect-timeout-ms")
        .arg(TEST_TIMEOUT.as_millis().to_string())
        .arg("--request-timeout-ms")
        .arg(TEST_TIMEOUT.as_millis().to_string())
        .arg("--daemon-binary")
        .arg(runner_daemon_binary())
        .arg("--request-json")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn JSON agent CLI");
    child
        .stdin
        .take()
        .expect("agent CLI stdin")
        .write_all(&request)
        .expect("write bounded JSON agent request");
    child.wait_with_output().expect("wait for JSON agent CLI")
}

fn successful_operation(output: Output) -> (Value, ResponseOperation) {
    assert!(
        output.status.success(),
        "JSON agent CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "success must not write stderr");
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("JSON agent success document");
    assert_eq!(document["cli_contract_version"], CLI_CONTRACT_VERSION);
    assert_eq!(document["result"]["kind"], "operation");
    let operation = serde_json::from_value(document["result"]["value"].clone())
        .expect("decode JSON agent operation response");
    (document, operation)
}

fn assert_daemon_error(output: Output, expected_code: &str) {
    assert!(!output.status.success(), "daemon error returned success");
    assert!(
        output.stdout.is_empty(),
        "daemon error must not write stdout"
    );
    let document: Value =
        serde_json::from_slice(&output.stderr).expect("JSON agent failure document");
    assert_eq!(document["cli_contract_version"], CLI_CONTRACT_VERSION);
    assert_eq!(document["category"], "daemon");
    assert_eq!(document["error"]["code"], expected_code);
}
