pub(crate) const VERSION_REPORT: &str = concat!(
    "unity-asset.build-identity.v1{version=",
    env!("CARGO_PKG_VERSION"),
    ";source-commit=",
    env!("UNITY_ASSET_SOURCE_COMMIT"),
    ";package=",
    env!("CARGO_PKG_NAME"),
    ";target=",
    env!("UNITY_ASSET_BUILD_TARGET"),
    "}"
);
