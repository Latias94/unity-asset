"""Verify the build identity embedded in one cargo-dist executable archive."""

from __future__ import annotations

import lzma
import stat
import tarfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import BinaryIO

from release_contract import (
    DISTRIBUTION_TARGET_TRIPLES,
    distribution_archive_extension,
    distribution_executable_name,
)
from release_path_safety import ReleasePathSafetyError, portable_path_alias_key


MAX_RELEASE_BINARY_BYTES = 512 * 1024 * 1024
MAX_RELEASE_ARCHIVE_MEMBERS = 1_024
MAX_RELEASE_ARCHIVE_UNCOMPRESSED_BYTES = 1024 * 1024 * 1024
READ_CHUNK_BYTES = 1024 * 1024
BUILD_IDENTITY_DOMAIN = "unity-asset.build-identity.v1"


class ReleaseBinaryIdentityError(RuntimeError):
    """A release archive does not contain the expected executable identity."""


def version_report(
    package: str, version: str, source_commit: str, target: str
) -> str:
    return (
        f"{BUILD_IDENTITY_DOMAIN}{{version={version};source-commit={source_commit};"
        f"package={package};target={target}}}"
    )


def verify_release_binary_identity(
    archive: Path,
    *,
    application: str,
    target: str,
    version: str,
    source_commit: str,
) -> None:
    """Verify one archive's executable embeds its exact package/source report."""

    if target not in DISTRIBUTION_TARGET_TRIPLES:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive.name} uses an unsupported executable target: {target}"
        )
    expected_name = distribution_executable_name(application, target)
    expected_report = version_report(
        application, version, source_commit, target
    ).encode("ascii")
    expected_extension = distribution_archive_extension(target)
    if not archive.name.endswith(expected_extension):
        raise ReleaseBinaryIdentityError(
            f"release target {target} requires a {expected_extension} archive: {archive.name}"
        )
    if expected_extension == ".zip":
        _verify_zip(archive, expected_name, expected_report)
    else:
        _verify_tar_xz(archive, expected_name, expected_report)


def _verify_zip(archive: Path, expected_name: str, expected_report: bytes) -> None:
    try:
        with zipfile.ZipFile(archive) as bundle:
            members = bundle.infolist()
            _validate_member_count(archive.name, len(members))
            candidates: list[zipfile.ZipInfo] = []
            aliases: set[tuple[str, ...]] = set()
            uncompressed_bytes = 0
            for member in members:
                alias = _validate_member_name(
                    member.filename,
                    archive.name,
                    is_directory=member.is_dir(),
                )
                _record_member_alias(archive.name, member.filename, alias, aliases)
                mode = member.external_attr >> 16
                if stat.S_ISLNK(mode):
                    raise ReleaseBinaryIdentityError(
                        f"release archive {archive.name} contains a symbolic link"
                    )
                _validate_zip_member_type(member, archive.name, mode)
                if not member.is_dir():
                    uncompressed_bytes = _add_uncompressed_size(
                        archive.name, uncompressed_bytes, member.file_size
                    )
                if not member.is_dir() and PurePosixPath(member.filename).name == expected_name:
                    candidates.append(member)
            member = _one_executable(archive.name, expected_name, candidates)
            _validate_binary_size(archive.name, member.file_size)
            with bundle.open(member) as stream:
                _require_binary(
                    stream,
                    expected_report,
                    archive.name,
                    member.file_size,
                )
    except ReleaseBinaryIdentityError:
        raise
    except (
        OSError,
        EOFError,
        RuntimeError,
        NotImplementedError,
        zipfile.BadZipFile,
        zipfile.LargeZipFile,
    ) as error:
        raise ReleaseBinaryIdentityError(
            f"cannot inspect release archive {archive}: {error}"
        ) from error


def _verify_tar_xz(archive: Path, expected_name: str, expected_report: bytes) -> None:
    try:
        with tarfile.open(archive, mode="r|xz") as bundle:
            aliases: set[tuple[str, ...]] = set()
            member_count = 0
            uncompressed_bytes = 0
            found_executable = False
            for member in bundle:
                member_count += 1
                _validate_member_count(archive.name, member_count)
                if member.issym() or member.islnk():
                    raise ReleaseBinaryIdentityError(
                        f"release archive {archive.name} contains a link"
                    )
                if not (member.isdir() or member.isfile()):
                    raise ReleaseBinaryIdentityError(
                        f"release archive {archive.name} contains a special file"
                    )
                alias = _validate_member_name(
                    member.name,
                    archive.name,
                    is_directory=member.isdir(),
                )
                _record_member_alias(archive.name, member.name, alias, aliases)
                if member.isfile():
                    uncompressed_bytes = _add_uncompressed_size(
                        archive.name, uncompressed_bytes, member.size
                    )
                if member.isfile() and PurePosixPath(member.name).name == expected_name:
                    if found_executable:
                        raise ReleaseBinaryIdentityError(
                            f"release archive {archive.name} must contain exactly one "
                            f"{expected_name} executable"
                        )
                    found_executable = True
                    _validate_binary_size(archive.name, member.size)
                    _validate_tar_mode(member, archive.name)
                    stream = bundle.extractfile(member)
                    if stream is None:
                        raise ReleaseBinaryIdentityError(
                            f"cannot read executable {expected_name} from {archive.name}"
                        )
                    with stream:
                        _require_binary(
                            stream,
                            expected_report,
                            archive.name,
                            member.size,
                        )
            if not found_executable:
                raise ReleaseBinaryIdentityError(
                    f"release archive {archive.name} must contain exactly one "
                    f"{expected_name} executable"
                )
    except (OSError, EOFError, lzma.LZMAError, tarfile.TarError) as error:
        raise ReleaseBinaryIdentityError(
            f"cannot inspect release archive {archive}: {error}"
        ) from error


def _validate_member_count(archive_name: str, count: int) -> None:
    if count <= 0 or count > MAX_RELEASE_ARCHIVE_MEMBERS:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} member count is outside its bound: {count}"
        )


def _validate_member_name(
    name: str, archive_name: str, *, is_directory: bool
) -> tuple[str, ...]:
    normalized = name[:-1] if is_directory and name.endswith("/") else name
    parts = normalized.split("/")
    if (
        not normalized
        or "\\" in normalized
        or normalized.startswith("/")
        or any(part in ("", ".", "..") for part in parts)
    ):
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} contains an unsafe member path: {name!r}"
        )
    try:
        return portable_path_alias_key(parts, f"release archive {archive_name} member")
    except ReleasePathSafetyError as error:
        raise ReleaseBinaryIdentityError(str(error)) from error


def _record_member_alias(
    archive_name: str,
    name: str,
    alias: tuple[str, ...],
    aliases: set[tuple[str, ...]],
) -> None:
    if alias in aliases:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} contains a portable path alias: {name!r}"
        )
    aliases.add(alias)


def _add_uncompressed_size(archive_name: str, total: int, size: int) -> int:
    if size < 0:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} contains a negative member size"
        )
    total += size
    if total > MAX_RELEASE_ARCHIVE_UNCOMPRESSED_BYTES:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} uncompressed size exceeds its bound"
        )
    return total


def _validate_zip_member_type(
    member: zipfile.ZipInfo, archive_name: str, mode: int
) -> None:
    if member.create_system != 3:
        return
    file_type = stat.S_IFMT(mode)
    allowed = (0, stat.S_IFDIR) if member.is_dir() else (0, stat.S_IFREG)
    if file_type not in allowed:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} contains a special file"
        )


def _one_executable(archive_name: str, expected_name: str, candidates: list[object]):
    if len(candidates) != 1:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} must contain exactly one {expected_name} executable"
        )
    return candidates[0]


def _validate_binary_size(archive_name: str, size: int) -> None:
    if size <= 0 or size > MAX_RELEASE_BINARY_BYTES:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} executable size is outside its bound: {size}"
        )


def _validate_tar_mode(member: tarfile.TarInfo, archive_name: str) -> None:
    if not member.mode & 0o111:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} executable is not marked executable"
        )


def _require_binary(
    stream: BinaryIO,
    expected: bytes,
    archive_name: str,
    expected_size: int,
) -> None:
    found = False
    actual_size = 0
    overlap_size = max(0, len(expected) - 1)
    overlap = b""
    while chunk := stream.read(READ_CHUNK_BYTES):
        actual_size += len(chunk)
        if not found:
            combined = overlap + chunk
            found = expected in combined
            overlap = combined[-overlap_size:] if overlap_size else b""
    if actual_size != expected_size:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} executable size does not match its metadata"
        )
    if not found:
        raise ReleaseBinaryIdentityError(
            f"release archive {archive_name} executable omitted the expected build identity"
        )
