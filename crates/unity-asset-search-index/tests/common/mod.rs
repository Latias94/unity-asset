use tempfile::TempDir;

pub fn secure_tempdir() -> TempDir {
    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .expect("Windows tests require a LocalAppData directory");
        tempfile::Builder::new()
            .prefix("unity-asset-search-test-")
            .tempdir_in(local_app_data)
            .expect("create a test directory below the private LocalAppData namespace")
    }
    #[cfg(not(windows))]
    {
        tempfile::tempdir().expect("create a private test directory")
    }
}
