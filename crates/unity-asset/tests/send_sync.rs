use unity_asset::workspace::{AssetWorkspace, WorkspaceInspector, WorkspaceSnapshot};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn workspace_read_boundaries_are_send_sync() {
    assert_send_sync::<AssetWorkspace>();
    assert_send_sync::<WorkspaceSnapshot>();
    assert_send_sync::<WorkspaceInspector<'static>>();
}
