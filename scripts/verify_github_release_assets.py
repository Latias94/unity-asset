#!/usr/bin/env python3
"""Fail closed on GitHub Release asset identity at every publication boundary."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

from release_contract import GIT_OBJECT_PATTERN
from release_evidence import (
    ReleaseEvidenceError,
    TAG_PATTERN,
    load_release_evidence,
)
from release_metadata import (
    ReleaseMetadata,
    ReleaseMetadataError,
    normalize_body as normalize_release_body,
    normalize_title as normalize_release_title,
    verify_metadata_files,
)
from verify_release_bundle import (
    ReleaseBundleError,
    VerifiedReleaseAsset,
    verify_release_bundle,
)

GITHUB_REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
GH_COMMAND_TIMEOUT_SECONDS = 60
ASSET_DOWNLOAD_TIMEOUT_SECONDS = 120


class ReleaseAssetError(RuntimeError):
    """An actionable GitHub Release asset verification failure."""


@dataclass(frozen=True)
class RemoteAsset:
    asset_id: int
    name: str
    size: int
    state: str = "uploaded"


@dataclass(frozen=True)
class RemoteAssetVerification:
    uploaded_names: frozenset[str]
    starter_asset_ids: tuple[int, ...]


@dataclass(frozen=True)
class ReleaseState:
    release_id: int | None
    draft: bool | None
    needs_upload: bool
    starter_asset_ids: tuple[int, ...] = ()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify GitHub Release assets against local release artifacts."
    )
    parser.add_argument("--github-repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument(
        "--phase",
        choices=("preflight", "staged", "published", "publish"),
        required=True,
    )
    parser.add_argument("--expected-release-id", type=int)
    parser.add_argument("--expected-title", required=True)
    parser.add_argument("--expected-body-file", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--expected-evidence-sha256", required=True)
    parser.add_argument("--github-output", type=Path)
    return parser.parse_args()


def read_expected_release_metadata(
    title: str,
    body_path: Path,
    evidence_path: Path,
) -> ReleaseMetadata:
    try:
        return verify_metadata_files(evidence_path, title, body_path)
    except ReleaseMetadataError as error:
        raise ReleaseAssetError(f"invalid verified GitHub Release metadata: {error}") from error


def run_gh(
    arguments: Sequence[str],
    *,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            ["gh", *arguments],
            check=False,
            text=True,
            encoding="utf-8",
            input=input_text,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=GH_COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise ReleaseAssetError(
            f"GitHub CLI command timed out after {GH_COMMAND_TIMEOUT_SECONDS}s: "
            f"{' '.join(arguments)}"
        ) from error
    except OSError as error:
        raise ReleaseAssetError(
            f"cannot execute GitHub CLI command {' '.join(arguments)}: {error}"
        ) from error


def gh_json(
    method: str,
    endpoint: str,
    *,
    allow_not_found: bool = False,
    paginate: bool = False,
    fields: Sequence[tuple[str, str]] = (),
    raw_fields: Sequence[tuple[str, str]] = (),
    json_body: Mapping[str, Any] | None = None,
) -> Any | None:
    if json_body is not None and (fields or raw_fields):
        raise ReleaseAssetError(
            "GitHub API JSON input cannot be combined with form fields"
        )
    arguments = [
        "api",
        "--method",
        method,
        "--header",
        "X-GitHub-Api-Version: 2022-11-28",
    ]
    if paginate:
        arguments.extend(("--paginate", "--slurp"))
    for key, value in fields:
        arguments.extend(("-F", f"{key}={value}"))
    for key, value in raw_fields:
        arguments.extend(("-f", f"{key}={value}"))
    request_body = None
    if json_body is not None:
        try:
            request_body = json.dumps(
                json_body,
                ensure_ascii=False,
                separators=(",", ":"),
            )
        except (TypeError, ValueError) as error:
            raise ReleaseAssetError("GitHub API request body is not valid JSON") from error
        arguments.extend(("--input", "-"))
    arguments.append(endpoint)
    result = run_gh(arguments, input_text=request_body)
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        if allow_not_found and "HTTP 404" in details:
            return None
        suffix = f"\n{details}" if details else ""
        raise ReleaseAssetError(f"GitHub API request failed: {endpoint}{suffix}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ReleaseAssetError(f"GitHub API returned invalid JSON for {endpoint}") from error


def fetch_release(repository: str, tag: str) -> Mapping[str, Any] | None:
    payload = gh_json(
        "GET", f"repos/{repository}/releases/tags/{tag}", allow_not_found=True
    )
    if payload is None:
        return None
    if not isinstance(payload, Mapping):
        raise ReleaseAssetError("GitHub Release endpoint returned a non-object")
    return payload


def list_remote_assets(repository: str, release_id: int) -> list[RemoteAsset]:
    payload = gh_json(
        "GET",
        f"repos/{repository}/releases/{release_id}/assets?per_page=100",
        paginate=True,
    )
    if not isinstance(payload, list):
        raise ReleaseAssetError("GitHub Release assets endpoint returned a non-array")
    pages = payload
    if pages and not all(isinstance(page, list) for page in pages):
        pages = [pages]
    assets: list[RemoteAsset] = []
    names: set[str] = set()
    for page in pages:
        if not isinstance(page, list):
            raise ReleaseAssetError("GitHub Release asset page is not an array")
        for raw_asset in page:
            if not isinstance(raw_asset, Mapping):
                raise ReleaseAssetError("GitHub Release asset is not an object")
            asset_id = raw_asset.get("id")
            name = raw_asset.get("name")
            size = raw_asset.get("size")
            state = raw_asset.get("state")
            if (
                not isinstance(asset_id, int)
                or not isinstance(name, str)
                or not isinstance(size, int)
                or not isinstance(state, str)
            ):
                raise ReleaseAssetError(
                    "GitHub Release asset has invalid id, name, size, or state"
                )
            if state not in {"uploaded", "starter"}:
                raise ReleaseAssetError(
                    f"GitHub Release asset {name} has unsupported state: {state}"
                )
            if name in names:
                raise ReleaseAssetError(f"GitHub Release has duplicate asset name: {name}")
            names.add(name)
            assets.append(
                RemoteAsset(asset_id=asset_id, name=name, size=size, state=state)
            )
    return assets


def download_remote_asset(
    repository: str,
    asset: RemoteAsset,
    expected: VerifiedReleaseAsset,
) -> str:
    arguments = [
        "api",
        "--method",
        "GET",
        "--header",
        "Accept: application/octet-stream",
        "--header",
        "X-GitHub-Api-Version: 2022-11-28",
        f"repos/{repository}/releases/assets/{asset.asset_id}",
    ]
    try:
        with tempfile.TemporaryFile(mode="w+b") as stderr:
            process = subprocess.Popen(
                ["gh", *arguments],
                stdout=subprocess.PIPE,
                stderr=stderr,
            )
            if process.stdout is None:
                raise ReleaseAssetError(
                    f"cannot capture GitHub Release asset {asset.name}"
                )
            digest = hashlib.sha256()
            bytes_read = 0
            stream_error: list[OSError] = []
            exceeded_limit = threading.Event()

            def consume_stdout() -> None:
                nonlocal bytes_read
                try:
                    for chunk in iter(lambda: process.stdout.read(1024 * 1024), b""):
                        bytes_read += len(chunk)
                        if bytes_read > expected.size:
                            exceeded_limit.set()
                            try:
                                process.kill()
                            except OSError:
                                pass
                            return
                        digest.update(chunk)
                except OSError as error:
                    stream_error.append(error)
                    try:
                        process.kill()
                    except OSError:
                        pass
                finally:
                    process.stdout.close()

            reader = threading.Thread(
                target=consume_stdout,
                name="github-release-asset-reader",
                daemon=True,
            )
            reader.start()
            try:
                returncode = process.wait(timeout=ASSET_DOWNLOAD_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired as error:
                try:
                    process.kill()
                except OSError:
                    pass
                process.wait()
                reader.join()
                raise ReleaseAssetError(
                    f"GitHub Release asset download timed out after "
                    f"{ASSET_DOWNLOAD_TIMEOUT_SECONDS}s: {asset.name}"
                ) from error
            reader.join()
            if stream_error:
                raise ReleaseAssetError(
                    f"cannot read downloaded GitHub Release asset {asset.name}: "
                    f"{stream_error[0]}"
                ) from stream_error[0]
            if exceeded_limit.is_set():
                raise ReleaseAssetError(
                    f"downloaded GitHub Release asset {asset.name} exceeds the "
                    f"expected {expected.size}-byte limit"
                )
            if returncode != 0:
                stderr.seek(0)
                details = stderr.read(64 * 1024).decode(
                    "utf-8", errors="replace"
                ).strip()
                raise ReleaseAssetError(
                    f"cannot download GitHub Release asset {asset.name}: "
                    f"{details or returncode}"
                )
            if bytes_read != expected.size:
                raise ReleaseAssetError(
                    f"downloaded GitHub Release asset {asset.name} has size "
                    f"{bytes_read}, expected {expected.size}"
                )
            return digest.hexdigest()
    except OSError as error:
        raise ReleaseAssetError(
            f"cannot stage downloaded GitHub Release asset {asset.name}: {error}"
        ) from error


def delete_remote_asset(repository: str, asset_id: int) -> None:
    result = run_gh(
        [
            "api",
            "--method",
            "DELETE",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            f"repos/{repository}/releases/assets/{asset_id}",
        ]
    )
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        suffix = f": {details}" if details else ""
        raise ReleaseAssetError(
            f"cannot delete failed GitHub Release starter asset {asset_id}{suffix}"
        )


def release_metadata(
    release: Mapping[str, Any],
    *,
    tag: str,
    commit: str,
    expected_metadata: ReleaseMetadata,
) -> tuple[int, bool]:
    release_id = release.get("id")
    target = release.get("target_commitish")
    draft = release.get("draft")
    prerelease = release.get("prerelease")
    if not isinstance(release_id, int) or not isinstance(draft, bool):
        raise ReleaseAssetError("GitHub Release has invalid id or draft state")
    if release.get("tag_name") != tag or target != commit:
        raise ReleaseAssetError("GitHub Release tag or target commit does not match source evidence")
    if prerelease is not False:
        raise ReleaseAssetError("stable release cannot be a GitHub prerelease")
    title = release.get("name")
    body = release.get("body")
    if not isinstance(title, str) or not isinstance(body, str):
        raise ReleaseAssetError("GitHub Release has invalid title or body")
    try:
        actual_metadata = ReleaseMetadata(
            title=normalize_release_title(title),
            body=normalize_release_body(body),
        )
    except ReleaseMetadataError as error:
        raise ReleaseAssetError(f"GitHub Release has invalid title or body: {error}") from error
    if actual_metadata != expected_metadata:
        raise ReleaseAssetError(
            "GitHub Release title or body does not match verified release metadata"
        )
    return release_id, draft


def verify_remote_assets(
    expected: Mapping[str, VerifiedReleaseAsset],
    remote_assets: Sequence[RemoteAsset],
    downloader: Callable[[RemoteAsset, VerifiedReleaseAsset], str],
    *,
    allow_expected_starters: bool = False,
) -> RemoteAssetVerification:
    remote_by_name: dict[str, RemoteAsset] = {}
    for asset in remote_assets:
        if asset.name in remote_by_name:
            raise ReleaseAssetError(
                f"GitHub Release has duplicate asset name: {asset.name}"
            )
        remote_by_name[asset.name] = asset
    unexpected = sorted(set(remote_by_name) - set(expected))
    if unexpected:
        raise ReleaseAssetError(
            "GitHub Release contains unexpected assets: " + ", ".join(unexpected)
        )
    uploaded_names: set[str] = set()
    starter_asset_ids: list[int] = []
    for name, remote in sorted(remote_by_name.items()):
        if remote.state == "starter":
            if not allow_expected_starters:
                raise ReleaseAssetError(
                    f"GitHub Release asset {name} remains in starter state"
                )
            starter_asset_ids.append(remote.asset_id)
            continue
        if remote.state != "uploaded":
            raise ReleaseAssetError(
                f"GitHub Release asset {name} has unsupported state: {remote.state}"
            )
        local = expected[name]
        if remote.size != local.size:
            raise ReleaseAssetError(
                f"GitHub Release asset {name} has size {remote.size}, expected {local.size}"
            )
        actual_sha256 = downloader(remote, local)
        if actual_sha256 != local.sha256:
            raise ReleaseAssetError(
                f"GitHub Release asset {name} SHA-256 mismatch: "
                f"expected {local.sha256}, got {actual_sha256}"
            )
        uploaded_names.add(name)
    return RemoteAssetVerification(
        uploaded_names=frozenset(uploaded_names),
        starter_asset_ids=tuple(sorted(starter_asset_ids)),
    )


def examine_release(
    expected: Mapping[str, VerifiedReleaseAsset],
    release: Mapping[str, Any] | None,
    *,
    tag: str,
    commit: str,
    phase: str,
    expected_metadata: ReleaseMetadata,
    assets_for_release: Callable[[int], Sequence[RemoteAsset]],
    download: Callable[[RemoteAsset, VerifiedReleaseAsset], str],
    expected_release_id: int | None = None,
) -> ReleaseState:
    if release is None:
        if phase != "preflight":
            raise ReleaseAssetError("GitHub Release does not exist after asset publication")
        return ReleaseState(release_id=None, draft=None, needs_upload=True)

    release_id, draft = release_metadata(
        release,
        tag=tag,
        commit=commit,
        expected_metadata=expected_metadata,
    )
    if expected_release_id is not None and release_id != expected_release_id:
        raise ReleaseAssetError(
            f"GitHub Release ID {release_id} does not match staged release ID "
            f"{expected_release_id}"
        )
    if phase == "published" and draft:
        raise ReleaseAssetError("GitHub Release remains a draft after final publication")

    remote = verify_remote_assets(
        expected,
        assets_for_release(release_id),
        download,
        allow_expected_starters=phase == "preflight" and draft,
    )
    expected_names = set(expected)
    if not draft and remote.uploaded_names != expected_names:
        raise ReleaseAssetError(
            "published GitHub Release asset set is incomplete and cannot be recovered"
        )
    if phase == "preflight":
        return ReleaseState(
            release_id=release_id,
            draft=draft,
            needs_upload=draft
            and (
                remote.uploaded_names != expected_names or bool(remote.starter_asset_ids)
            ),
            starter_asset_ids=remote.starter_asset_ids,
        )
    if remote.uploaded_names != expected_names:
        raise ReleaseAssetError("GitHub Release asset set is incomplete after publication")
    return ReleaseState(release_id=release_id, draft=draft, needs_upload=False)


def publish_draft(
    repository: str,
    release_id: int,
    *,
    tag: str,
    commit: str,
    metadata: ReleaseMetadata,
) -> None:
    payload = gh_json(
        "PATCH",
        f"repos/{repository}/releases/{release_id}",
        json_body={
            "tag_name": tag,
            "target_commitish": commit,
            "name": metadata.title,
            "body": metadata.body,
            "draft": False,
            "prerelease": False,
        },
    )
    if not isinstance(payload, Mapping):
        raise ReleaseAssetError("GitHub Release publish response is not an object")


def append_github_outputs(path: Path, state: ReleaseState) -> None:
    lines = [
        f"needs_upload={'true' if state.needs_upload else 'false'}",
        f"release_id={'' if state.release_id is None else state.release_id}",
        f"release_state={'absent' if state.draft is None else ('draft' if state.draft else 'published')}",
    ]
    try:
        with path.open("a", encoding="utf-8", newline="\n") as stream:
            stream.write("\n".join(lines) + "\n")
    except OSError as error:
        raise ReleaseAssetError(f"cannot append GitHub outputs to {path}: {error}") from error


def main() -> int:
    args = parse_args()
    if GITHUB_REPOSITORY_PATTERN.fullmatch(args.github_repository) is None:
        raise ReleaseAssetError(f"invalid GitHub repository name: {args.github_repository!r}")
    if TAG_PATTERN.fullmatch(args.tag) is None:
        raise ReleaseAssetError(f"invalid stable release tag: {args.tag!r}")
    commit = args.commit.lower()
    if GIT_OBJECT_PATTERN.fullmatch(commit) is None:
        raise ReleaseAssetError(f"invalid release commit: {args.commit!r}")
    expected_release_id = args.expected_release_id
    if expected_release_id is not None and expected_release_id < 1:
        raise ReleaseAssetError("expected GitHub Release ID must be positive")
    if args.phase == "publish" and expected_release_id is None:
        raise ReleaseAssetError(
            "final publication requires the staged GitHub Release ID"
        )
    try:
        bundle = verify_release_bundle(
            args.assets,
            args.tag,
            args.expected_evidence_sha256,
            expected_commit=commit,
        )
    except ReleaseBundleError as error:
        raise ReleaseAssetError(f"invalid local release bundle: {error}") from error
    try:
        proof_evidence = load_release_evidence(
            args.evidence,
            expected_sha256=args.expected_evidence_sha256,
            expected_tag=args.tag,
            expected_commit=commit,
        )
    except ReleaseEvidenceError as error:
        raise ReleaseAssetError(
            f"invalid verified release evidence: {error}"
        ) from error
    if proof_evidence != bundle.evidence:
        raise ReleaseAssetError(
            "verified release metadata evidence differs from the release bundle"
        )
    expected = bundle.assets
    expected_metadata = read_expected_release_metadata(
        args.expected_title,
        args.expected_body_file,
        args.evidence,
    )
    release = fetch_release(args.github_repository, args.tag)
    state = examine_release(
        expected,
        release,
        tag=args.tag,
        commit=commit,
        phase=args.phase,
        expected_metadata=expected_metadata,
        assets_for_release=lambda release_id: list_remote_assets(
            args.github_repository, release_id
        ),
        download=lambda asset, local: download_remote_asset(
            args.github_repository, asset, local
        ),
        expected_release_id=expected_release_id,
    )
    if args.phase == "preflight":
        for asset_id in state.starter_asset_ids:
            delete_remote_asset(args.github_repository, asset_id)
    if args.phase == "publish":
        if state.release_id is None:
            raise ReleaseAssetError("staged GitHub Release is missing before publication")
        if state.draft:
            publish_error: ReleaseAssetError | None = None
            try:
                publish_draft(
                    args.github_repository,
                    state.release_id,
                    tag=args.tag,
                    commit=commit,
                    metadata=expected_metadata,
                )
            except ReleaseAssetError as error:
                publish_error = error
            try:
                release = fetch_release(args.github_repository, args.tag)
                state = examine_release(
                    expected,
                    release,
                    tag=args.tag,
                    commit=commit,
                    phase="published",
                    expected_metadata=expected_metadata,
                    assets_for_release=lambda release_id: list_remote_assets(
                        args.github_repository, release_id
                    ),
                    download=lambda asset, local: download_remote_asset(
                        args.github_repository, asset, local
                    ),
                    expected_release_id=expected_release_id,
                )
            except ReleaseAssetError as readback_error:
                if publish_error is None:
                    raise
                raise ReleaseAssetError(
                    "GitHub Release publish request failed and immediate readback "
                    f"did not verify the terminal state: {publish_error}; "
                    f"readback: {readback_error}"
                ) from publish_error
    if args.github_output is not None:
        append_github_outputs(args.github_output, state)
    print(
        f"GitHub Release asset verification succeeded for {args.tag}: "
        f"{len(expected)} expected assets"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ReleaseAssetError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
