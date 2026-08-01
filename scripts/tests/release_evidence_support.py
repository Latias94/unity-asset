from __future__ import annotations

import hashlib
from typing import Any, Mapping, Sequence

from protocol_sdk_bundle import ProtocolSdkBundleMetadata
from release_contract import PUBLISHABLE_PACKAGE_NAMES
from release_evidence import EVIDENCE_SCHEMA, expected_feature_profiles
from release_metadata import ReleaseMetadata


def make_release_evidence(
    *,
    tag: str = "v1.2.3",
    version: str = "1.2.3",
    dist_plan_sha256: str = "d" * 64,
    dist_artifacts: Sequence[str] = (),
    protocol_sdk: Mapping[str, Any] | None = None,
    github_release: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    protocol = protocol_sdk or ProtocolSdkBundleMetadata(
        release_tag=tag,
        version=version,
        artifact_name=f"unity-asset-search-protocol-sdk-{tag}.zip",
        encoded_bytes=1024,
        sha256="a" * 64,
        manifest_sha256="b" * 64,
        file_count=8,
    ).as_dict()
    metadata = github_release or ReleaseMetadata(tag, "Release notes.\n").evidence()
    packages = [
        {
            "name": name,
            "version": version,
            "manifest": f"crates/{name}/Cargo.toml",
        }
        for name in PUBLISHABLE_PACKAGE_NAMES
    ]
    return {
        "schema": EVIDENCE_SCHEMA,
        "tag": tag,
        "tag_object": "1" * 40,
        "commit": "2" * 40,
        "version": version,
        "cargo_lock_sha256": hashlib.sha256(b"Cargo.lock").hexdigest(),
        "msrv": "1.88.0",
        "release_toolchain": "1.97.1",
        "dist_plan_sha256": dist_plan_sha256,
        "dist_artifacts": sorted(dist_artifacts),
        "protocol_sdk": dict(protocol),
        "github_release": dict(metadata),
        "publish_order": list(PUBLISHABLE_PACKAGE_NAMES),
        "packages": packages,
        "documented_feature_profiles": [
            profile.as_dict() for profile in expected_feature_profiles()
        ],
    }
