use unity_asset::environment::Environment;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn environment_is_send_sync() {
    assert_send_sync::<Environment>();
}
