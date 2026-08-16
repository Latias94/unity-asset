#!/usr/bin/env python3
"""Credential-free bounded HTTP downloads for release tooling."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from release_subprocess import credential_free_environment


SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")
READ_CHUNK_BYTES = 1024 * 1024
WORKER_NOT_FOUND_EXIT_CODE = 3
class ReleaseHttpError(RuntimeError):
    """A release download exceeded its transport contract."""


class ReleaseHttpNotFound(ReleaseHttpError):
    """The remote server definitively reported that the resource is absent."""


@dataclass(frozen=True)
class DownloadMetadata:
    encoded_bytes: int
    sha256: str


def _measure_file(path: Path, max_bytes: int) -> DownloadMetadata:
    digest = hashlib.sha256()
    encoded_bytes = 0
    try:
        with path.open("rb") as stream:
            while chunk := stream.read(READ_CHUNK_BYTES):
                encoded_bytes += len(chunk)
                if encoded_bytes > max_bytes:
                    raise ReleaseHttpError(
                        f"downloaded file exceeds the maximum size of {max_bytes} bytes"
                    )
                digest.update(chunk)
    except OSError as error:
        raise ReleaseHttpError(
            f"download worker did not produce a readable file: {path}"
        ) from error
    return DownloadMetadata(encoded_bytes=encoded_bytes, sha256=digest.hexdigest())


def _download_once(
    url: str,
    destination: Path,
    *,
    user_agent: str,
    max_bytes: int,
    connect_timeout_seconds: float,
) -> DownloadMetadata:
    if max_bytes < 1 or connect_timeout_seconds <= 0:
        raise ReleaseHttpError("download limits must be positive")
    if destination.exists():
        raise ReleaseHttpError(f"download destination already exists: {destination}")
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ReleaseHttpError(
            f"cannot create download directory {destination.parent}: {error}"
        ) from error
    request = urllib.request.Request(url, headers={"User-Agent": user_agent})
    digest = hashlib.sha256()
    encoded_bytes = 0
    temporary = destination.with_name(destination.name + ".partial")
    try:
        with urllib.request.urlopen(
            request,
            timeout=connect_timeout_seconds,
        ) as response, temporary.open("xb") as stream:
            while True:
                chunk = response.read(READ_CHUNK_BYTES)
                if not chunk:
                    break
                encoded_bytes += len(chunk)
                if encoded_bytes > max_bytes:
                    raise ReleaseHttpError(
                        f"download exceeds the maximum size of {max_bytes} bytes"
                    )
                digest.update(chunk)
                stream.write(chunk)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, destination)
    except ReleaseHttpError:
        temporary.unlink(missing_ok=True)
        raise
    except urllib.error.HTTPError as error:
        temporary.unlink(missing_ok=True)
        if error.code == 404:
            raise ReleaseHttpNotFound(f"download returned HTTP 404: {url}") from error
        raise ReleaseHttpError(f"download failed with HTTP {error.code}: {url}") from error
    except (OSError, urllib.error.URLError) as error:
        temporary.unlink(missing_ok=True)
        raise ReleaseHttpError(f"download failed: {error}") from error
    return DownloadMetadata(encoded_bytes=encoded_bytes, sha256=digest.hexdigest())


def _worker_command(
    url: str,
    destination: Path,
    *,
    user_agent: str,
    max_bytes: int,
    connect_timeout_seconds: float,
) -> list[str]:
    return [
        sys.executable,
        str(Path(__file__).resolve()),
        "--worker",
        "--url",
        url,
        "--destination",
        str(destination),
        "--user-agent",
        user_agent,
        "--max-bytes",
        str(max_bytes),
        "--connect-timeout-seconds",
        str(connect_timeout_seconds),
    ]


def download_with_deadline(
    url: str,
    destination: Path,
    *,
    user_agent: str,
    max_bytes: int,
    connect_timeout_seconds: float,
    total_timeout_seconds: float,
) -> DownloadMetadata:
    """Download in a killable child process and verify the resulting bytes."""

    if total_timeout_seconds <= 0:
        raise ReleaseHttpError("download total timeout must be positive")
    destination = destination.resolve()
    command = _worker_command(
        url,
        destination,
        user_agent=user_agent,
        max_bytes=max_bytes,
        connect_timeout_seconds=connect_timeout_seconds,
    )
    try:
        result = subprocess.run(
            command,
            check=False,
            text=True,
            encoding="utf-8",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=total_timeout_seconds,
            env=credential_free_environment(),
        )
    except subprocess.TimeoutExpired as error:
        destination.unlink(missing_ok=True)
        destination.with_name(destination.name + ".partial").unlink(missing_ok=True)
        raise ReleaseHttpError(
            f"download exceeded its hard total timeout of {total_timeout_seconds:g}s"
        ) from error
    if result.returncode == WORKER_NOT_FOUND_EXIT_CODE:
        details = (result.stderr or result.stdout).strip()
        raise ReleaseHttpNotFound(details or f"resource not found: {url}")
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        raise ReleaseHttpError(details or f"download worker exited with {result.returncode}")
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ReleaseHttpError("download worker returned invalid metadata") from error
    if not isinstance(payload, dict):
        raise ReleaseHttpError("download worker metadata must be an object")
    encoded_bytes = payload.get("encoded_bytes")
    digest = payload.get("sha256")
    if (
        not isinstance(encoded_bytes, int)
        or isinstance(encoded_bytes, bool)
        or encoded_bytes < 0
        or encoded_bytes > max_bytes
        or not isinstance(digest, str)
        or SHA256_PATTERN.fullmatch(digest) is None
    ):
        raise ReleaseHttpError("download worker returned invalid size or digest")
    actual = _measure_file(destination, max_bytes)
    expected = DownloadMetadata(encoded_bytes=encoded_bytes, sha256=digest)
    if actual != expected:
        raise ReleaseHttpError("downloaded bytes do not match worker metadata")
    return actual


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Internal bounded release downloader.")
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--url", help=argparse.SUPPRESS)
    parser.add_argument("--destination", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--user-agent", help=argparse.SUPPRESS)
    parser.add_argument("--max-bytes", type=int, help=argparse.SUPPRESS)
    parser.add_argument(
        "--connect-timeout-seconds",
        type=float,
        help=argparse.SUPPRESS,
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if not args.worker:
        raise ReleaseHttpError("release_http.py is an internal worker")
    if (
        not isinstance(args.url, str)
        or args.destination is None
        or not isinstance(args.user_agent, str)
        or args.max_bytes is None
        or args.connect_timeout_seconds is None
    ):
        raise ReleaseHttpError("download worker arguments are incomplete")
    metadata = _download_once(
        args.url,
        args.destination,
        user_agent=args.user_agent,
        max_bytes=args.max_bytes,
        connect_timeout_seconds=args.connect_timeout_seconds,
    )
    print(
        json.dumps(
            {
                "encoded_bytes": metadata.encoded_bytes,
                "sha256": metadata.sha256,
            },
            ensure_ascii=True,
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ReleaseHttpNotFound as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(WORKER_NOT_FOUND_EXIT_CODE) from None
    except ReleaseHttpError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
