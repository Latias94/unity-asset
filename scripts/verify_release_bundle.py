#!/usr/bin/env python3
"""Verify the complete assembled release bundle without publishing it."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any, Mapping, Sequence

from protocol_sdk_bundle import ProtocolSdkBundleError, verify_protocol_sdk_bundle
from release_contract import ReleaseContractError, validate_local_dist_plan
from release_evidence import ReleaseEvidenceError, load_release_evidence
from release_metadata import ReleaseMetadataError, verify_metadata_files
from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    reject_link_components,
)


TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
CHECKSUM_LINE_PATTERN = re.compile(r"([0-9a-f]{64})  ([^\r\n]+)\n")


class ReleaseBundleError(RuntimeError):
    """The assembled release bundle differs from its evidence."""


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify a complete release asset directory without publishing it."
    )
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--expected-evidence-sha256", required=True)
    parser.add_argument("--release-title", type=Path, required=True)
    parser.add_argument("--release-body", type=Path, required=True)
    return parser.parse_args(argv)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ReleaseBundleError(f"cannot read release asset {path}: {error}") from error
    return digest.hexdigest()


def regular_files(root: Path) -> Mapping[str, Path]:
    try:
        root = reject_link_components(root, "release bundle")
    except ReleasePathSafetyError as error:
        raise ReleaseBundleError(str(error)) from error
    if is_link_or_junction(root) or not root.is_dir():
        raise ReleaseBundleError(f"release bundle must be a real directory: {root}")
    files: dict[str, Path] = {}
    for path in sorted(root.iterdir(), key=lambda candidate: candidate.name):
        if is_link_or_junction(path) or not path.is_file():
            raise ReleaseBundleError(
                f"release bundle must contain only flat regular files: {path}"
            )
        if "\n" in path.name or "\r" in path.name:
            raise ReleaseBundleError(
                f"release asset name contains a line break: {path.name!r}"
            )
        files[path.name] = path
    if not files:
        raise ReleaseBundleError("release bundle is empty")
    return files


def read_json(path: Path, label: str) -> Mapping[str, Any]:
    try:
        document = json.loads(path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise ReleaseBundleError(f"cannot read {label} {path}: {error}") from error
    if not isinstance(document, dict):
        raise ReleaseBundleError(f"{label} root must be an object")
    return document


def verify_checksum_manifest(files: Mapping[str, Path]) -> None:
    manifest = files.get("SHA256SUMS")
    if manifest is None:
        raise ReleaseBundleError("release bundle omitted SHA256SUMS")
    try:
        encoded = manifest.read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError) as error:
        raise ReleaseBundleError(f"cannot read SHA256SUMS: {error}") from error
    matches = list(CHECKSUM_LINE_PATTERN.finditer(encoded))
    if "".join(match.group(0) for match in matches) != encoded:
        raise ReleaseBundleError("SHA256SUMS is not canonically encoded")
    entries = {match.group(2): match.group(1) for match in matches}
    if len(entries) != len(matches):
        raise ReleaseBundleError("SHA256SUMS contains duplicate filenames")
    expected_names = set(files) - {"SHA256SUMS"}
    if set(entries) != expected_names:
        raise ReleaseBundleError("SHA256SUMS inventory does not match release assets")
    for name, expected_digest in entries.items():
        if sha256_file(files[name]) != expected_digest:
            raise ReleaseBundleError(f"SHA256SUMS digest mismatch for {name}")


def verify_sidecars(files: Mapping[str, Path], dist_artifacts: Sequence[str]) -> None:
    for checksum_name in sorted(
        name for name in dist_artifacts if name.endswith(".sha256")
    ):
        archive_name = checksum_name.removesuffix(".sha256")
        try:
            encoded = files[checksum_name].read_text(encoding="ascii")
        except (KeyError, OSError, UnicodeDecodeError) as error:
            raise ReleaseBundleError(
                f"cannot read dist checksum sidecar {checksum_name}: {error}"
            ) from error
        expected = f"{sha256_file(files[archive_name])}  {archive_name}\n"
        if encoded != expected:
            raise ReleaseBundleError(
                f"dist checksum sidecar does not match {archive_name}"
            )


def verify_release_bundle(
    assets: Path,
    tag: str,
    expected_evidence_sha256: str,
    *,
    release_title: Path | None = None,
    release_body: Path | None = None,
) -> None:
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ReleaseBundleError(f"invalid stable release tag: {tag!r}")
    version = ".".join(match.groups())
    files = regular_files(assets)
    required = {"release-evidence.json", "release-dist-plan.json", "SHA256SUMS"}
    if not required.issubset(files):
        raise ReleaseBundleError(
            "release bundle omitted provenance files: "
            + ", ".join(sorted(required - set(files)))
        )
    evidence_path = files["release-evidence.json"]
    try:
        evidence = load_release_evidence(
            evidence_path,
            expected_sha256=expected_evidence_sha256,
            expected_tag=tag,
            expected_version=version,
        )
    except ReleaseEvidenceError as error:
        raise ReleaseBundleError(f"invalid release evidence: {error}") from error

    dist_plan_path = files["release-dist-plan.json"]
    if sha256_file(dist_plan_path) != evidence.dist_plan_sha256:
        raise ReleaseBundleError("release dist plan digest does not match release evidence")
    dist_plan = read_json(dist_plan_path, "release dist plan")
    try:
        dist_artifacts = validate_local_dist_plan(dist_plan, tag=tag, version=version)
    except ReleaseContractError as error:
        raise ReleaseBundleError(f"invalid release dist plan: {error}") from error
    if evidence.dist_artifacts != tuple(dist_artifacts):
        raise ReleaseBundleError("release evidence does not bind the dist inventory")

    protocol_name = evidence.protocol_sdk["artifact_name"]
    if protocol_name not in files:
        raise ReleaseBundleError("release bundle omitted the protocol SDK artifact")
    try:
        protocol = verify_protocol_sdk_bundle(files[protocol_name], tag)
    except ProtocolSdkBundleError as error:
        raise ReleaseBundleError(f"invalid protocol SDK artifact: {error}") from error
    if protocol.as_dict() != evidence.protocol_sdk:
        raise ReleaseBundleError("protocol SDK bytes do not match release evidence")

    if (release_title is None) != (release_body is None):
        raise ReleaseBundleError("release title and body proof paths must be provided together")
    if release_title is not None and release_body is not None:
        try:
            title_path = reject_link_components(release_title, "release title")
        except ReleasePathSafetyError as error:
            raise ReleaseBundleError(str(error)) from error
        if is_link_or_junction(title_path) or not title_path.is_file():
            raise ReleaseBundleError(f"release title must be a real regular file: {release_title}")
        try:
            title_bytes = title_path.read_bytes()
        except OSError as error:
            raise ReleaseBundleError(f"cannot read release title {release_title}: {error}") from error
        if title_bytes != f"{tag}\n".encode("utf-8"):
            raise ReleaseBundleError("release title proof does not match the release tag")
        try:
            verify_metadata_files(evidence_path, tag, release_body)
        except ReleaseMetadataError as error:
            raise ReleaseBundleError(
                f"release title or body does not match release evidence: {error}"
            ) from error

    expected_names = {
        *dist_artifacts,
        "SHA256SUMS",
        "release-dist-plan.json",
        "release-evidence.json",
        protocol_name,
    }
    if set(files) != expected_names:
        raise ReleaseBundleError("release bundle inventory does not match release evidence")
    verify_sidecars(files, dist_artifacts)
    verify_checksum_manifest(files)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    verify_release_bundle(
        args.assets,
        args.tag,
        args.expected_evidence_sha256,
        release_title=args.release_title,
        release_body=args.release_body,
    )
    print(f"release dry-run bundle verified for {args.tag}: {args.assets}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ReleaseBundleError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
