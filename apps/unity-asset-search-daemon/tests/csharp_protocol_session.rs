mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use support::{SearchDaemonFixture, TEST_TIMEOUT};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use unity_asset_core::AssetLoadBudget;
use unity_asset_search_local::{
    EndpointStoreError, FrameReadTimeoutsV1, VerifiedFramedTransportV1,
};
use unity_asset_search_protocol::{
    BootstrapErrorCode, BootstrapReplyV2, FrameLimits, OperationKind, decode_request_frame,
    decode_validated_frame,
};

const OWNER_GUID: &str = "11111111111111111111111111111111";
const TARGET_GUID: &str = "0123456789abcdef0123456789abcdef";
const OWNER: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: AgentBeacon
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
"#;
const TARGET: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: TargetBeacon
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the .NET 8 SDK; CI runs this explicitly on every supported platform"]
async fn public_csharp_session_reaches_every_real_daemon_operation_through_verified_relay() {
    let fixture = SearchDaemonFixture::new();
    fixture.write_asset("Owner.prefab", OWNER, OWNER_GUID);
    fixture.write_asset("Target.prefab", TARGET, TARGET_GUID);

    let mut daemon = fixture.spawn_daemon(true);
    let discovered = fixture.wait_for_endpoint(&mut daemon).await;
    let descriptor = discovered.descriptor();

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind test-only C# relay");
    let relay_port = listener.local_addr().expect("relay address").port();
    let project = conformance_project();
    let mut child = Command::new("dotnet");
    child
        .arg("run")
        .arg("--project")
        .arg(&project)
        .arg("--configuration")
        .arg("Release")
        .arg("--")
        .arg("--real-daemon-relay")
        .arg(relay_port.to_string())
        .arg(fixture.project().project_id().to_string())
        .arg(descriptor.daemon_instance_id().to_string())
        .current_dir(repository_root())
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = child.spawn().expect("spawn C# protocol conformance");
    let output = child.wait_with_output();
    tokio::pin!(output);

    for expected in [
        BootstrapErrorCode::ProjectMismatch,
        BootstrapErrorCode::InstanceMismatch,
        BootstrapErrorCode::NoCommonRevision,
    ] {
        let (relay_stream, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
            .await
            .expect("C# negative Bootstrap did not connect to the relay")
            .expect("accept C# negative Bootstrap relay connection");
        let daemon_stream = discovered
            .connect_verified(fixture.namespace(), Instant::now() + TEST_TIMEOUT)
            .await
            .expect("connect verified negative Bootstrap relay to real daemon");
        let reply = tokio::time::timeout(
            TEST_TIMEOUT,
            relay_bootstrap_only(relay_stream, daemon_stream),
        )
        .await
        .expect("C# negative Bootstrap relay timed out")
        .expect("C# negative Bootstrap relay failed");
        assert_eq!(
            reply,
            BootstrapReplyV2::Rejected {
                bootstrap_version: unity_asset_search_protocol::BOOTSTRAP_VERSION,
                code: expected,
            }
        );
    }

    let (relay_stream, _) = tokio::select! {
        accepted = listener.accept() => accepted.expect("accept C# relay connection"),
        early = &mut output => {
            let early = early.expect("wait for early C# conformance exit");
            panic!("C# conformance exited before connecting: {}\nstdout:\n{}\nstderr:\n{}",
                early.status,
                String::from_utf8_lossy(&early.stdout),
                String::from_utf8_lossy(&early.stderr));
        }
        () = tokio::time::sleep(TEST_TIMEOUT) => panic!("C# conformance did not connect to the relay"),
    };
    let daemon_stream = discovered
        .connect_verified(fixture.namespace(), Instant::now() + TEST_TIMEOUT)
        .await
        .expect("connect verified relay to real daemon");

    tokio::time::timeout(TEST_TIMEOUT, relay_session(relay_stream, daemon_stream))
        .await
        .expect("C# relay session timed out")
        .expect("C# relay session failed");
    let output = tokio::time::timeout(TEST_TIMEOUT, &mut output)
        .await
        .expect("C# conformance did not exit")
        .expect("wait for C# conformance exit");
    assert!(
        output.status.success(),
        "C# conformance failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("PASS: public C# session reached every real daemon operation"),
        "C# conformance did not report the live-session proof: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let status = daemon.wait_for_exit().await;
    assert!(
        status.success(),
        "real daemon exited unsuccessfully after C# shutdown: {status}; stderr: {}",
        daemon.stderr()
    );
    assert!(matches!(
        fixture.namespace().discover_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));
}

async fn relay_session(mut relay: TcpStream, mut daemon: VerifiedFramedTransportV1) -> Result<()> {
    relay.set_nodelay(true).context("configure C# relay")?;
    let reply = relay_bootstrap(&mut relay, &mut daemon).await?;
    ensure!(
        matches!(reply, BootstrapReplyV2::Accepted { .. }),
        "real daemon rejected the positive C# Bootstrap: {reply:?}"
    );

    loop {
        let request_frame = read_relay_frame(&mut relay, FrameLimits::request_envelope())
            .await?
            .ok_or_else(|| anyhow!("C# relay closed before structured shutdown"))?;
        let mut budget = AssetLoadBudget::default();
        let request = decode_request_frame(&request_frame, &mut budget)
            .context("decode relayed C# request")?;
        let operation = request.operation().kind();
        daemon
            .write_frame(
                &request_frame,
                FrameLimits::request_envelope(),
                TEST_TIMEOUT,
            )
            .await
            .context("forward C# request to daemon")?;
        let response = daemon
            .read_frame(
                FrameLimits::response(operation),
                FrameReadTimeoutsV1::uniform(TEST_TIMEOUT),
            )
            .await
            .context("read real daemon response")?
            .ok_or_else(|| anyhow!("daemon closed before returning a response"))?;
        write_relay_frame(&mut relay, &response).await?;
        if operation == OperationKind::Shutdown {
            return Ok(());
        }
    }
}

async fn relay_bootstrap_only(
    mut relay: TcpStream,
    mut daemon: VerifiedFramedTransportV1,
) -> Result<BootstrapReplyV2> {
    relay.set_nodelay(true).context("configure C# relay")?;
    relay_bootstrap(&mut relay, &mut daemon).await
}

async fn relay_bootstrap(
    relay: &mut TcpStream,
    daemon: &mut VerifiedFramedTransportV1,
) -> Result<BootstrapReplyV2> {
    let hello = read_relay_frame(relay, FrameLimits::bootstrap())
        .await?
        .ok_or_else(|| anyhow!("C# relay closed before Bootstrap"))?;
    daemon
        .write_frame(&hello, FrameLimits::bootstrap(), TEST_TIMEOUT)
        .await
        .context("forward C# Bootstrap to daemon")?;
    let reply = daemon
        .read_frame(
            FrameLimits::bootstrap(),
            FrameReadTimeoutsV1::uniform(TEST_TIMEOUT),
        )
        .await
        .context("read daemon Bootstrap reply")?
        .ok_or_else(|| anyhow!("daemon closed before Bootstrap reply"))?;
    write_relay_frame(relay, &reply).await?;
    let mut budget = AssetLoadBudget::default();
    decode_validated_frame(&reply, &mut budget, FrameLimits::bootstrap())
        .context("decode real daemon Bootstrap reply")
}

async fn read_relay_frame(stream: &mut TcpStream, limits: FrameLimits) -> Result<Option<Vec<u8>>> {
    let mut header = [0_u8; 4];
    match stream.read(&mut header[..1]).await {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("the relay reads one header byte"),
        Err(error) => return Err(error).context("read C# relay frame header"),
    }
    stream
        .read_exact(&mut header[1..])
        .await
        .context("read complete C# relay frame header")?;
    let declared = u32::from_be_bytes(header) as usize;
    ensure!(
        declared <= limits.max_encoded_bytes(),
        "C# relay frame declared {declared} bytes; maximum is {}",
        limits.max_encoded_bytes()
    );
    let frame_length = declared
        .checked_add(header.len())
        .ok_or_else(|| anyhow!("C# relay frame length overflow"))?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(frame_length)
        .map_err(|error| anyhow!("reserve {frame_length} bytes for C# relay frame: {error}"))?;
    frame.extend_from_slice(&header);
    frame.resize(frame_length, 0);
    stream
        .read_exact(&mut frame[header.len()..])
        .await
        .context("read C# relay frame body")?;
    Ok(Some(frame))
}

async fn write_relay_frame(stream: &mut TcpStream, frame: &[u8]) -> Result<()> {
    if frame.len() < 4 {
        bail!("daemon returned a truncated frame");
    }
    stream
        .write_all(frame)
        .await
        .context("write frame to C# relay")?;
    stream.flush().await.context("flush C# relay frame")
}

fn conformance_project() -> PathBuf {
    repository_root().join(
        "integration/search-protocol/csharp/UnityAsset.SearchProtocol.Conformance/UnityAsset.SearchProtocol.Conformance.csproj",
    )
}

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is nested below the repository root")
}
