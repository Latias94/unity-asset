use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use unity_asset_search_local::{
    DiscoveredLoopbackEndpoint, EndpointNamespaceV1, EndpointStoreError, PrivateRootsV1,
    ProjectLocatorV1,
};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SearchDaemonFixture {
    project_directory: tempfile::TempDir,
    _roots: PrivateRootsV1,
    namespace: EndpointNamespaceV1,
    index_directory: PathBuf,
}

impl SearchDaemonFixture {
    pub fn new() -> Self {
        let project_directory = secure_test_tempdir();
        fs::create_dir_all(project_directory.path().join("Assets")).expect("Assets marker");
        fs::create_dir(project_directory.path().join("ProjectSettings"))
            .expect("ProjectSettings marker");
        let project = ProjectLocatorV1::open(project_directory.path()).expect("locate project");
        let roots = PrivateRootsV1::discover_for_current_context().expect("private local roots");
        let namespace = roots
            .runtime()
            .endpoint_namespace(project.project_id())
            .expect("derive endpoint namespace");
        let index_directory = project.root().join(".process-contract-index");
        Self {
            project_directory,
            _roots: roots,
            namespace,
            index_directory,
        }
    }

    pub fn project_root(&self) -> &Path {
        self.project_directory.path()
    }

    pub fn assets(&self) -> PathBuf {
        self.project_root().join("Assets")
    }

    pub fn index_directory(&self) -> &Path {
        &self.index_directory
    }

    pub const fn namespace(&self) -> &EndpointNamespaceV1 {
        &self.namespace
    }

    pub fn write_asset(&self, name: &str, contents: &str, guid: &str) {
        let assets = self.assets();
        fs::write(assets.join(name), contents).expect("write daemon fixture asset");
        fs::write(
            assets.join(format!("{name}.meta")),
            format!("fileFormatVersion: 2\nguid: {guid}\n"),
        )
        .expect("write daemon fixture asset metadata");
    }

    pub fn spawn_daemon(&self, startup_reindex: bool) -> DaemonChild {
        DaemonChild::spawn(self.project_root(), self.index_directory(), startup_reindex)
    }

    pub async fn wait_for_endpoint(&self, daemon: &mut DaemonChild) -> DiscoveredLoopbackEndpoint {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            daemon.assert_running();
            match self.namespace.discover_loopback_endpoint() {
                Ok(discovered) => return discovered,
                Err(
                    EndpointStoreError::DescriptorMissing | EndpointStoreError::EndpointChanged,
                ) => {}
                Err(error) => panic!("discover real daemon endpoint: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "real daemon did not publish its endpoint"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

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

pub struct DaemonChild {
    child: Child,
}

impl DaemonChild {
    fn spawn(project_root: &Path, index_directory: &Path, startup_reindex: bool) -> Self {
        let mut command = Command::new(daemon_executable());
        command
            .arg("--project-root")
            .arg(project_root)
            .arg("--index-dir")
            .arg(index_directory)
            .arg("--reconcile-interval-ms")
            .arg("0");
        if !startup_reindex {
            command.arg("--no-startup-reindex");
        }
        let child = command
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

    pub async fn wait_for_exit(&mut self) -> ExitStatus {
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

    pub fn stderr(&mut self) -> String {
        let mut output = String::new();
        if let Some(stderr) = self.child.stderr.as_mut() {
            let _ = stderr.read_to_string(&mut output);
        }
        output
    }
}

fn daemon_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("UNITY_ASSET_SEARCH_DAEMON") {
        return path.into();
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_unity-asset-search-daemon") {
        return path.into();
    }

    let cli = option_env!("CARGO_BIN_EXE_unity-asset-search-cli")
        .map(PathBuf::from)
        .expect("the shared process harness requires a Cargo-built daemon or CLI binary");
    let daemon = cli.with_file_name(if cfg!(windows) {
        "unity-asset-search-daemon.exe"
    } else {
        "unity-asset-search-daemon"
    });
    assert!(
        daemon.is_file(),
        "real daemon binary is not available beside the CLI at {}; build both search applications before running the cross-package conformance test",
        daemon.display()
    );
    daemon
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
