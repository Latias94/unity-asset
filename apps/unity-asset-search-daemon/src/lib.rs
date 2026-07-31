//! Reusable local search daemon boundaries.

pub mod coordinator;
pub mod ipc;
pub mod lifecycle;
pub mod watcher;

#[cfg(test)]
pub(crate) fn secure_test_tempdir() -> tempfile::TempDir {
    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .expect("Windows tests require a LocalAppData directory");
        tempfile::Builder::new()
            .prefix("unity-asset-search-daemon-test-")
            .tempdir_in(local_app_data)
            .expect("create a daemon test directory below the private LocalAppData namespace")
    }
    #[cfg(not(windows))]
    {
        tempfile::tempdir().expect("create a private daemon test directory")
    }
}
