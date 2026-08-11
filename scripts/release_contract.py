"""Reviewed product policy for the public unity-asset release surface."""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any


CARGO_DIST_VERSION = "0.30.3"

PUBLISHABLE_PACKAGE_NAMES = (
    "unity-asset-core",
    "unity-asset-binary",
    "unity-asset-decode",
    "unity-asset-yaml",
    "unity-asset-write",
    "unity-asset",
    "unity-asset-cli",
    "unity-asset-search-core",
    "unity-asset-search-protocol",
    "unity-asset-search-local",
    "unity-asset-search-cli",
    "unity-asset-search-index",
    "unity-asset-search-daemon",
)

DISTRIBUTED_APPLICATION_NAMES = (
    "unity-asset-search-cli",
    "unity-asset-search-daemon",
)

DISTRIBUTION_RUNNER_TARGETS = (
    ("ubuntu-latest", "x86_64-unknown-linux-musl"),
    ("windows-latest", "x86_64-pc-windows-msvc"),
    ("macos-latest", "aarch64-apple-darwin"),
    ("macos-15-intel", "x86_64-apple-darwin"),
)
DISTRIBUTION_TARGET_TRIPLES = tuple(
    target for _, target in DISTRIBUTION_RUNNER_TARGETS
)
GIT_OBJECT_PATTERN = re.compile(r"[0-9a-f]{40}")


class ReleaseContractError(RuntimeError):
    """A release input differs from the reviewed public product contract."""


@dataclass(frozen=True)
class DistributionArtifactPair:
    """The one archive and checksum required for one application and target."""

    application: str
    target: str
    archive_name: str
    checksum_name: str


def github_distribution_matrix() -> Mapping[str, Any]:
    """Return the workflow matrix derived from the reviewed target inventory."""

    return {
        "include": [
            {
                "os": runner,
                "target": target,
                "applications": " ".join(DISTRIBUTED_APPLICATION_NAMES),
            }
            for runner, target in DISTRIBUTION_RUNNER_TARGETS
        ]
    }


def _string_sequence(value: object, label: str) -> tuple[str, ...]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)):
        raise ReleaseContractError(f"{label} must be a string array")
    if any(not isinstance(item, str) for item in value):
        raise ReleaseContractError(f"{label} must contain only strings")
    return tuple(value)


def _archive_name(application: str, target: str) -> str:
    extension = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
    return f"{application}-{target}{extension}"


def validate_local_dist_plan_matrix(
    document: Mapping[str, Any], *, tag: str, version: str
) -> tuple[DistributionArtifactPair, ...]:
    """Validate a cargo-dist plan and return its typed application-target matrix."""

    if document.get("dist_version") != CARGO_DIST_VERSION:
        raise ReleaseContractError(
            "cargo-dist plan version mismatch: "
            f"expected {CARGO_DIST_VERSION}, got {document.get('dist_version')!r}"
        )
    if document.get("announcement_tag") != tag:
        raise ReleaseContractError(
            f"cargo-dist plan tag mismatch: expected {tag}, "
            f"got {document.get('announcement_tag')!r}"
        )
    if document.get("announcement_tag_is_implicit") is not False:
        raise ReleaseContractError("cargo-dist plan must use an explicit release tag")
    if document.get("announcement_is_prerelease") is not False:
        raise ReleaseContractError("stable releases cannot use a prerelease dist plan")

    raw_artifacts = document.get("artifacts")
    raw_releases = document.get("releases")
    if not isinstance(raw_artifacts, Mapping) or not isinstance(raw_releases, list):
        raise ReleaseContractError("cargo-dist plan omitted artifacts or releases")

    releases: dict[str, Mapping[str, Any]] = {}
    for raw_release in raw_releases:
        if not isinstance(raw_release, Mapping):
            raise ReleaseContractError("cargo-dist release entries must be objects")
        app_name = raw_release.get("app_name")
        if not isinstance(app_name, str) or app_name in releases:
            raise ReleaseContractError("cargo-dist plan has an invalid application name")
        releases[app_name] = raw_release

    expected_apps = set(DISTRIBUTED_APPLICATION_NAMES)
    actual_apps = set(releases)
    if actual_apps != expected_apps:
        raise ReleaseContractError(
            "cargo-dist application set mismatch: "
            f"expected {sorted(expected_apps)}, got {sorted(actual_apps)}"
        )

    expected_targets = set(DISTRIBUTION_TARGET_TRIPLES)
    matrix: list[DistributionArtifactPair] = []
    for app_name in DISTRIBUTED_APPLICATION_NAMES:
        release = releases[app_name]
        if release.get("app_version") != version:
            raise ReleaseContractError(
                f"cargo-dist app {app_name} has version {release.get('app_version')!r}, "
                f"expected {version}"
            )
        release_artifacts = _string_sequence(
            release.get("artifacts"), f"{app_name}.artifacts"
        )
        if len(release_artifacts) != len(set(release_artifacts)):
            raise ReleaseContractError(
                f"cargo-dist release {app_name} has a duplicate artifact reference"
            )
        app_targets: set[str] = set()
        expected_release_artifacts: set[str] = set()
        executable_artifacts: list[tuple[str, Mapping[str, Any]]] = []
        for artifact_name in release_artifacts:
            raw_artifact = raw_artifacts.get(artifact_name)
            if not isinstance(raw_artifact, Mapping):
                raise ReleaseContractError(
                    f"cargo-dist release {app_name} references unknown artifact "
                    f"{artifact_name!r}"
                )
            if raw_artifact.get("name") != artifact_name:
                raise ReleaseContractError(
                    f"cargo-dist artifact key/name mismatch for {artifact_name!r}"
                )
            if raw_artifact.get("kind") == "executable-zip":
                executable_artifacts.append((artifact_name, raw_artifact))

        for artifact_name, raw_artifact in executable_artifacts:
            targets = _string_sequence(
                raw_artifact.get("target_triples"),
                f"artifacts.{artifact_name}.target_triples",
            )
            if len(targets) != 1:
                raise ReleaseContractError(
                    f"cargo-dist executable artifact {artifact_name!r} must name one target"
            )
            target = targets[0]
            expected_archive_name = _archive_name(app_name, target)
            if artifact_name != expected_archive_name:
                raise ReleaseContractError(
                    f"cargo-dist release {app_name} target {target} must use its "
                    f"canonical archive {expected_archive_name!r}, got {artifact_name!r}"
                )
            if target in app_targets:
                raise ReleaseContractError(
                    f"cargo-dist release {app_name} has duplicate archive targets: {target}"
                )
            app_targets.add(target)
            checksum_name = raw_artifact.get("checksum")
            if not isinstance(checksum_name, str):
                raise ReleaseContractError(
                    f"cargo-dist executable artifact {artifact_name!r} omitted its checksum"
                )
            expected_checksum_name = f"{artifact_name}.sha256"
            if checksum_name != expected_checksum_name:
                raise ReleaseContractError(
                    f"cargo-dist archive {artifact_name!r} must use its canonical checksum "
                    f"{expected_checksum_name!r}, got {checksum_name!r}"
                )
            checksum = raw_artifacts.get(checksum_name)
            if not isinstance(checksum, Mapping) or checksum.get("kind") != "checksum":
                raise ReleaseContractError(
                    f"cargo-dist checksum {checksum_name!r} is missing or invalid"
                )
            checksum_targets = _string_sequence(
                checksum.get("target_triples"),
                f"artifacts.{checksum_name}.target_triples",
            )
            if checksum.get("name") != checksum_name or checksum_targets != (target,):
                raise ReleaseContractError(
                    f"cargo-dist checksum {checksum_name!r} does not match {artifact_name!r}"
                )
            expected_release_artifacts.update((artifact_name, checksum_name))
            matrix.append(
                DistributionArtifactPair(
                    application=app_name,
                    target=target,
                    archive_name=artifact_name,
                    checksum_name=checksum_name,
                )
            )

        if app_targets != expected_targets:
            raise ReleaseContractError(
                f"cargo-dist target set mismatch for {app_name}: "
                f"expected {sorted(expected_targets)}, got {sorted(app_targets)}"
            )
        if set(release_artifacts) != expected_release_artifacts:
            raise ReleaseContractError(
                f"cargo-dist release {app_name} contains an unexpected artifact set"
            )

    artifact_names = set(raw_artifacts)
    if any(not isinstance(name, str) for name in artifact_names):
        raise ReleaseContractError("cargo-dist artifact names must be strings")
    matrix_artifacts = {
        name for pair in matrix for name in (pair.archive_name, pair.checksum_name)
    }
    if artifact_names != matrix_artifacts:
        raise ReleaseContractError(
            "cargo-dist local plan contains unreferenced or missing artifacts"
        )
    for artifact_name in artifact_names:
        if "/" in artifact_name or "\\" in artifact_name or "\n" in artifact_name:
            raise ReleaseContractError(
                f"cargo-dist artifact name is not release-safe: {artifact_name!r}"
            )
    return tuple(
        sorted(matrix, key=lambda pair: (pair.application, pair.target))
    )


def validate_local_dist_plan(
    document: Mapping[str, Any], *, tag: str, version: str
) -> tuple[str, ...]:
    """Validate a cargo-dist local-artifact plan and return its exact inventory."""

    matrix = validate_local_dist_plan_matrix(document, tag=tag, version=version)
    return tuple(
        sorted(name for pair in matrix for name in (pair.archive_name, pair.checksum_name))
    )
