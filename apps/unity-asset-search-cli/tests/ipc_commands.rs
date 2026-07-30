use std::fs;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    ClaimedEndpointV1, PrivateRootsV1, ProjectLocatorV1, VerifiedLocalStreamV1,
    generate_daemon_instance_id,
};
use unity_asset_search_protocol::{
    BUSINESS_PROTOCOL_REVISION, BootstrapHelloV2, BootstrapReplyV2, CapabilitiesResponse,
    FrameLimits, QueryPolicyId, RequestOperation, ResponseEnvelope, ResponseOperation,
    SearchCapabilities, decode_request_frame, decode_validated_frame, encode_frame,
    encode_response_frame,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

fn run_cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .args(arguments)
        .output()
        .expect("run search CLI")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convenience_and_json_requests_share_the_verified_ipc_path() {
    let root = tempfile::tempdir().expect("temporary Unity project");
    fs::create_dir(root.path().join("Assets")).expect("Assets marker");
    fs::create_dir(root.path().join("ProjectSettings")).expect("ProjectSettings marker");
    let project = ProjectLocatorV1::open(root.path()).expect("locate test project");
    let roots = PrivateRootsV1::discover_for_current_context().expect("private roots");
    let namespace = roots
        .runtime()
        .endpoint_namespace(project.project_id())
        .expect("endpoint namespace");
    let mut claim = namespace
        .claim_daemon_endpoint()
        .expect("claim daemon endpoint");
    let instance = generate_daemon_instance_id().expect("daemon instance");
    let endpoint = claim.publish(instance).expect("publish endpoint");
    let query_policy = QueryPolicyId::from_bytes([0x44; 32]);
    let expected_context = namespace.security_context_id();
    let project_id = project.project_id();

    let server_task = tokio::spawn(async move {
        serve_capabilities(
            endpoint,
            project_id,
            instance,
            query_policy,
            expected_context,
            2,
        )
        .await
    });

    let project_root = root.path().to_str().expect("UTF-8 test path");
    let convenience = run_cli(&["--project-root", project_root, "capabilities"]);
    assert_success(&convenience, project_id.to_string(), instance.to_string());

    let request_path = root.path().join("capabilities.json");
    fs::write(
        &request_path,
        r#"{"cli_contract_version":1,"operation":{"kind":"capabilities","request":{}}}"#,
    )
    .expect("write CLI request");
    let json = run_cli(&[
        "--project-root",
        project_root,
        "--request-json",
        request_path.to_str().expect("UTF-8 test path"),
    ]);
    assert_success(&json, project_id.to_string(), instance.to_string());
    assert_eq!(convenience.stdout, json.stdout);

    let observed = server_task
        .await
        .expect("join IPC fixture")
        .expect("serve IPC fixture");
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], observed[1]);
    assert!(matches!(observed[0], RequestOperation::Capabilities(_)));
}

fn assert_success(output: &Output, project_id: String, instance_id: String) {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "success must not write stderr");
    let document: Value = serde_json::from_slice(&output.stdout).expect("success JSON");
    assert_eq!(document["cli_contract_version"], 1);
    assert_eq!(document["project_id"], project_id);
    assert_eq!(document["daemon_instance_id"], instance_id);
    assert_eq!(document["result"]["kind"], "operation");
    assert_eq!(document["result"]["value"]["kind"], "capabilities");
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
}

async fn serve_capabilities(
    mut endpoint: ClaimedEndpointV1,
    project_id: unity_asset_search_protocol::ProjectId,
    instance: unity_asset_search_protocol::DaemonInstanceId,
    query_policy: QueryPolicyId,
    expected_context: unity_asset_search_local::SecurityContextIdV1,
    exchanges: usize,
) -> Result<Vec<RequestOperation>, String> {
    let mut observed = Vec::with_capacity(exchanges);
    for _ in 0..exchanges {
        let mut stream = tokio::time::timeout(TEST_TIMEOUT, endpoint.accept_verified())
            .await
            .map_err(|_| "accept timeout".to_owned())?
            .map_err(|error| error.to_string())?;
        let hello_frame =
            read_frame(&mut stream, FrameLimits::bootstrap().max_encoded_bytes()).await?;
        stream
            .verify_received_message_principal(expected_context)
            .map_err(|error| error.to_string())?;
        let mut budget = AssetLoadBudget::default();
        let hello: BootstrapHelloV2 =
            decode_validated_frame(&hello_frame, &mut budget, FrameLimits::bootstrap())
                .map_err(|error| error.to_string())?;
        let reply = BootstrapReplyV2::negotiate(
            &hello,
            project_id,
            instance,
            query_policy,
            &[BUSINESS_PROTOCOL_REVISION],
        );
        write_frame(
            &mut stream,
            &encode_frame(&reply, FrameLimits::bootstrap()).map_err(|error| error.to_string())?,
        )
        .await?;

        let request_frame = read_frame(
            &mut stream,
            FrameLimits::request_envelope().max_encoded_bytes(),
        )
        .await?;
        stream
            .verify_received_message_principal(expected_context)
            .map_err(|error| error.to_string())?;
        let mut budget = AssetLoadBudget::default();
        let request =
            decode_request_frame(&request_frame, &mut budget).map_err(|error| error.to_string())?;
        request
            .validate_binding(project_id, instance, query_policy)
            .map_err(|error| error.to_string())?;
        observed.push(request.operation().clone());
        let response = ResponseEnvelope::success(
            &request,
            ResponseOperation::Capabilities(CapabilitiesResponse {
                daemon_version: "ipc-test-v1".to_owned(),
                capabilities: SearchCapabilities::current(),
            }),
        );
        let response_frame =
            encode_response_frame(&response, &request).map_err(|error| error.to_string())?;
        write_frame(&mut stream, &response_frame).await?;
    }
    Ok(observed)
}

async fn read_frame(stream: &mut VerifiedLocalStreamV1, maximum: usize) -> Result<Vec<u8>, String> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let mut header = [0_u8; 4];
        stream.read_exact(&mut header).await?;
        let declared = u32::from_be_bytes(header) as usize;
        if declared > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame exceeds fixture limit",
            ));
        }
        let mut frame = Vec::with_capacity(4 + declared);
        frame.extend_from_slice(&header);
        frame.resize(4 + declared, 0);
        stream.read_exact(&mut frame[4..]).await?;
        Ok::<_, std::io::Error>(frame)
    })
    .await
    .map_err(|_| "read timeout".to_owned())?
    .map_err(|error| error.to_string())
}

async fn write_frame(stream: &mut VerifiedLocalStreamV1, frame: &[u8]) -> Result<(), String> {
    tokio::time::timeout(TEST_TIMEOUT, async {
        stream.write_all(frame).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| "write timeout".to_owned())?
    .map_err(|error| error.to_string())
}
