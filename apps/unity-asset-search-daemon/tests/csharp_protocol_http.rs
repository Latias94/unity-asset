mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use support::{SearchDaemonFixture, TEST_TIMEOUT};
use tokio::process::Command;
use unity_asset_search_local::EndpointStoreError;

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
async fn public_csharp_http_client_reaches_every_real_daemon_operation() {
    let fixture = SearchDaemonFixture::new();
    fixture.write_asset("Owner.prefab", OWNER, OWNER_GUID);
    fixture.write_asset("Target.prefab", TARGET, TARGET_GUID);

    let mut daemon = fixture.spawn_daemon(true);
    let discovered = fixture.wait_for_endpoint(&mut daemon).await;
    let descriptor = discovered.descriptor();
    let project = conformance_project();
    let mut child = Command::new("dotnet");
    child
        .arg("run")
        .arg("--project")
        .arg(&project)
        .arg("--configuration")
        .arg("Release")
        .arg("--")
        .arg("--real-daemon-http")
        .arg(fixture.namespace().path().join("endpoint.v2.json"))
        .arg(descriptor.project_id().to_string())
        .arg(descriptor.query_policy_id().to_string())
        .current_dir(repository_root())
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = child.spawn().expect("spawn C# protocol conformance");
    let live_conformance_timeout = TEST_TIMEOUT
        .checked_mul(3)
        .expect("live C# conformance timeout is representable");
    let output = tokio::time::timeout(live_conformance_timeout, child.wait_with_output())
        .await
        .expect("C# HTTP conformance did not exit")
        .expect("wait for C# HTTP conformance exit");
    assert!(
        output.status.success(),
        concat!(
            "C# HTTP conformance failed: {}\n",
            "stdout:\n{}\n",
            "stderr:\n{}"
        ),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("PASS: public C# HTTP client reached every real daemon operation"),
        "C# conformance did not report the live HTTP proof: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let status = daemon.wait_for_exit().await;
    assert!(
        status.success(),
        "real daemon exited unsuccessfully after C# shutdown: {status}; stderr: {}",
        daemon.stderr()
    );
    assert!(matches!(
        fixture.namespace().discover_loopback_endpoint(),
        Err(EndpointStoreError::DescriptorMissing)
    ));
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
