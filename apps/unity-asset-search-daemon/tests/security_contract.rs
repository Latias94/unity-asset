use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use unity_asset_search_daemon::security::{
    DaemonToken, SecurityError, TokenStore, UnsafePathReason, validate_listen_addr,
    verify_bearer_token,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock must follow the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "unity-asset-search-daemon-security-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("the unique test directory must be creatable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn child(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir(&path).expect("the child test directory must be creatable");
        path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn bearer(token: &DaemonToken) -> String {
    format!("Bearer {}", token.expose_secret())
}

#[test]
fn listener_accepts_only_ipv4_and_ipv6_loopback() {
    for address in ["127.0.0.1:9781", "127.23.4.5:1", "[::1]:9781"] {
        let address: SocketAddr = address.parse().expect("address must parse");
        validate_listen_addr(address).expect("loopback must be accepted");
    }

    for address in ["0.0.0.0:9781", "192.0.2.1:9781", "[::]:9781"] {
        let address: SocketAddr = address.parse().expect("address must parse");
        assert!(matches!(
            validate_listen_addr(address),
            Err(SecurityError::NonLoopbackListenAddress { .. })
        ));
    }
}

#[test]
fn missing_and_wrong_bearer_tokens_are_rejected() {
    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    assert!(matches!(
        store.load(),
        Err(SecurityError::TokenMissing { .. })
    ));

    let expected = store.create().expect("token must be created");
    let wrong = DaemonToken::generate().expect("wrong token must be generated");
    assert!(!verify_bearer_token(None, &expected));
    assert!(!verify_bearer_token(Some("Basic ignored"), &expected));
    assert!(!verify_bearer_token(Some(&bearer(&wrong)), &expected));
    assert!(verify_bearer_token(Some(&bearer(&expected)), &expected));
    assert!(verify_bearer_token(
        Some(&format!("bearer {}", expected.expose_secret())),
        &expected
    ));
}

#[test]
fn rotation_invalidates_the_stale_token_and_persists_the_new_token() {
    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    let stale = store.create().expect("initial token must be created");
    let current = store
        .rotate_if_current(&stale)
        .expect("token must rotate")
        .into_token();
    let persisted = store.load().expect("rotated token must load");

    assert!(!verify_bearer_token(Some(&bearer(&stale)), &current));
    assert!(verify_bearer_token(Some(&bearer(&current)), &persisted));
    assert_eq!(
        fs::read_to_string(store.token_path()).expect("token file must be readable"),
        current.expose_secret()
    );
    assert_eq!(
        fs::metadata(store.token_path())
            .expect("token metadata must exist")
            .len(),
        64
    );
}

#[test]
fn one_project_token_cannot_authorize_another_project() {
    let directory = TestDirectory::create();
    let first = TokenStore::open(directory.child("first")).expect("first store must open");
    let second = TokenStore::open(directory.child("second")).expect("second store must open");
    let first_token = first.create().expect("first token must be created");
    let second_token = second.create().expect("second token must be created");

    assert!(!verify_bearer_token(
        Some(&bearer(&first_token)),
        &second_token
    ));
    assert!(!verify_bearer_token(
        Some(&bearer(&second_token)),
        &first_token
    ));
}

#[test]
fn create_is_exclusive_and_create_or_rotate_replaces_a_valid_token() {
    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    let first = store.create().expect("initial token must be created");
    assert!(matches!(
        store.create(),
        Err(SecurityError::TokenAlreadyExists { .. })
    ));

    let second = store
        .create_or_rotate()
        .expect("existing token must be rotated")
        .into_token();
    assert!(!verify_bearer_token(Some(&bearer(&first)), &second));
}

#[test]
fn debug_output_never_contains_credential_material() {
    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    let token = store.create().expect("token must be created");
    let secret = token.expose_secret().to_owned();

    assert_eq!(format!("{token:?}"), "DaemonToken([REDACTED])");
    assert!(!format!("{token:?}").contains(&secret));
    assert!(!format!("{store:?}").contains(&secret));
}

#[test]
fn token_path_must_be_an_ordinary_file() {
    let directory = TestDirectory::create();
    fs::create_dir(directory.path().join("daemon.token"))
        .expect("conflicting directory must be created");
    let store = TokenStore::open(directory.path()).expect("store must open");

    assert!(matches!(
        store.load(),
        Err(SecurityError::UnsafePath {
            reason: UnsafePathReason::NotRegularFile,
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn token_file_has_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    store.create().expect("token must be created");
    let _daemon_lease = store
        .acquire_daemon_lease()
        .expect("daemon lease must be acquired");
    let mode = fs::metadata(store.token_path())
        .expect("token metadata must exist")
        .permissions()
        .mode()
        & 0o7777;

    assert_eq!(mode, 0o600);
    for lock_name in [".daemon-token.lock", ".daemon-instance.lock"] {
        let lock_mode = fs::metadata(directory.path().join(lock_name))
            .expect("lock metadata must exist")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(lock_mode, 0o600, "{lock_name}");
    }
}

#[cfg(unix)]
#[test]
fn token_file_with_widened_permissions_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::create();
    let store = TokenStore::open(directory.path()).expect("store must open");
    store.create().expect("token must be created");
    fs::set_permissions(store.token_path(), fs::Permissions::from_mode(0o640))
        .expect("token permissions must be widened for the test");

    assert!(matches!(
        store.load(),
        Err(SecurityError::UnsafePath {
            reason: UnsafePathReason::InsecurePermissions { mode: 0o640 },
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_or_group_writable_index_roots_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = TestDirectory::create();
    let actual = directory.child("actual");
    let linked = directory.path().join("linked");
    symlink(&actual, &linked).expect("index root symlink must be created");
    assert!(matches!(
        TokenStore::open(&linked),
        Err(SecurityError::UnsafePath {
            reason: UnsafePathReason::LinkOrReparsePoint,
            ..
        })
    ));

    fs::set_permissions(&actual, fs::Permissions::from_mode(0o770))
        .expect("index root permissions must be widened for the test");
    assert!(matches!(
        TokenStore::open(&actual),
        Err(SecurityError::UnsafePath {
            reason: UnsafePathReason::InsecureDirectoryPermissions { mode: 0o770 },
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn token_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::create();
    let target = directory.path().join("target");
    fs::write(&target, "0".repeat(64)).expect("target token must be written");
    symlink(&target, directory.path().join("daemon.token")).expect("symlink must be created");
    let store = TokenStore::open(directory.path()).expect("store must open");

    assert!(matches!(
        store.load(),
        Err(SecurityError::UnsafePath {
            reason: UnsafePathReason::LinkOrReparsePoint,
            ..
        })
    ));
}
