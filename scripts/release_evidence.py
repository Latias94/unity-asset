"""Parse and validate the canonical source evidence for one release."""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Sequence

from protocol_sdk_bundle import (
    BUNDLE_FORMAT,
    BUNDLE_METADATA_SCHEMA,
    MAX_BUNDLE_BYTES,
    RELEASE_TAG_PATTERN,
    archive_name_for_tag,
    canonical_json_bytes,
)
from release_contract import GIT_OBJECT_PATTERN, PUBLISHABLE_PACKAGE_NAMES
from release_metadata import ReleaseMetadataError, validate_metadata_evidence_shape
from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    reject_link_components,
)
from workspace_package_contract import DOCUMENTED_FEATURE_PROFILES


EVIDENCE_SCHEMA = "unity-asset.release-evidence.v3"
MAX_EVIDENCE_BYTES = 4 * 1024 * 1024
TAG_PATTERN = RELEASE_TAG_PATTERN
SEMVER_PATTERN = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")

ROOT_KEYS = {
    "schema",
    "tag",
    "tag_object",
    "commit",
    "version",
    "cargo_lock_sha256",
    "msrv",
    "release_toolchain",
    "dist_plan_sha256",
    "dist_artifacts",
    "protocol_sdk",
    "github_release",
    "packages",
    "documented_feature_profiles",
}
PACKAGE_KEYS = {"name", "version", "manifest"}
PROFILE_KEYS = {
    "name",
    "package",
    "features",
    "default_features",
}
PROTOCOL_KEYS = {
    "schema",
    "bundle_format",
    "release_tag",
    "version",
    "artifact_name",
    "encoded_bytes",
    "sha256",
    "manifest_sha256",
    "file_count",
}


class ReleaseEvidenceError(RuntimeError):
    """Release evidence is malformed, incomplete, or bound to different inputs."""


@dataclass(frozen=True)
class PackageEvidence:
    """One publishable package in dependency-first order."""

    name: str
    version: str
    manifest: str

    def as_dict(self) -> Mapping[str, str]:
        return {"name": self.name, "version": self.version, "manifest": self.manifest}


@dataclass(frozen=True)
class FeatureProfileEvidence:
    """One exact feature combination promised by public documentation."""

    name: str
    package: str
    features: tuple[str, ...]
    default_features: bool

    def as_dict(self) -> Mapping[str, Any]:
        return {
            "name": self.name,
            "package": self.package,
            "features": list(self.features),
            "default_features": self.default_features,
        }


@dataclass(frozen=True)
class ReleaseEvidence:
    """The complete, canonical proof contract consumed by release stages."""

    tag: str
    tag_object: str
    commit: str
    version: str
    cargo_lock_sha256: str
    msrv: str
    release_toolchain: str
    dist_plan_sha256: str
    dist_artifacts: tuple[str, ...]
    protocol_sdk: Mapping[str, Any]
    github_release: Mapping[str, Any]
    packages: tuple[PackageEvidence, ...]
    documented_feature_profiles: tuple[FeatureProfileEvidence, ...]

    @property
    def publish_order(self) -> tuple[str, ...]:
        return tuple(package.name for package in self.packages)

    def as_dict(self) -> Mapping[str, Any]:
        return {
            "schema": EVIDENCE_SCHEMA,
            "tag": self.tag,
            "tag_object": self.tag_object,
            "commit": self.commit,
            "version": self.version,
            "cargo_lock_sha256": self.cargo_lock_sha256,
            "msrv": self.msrv,
            "release_toolchain": self.release_toolchain,
            "dist_plan_sha256": self.dist_plan_sha256,
            "dist_artifacts": list(self.dist_artifacts),
            "protocol_sdk": dict(self.protocol_sdk),
            "github_release": dict(self.github_release),
            "packages": [package.as_dict() for package in self.packages],
            "documented_feature_profiles": [
                profile.as_dict() for profile in self.documented_feature_profiles
            ],
        }


def _mapping(value: object, label: str, keys: set[str]) -> Mapping[str, Any]:
    if not isinstance(value, dict) or set(value) != keys:
        raise ReleaseEvidenceError(f"{label} has an invalid schema")
    return value


def _string(value: object, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise ReleaseEvidenceError(f"{label} must be a non-empty string")
    return value


def _digest(value: object, label: str) -> str:
    digest = _string(value, label)
    if SHA256_PATTERN.fullmatch(digest) is None:
        raise ReleaseEvidenceError(f"{label} must be a lowercase SHA-256 digest")
    return digest


def _positive_int(value: object, label: str, *, maximum: int | None = None) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise ReleaseEvidenceError(f"{label} must be a positive integer")
    if maximum is not None and value > maximum:
        raise ReleaseEvidenceError(f"{label} exceeds {maximum}")
    return value


def _string_list(value: object, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or any(
        not isinstance(item, str) or not item for item in value
    ):
        raise ReleaseEvidenceError(f"{label} must be an array of non-empty strings")
    result = tuple(value)
    if len(result) != len(set(result)):
        raise ReleaseEvidenceError(f"{label} contains duplicates")
    return result


def _portable_manifest(value: object, package: str) -> str:
    manifest = _string(value, f"package {package} manifest")
    path = PurePosixPath(manifest)
    if (
        path.is_absolute()
        or "\\" in manifest
        or any(component in {"", ".", ".."} for component in path.parts)
        or path.name != "Cargo.toml"
    ):
        raise ReleaseEvidenceError(
            f"package {package} manifest is not a portable repository-relative Cargo.toml"
        )
    return manifest


def _packages(value: object, version: str) -> tuple[PackageEvidence, ...]:
    if not isinstance(value, list):
        raise ReleaseEvidenceError("release packages must be an array")
    packages: list[PackageEvidence] = []
    manifests: set[str] = set()
    for index, item in enumerate(value):
        raw = _mapping(item, f"release package {index}", PACKAGE_KEYS)
        name = _string(raw.get("name"), f"release package {index} name")
        package_version = _string(raw.get("version"), f"release package {name} version")
        if package_version != version:
            raise ReleaseEvidenceError(
                f"release package {name} version does not match {version}"
            )
        manifest = _portable_manifest(raw.get("manifest"), name)
        if manifest in manifests:
            raise ReleaseEvidenceError(f"release package manifest is duplicated: {manifest}")
        manifests.add(manifest)
        packages.append(PackageEvidence(name, package_version, manifest))
    names = tuple(package.name for package in packages)
    if names != PUBLISHABLE_PACKAGE_NAMES:
        raise ReleaseEvidenceError(
            "release package topology/order does not match the reviewed publication closure"
        )
    return tuple(packages)


def expected_feature_profiles() -> tuple[FeatureProfileEvidence, ...]:
    return tuple(
        FeatureProfileEvidence(
            name=profile.name,
            package=profile.package,
            features=tuple(profile.features),
            default_features=profile.default_features,
        )
        for profile in DOCUMENTED_FEATURE_PROFILES
    )


def _feature_profiles(value: object) -> tuple[FeatureProfileEvidence, ...]:
    if not isinstance(value, list):
        raise ReleaseEvidenceError("documented feature profiles must be an array")
    profiles: list[FeatureProfileEvidence] = []
    for index, item in enumerate(value):
        raw = _mapping(item, f"documented feature profile {index}", PROFILE_KEYS)
        name = _string(raw.get("name"), f"documented feature profile {index} name")
        package = _string(raw.get("package"), f"documented feature profile {name} package")
        features = _string_list(raw.get("features"), f"documented feature profile {name} features")
        default_features = raw.get("default_features")
        if not isinstance(default_features, bool):
            raise ReleaseEvidenceError(
                f"documented feature profile {name} default_features must be boolean"
            )
        profiles.append(
            FeatureProfileEvidence(
                name,
                package,
                features,
                default_features,
            )
        )
    result = tuple(profiles)
    if result != expected_feature_profiles():
        raise ReleaseEvidenceError(
            "documented feature profiles do not match the reviewed public contract"
        )
    return result


def _protocol_sdk(value: object, tag: str, version: str) -> Mapping[str, Any]:
    raw = _mapping(value, "search protocol SDK evidence", PROTOCOL_KEYS)
    expected_name = archive_name_for_tag(tag)
    if (
        raw.get("schema") != BUNDLE_METADATA_SCHEMA
        or raw.get("bundle_format") != BUNDLE_FORMAT
        or raw.get("release_tag") != tag
        or raw.get("version") != version
        or raw.get("artifact_name") != expected_name
    ):
        raise ReleaseEvidenceError("search protocol SDK identity is invalid")
    _positive_int(raw.get("encoded_bytes"), "search protocol SDK encoded_bytes", maximum=MAX_BUNDLE_BYTES)
    _positive_int(raw.get("file_count"), "search protocol SDK file_count")
    _digest(raw.get("sha256"), "search protocol SDK sha256")
    _digest(raw.get("manifest_sha256"), "search protocol SDK manifest_sha256")
    return dict(raw)


def parse_release_evidence(
    value: object,
    *,
    expected_tag: str | None = None,
    expected_version: str | None = None,
    expected_commit: str | None = None,
    expected_tag_object: str | None = None,
    expected_dist_plan_sha256: str | None = None,
    expected_dist_artifacts: Sequence[str] | None = None,
    expected_protocol_sdk: Mapping[str, Any] | None = None,
    expected_github_release: Mapping[str, Any] | None = None,
) -> ReleaseEvidence:
    """Validate the complete evidence schema and any caller-owned exact bindings."""

    raw = _mapping(value, "release evidence", ROOT_KEYS)
    if raw.get("schema") != EVIDENCE_SCHEMA:
        raise ReleaseEvidenceError("release evidence schema version is invalid")
    tag = _string(raw.get("tag"), "release tag")
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ReleaseEvidenceError("release evidence tag is not stable vMAJOR.MINOR.PATCH")
    version = _string(raw.get("version"), "release version")
    if version != ".".join(match.groups()):
        raise ReleaseEvidenceError("release version does not match the release tag")
    if expected_tag is not None and tag != expected_tag:
        raise ReleaseEvidenceError("release evidence tag does not match the requested tag")
    if expected_version is not None and version != expected_version:
        raise ReleaseEvidenceError("release evidence version does not match the requested version")

    tag_object = _string(raw.get("tag_object"), "release tag object")
    commit = _string(raw.get("commit"), "release commit")
    for label, object_id in (("release tag object", tag_object), ("release commit", commit)):
        if GIT_OBJECT_PATTERN.fullmatch(object_id) is None:
            raise ReleaseEvidenceError(f"{label} must be a full lowercase Git object ID")
    if expected_commit is not None and commit != expected_commit:
        raise ReleaseEvidenceError("release evidence commit does not match the expected commit")
    if expected_tag_object is not None and tag_object != expected_tag_object:
        raise ReleaseEvidenceError(
            "release evidence tag object does not match the expected annotated tag"
        )

    cargo_lock_sha256 = _digest(raw.get("cargo_lock_sha256"), "Cargo.lock SHA-256")
    dist_plan_sha256 = _digest(raw.get("dist_plan_sha256"), "dist plan SHA-256")
    if expected_dist_plan_sha256 is not None and dist_plan_sha256 != expected_dist_plan_sha256:
        raise ReleaseEvidenceError("release evidence does not bind the expected dist plan")

    msrv = _string(raw.get("msrv"), "release MSRV")
    release_toolchain = _string(raw.get("release_toolchain"), "release Rust toolchain")
    msrv_match = SEMVER_PATTERN.fullmatch(msrv)
    toolchain_match = SEMVER_PATTERN.fullmatch(release_toolchain)
    if msrv_match is None or toolchain_match is None:
        raise ReleaseEvidenceError("release toolchains must be exact stable patch versions")
    if tuple(map(int, toolchain_match.groups())) < tuple(map(int, msrv_match.groups())):
        raise ReleaseEvidenceError("release Rust toolchain cannot be older than the MSRV")

    dist_artifacts = _string_list(raw.get("dist_artifacts"), "dist artifact inventory")
    if tuple(sorted(dist_artifacts)) != dist_artifacts:
        raise ReleaseEvidenceError("dist artifact inventory must be canonically sorted")
    if expected_dist_artifacts is not None and dist_artifacts != tuple(expected_dist_artifacts):
        raise ReleaseEvidenceError(
            "release evidence does not bind the expected dist artifact inventory"
        )

    packages = _packages(raw.get("packages"), version)
    profiles = _feature_profiles(raw.get("documented_feature_profiles"))
    protocol_sdk = _protocol_sdk(raw.get("protocol_sdk"), tag, version)
    if expected_protocol_sdk is not None and protocol_sdk != dict(expected_protocol_sdk):
        raise ReleaseEvidenceError(
            "release evidence does not bind the expected search protocol SDK"
        )
    try:
        github_release = dict(validate_metadata_evidence_shape(raw.get("github_release")))
    except ReleaseMetadataError as error:
        raise ReleaseEvidenceError(f"invalid GitHub Release metadata evidence: {error}") from error
    if github_release.get("title") != tag:
        raise ReleaseEvidenceError("GitHub Release title does not match the release tag")
    if expected_github_release is not None and github_release != dict(expected_github_release):
        raise ReleaseEvidenceError(
            "release evidence does not bind the expected GitHub Release metadata"
        )

    return ReleaseEvidence(
        tag=tag,
        tag_object=tag_object,
        commit=commit,
        version=version,
        cargo_lock_sha256=cargo_lock_sha256,
        msrv=msrv,
        release_toolchain=release_toolchain,
        dist_plan_sha256=dist_plan_sha256,
        dist_artifacts=dist_artifacts,
        protocol_sdk=protocol_sdk,
        github_release=github_release,
        packages=packages,
        documented_feature_profiles=profiles,
    )


def _object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReleaseEvidenceError(f"release evidence contains duplicate key {key!r}")
        result[key] = value
    return result


def load_release_evidence(
    path: Path,
    *,
    expected_sha256: str | None = None,
    **expected: Any,
) -> ReleaseEvidence:
    """Read canonical evidence bytes from one real regular file and validate them."""

    if expected_sha256 is not None and (
        not isinstance(expected_sha256, str)
        or SHA256_PATTERN.fullmatch(expected_sha256) is None
    ):
        raise ReleaseEvidenceError("expected evidence SHA-256 is invalid")
    try:
        safe_path = reject_link_components(path, "release evidence")
    except ReleasePathSafetyError as error:
        raise ReleaseEvidenceError(str(error)) from error
    if is_link_or_junction(safe_path) or not safe_path.is_file():
        raise ReleaseEvidenceError(f"release evidence must be a real regular file: {path}")
    try:
        size = safe_path.stat().st_size
        if size > MAX_EVIDENCE_BYTES:
            raise ReleaseEvidenceError(
                f"release evidence exceeds {MAX_EVIDENCE_BYTES} bytes"
            )
        encoded = safe_path.read_bytes()
    except OSError as error:
        raise ReleaseEvidenceError(f"cannot read release evidence {path}: {error}") from error
    actual_sha256 = hashlib.sha256(encoded).hexdigest()
    if expected_sha256 is not None and actual_sha256 != expected_sha256:
        raise ReleaseEvidenceError(
            "release evidence SHA-256 does not match the validated source output"
        )
    try:
        value = json.loads(encoded, object_pairs_hook=_object_without_duplicates)
    except UnicodeDecodeError as error:
        raise ReleaseEvidenceError("release evidence is not UTF-8") from error
    except json.JSONDecodeError as error:
        raise ReleaseEvidenceError("release evidence is not valid JSON") from error
    evidence = parse_release_evidence(value, **expected)
    if encoded != canonical_json_bytes(evidence.as_dict()):
        raise ReleaseEvidenceError("release evidence is not canonically encoded")
    return evidence
