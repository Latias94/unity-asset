use std::fs;
use std::io::Read as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    EndpointNamespaceV1, EndpointStoreError, EndpointTransportError, FrameReadTimeoutsV1,
    PrivateRootsV1, ProjectLocatorV1, VerifiedFramedTransportV1,
};
use unity_asset_search_protocol::{
    ApiErrorCode, BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2,
    CapabilitiesRequest, DaemonLifecycleState, FilesystemReindexIntent, FrameLimits, QueryPolicyId,
    ReferenceRequest, ReindexAdmitRequest, ReindexCancelRequest, ReindexOperationState,
    ReindexStatusRequest, ReindexWaitRequest, RequestEnvelope, RequestId, RequestOperation,
    ResponseOperation, ResponseOutcome, SearchCapabilities, SearchRequest, ShutdownRequest,
    StatusRequest, SuggestRequest, decode_response_frame, decode_validated_frame, encode_frame,
    encode_request_frame,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
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

fn secure_test_tempdir() -> tempfile::TempDir {
    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .expect("Windows tests require a LocalAppData directory");
        tempfile::Builder::new()
            .prefix("unity-asset-search-process-test-")
            .tempdir_in(local_app_data)
            .expect("create a process test directory below the private LocalAppData namespace")
    }
    #[cfg(not(windows))]
    {
        tempfile::tempdir().expect("create a private process test directory")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_daemon_process_exercises_every_operation_and_rejects_stale_state() {
    let project_directory = secure_test_tempdir();
    let assets = project_directory.path().join("Assets");
    fs::create_dir_all(&assets).expect("Assets marker");
    fs::create_dir(project_directory.path().join("ProjectSettings"))
        .expect("ProjectSettings marker");
    write_asset(&assets, "OwnerOne.prefab", OWNER_ONE, OWNER_ONE_GUID);
    write_asset(&assets, "OwnerTwo.prefab", OWNER_TWO, OWNER_TWO_GUID);
    write_asset(&assets, "Target.prefab", TARGET, TARGET_GUID);
    write_asset(&assets, "TargetTwo.prefab", TARGET_TWO, TARGET_TWO_GUID);

    let project = ProjectLocatorV1::open(project_directory.path()).expect("locate project");
    let roots = PrivateRootsV1::discover_for_current_context().expect("private local roots");
    let namespace = roots
        .runtime()
        .endpoint_namespace(project.project_id())
        .expect("endpoint namespace");
    let index_directory = project_directory.path().join(".process-contract-index");
    let mut daemon = DaemonChild::spawn(project_directory.path(), &index_directory);
    let discovered = wait_for_endpoint(&namespace, &mut daemon).await;
    let stale_discovered = discovered;
    let descriptor = discovered.descriptor();
    let stream = discovered
        .connect_verified(&namespace, Instant::now() + TEST_TIMEOUT)
        .await
        .expect("connect to real daemon");
    let mut client = ProcessClient::bootstrap(
        stream,
        project.project_id(),
        descriptor.daemon_instance_id(),
    )
    .await;

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

    fs::write(assets.join("OwnerOne.prefab"), OWNER_ONE_UPDATED).expect("update owner prefab");
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

    shutdown(&mut client).await;
    drop(client);

    let status = daemon.wait_for_exit().await;
    assert!(
        status.success(),
        "real daemon exited unsuccessfully: {status}; stderr: {}",
        daemon.stderr()
    );
    assert!(matches!(
        namespace.discover_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));

    let mut replacement = DaemonChild::spawn(project_directory.path(), &index_directory);
    let replacement_discovered = wait_for_endpoint(&namespace, &mut replacement).await;
    assert_ne!(
        replacement_discovered.descriptor().daemon_instance_id(),
        descriptor.daemon_instance_id()
    );
    assert!(matches!(
        stale_discovered
            .connect_verified(&namespace, Instant::now() + TEST_TIMEOUT)
            .await,
        Err(EndpointTransportError::Store(
            EndpointStoreError::EndpointChanged
        ))
    ));
    let replacement_descriptor = replacement_discovered.descriptor();
    let replacement_stream = replacement_discovered
        .connect_verified(&namespace, Instant::now() + TEST_TIMEOUT)
        .await
        .expect("connect to replacement daemon");
    let mut replacement_client = ProcessClient::bootstrap(
        replacement_stream,
        project.project_id(),
        replacement_descriptor.daemon_instance_id(),
    )
    .await;
    shutdown(&mut replacement_client).await;
    drop(replacement_client);
    assert!(replacement.wait_for_exit().await.success());
    assert!(matches!(
        namespace.discover_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));
}

fn write_asset(assets: &std::path::Path, name: &str, contents: &str, guid: &str) {
    fs::write(assets.join(name), contents).expect("write process-contract asset");
    fs::write(
        assets.join(format!("{name}.meta")),
        format!("fileFormatVersion: 2\nguid: {guid}\n"),
    )
    .expect("write process-contract asset metadata");
}

async fn complete_reindex(client: &mut ProcessClient, idempotency_key: &str) {
    let admitted = client
        .exchange(RequestOperation::ReindexAdmit(ReindexAdmitRequest {
            intent: FilesystemReindexIntent::full(),
            idempotency_key: Some(idempotency_key.to_owned()),
        }))
        .await;
    let ResponseOperation::ReindexAdmit(admitted) = admitted else {
        panic!("real daemon returned a non-reindex-admission response");
    };
    let status = client
        .exchange(RequestOperation::ReindexStatus(ReindexStatusRequest {
            operation_id: admitted.operation_id,
        }))
        .await;
    let ResponseOperation::ReindexStatus(status) = status else {
        panic!("real daemon returned a non-reindex-status response");
    };
    assert_eq!(status.operation_id, admitted.operation_id);
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
            operation_id: admitted.operation_id,
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
            operation_id: admitted.operation_id,
        }))
        .await;
    let ResponseOperation::ReindexCancel(cancelled) = cancelled else {
        panic!("real daemon returned a non-reindex-cancel response");
    };
    assert_eq!(cancelled.state, ReindexOperationState::Succeeded);
    assert!(!cancelled.cancelled);
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
    stream: VerifiedFramedTransportV1,
    project_id: unity_asset_search_protocol::ProjectId,
    daemon_instance_id: unity_asset_search_protocol::DaemonInstanceId,
    query_policy_id: QueryPolicyId,
    next_request_id: u8,
}

impl ProcessClient {
    async fn bootstrap(
        mut stream: VerifiedFramedTransportV1,
        project_id: unity_asset_search_protocol::ProjectId,
        daemon_instance_id: unity_asset_search_protocol::DaemonInstanceId,
    ) -> Self {
        let hello = BootstrapHelloV2::new(
            project_id,
            daemon_instance_id,
            vec![BUSINESS_PROTOCOL_REVISION],
        )
        .expect("valid bootstrap hello");
        let hello_frame = encode_frame(&hello, FrameLimits::bootstrap()).expect("encode hello");
        write_frame(&mut stream, &hello_frame, FrameLimits::bootstrap()).await;
        let reply_frame = read_frame(&mut stream, FrameLimits::bootstrap()).await;
        let mut budget = AssetLoadBudget::default();
        let reply: BootstrapReplyV2 =
            decode_validated_frame(&reply_frame, &mut budget, FrameLimits::bootstrap())
                .expect("decode bootstrap reply");
        reply
            .validate_for(&hello)
            .expect("validate bootstrap reply");
        let query_policy_id = reply
            .query_policy_id()
            .expect("daemon accepted bootstrap with query policy");
        assert_eq!(reply.selected_revision(), Some(BUSINESS_PROTOCOL_REVISION));
        Self {
            stream,
            project_id,
            daemon_instance_id,
            query_policy_id,
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
        let request_id = RequestId::from_bytes([self.next_request_id; 16]);
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("request ID space");
        let request = RequestEnvelope::new(
            BUSINESS_PROTOCOL_REVISION,
            request_id,
            self.project_id,
            self.daemon_instance_id,
            self.query_policy_id,
            operation,
        )
        .expect("valid process-contract request");
        let request_frame = encode_request_frame(&request).expect("encode business request");
        write_frame(
            &mut self.stream,
            &request_frame,
            FrameLimits::request_envelope(),
        )
        .await;
        let response_frame = read_frame(
            &mut self.stream,
            FrameLimits::response(request.operation().kind()),
        )
        .await;
        let mut budget = AssetLoadBudget::default();
        let response = decode_response_frame(&response_frame, &mut budget, &request)
            .expect("decode business response");
        response.into_outcome()
    }
}

struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(project_root: &std::path::Path, index_directory: &std::path::Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_unity-asset-search-daemon"))
            .arg("--project-root")
            .arg(project_root)
            .arg("--index-dir")
            .arg(index_directory)
            .arg("--no-startup-reindex")
            .arg("--reconcile-interval-ms")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn real search daemon");
        Self { child }
    }

    fn assert_running(&mut self) {
        if let Some(status) = self.child.try_wait().expect("poll daemon process") {
            panic!(
                "real daemon exited before endpoint publication: {status}; stderr: {}",
                self.stderr()
            );
        }
    }

    async fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll daemon exit") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "real daemon did not exit after structured shutdown"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn stderr(&mut self) -> String {
        let mut output = String::new();
        if let Some(stderr) = self.child.stderr.as_mut() {
            let _ = stderr.read_to_string(&mut output);
        }
        output
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

async fn wait_for_endpoint(
    namespace: &EndpointNamespaceV1,
    daemon: &mut DaemonChild,
) -> unity_asset_search_local::DiscoveredEndpointV1 {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        daemon.assert_running();
        match namespace.discover_endpoint() {
            Ok(discovered) => return discovered,
            Err(EndpointStoreError::DescriptorMissing | EndpointStoreError::EndpointChanged) => {}
            Err(error) => panic!("discover real daemon endpoint: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "real daemon did not publish its endpoint"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn read_frame(stream: &mut VerifiedFramedTransportV1, limits: FrameLimits) -> Vec<u8> {
    stream
        .read_frame(limits, FrameReadTimeoutsV1::uniform(TEST_TIMEOUT))
        .await
        .expect("read real daemon frame")
        .expect("real daemon closed before returning a frame")
}

async fn write_frame(stream: &mut VerifiedFramedTransportV1, frame: &[u8], limits: FrameLimits) {
    stream
        .write_frame(frame, limits, TEST_TIMEOUT)
        .await
        .expect("write real daemon frame");
}
