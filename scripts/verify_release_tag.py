#!/usr/bin/env python3
"""Revalidate one immutable signed GitHub release tag at a publication boundary."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping

from verify_release_source import (
    COMMIT_PATTERN,
    GitIdentity,
    VerificationError,
    parse_release_tag,
    run_text,
    verify_git_identity,
)


GITHUB_REPOSITORY_PATTERN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
GH_COMMAND_TIMEOUT_SECONDS = 60


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Revalidate an immutable signed release tag before a release boundary."
    )
    parser.add_argument(
        "--repository-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
    )
    parser.add_argument("--tag", required=True)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-tag-object", required=True)
    parser.add_argument("--github-repository", required=True)
    parser.add_argument("--expected-event-sha")
    parser.add_argument(
        "--refresh-tag",
        action="store_true",
        help="Force-fetch the tag ref from origin before verifying it.",
    )
    return parser.parse_args()


def validate_object_id(value: str, label: str) -> str:
    normalized = value.lower()
    if COMMIT_PATTERN.fullmatch(normalized) is None:
        raise VerificationError(f"{label} must be a full lowercase SHA-1 object ID")
    return normalized


def gh_json(endpoint: str) -> Mapping[str, Any]:
    try:
        result = subprocess.run(
            [
                "gh",
                "api",
                "--method",
                "GET",
                "--header",
                "X-GitHub-Api-Version: 2022-11-28",
                endpoint,
            ],
            check=False,
            text=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=GH_COMMAND_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise VerificationError(
            f"GitHub API command timed out after {GH_COMMAND_TIMEOUT_SECONDS}s: {endpoint}"
        ) from error
    if result.returncode != 0:
        details = result.stderr.rstrip() or result.stdout.rstrip()
        suffix = f"\n{details}" if details else ""
        raise VerificationError(f"GitHub API request failed: {endpoint}{suffix}")
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise VerificationError(f"GitHub API returned invalid JSON for {endpoint}") from error
    if not isinstance(payload, dict):
        raise VerificationError(f"GitHub API returned a non-object for {endpoint}")
    return payload


def verify_remote_signed_tag(
    repository: str,
    identity: GitIdentity,
    expected_tag_object: str,
    expected_event_sha: str | None,
) -> None:
    if GITHUB_REPOSITORY_PATTERN.fullmatch(repository) is None:
        raise VerificationError(f"invalid GitHub repository name: {repository!r}")
    expected_tag_object = validate_object_id(expected_tag_object, "expected tag object")
    if identity.tag_object != expected_tag_object:
        raise VerificationError(
            f"local tag object {identity.tag_object} does not match expected "
            f"tag object {expected_tag_object}"
        )
    if expected_event_sha is not None:
        event_sha = validate_object_id(expected_event_sha, "expected event SHA")
        if event_sha not in {identity.commit, identity.tag_object}:
            raise VerificationError(
                f"event SHA {event_sha} is neither the release commit nor tag object"
            )

    ref = gh_json(f"repos/{repository}/git/ref/tags/{identity.tag}")
    ref_object = ref.get("object")
    if not isinstance(ref_object, dict) or ref_object.get("type") != "tag":
        raise VerificationError("GitHub release ref must point to an annotated tag object")
    if ref_object.get("sha") != identity.tag_object:
        raise VerificationError("GitHub release ref does not match the expected tag object")

    tag = gh_json(f"repos/{repository}/git/tags/{identity.tag_object}")
    tagged_object = tag.get("object")
    verification = tag.get("verification")
    if tag.get("tag") != identity.tag:
        raise VerificationError("GitHub tag object has a different tag name")
    if (
        not isinstance(tagged_object, dict)
        or tagged_object.get("type") != "commit"
        or tagged_object.get("sha") != identity.commit
    ):
        raise VerificationError("GitHub tag object does not peel to the expected commit")
    if not isinstance(verification, dict) or verification.get("verified") is not True:
        raise VerificationError("GitHub did not verify the release tag signature")


def main() -> int:
    args = parse_args()
    parse_release_tag(args.tag)
    repository_root = args.repository_root.resolve()
    expected_commit = validate_object_id(args.expected_commit, "expected commit")
    expected_tag_object = validate_object_id(args.expected_tag_object, "expected tag object")
    if args.refresh_tag:
        run_text(
            [
                "git",
                "fetch",
                "--force",
                "--no-tags",
                "origin",
                f"+refs/tags/{args.tag}:refs/tags/{args.tag}",
            ],
            cwd=repository_root,
        )
    identity = verify_git_identity(repository_root, args.tag, expected_commit)
    verify_remote_signed_tag(
        args.github_repository,
        identity,
        expected_tag_object,
        args.expected_event_sha,
    )
    print(f"release tag verified: {identity.tag} -> {identity.commit}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
