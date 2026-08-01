"""Build and verify deterministic GitHub Release metadata."""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any, Mapping

from release_atomic import ReleaseAtomicWriteError, atomic_write_bytes
from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    reject_link_components,
)


RELEASE_METADATA_SCHEMA = "unity-asset.github-release-metadata.v1"
CHANGELOG_MAX_BYTES = 2 * 1024 * 1024
RELEASE_BODY_MAX_BYTES = 1024 * 1024
SECTION_HEADING_PATTERN = re.compile(
    r"^## \[(?P<version>[^\]\r\n]+)\](?P<suffix>[^\r\n]*)$", re.MULTILINE
)
RELEASE_DATE_SUFFIX_PATTERN = re.compile(r" - (?P<date>\d{4}-\d{2}-\d{2})")
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")


class ReleaseMetadataError(RuntimeError):
    """Release title, notes, or their evidence are invalid."""


@dataclass(frozen=True)
class ReleaseMetadata:
    """Canonical title and body used for one GitHub Release."""

    title: str
    body: str

    def evidence(self) -> Mapping[str, Any]:
        title_bytes = self.title.encode("utf-8")
        body_bytes = self.body.encode("utf-8")
        return {
            "schema": RELEASE_METADATA_SCHEMA,
            "title": self.title,
            "title_sha256": hashlib.sha256(title_bytes).hexdigest(),
            "body_sha256": hashlib.sha256(body_bytes).hexdigest(),
            "body_bytes": len(body_bytes),
        }


def normalize_title(title: str) -> str:
    if not title or "\0" in title or "\n" in title or "\r" in title:
        raise ReleaseMetadataError("release title must be a non-empty line")
    return title


def normalize_body(body: str) -> str:
    if "\0" in body:
        raise ReleaseMetadataError("release body contains a NUL byte")
    normalized = body.replace("\r\n", "\n").replace("\r", "\n")
    if not normalized:
        raise ReleaseMetadataError("release body must not be empty")
    if not normalized.strip():
        raise ReleaseMetadataError("release body must not contain only whitespace")
    encoded = normalized.encode("utf-8")
    if len(encoded) > RELEASE_BODY_MAX_BYTES:
        raise ReleaseMetadataError(
            f"release body exceeds {RELEASE_BODY_MAX_BYTES} UTF-8 bytes"
        )
    return normalized


def _read_regular_utf8(path: Path, label: str, max_bytes: int) -> str:
    try:
        path = reject_link_components(path, label)
    except ReleasePathSafetyError as error:
        raise ReleaseMetadataError(str(error)) from error
    if is_link_or_junction(path) or not path.is_file():
        raise ReleaseMetadataError(f"{label} must be a real regular file: {path}")
    try:
        size = path.stat().st_size
        if size > max_bytes:
            raise ReleaseMetadataError(f"{label} exceeds {max_bytes} bytes: {path}")
        return path.read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        raise ReleaseMetadataError(f"{label} is not UTF-8: {path}") from error
    except OSError as error:
        raise ReleaseMetadataError(f"cannot read {label} {path}: {error}") from error


def metadata_from_changelog(changelog: Path, tag: str, version: str) -> ReleaseMetadata:
    """Extract the exact version section from CHANGELOG.md."""

    title = normalize_title(tag)
    document = _read_regular_utf8(changelog, "release changelog", CHANGELOG_MAX_BYTES)
    normalized = document.replace("\r\n", "\n").replace("\r", "\n")
    matches = list(SECTION_HEADING_PATTERN.finditer(normalized))
    version_matches = [match for match in matches if match.group("version") == version]
    if len(version_matches) != 1:
        raise ReleaseMetadataError(
            f"CHANGELOG.md must contain exactly one release section for [{version}]"
        )
    match = version_matches[0]
    date_match = RELEASE_DATE_SUFFIX_PATTERN.fullmatch(match.group("suffix"))
    if date_match is None:
        raise ReleaseMetadataError(
            f"CHANGELOG.md release [{version}] must use the exact heading "
            f"'## [{version}] - YYYY-MM-DD'"
        )
    release_date = date_match.group("date")
    try:
        date.fromisoformat(release_date)
    except ValueError as error:
        raise ReleaseMetadataError(
            f"CHANGELOG.md release [{version}] has an invalid calendar date: "
            f"{release_date}"
        ) from error
    next_heading = next(
        (candidate for candidate in matches if candidate.start() > match.start()),
        None,
    )
    body_end = len(normalized) if next_heading is None else next_heading.start()
    section = normalized[match.end() : body_end].strip()
    body = normalize_body(section + "\n")
    return ReleaseMetadata(title=title, body=body)


def write_metadata_files(
    metadata: ReleaseMetadata,
    title_path: Path,
    body_path: Path,
) -> None:
    """Atomically write the exact title and body passed to GitHub."""

    for path, contents, label in (
        (title_path, metadata.title + "\n", "release title"),
        (body_path, metadata.body, "release body"),
    ):
        try:
            atomic_write_bytes(path, contents.encode("utf-8"), label)
        except ReleaseAtomicWriteError as error:
            raise ReleaseMetadataError(str(error)) from error


def verify_metadata_evidence(
    evidence_path: Path,
    title: str,
    body: str,
) -> ReleaseMetadata:
    """Verify exact metadata bytes against canonical release evidence."""

    metadata = ReleaseMetadata(
        title=normalize_title(title),
        body=normalize_body(body),
    )
    encoded = _read_regular_utf8(
        evidence_path,
        "release evidence",
        CHANGELOG_MAX_BYTES,
    )
    try:
        document = json.loads(encoded)
    except json.JSONDecodeError as error:
        raise ReleaseMetadataError("release evidence is not valid JSON") from error
    if not isinstance(document, dict):
        raise ReleaseMetadataError("release evidence root must be an object")
    raw = document.get("github_release")
    if not isinstance(raw, dict):
        raise ReleaseMetadataError("release evidence omitted GitHub Release metadata")
    expected = metadata.evidence()
    if raw != expected:
        raise ReleaseMetadataError(
            "release title or body does not match canonical release evidence"
        )
    return metadata


def verify_metadata_files(
    evidence_path: Path,
    title: str,
    body_path: Path,
) -> ReleaseMetadata:
    """Read a safe body file and verify it against release evidence."""

    body = _read_regular_utf8(
        body_path,
        "release body",
        RELEASE_BODY_MAX_BYTES,
    )
    return verify_metadata_evidence(evidence_path, title, body)


def validate_metadata_evidence_shape(value: object) -> Mapping[str, Any]:
    """Validate a metadata evidence object before composing wider evidence."""

    if not isinstance(value, dict):
        raise ReleaseMetadataError("GitHub Release metadata evidence must be an object")
    expected_keys = {
        "schema",
        "title",
        "title_sha256",
        "body_sha256",
        "body_bytes",
    }
    if set(value) != expected_keys or value.get("schema") != RELEASE_METADATA_SCHEMA:
        raise ReleaseMetadataError("GitHub Release metadata evidence has an invalid schema")
    title = value.get("title")
    title_sha256 = value.get("title_sha256")
    body_sha256 = value.get("body_sha256")
    body_bytes = value.get("body_bytes")
    if not isinstance(title, str):
        raise ReleaseMetadataError("GitHub Release metadata title must be a string")
    normalize_title(title)
    if (
        not isinstance(title_sha256, str)
        or SHA256_PATTERN.fullmatch(title_sha256) is None
        or title_sha256 != hashlib.sha256(title.encode("utf-8")).hexdigest()
    ):
        raise ReleaseMetadataError("GitHub Release title digest is invalid")
    if not isinstance(body_sha256, str) or SHA256_PATTERN.fullmatch(body_sha256) is None:
        raise ReleaseMetadataError("GitHub Release body digest is invalid")
    if (
        not isinstance(body_bytes, int)
        or isinstance(body_bytes, bool)
        or body_bytes <= 0
        or body_bytes > RELEASE_BODY_MAX_BYTES
    ):
        raise ReleaseMetadataError("GitHub Release body length is invalid")
    return value
