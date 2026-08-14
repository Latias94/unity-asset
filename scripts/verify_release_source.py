#!/usr/bin/env python3
"""Verify tag source identity and emit deterministic release evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Sequence

from protocol_sdk_bundle import (
    ProtocolSdkBundleError,
    ProtocolSdkBundleMetadata,
    RELEASE_TAG_PATTERN,
    verify_protocol_sdk_bundle,
)
from release_atomic import ReleaseAtomicWriteError, atomic_write_bytes
from release_contract import (
    ReleaseContractError,
    github_distribution_matrix,
    validate_local_dist_plan,
)
from release_evidence import (
    EVIDENCE_SCHEMA,
    ReleaseEvidence,
    ReleaseEvidenceError,
    canonical_json_bytes,
    parse_release_evidence,
)
from release_metadata import (
    ReleaseMetadata,
    ReleaseMetadataError,
    metadata_from_changelog,
    write_metadata_files,
)
from workspace_package_contract import (
    VerificationError,
    WorkspacePackage,
    discover_workspace_packages,
    load_toml,
    published_production_closure,
    validate_source_dependencies,
    validate_documented_feature_profiles,
)


COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
COMMAND_TIMEOUT_SECONDS = 120


@dataclass(frozen=True)
class GitIdentity:
    tag: str
    tag_object: str
    commit: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify a release tag and emit its canonical source evidence."
    )
    parser.add_argument(
        "--repository-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
    )
    parser.add_argument("--tag", required=True)
    parser.add_argument("--expected-commit")
    parser.add_argument("--dist-plan", type=Path, required=True)
    parser.add_argument("--protocol-sdk-bundle", type=Path, required=True)
    parser.add_argument("--release-title-output", type=Path, required=True)
    parser.add_argument("--release-body-output", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--cargo", default=os.environ.get("CARGO", "cargo"))
    return parser.parse_args()


def run_text(command: Sequence[str], *, cwd: Path) -> str:
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            check=False,
            text=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"command timed out after {COMMAND_TIMEOUT_SECONDS}s: {' '.join(command)}"
        ) from error
    if result.returncode != 0:
        details = "\n".join(
            part.rstrip() for part in (result.stdout, result.stderr) if part.strip()
        )
        suffix = f"\n{details}" if details else ""
        raise VerificationError(
            f"command failed with exit code {result.returncode}: "
            f"{' '.join(command)}{suffix}"
        )
    return result.stdout.strip()


def parse_release_tag(tag: str) -> str:
    match = RELEASE_TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise VerificationError(f"release tag must be vMAJOR.MINOR.PATCH, got {tag!r}")
    return ".".join(match.groups())


def verify_git_identity(
    repository_root: Path,
    tag: str,
    expected_commit: str | None,
) -> GitIdentity:
    tag_ref = f"refs/tags/{tag}"
    tag_type = run_text(["git", "cat-file", "-t", tag_ref], cwd=repository_root)
    if tag_type != "tag":
        raise VerificationError("release tag must be an annotated tag")
    tag_object = run_text(
        ["git", "rev-parse", "--verify", tag_ref], cwd=repository_root
    )
    commit = run_text(
        ["git", "rev-parse", "--verify", f"{tag_ref}^{{commit}}"],
        cwd=repository_root,
    )
    head = run_text(["git", "rev-parse", "--verify", "HEAD"], cwd=repository_root)
    for label, value in (("tag object", tag_object), ("peeled commit", commit), ("HEAD", head)):
        if COMMIT_PATTERN.fullmatch(value) is None:
            raise VerificationError(f"{label} is not a full Git object ID: {value!r}")
    if head != commit:
        raise VerificationError(
            f"checkout HEAD {head} does not match peeled tag commit {commit}"
        )
    if expected_commit is not None:
        normalized = expected_commit.lower()
        if COMMIT_PATTERN.fullmatch(normalized) is None:
            raise VerificationError(
                f"expected commit must be a full lowercase SHA-1 object ID: {expected_commit!r}"
            )
        if normalized != commit:
            raise VerificationError(
                f"event commit {normalized} does not match peeled tag commit {commit}"
            )
    dirty = run_text(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=repository_root,
    )
    if dirty:
        raise VerificationError("release checkout is not clean")
    return GitIdentity(tag=tag, tag_object=tag_object, commit=commit)


def sha256_git_blob(repository_root: Path, repository_path: str) -> str:
    object_mode = run_text(
        ["git", "ls-tree", "--format=%(objectmode)", "HEAD", "--", repository_path],
        cwd=repository_root,
    )
    if object_mode != "100644":
        raise VerificationError(
            f"release evidence requires tracked regular file {repository_path}, "
            f"got Git mode {object_mode!r}"
        )
    try:
        result = subprocess.run(
            ["git", "cat-file", "blob", f"HEAD:{repository_path}"],
            cwd=repository_root,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"git blob read timed out after {COMMAND_TIMEOUT_SECONDS}s: {repository_path}"
        ) from error
    if result.returncode != 0:
        details = result.stderr.decode("utf-8", errors="replace").rstrip()
        suffix = f"\n{details}" if details else ""
        raise VerificationError(
            f"cannot read tracked Git blob {repository_path}{suffix}"
        )
    return hashlib.sha256(result.stdout).hexdigest()


def workspace_release_contract(repository_root: Path) -> tuple[str, str]:
    root_document = load_toml(repository_root / "Cargo.toml")
    workspace = root_document.get("workspace")
    if not isinstance(workspace, dict) or workspace.get("resolver") != "3":
        raise VerificationError("release workspace must use Cargo resolver 3")
    package = root_document.get("workspace", {}).get("package")
    if not isinstance(package, dict):
        raise VerificationError("root Cargo.toml has no [workspace.package]")
    msrv = package.get("rust-version")
    if not isinstance(msrv, str) or not re.fullmatch(r"\d+\.\d+\.\d+", msrv):
        raise VerificationError(
            "workspace rust-version must be an exact patch version"
        )

    toolchain_document = load_toml(repository_root / "rust-toolchain.toml")
    toolchain = toolchain_document.get("toolchain")
    channel = toolchain.get("channel") if isinstance(toolchain, dict) else None
    if not isinstance(channel, str) or not re.fullmatch(r"\d+\.\d+\.\d+", channel):
        raise VerificationError("release Rust toolchain must be an exact patch version")
    return msrv, channel


def validate_internal_requirements(
    packages: Mapping[str, WorkspacePackage], release_version: str
) -> None:
    expected = f"^{release_version}"
    errors: list[str] = []
    for package in packages.values():
        if package.version != release_version:
            errors.append(
                f"{package.name} has version {package.version}, expected {release_version}"
            )
        for dependency in package.dependencies:
            name = dependency.get("name")
            if dependency.get("path") is None or name not in packages:
                continue
            requirement = dependency.get("req")
            if requirement != expected:
                errors.append(
                    f"{package.name} requires internal package {name} as {requirement!r}, "
                    f"expected {expected!r}"
                )
    if errors:
        raise VerificationError("invalid workspace release contract: " + "; ".join(errors))


def package_evidence(
    repository_root: Path,
    packages: Mapping[str, WorkspacePackage],
    release_version: str,
) -> list[dict[str, str]]:
    closure = published_production_closure(packages)
    validate_source_dependencies(closure, packages)
    validate_internal_requirements(packages, release_version)

    evidence: list[dict[str, str]] = []
    for package in closure:
        try:
            manifest = package.manifest_path.relative_to(repository_root).as_posix()
        except ValueError as error:
            raise VerificationError(
                f"workspace package {package.name} escaped the repository"
            ) from error
        evidence.append(
            {
                "name": package.name,
                "version": package.version,
                "manifest": manifest,
            }
        )
    return evidence


def documented_feature_profile_evidence(
    packages: Mapping[str, WorkspacePackage],
) -> list[dict[str, Any]]:
    return [
        {
            "name": profile.name,
            "package": profile.package,
            "features": list(profile.features),
            "default_features": profile.default_features,
        }
        for profile in validate_documented_feature_profiles(packages)
    ]


def dist_plan_evidence(path: Path, tag: str, version: str) -> tuple[str, list[str]]:
    if path.is_symlink() or not path.is_file():
        raise VerificationError(f"release dist plan must be a regular file: {path}")
    try:
        encoded = path.read_bytes()
        document = json.loads(encoded)
    except (OSError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read release dist plan {path}: {error}") from error
    if not isinstance(document, dict):
        raise VerificationError("release dist plan root must be an object")
    try:
        artifacts = validate_local_dist_plan(document, tag=tag, version=version)
    except ReleaseContractError as error:
        raise VerificationError(f"invalid release dist plan: {error}") from error
    return hashlib.sha256(encoded).hexdigest(), list(artifacts)


def protocol_sdk_evidence(path: Path, tag: str) -> ProtocolSdkBundleMetadata:
    try:
        return verify_protocol_sdk_bundle(path, tag)
    except ProtocolSdkBundleError as error:
        raise VerificationError(f"invalid search protocol SDK bundle: {error}") from error


def release_metadata(
    repository_root: Path,
    tag: str,
    version: str,
    title_output: Path,
    body_output: Path,
) -> ReleaseMetadata:
    try:
        metadata = metadata_from_changelog(
            repository_root / "CHANGELOG.md",
            tag,
            version,
        )
        write_metadata_files(metadata, title_output, body_output)
        return metadata
    except ReleaseMetadataError as error:
        raise VerificationError(f"invalid GitHub Release metadata: {error}") from error


def write_canonical_json(path: Path, payload: Mapping[str, Any]) -> str:
    encoded = canonical_json_bytes(payload)
    try:
        atomic_write_bytes(path, encoded, "release evidence")
    except ReleaseAtomicWriteError as error:
        raise VerificationError(f"cannot write release evidence {path}: {error}") from error
    return hashlib.sha256(encoded).hexdigest()


def append_github_outputs(
    path: Path,
    *,
    evidence: ReleaseEvidence,
    evidence_sha256: str,
    dist_matrix: Mapping[str, Any],
) -> None:
    lines = [
        f"version={evidence.version}",
        f"commit={evidence.commit}",
        f"tag_object={evidence.tag_object}",
        f"publish_crates={' '.join(evidence.publish_order)}",
        f"evidence_sha256={evidence_sha256}",
        f"msrv={evidence.msrv}",
        f"release_toolchain={evidence.release_toolchain}",
        f"dist_plan_sha256={evidence.dist_plan_sha256}",
        f"protocol_sdk_artifact={evidence.protocol_sdk['artifact_name']}",
        "dist_matrix="
        + json.dumps(dist_matrix, ensure_ascii=True, sort_keys=True, separators=(",", ":")),
    ]
    try:
        with path.open("a", encoding="utf-8", newline="\n") as stream:
            stream.write("\n".join(lines) + "\n")
    except OSError as error:
        raise VerificationError(f"cannot append GitHub outputs to {path}: {error}") from error


def main() -> int:
    args = parse_args()
    repository_root = args.repository_root.resolve()
    version = parse_release_tag(args.tag)
    identity = verify_git_identity(repository_root, args.tag, args.expected_commit)
    msrv, release_toolchain = workspace_release_contract(repository_root)
    dist_plan_sha256, dist_artifacts = dist_plan_evidence(
        args.dist_plan, args.tag, version
    )
    protocol_sdk = protocol_sdk_evidence(args.protocol_sdk_bundle, args.tag)
    github_release = release_metadata(
        repository_root,
        args.tag,
        version,
        args.release_title_output,
        args.release_body_output,
    )
    metadata_text = run_text(
        [
            args.cargo,
            "metadata",
            "--manifest-path",
            str(repository_root / "Cargo.toml"),
            "--no-deps",
            "--format-version",
            "1",
            "--locked",
        ],
        cwd=repository_root,
    )
    workspace_packages = discover_workspace_packages(metadata_text)
    packages = package_evidence(repository_root, workspace_packages, version)
    feature_profiles = documented_feature_profile_evidence(workspace_packages)
    raw_payload = {
        "schema": EVIDENCE_SCHEMA,
        "tag": identity.tag,
        "tag_object": identity.tag_object,
        "commit": identity.commit,
        "version": version,
        "cargo_lock_sha256": sha256_git_blob(repository_root, "Cargo.lock"),
        "msrv": msrv,
        "release_toolchain": release_toolchain,
        "dist_plan_sha256": dist_plan_sha256,
        "dist_artifacts": dist_artifacts,
        "protocol_sdk": protocol_sdk.as_dict(),
        "github_release": github_release.evidence(),
        "packages": packages,
        "documented_feature_profiles": feature_profiles,
    }
    try:
        evidence = parse_release_evidence(
            raw_payload,
            expected_tag=identity.tag,
            expected_version=version,
            expected_commit=identity.commit,
            expected_tag_object=identity.tag_object,
            expected_dist_plan_sha256=dist_plan_sha256,
            expected_dist_artifacts=dist_artifacts,
            expected_protocol_sdk=protocol_sdk.as_dict(),
            expected_github_release=github_release.evidence(),
        )
    except ReleaseEvidenceError as error:
        raise VerificationError(f"invalid constructed release evidence: {error}") from error
    dist_matrix = github_distribution_matrix()
    evidence_sha256 = write_canonical_json(args.evidence, evidence.as_dict())
    if args.github_output is not None:
        append_github_outputs(
            args.github_output,
            evidence=evidence,
            evidence_sha256=evidence_sha256,
            dist_matrix=dist_matrix,
        )
    print(
        f"release source verified: {args.tag} -> {identity.commit}; "
        f"{len(packages)} packages; evidence sha256 {evidence_sha256}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
