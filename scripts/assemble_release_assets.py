#!/usr/bin/env python3
"""Assemble collision-free release assets and a deterministic checksum file."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
from pathlib import Path

from protocol_sdk_bundle import ProtocolSdkBundleError, verify_protocol_sdk_bundle
from release_contract import (
    DistributionArtifactPair,
    ReleaseContractError,
    validate_local_dist_plan_matrix,
)
from release_evidence import ReleaseEvidenceError, load_release_evidence
from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    reject_link_components,
)


class AssemblyError(RuntimeError):
    """An actionable release asset assembly failure."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Assemble release binaries, source evidence, and checksums."
    )
    parser.add_argument("--dist-root", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-evidence-sha256", required=True)
    parser.add_argument("--dist-plan", type=Path, required=True)
    parser.add_argument("--expected-dist-plan-sha256", required=True)
    parser.add_argument("--protocol-sdk-bundle", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise AssemblyError(f"cannot read {path}: {error}") from error
    return digest.hexdigest()


def resolve_real_path(path: Path, label: str, *, require_exists: bool) -> Path:
    try:
        absolute = reject_link_components(path, label)
    except ReleasePathSafetyError as error:
        raise AssemblyError(str(error)) from error
    try:
        resolved = absolute.resolve(strict=require_exists)
    except OSError as error:
        raise AssemblyError(f"cannot resolve {label} path {path}: {error}") from error
    if is_link_or_junction(resolved):
        raise AssemblyError(f"{label} must not be a symlink or junction: {path}")
    return resolved


def require_regular_file(path: Path, label: str) -> None:
    if is_link_or_junction(path) or not path.is_file():
        raise AssemblyError(f"{label} must be a regular file: {path}")


def load_dist_plan(
    path: Path,
    expected_sha256: str,
    *,
    tag: str,
    version: str,
) -> tuple[Path, tuple[DistributionArtifactPair, ...]]:
    resolved = resolve_real_path(path, "release dist plan", require_exists=True)
    require_regular_file(resolved, "release dist plan")
    try:
        encoded = resolved.read_bytes()
    except OSError as error:
        raise AssemblyError(f"cannot read release dist plan {resolved}: {error}") from error
    actual_sha256 = hashlib.sha256(encoded).hexdigest()
    if actual_sha256 != expected_sha256:
        raise AssemblyError(
            "release dist plan SHA-256 mismatch: "
            f"expected {expected_sha256}, got {actual_sha256}"
        )
    try:
        document = json.loads(encoded)
    except json.JSONDecodeError as error:
        raise AssemblyError(f"cannot read release dist plan {resolved}: {error}") from error
    if not isinstance(document, dict):
        raise AssemblyError("release dist plan root must be an object")
    try:
        artifacts = validate_local_dist_plan_matrix(document, tag=tag, version=version)
    except ReleaseContractError as error:
        raise AssemblyError(f"invalid release dist plan: {error}") from error
    return resolved, artifacts


def validate_checksum_sidecars(
    artifacts: dict[str, Path], matrix: tuple[DistributionArtifactPair, ...]
) -> None:
    for pair in matrix:
        sidecar = artifacts[pair.checksum_name]
        try:
            content = sidecar.read_text(encoding="ascii")
        except (OSError, UnicodeDecodeError) as error:
            raise AssemblyError(f"cannot read checksum sidecar {sidecar}: {error}") from error
        pattern = rf"([0-9a-f]{{64}})  {re.escape(pair.archive_name)}\n"
        match = re.fullmatch(pattern, content)
        if match is None:
            raise AssemblyError(
                f"checksum sidecar {pair.checksum_name} must contain one SHA-256 digest "
                f"and filename {pair.archive_name!r}"
            )
        actual_digest = sha256_file(artifacts[pair.archive_name])
        if match.group(1) != actual_digest:
            raise AssemblyError(
                f"checksum digest mismatch for {pair.archive_name}: "
                f"expected {actual_digest}, got {match.group(1)}"
            )


def collect_dist_files(dist_root: Path, expected_names: tuple[str, ...]) -> dict[str, Path]:
    if not dist_root.is_dir() or is_link_or_junction(dist_root):
        raise AssemblyError(f"dist root must be a real directory: {dist_root}")
    files: dict[str, Path] = {}
    pending = [dist_root]
    while pending:
        directory = pending.pop()
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as error:
            raise AssemblyError(f"cannot enumerate dist artifact tree {directory}: {error}") from error
        for entry in entries:
            path = Path(entry.path)
            if is_link_or_junction(path):
                raise AssemblyError(f"dist artifact tree contains a symlink or junction: {path}")
            if entry.is_dir(follow_symlinks=False):
                pending.append(path)
                continue
            if not entry.is_file(follow_symlinks=False):
                raise AssemblyError(f"dist artifact tree contains a non-regular entry: {path}")
            name = path.name
            if name in {"SHA256SUMS", "release-evidence.json", "release-dist-plan.json"}:
                raise AssemblyError(f"dist artifact uses a reserved release name: {name}")
            if "\n" in name or "\r" in name:
                raise AssemblyError(f"dist artifact name contains a line break: {name!r}")
            previous = files.get(name)
            if previous is not None:
                raise AssemblyError(
                    f"dist artifacts collide after release flattening: {previous} and {path}"
                )
            files[name] = path
    expected = set(expected_names)
    actual = set(files)
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        details: list[str] = []
        if missing:
            details.append(f"missing: {', '.join(missing)}")
        if unexpected:
            details.append(f"unexpected: {', '.join(unexpected)}")
        raise AssemblyError("dist artifact inventory differs from the release plan (" + "; ".join(details) + ")")
    return files


def assemble(
    dist_root: Path,
    evidence: Path,
    expected_evidence_sha256: str,
    dist_plan: Path,
    expected_dist_plan_sha256: str,
    protocol_sdk_bundle: Path,
    output: Path,
) -> None:
    dist_root = resolve_real_path(dist_root, "dist root", require_exists=True)
    evidence = resolve_real_path(evidence, "release evidence", require_exists=True)
    output = resolve_real_path(output, "release asset output", require_exists=False)
    if output == dist_root or output.is_relative_to(dist_root):
        raise AssemblyError("output directory must not be inside the dist artifact tree")
    if output.exists():
        if is_link_or_junction(output) or not output.is_dir() or any(output.iterdir()):
            raise AssemblyError(f"output directory must be empty or absent: {output}")
    require_regular_file(evidence, "release evidence")
    try:
        release_evidence = load_release_evidence(
            evidence,
            expected_sha256=expected_evidence_sha256,
        )
    except ReleaseEvidenceError as error:
        raise AssemblyError(f"invalid release evidence: {error}") from error
    tag = release_evidence.tag
    version = release_evidence.version
    resolved_dist_plan, artifact_matrix = load_dist_plan(
        dist_plan,
        expected_dist_plan_sha256,
        tag=tag,
        version=version,
    )
    if release_evidence.dist_plan_sha256 != expected_dist_plan_sha256:
        raise AssemblyError("release evidence does not bind the expected dist plan")
    expected_artifacts = tuple(
        sorted(
            name
            for pair in artifact_matrix
            for name in (pair.archive_name, pair.checksum_name)
        )
    )
    if release_evidence.dist_artifacts != expected_artifacts:
        raise AssemblyError("release evidence does not bind the expected dist artifact inventory")
    protocol_sdk_bundle = resolve_real_path(
        protocol_sdk_bundle,
        "search protocol SDK bundle",
        require_exists=True,
    )
    require_regular_file(protocol_sdk_bundle, "search protocol SDK bundle")
    try:
        protocol_sdk = verify_protocol_sdk_bundle(protocol_sdk_bundle, tag)
    except ProtocolSdkBundleError as error:
        raise AssemblyError(f"invalid search protocol SDK bundle: {error}") from error
    if release_evidence.protocol_sdk != protocol_sdk.as_dict():
        raise AssemblyError(
            "release evidence does not bind the search protocol SDK bundle"
        )
    if protocol_sdk.artifact_name in expected_artifacts:
        raise AssemblyError("search protocol SDK bundle collides with a dist artifact")

    artifacts = collect_dist_files(dist_root, expected_artifacts)
    validate_checksum_sidecars(artifacts, artifact_matrix)
    output.mkdir(parents=True, exist_ok=True)
    for name, source in artifacts.items():
        shutil.copyfile(source, output / name)
    shutil.copyfile(evidence, output / "release-evidence.json")
    shutil.copyfile(resolved_dist_plan, output / "release-dist-plan.json")
    shutil.copyfile(
        protocol_sdk_bundle,
        output / protocol_sdk.artifact_name,
    )

    checksummed_names = sorted(
        (
            *artifacts,
            "release-evidence.json",
            "release-dist-plan.json",
            protocol_sdk.artifact_name,
        )
    )
    checksum_lines = [
        f"{sha256_file(output / name)}  {name}\n" for name in checksummed_names
    ]
    (output / "SHA256SUMS").write_text("".join(checksum_lines), encoding="utf-8")


def main() -> int:
    args = parse_args()
    assemble(
        args.dist_root,
        args.evidence,
        args.expected_evidence_sha256,
        args.dist_plan,
        args.expected_dist_plan_sha256,
        args.protocol_sdk_bundle,
        args.output,
    )
    print(f"assembled {args.output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssemblyError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
