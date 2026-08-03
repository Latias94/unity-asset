use std::fs;
use std::io::Read as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    EndpointNamespaceV1, EndpointStoreError, FrameReadTimeoutsV1, PrivateRootsV1, ProjectLocatorV1,
    VerifiedFramedTransportV1,
};
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2, FilesystemReindexIntent,
    FrameLimits, QueryPolicyId, ReindexAdmitRequest, ReindexOperationState, ReindexWaitRequest,
    RequestEnvelope, RequestId, RequestOperation, ResponseOperation, ResponseOutcome,
    SearchRequest, ShutdownRequest, StatusRequest, decode_response_frame, decode_validated_frame,
    encode_frame, encode_request_frame,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const PREFAB: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: AgentBeacon
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
async fn real_daemon_process_publishes_reindexes_searches_and_shuts_down() {
    let project_directory = secure_test_tempdir();
    let assets = project_directory.path().join("Assets");
    fs::create_dir_all(&assets).expect("Assets marker");
    fs::create_dir(project_directory.path().join("ProjectSettings"))
        .expect("ProjectSettings marker");
    fs::write(assets.join("AgentBeacon.prefab"), PREFAB).expect("write prefab");
    fs::write(
        assets.join("AgentBeacon.prefab.meta"),
        "fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n",
    )
    .expect("write prefab metadata");

    let project = ProjectLocatorV1::open(project_directory.path()).expect("locate project");
    let roots = PrivateRootsV1::discover_for_current_context().expect("private local roots");
    let namespace = roots
        .runtime()
        .endpoint_namespace(project.project_id())
        .expect("endpoint namespace");
    let index_directory = project_directory.path().join(".process-contract-index");
    let mut daemon = DaemonChild::spawn(project_directory.path(), &index_directory);
    let discovered = wait_for_endpoint(&namespace, &mut daemon).await;
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

    let initial = client
        .exchange(RequestOperation::Status(StatusRequest::default()))
        .await;
    let ResponseOperation::Status(initial) = initial else {
        panic!("real daemon returned a non-status response");
    };
    assert!(!initial.indexing);

    let admitted = client
        .exchange(RequestOperation::ReindexAdmit(ReindexAdmitRequest {
            intent: FilesystemReindexIntent::full(),
            idempotency_key: Some("process-contract-v1".to_owned()),
        }))
        .await;
    let ResponseOperation::ReindexAdmit(admitted) = admitted else {
        panic!("real daemon returned a non-reindex-admission response");
    };
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
            hit.name == "AgentBeacon" && hit.path.as_str() == "Assets/AgentBeacon.prefab"
        }),
        "real daemon search did not return the indexed prefab: {:?}",
        search.hits
    );

    let shutdown = client
        .exchange(RequestOperation::Shutdown(ShutdownRequest {
            drain_timeout_ms: 5_000,
        }))
        .await;
    let ResponseOperation::Shutdown(shutdown) = shutdown else {
        panic!("real daemon returned a non-shutdown response");
    };
    assert!(shutdown.accepted);
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
        match response.into_outcome() {
            ResponseOutcome::Success(operation) => *operation,
            ResponseOutcome::Error(error) => panic!("real daemon returned an API error: {error:?}"),
        }
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
