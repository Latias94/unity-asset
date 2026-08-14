"""Build and verify the deterministic search-protocol SDK release bundle."""

from __future__ import annotations

import hashlib
import io
import json
import os
import re
import stat
import tempfile
import zipfile
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Sequence

from release_atomic import ReleaseAtomicWriteError, atomic_write_bytes
from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    portable_path_alias_key,
    reject_link_components,
)


BUNDLE_MANIFEST_SCHEMA = "unity-asset.search-protocol-sdk-bundle.v1"
BUNDLE_METADATA_SCHEMA = "unity-asset.search-protocol-sdk-artifact.v1"
BUNDLE_FORMAT = 1
RELEASE_TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
FIXED_ZIP_TIMESTAMP = (1980, 1, 1, 0, 0, 0)
ARCHIVE_FILE_MODE = stat.S_IFREG | 0o644
MAX_SOURCE_FILES = 2_048
MAX_SOURCE_FILE_BYTES = 16 * 1024 * 1024
MAX_BUNDLE_BYTES = 64 * 1024 * 1024

REFERENCE_SOURCE_DIRECTORY = Path(
    "integration/search-protocol/csharp/UnityAsset.SearchProtocol.Reference"
)
FIXTURE_SOURCE_DIRECTORY = Path("integration/search-protocol/fixtures")
SCHEMA_SOURCE_DIRECTORY = Path("integration/search-protocol/schema")
REFERENCE_ARCHIVE_DIRECTORY = PurePosixPath(
    "csharp/UnityAsset.SearchProtocol.Reference"
)
FIXTURE_ARCHIVE_DIRECTORY = PurePosixPath("fixtures")
SCHEMA_ARCHIVE_DIRECTORY = PurePosixPath("schema")
MANIFEST_FILENAME = "bundle-manifest.json"
REQUIRED_SDK_PATHS = frozenset(
    {
        (
            REFERENCE_ARCHIVE_DIRECTORY
            / "UnityAsset.SearchProtocol.Reference.csproj"
        ).as_posix(),
        (FIXTURE_ARCHIVE_DIRECTORY / "manifest.json").as_posix(),
        (SCHEMA_ARCHIVE_DIRECTORY / "bootstrap-v2.schema.json").as_posix(),
        (SCHEMA_ARCHIVE_DIRECTORY / "business-v5.schema.json").as_posix(),
    }
)

EXCLUDED_DIRECTORY_NAMES = frozenset(
    {
        ".vs",
        "bin",
        "obj",
        "packages",
        "testresults",
    }
)
EXCLUDED_FILE_NAMES = frozenset(
    {
        "project.assets.json",
        "project.nuget.cache",
    }
)
EXCLUDED_FILE_SUFFIXES = (
    ".csproj.user",
    ".suo",
)


class ProtocolSdkBundleError(RuntimeError):
    """The protocol SDK bundle cannot be built or verified safely."""


@dataclass(frozen=True)
class ProtocolSdkBundleMetadata:
    """Portable release evidence for one generated SDK archive."""

    release_tag: str
    version: str
    artifact_name: str
    encoded_bytes: int
    sha256: str
    manifest_sha256: str
    file_count: int

    def as_dict(self) -> Mapping[str, Any]:
        return {
            "schema": BUNDLE_METADATA_SCHEMA,
            "bundle_format": BUNDLE_FORMAT,
            "release_tag": self.release_tag,
            "version": self.version,
            "artifact_name": self.artifact_name,
            "encoded_bytes": self.encoded_bytes,
            "sha256": self.sha256,
            "manifest_sha256": self.manifest_sha256,
            "file_count": self.file_count,
        }

    def canonical_json(self) -> str:
        return canonical_json_bytes(self.as_dict()).decode("utf-8").rstrip("\n")


@dataclass(frozen=True)
class _SourceFile:
    archive_path: str
    contents: bytes


@dataclass
class _SourceInventory:
    file_count: int = 0
    encoded_bytes: int = 0
    path_keys: set[tuple[str, ...]] = field(default_factory=set)

    def reserve(self, archive_path: str, encoded_bytes: int) -> None:
        next_file_count = self.file_count + 1
        if next_file_count > MAX_SOURCE_FILES:
            raise ProtocolSdkBundleError(
                f"protocol SDK source inventory exceeds {MAX_SOURCE_FILES} files"
            )
        next_encoded_bytes = self.encoded_bytes + encoded_bytes
        if next_encoded_bytes > MAX_BUNDLE_BYTES:
            raise ProtocolSdkBundleError(
                "protocol SDK source inventory exceeds its allowed byte budget"
            )
        path_key = _portable_archive_key(archive_path)
        if path_key in self.path_keys:
            raise ProtocolSdkBundleError(
                "protocol SDK source inventory contains a portable path alias"
            )
        self.file_count = next_file_count
        self.encoded_bytes = next_encoded_bytes
        self.path_keys.add(path_key)


def canonical_json_bytes(value: Any) -> bytes:
    return (
        json.dumps(
            value,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def normalize_release_tag(release_tag: str) -> tuple[str, str]:
    match = RELEASE_TAG_PATTERN.fullmatch(release_tag)
    if match is None:
        raise ProtocolSdkBundleError(
            f"release tag must be vMAJOR.MINOR.PATCH, got {release_tag!r}"
        )
    version = ".".join(match.groups())
    return release_tag, version


def archive_name_for_tag(release_tag: str) -> str:
    normalized_tag, _ = normalize_release_tag(release_tag)
    return f"unity-asset-search-protocol-sdk-{normalized_tag}.zip"


def archive_root_for_tag(release_tag: str) -> str:
    normalized_tag, _ = normalize_release_tag(release_tag)
    return f"unity-asset-search-protocol-sdk-{normalized_tag}"


def _sha256(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def _safe_path(path: Path, label: str) -> Path:
    try:
        return reject_link_components(path, label)
    except ReleasePathSafetyError as error:
        raise ProtocolSdkBundleError(str(error)) from error


def _is_excluded_directory(name: str) -> bool:
    return name.casefold() in EXCLUDED_DIRECTORY_NAMES


def _is_excluded_file(name: str) -> bool:
    folded = name.casefold()
    return folded in EXCLUDED_FILE_NAMES or folded.endswith(EXCLUDED_FILE_SUFFIXES)


def _canonical_text_contents(contents: bytes, path: str) -> bytes:
    try:
        text = contents.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ProtocolSdkBundleError(
            f"protocol SDK text input is not valid UTF-8: {path}"
        ) from error
    return text.replace("\r\n", "\n").replace("\r", "\n").encode("utf-8")


def _require_confined_path(path: Path, resolved_root: Path, label: str) -> Path:
    try:
        path.relative_to(resolved_root)
    except ValueError as error:
        raise ProtocolSdkBundleError(f"{label} escapes its source root: {path}") from error
    try:
        resolved = path.resolve(strict=True)
        resolved.relative_to(resolved_root)
    except (OSError, ValueError) as error:
        raise ProtocolSdkBundleError(f"{label} escapes its source root: {path}") from error
    return resolved


def _collect_directory(
    source_root: Path,
    archive_root: PurePosixPath,
    *,
    exclude_generated: bool,
    inventory: _SourceInventory,
) -> list[_SourceFile]:
    if not source_root.is_dir():
        raise ProtocolSdkBundleError(f"required source directory is missing: {source_root}")
    try:
        source_root = source_root.resolve(strict=True)
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to resolve protocol SDK source directory: {source_root}"
        ) from error

    def raise_walk_error(error: OSError) -> None:
        raise ProtocolSdkBundleError(
            f"failed to enumerate protocol SDK source directory: {source_root}"
        ) from error

    collected: list[_SourceFile] = []
    for current_text, directory_names, file_names in os.walk(
        source_root,
        topdown=True,
        onerror=raise_walk_error,
        followlinks=False,
    ):
        current = Path(current_text)
        retained_directories: list[str] = []
        for name in sorted(directory_names):
            candidate = current / name
            if exclude_generated and _is_excluded_directory(name):
                continue
            if is_link_or_junction(candidate):
                raise ProtocolSdkBundleError(
                    f"source directory contains a symlink or junction: {candidate}"
                )
            _require_confined_path(candidate, source_root, "source directory")
            retained_directories.append(name)
        directory_names[:] = retained_directories

        for name in sorted(file_names):
            candidate = current / name
            if exclude_generated and _is_excluded_file(name):
                continue
            if is_link_or_junction(candidate):
                raise ProtocolSdkBundleError(
                    f"source file is a symlink or junction: {candidate}"
                )
            resolved = _require_confined_path(candidate, source_root, "source file")
            try:
                metadata = resolved.stat()
            except OSError as error:
                raise ProtocolSdkBundleError(
                    f"failed to inspect source file: {candidate}"
                ) from error
            if not stat.S_ISREG(metadata.st_mode):
                raise ProtocolSdkBundleError(f"source path is not a regular file: {candidate}")
            size = metadata.st_size
            if size > MAX_SOURCE_FILE_BYTES:
                raise ProtocolSdkBundleError(
                    f"source file exceeds {MAX_SOURCE_FILE_BYTES} bytes: {candidate}"
                )
            relative = resolved.relative_to(source_root)
            archive_path = (archive_root / PurePosixPath(relative.as_posix())).as_posix()
            inventory.reserve(archive_path, size)
            try:
                raw_contents = resolved.read_bytes()
            except OSError as error:
                raise ProtocolSdkBundleError(
                    f"failed to read source file: {candidate}"
                ) from error
            if len(raw_contents) != size:
                raise ProtocolSdkBundleError(
                    f"source file changed while the bundle was being built: {candidate}"
                )
            contents = _canonical_text_contents(raw_contents, archive_path)
            collected.append(_SourceFile(archive_path=archive_path, contents=contents))

    return collected


def _collect_sources(repository_root: Path) -> list[_SourceFile]:
    repository_root = _safe_path(repository_root, "repository root")
    if not repository_root.is_dir():
        raise ProtocolSdkBundleError(
            f"repository root is not a directory: {repository_root}"
        )
    repository_root = repository_root.resolve(strict=True)

    reference_root = repository_root / REFERENCE_SOURCE_DIRECTORY
    fixture_root = repository_root / FIXTURE_SOURCE_DIRECTORY
    schema_root = repository_root / SCHEMA_SOURCE_DIRECTORY
    for source_root, label in (
        (reference_root, "reference codec"),
        (fixture_root, "golden fixtures"),
        (schema_root, "protocol schemas"),
    ):
        _safe_path(source_root, label)
        _require_confined_path(source_root, repository_root, label)

    inventory = _SourceInventory()
    sources = _collect_directory(
        reference_root,
        REFERENCE_ARCHIVE_DIRECTORY,
        exclude_generated=True,
        inventory=inventory,
    )
    sources.extend(
        _collect_directory(
            fixture_root,
            FIXTURE_ARCHIVE_DIRECTORY,
            exclude_generated=True,
            inventory=inventory,
        )
    )
    sources.extend(
        _collect_directory(
            schema_root,
            SCHEMA_ARCHIVE_DIRECTORY,
            exclude_generated=True,
            inventory=inventory,
        )
    )
    sources.sort(key=lambda source: source.archive_path)

    archive_paths = [source.archive_path for source in sources]
    missing = sorted(REQUIRED_SDK_PATHS.difference(archive_paths))
    if missing:
        raise ProtocolSdkBundleError(
            f"protocol SDK source inventory is incomplete: {', '.join(missing)}"
        )
    return sources


def _reject_output_source_overlap(
    repository_root: Path,
    output_directory: Path,
) -> None:
    repository_root = _safe_path(repository_root, "repository root").resolve(strict=True)
    output_directory = _safe_path(
        output_directory, "protocol SDK bundle output"
    )
    output_absolute = Path(os.path.abspath(output_directory))
    for relative_source in (
        REFERENCE_SOURCE_DIRECTORY,
        FIXTURE_SOURCE_DIRECTORY,
        SCHEMA_SOURCE_DIRECTORY,
    ):
        source_root = (repository_root / relative_source).resolve(strict=True)
        try:
            output_absolute.relative_to(source_root)
        except ValueError:
            continue
        raise ProtocolSdkBundleError(
            "protocol SDK output directory must be outside the bundled source trees"
        )


def _manifest_bytes(
    release_tag: str,
    version: str,
    archive_root: str,
    sources: Sequence[_SourceFile],
) -> bytes:
    return canonical_json_bytes(
        {
            "schema": BUNDLE_MANIFEST_SCHEMA,
            "bundle_format": BUNDLE_FORMAT,
            "release_tag": release_tag,
            "version": version,
            "archive_root": archive_root,
            "files": [
                {
                    "path": source.archive_path,
                    "encoded_bytes": len(source.contents),
                    "sha256": _sha256(source.contents),
                }
                for source in sources
            ],
        }
    )


def _zip_info(path: str) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(path, date_time=FIXED_ZIP_TIMESTAMP)
    info.compress_type = zipfile.ZIP_STORED
    info.create_system = 3
    info.external_attr = ARCHIVE_FILE_MODE << 16
    info.extra = b""
    info.comment = b""
    return info


def _build_bundle_bytes(release_tag: str, sources: Sequence[_SourceFile]) -> bytes:
    release_tag, version = normalize_release_tag(release_tag)
    archive_root = archive_root_for_tag(release_tag)
    manifest = _manifest_bytes(release_tag, version, archive_root, sources)
    entries = [
        (f"{archive_root}/{MANIFEST_FILENAME}", manifest),
        *[
            (f"{archive_root}/{source.archive_path}", source.contents)
            for source in sources
        ],
    ]
    entries.sort(key=lambda entry: entry[0])

    output = io.BytesIO()
    with zipfile.ZipFile(
        output,
        mode="w",
        compression=zipfile.ZIP_STORED,
        allowZip64=False,
        strict_timestamps=True,
    ) as archive:
        archive.comment = b""
        for path, contents in entries:
            archive.writestr(_zip_info(path), contents)
    bundle = output.getvalue()
    if len(bundle) > MAX_BUNDLE_BYTES:
        raise ProtocolSdkBundleError(
            f"protocol SDK bundle exceeds {MAX_BUNDLE_BYTES} bytes"
        )
    return bundle


def _validated_archive_path(path: str) -> tuple[PurePosixPath, tuple[str, ...]]:
    if not path or "\\" in path or any(ord(character) < 32 for character in path):
        raise ProtocolSdkBundleError(f"unsafe archive path: {path!r}")
    segments = path.split("/")
    if any(segment in {"", ".", ".."} for segment in segments):
        raise ProtocolSdkBundleError(f"unsafe archive path: {path!r}")
    parsed = PurePosixPath(path)
    if parsed.is_absolute() or parsed.as_posix() != path:
        raise ProtocolSdkBundleError(f"unsafe archive path: {path!r}")
    try:
        portable_key = portable_path_alias_key(parsed.parts, "archive path")
    except ReleasePathSafetyError as error:
        raise ProtocolSdkBundleError(f"unsafe archive path: {path!r}") from error
    return parsed, portable_key


def _portable_archive_key(path: str) -> tuple[str, ...]:
    _, portable_key = _validated_archive_path(path)
    return portable_key


def _validate_payload_path_policy(path: str) -> tuple[str, ...]:
    parsed, portable_key = _validated_archive_path(path)
    parts = parsed.parts
    if parts[: len(REFERENCE_ARCHIVE_DIRECTORY.parts)] == (
        REFERENCE_ARCHIVE_DIRECTORY.parts
    ):
        relative_parts = parts[len(REFERENCE_ARCHIVE_DIRECTORY.parts) :]
    elif parts[: len(FIXTURE_ARCHIVE_DIRECTORY.parts)] == (
        FIXTURE_ARCHIVE_DIRECTORY.parts
    ):
        relative_parts = parts[len(FIXTURE_ARCHIVE_DIRECTORY.parts) :]
    elif parts[: len(SCHEMA_ARCHIVE_DIRECTORY.parts)] == (
        SCHEMA_ARCHIVE_DIRECTORY.parts
    ):
        relative_parts = parts[len(SCHEMA_ARCHIVE_DIRECTORY.parts) :]
    else:
        raise ProtocolSdkBundleError(
            f"bundle manifest file is outside the public SDK roots: {path}"
        )
    if not relative_parts:
        raise ProtocolSdkBundleError(f"bundle manifest path is not a file: {path}")
    if any(_is_excluded_directory(part) for part in relative_parts[:-1]):
        raise ProtocolSdkBundleError(
            f"bundle manifest includes a generated directory: {path}"
        )
    if _is_excluded_file(relative_parts[-1]):
        raise ProtocolSdkBundleError(f"bundle manifest includes a generated file: {path}")
    return portable_key


def _require_mapping(value: Any, label: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        raise ProtocolSdkBundleError(f"{label} must be a JSON object")
    return value


def _require_exact_keys(
    value: Mapping[str, Any], expected: set[str], label: str
) -> None:
    actual = set(value)
    if actual != expected:
        raise ProtocolSdkBundleError(
            f"{label} has unexpected fields: expected {sorted(expected)}, got {sorted(actual)}"
        )


def _parse_manifest(
    manifest_bytes: bytes,
    expected_release_tag: str,
    expected_archive_root: str,
) -> tuple[Mapping[str, Any], list[Mapping[str, Any]]]:
    try:
        decoded = manifest_bytes.decode("utf-8")
        document = json.loads(decoded)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolSdkBundleError("bundle manifest is not valid UTF-8 JSON") from error
    if canonical_json_bytes(document) != manifest_bytes:
        raise ProtocolSdkBundleError("bundle manifest is not canonically encoded")

    manifest = _require_mapping(document, "bundle manifest")
    _require_exact_keys(
        manifest,
        {
            "schema",
            "bundle_format",
            "release_tag",
            "version",
            "archive_root",
            "files",
        },
        "bundle manifest",
    )
    _, expected_version = normalize_release_tag(expected_release_tag)
    expected_values = {
        "schema": BUNDLE_MANIFEST_SCHEMA,
        "bundle_format": BUNDLE_FORMAT,
        "release_tag": expected_release_tag,
        "version": expected_version,
        "archive_root": expected_archive_root,
    }
    for key, expected in expected_values.items():
        if manifest.get(key) != expected:
            raise ProtocolSdkBundleError(
                f"bundle manifest {key} does not match the expected value"
            )

    raw_files = manifest.get("files")
    if not isinstance(raw_files, list) or not raw_files:
        raise ProtocolSdkBundleError("bundle manifest files must be a non-empty array")
    files: list[Mapping[str, Any]] = []
    previous_path: str | None = None
    portable_paths: set[tuple[str, ...]] = set()
    for index, raw_file in enumerate(raw_files):
        file = _require_mapping(raw_file, f"bundle manifest file {index}")
        _require_exact_keys(
            file,
            {"path", "encoded_bytes", "sha256"},
            f"bundle manifest file {index}",
        )
        path = file.get("path")
        encoded_bytes = file.get("encoded_bytes")
        digest = file.get("sha256")
        if not isinstance(path, str):
            raise ProtocolSdkBundleError(
                f"bundle manifest file {index} path must be a string"
            )
        portable_key = _validate_payload_path_policy(path)
        if previous_path is not None and path <= previous_path:
            raise ProtocolSdkBundleError(
                "bundle manifest file paths must be unique and strictly sorted"
            )
        previous_path = path
        if portable_key in portable_paths:
            raise ProtocolSdkBundleError(
                "bundle manifest contains portable path aliases"
            )
        portable_paths.add(portable_key)
        if (
            not isinstance(encoded_bytes, int)
            or isinstance(encoded_bytes, bool)
            or encoded_bytes < 0
            or encoded_bytes > MAX_SOURCE_FILE_BYTES
        ):
            raise ProtocolSdkBundleError(
                f"bundle manifest file has an invalid encoded byte length: {path}"
            )
        if (
            not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-f]{64}", digest) is None
        ):
            raise ProtocolSdkBundleError(
                f"bundle manifest file has an invalid SHA-256 digest: {path}"
            )
        files.append(file)
    file_paths = {str(file["path"]) for file in files}
    missing = sorted(REQUIRED_SDK_PATHS.difference(file_paths))
    if missing:
        raise ProtocolSdkBundleError(
            f"bundle manifest is missing required SDK inputs: {', '.join(missing)}"
        )
    reference_prefix = f"{REFERENCE_ARCHIVE_DIRECTORY.as_posix()}/"
    if not any(
        path.startswith(reference_prefix) and path.casefold().endswith(".cs")
        for path in file_paths
    ):
        raise ProtocolSdkBundleError(
            "bundle manifest does not contain any C# reference codec source files"
        )
    return manifest, files


def _read_archive_entry(
    archive: zipfile.ZipFile,
    path: str,
    *,
    missing_message: str,
) -> bytes:
    try:
        return archive.read(path)
    except KeyError as error:
        raise ProtocolSdkBundleError(missing_message) from error
    except (OSError, RuntimeError, zipfile.BadZipFile) as error:
        raise ProtocolSdkBundleError(
            f"failed to read protocol SDK bundle entry: {path}"
        ) from error


def _verify_bundle_bytes(
    bundle: bytes,
    artifact_name: str,
    expected_release_tag: str,
    *,
    staging_root: Path | None = None,
) -> ProtocolSdkBundleMetadata:
    expected_release_tag, expected_version = normalize_release_tag(expected_release_tag)
    expected_artifact_name = archive_name_for_tag(expected_release_tag)
    if artifact_name != expected_artifact_name:
        raise ProtocolSdkBundleError(
            f"bundle filename must be {expected_artifact_name!r}, got {artifact_name!r}"
        )
    if len(bundle) > MAX_BUNDLE_BYTES:
        raise ProtocolSdkBundleError(
            f"protocol SDK bundle exceeds {MAX_BUNDLE_BYTES} bytes"
        )

    expected_root = archive_root_for_tag(expected_release_tag)
    expected_manifest_path = f"{expected_root}/{MANIFEST_FILENAME}"
    try:
        archive_context = zipfile.ZipFile(io.BytesIO(bundle), mode="r")
    except (OSError, zipfile.BadZipFile) as error:
        raise ProtocolSdkBundleError("protocol SDK bundle is not a valid ZIP archive") from error

    with archive_context as archive:
        if archive.comment:
            raise ProtocolSdkBundleError("protocol SDK bundle must not have a ZIP comment")
        infos = archive.infolist()
        if not infos or len(infos) > MAX_SOURCE_FILES + 1:
            raise ProtocolSdkBundleError("protocol SDK bundle has an invalid file count")
        names = [info.filename for info in infos]
        if names != sorted(names):
            raise ProtocolSdkBundleError("protocol SDK bundle entries are not sorted")
        if len(names) != len(set(names)):
            raise ProtocolSdkBundleError("protocol SDK bundle contains duplicate paths")
        if len({_portable_archive_key(name) for name in names}) != len(names):
            raise ProtocolSdkBundleError(
                "protocol SDK bundle contains portable path aliases"
            )
        for info in infos:
            if info.is_dir():
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle contains a directory entry: {info.filename}"
                )
            if info.date_time != FIXED_ZIP_TIMESTAMP:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle entry has a variable timestamp: {info.filename}"
                )
            if info.compress_type != zipfile.ZIP_STORED:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle entry is not stored deterministically: {info.filename}"
                )
            mode = (info.external_attr >> 16) & 0xFFFF
            if info.create_system != 3 or mode != ARCHIVE_FILE_MODE:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle entry has a non-canonical mode: {info.filename}"
                )
            if info.extra or info.comment:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle entry has variable metadata: {info.filename}"
                )
            if not info.filename.startswith(f"{expected_root}/"):
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle entry escapes the versioned root: {info.filename}"
                )

        manifest_bytes = _read_archive_entry(
            archive,
            expected_manifest_path,
            missing_message="protocol SDK bundle manifest is missing",
        )
        _, files = _parse_manifest(
            manifest_bytes,
            expected_release_tag,
            expected_root,
        )
        expected_names = [expected_manifest_path]
        expected_names.extend(f"{expected_root}/{file['path']}" for file in files)
        expected_names.sort()
        if names != expected_names:
            raise ProtocolSdkBundleError(
                "protocol SDK bundle contents do not match its manifest"
            )

        if staging_root is not None:
            _write_staged_file(staging_root / MANIFEST_FILENAME, manifest_bytes)

        total_payload_bytes = 0
        for file in files:
            path = str(file["path"])
            entry_name = f"{expected_root}/{path}"
            contents = _read_archive_entry(
                archive,
                entry_name,
                missing_message=f"protocol SDK bundle file is missing: {path}",
            )
            total_payload_bytes += len(contents)
            if total_payload_bytes > MAX_BUNDLE_BYTES:
                raise ProtocolSdkBundleError(
                    "protocol SDK bundle payload exceeds its allowed byte budget"
                )
            if len(contents) != file["encoded_bytes"]:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle file length mismatch: {path}"
                )
            if _sha256(contents) != file["sha256"]:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle file digest mismatch: {path}"
                )
            if _canonical_text_contents(contents, path) != contents:
                raise ProtocolSdkBundleError(
                    f"protocol SDK bundle file is not canonical UTF-8 LF text: {path}"
                )
            if staging_root is not None:
                _write_staged_file(
                    staging_root.joinpath(*PurePosixPath(path).parts), contents
                )

    return ProtocolSdkBundleMetadata(
        release_tag=expected_release_tag,
        version=expected_version,
        artifact_name=expected_artifact_name,
        encoded_bytes=len(bundle),
        sha256=_sha256(bundle),
        manifest_sha256=_sha256(manifest_bytes),
        file_count=len(files),
    )


def _write_staged_file(path: Path, contents: bytes) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as stream:
            stream.write(contents)
        path.chmod(0o644)
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to stage protocol SDK bundle file: {path}"
        ) from error


def _read_bundle_file(bundle_path: Path) -> tuple[Path, bytes]:
    bundle_path = _safe_path(bundle_path, "protocol SDK bundle")
    if not bundle_path.is_file():
        raise ProtocolSdkBundleError(
            f"protocol SDK bundle is not a regular file: {bundle_path}"
        )
    try:
        encoded_bytes = bundle_path.stat().st_size
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to inspect protocol SDK bundle: {bundle_path}"
        ) from error
    if encoded_bytes > MAX_BUNDLE_BYTES:
        raise ProtocolSdkBundleError(
            f"protocol SDK bundle exceeds {MAX_BUNDLE_BYTES} bytes"
        )
    try:
        bundle = bundle_path.read_bytes()
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to read protocol SDK bundle: {bundle_path}"
        ) from error
    if len(bundle) != encoded_bytes:
        raise ProtocolSdkBundleError(
            f"protocol SDK bundle changed while it was being read: {bundle_path}"
        )
    return bundle_path, bundle


def verify_protocol_sdk_bundle(
    bundle_path: Path,
    expected_release_tag: str,
) -> ProtocolSdkBundleMetadata:
    """Verify one archive and return portable evidence for its exact bytes."""

    bundle_path, bundle = _read_bundle_file(bundle_path)
    return _verify_bundle_bytes(bundle, bundle_path.name, expected_release_tag)


def extract_protocol_sdk_bundle(
    bundle_path: Path,
    output_directory: Path,
    expected_release_tag: str,
) -> ProtocolSdkBundleMetadata:
    """Verify and safely extract one SDK archive for an exact consumer build."""

    bundle_path, bundle = _read_bundle_file(bundle_path)
    expected_root = archive_root_for_tag(expected_release_tag)

    output_directory = _safe_path(output_directory, "protocol SDK extraction output")
    try:
        output_directory.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to create protocol SDK extraction output: {output_directory}"
        ) from error
    output_directory = _safe_path(
        output_directory, "protocol SDK extraction output"
    ).resolve(strict=True)
    destination_root = output_directory / expected_root
    if os.path.lexists(destination_root):
        raise ProtocolSdkBundleError(
            f"protocol SDK extraction root already exists: {destination_root}"
        )

    try:
        with tempfile.TemporaryDirectory(
            prefix=f".{expected_root}.", dir=output_directory
        ) as temporary:
            staging_root = Path(temporary) / expected_root
            staging_root.mkdir()
            metadata = _verify_bundle_bytes(
                bundle,
                bundle_path.name,
                expected_release_tag,
                staging_root=staging_root,
            )
            staging_root.rename(destination_root)
    except ProtocolSdkBundleError:
        raise
    except (OSError, zipfile.BadZipFile) as error:
        raise ProtocolSdkBundleError(
            f"failed to extract protocol SDK bundle: {bundle_path}"
        ) from error

    return metadata


def build_protocol_sdk_bundle(
    repository_root: Path,
    output_directory: Path,
    release_tag: str,
) -> ProtocolSdkBundleMetadata:
    """Build one deterministic archive atomically and return its exact evidence."""

    release_tag, _ = normalize_release_tag(release_tag)
    _reject_output_source_overlap(repository_root, output_directory)
    sources = _collect_sources(repository_root)
    bundle = _build_bundle_bytes(release_tag, sources)
    artifact_name = archive_name_for_tag(release_tag)
    metadata = _verify_bundle_bytes(bundle, artifact_name, release_tag)

    output_directory = _safe_path(output_directory, "protocol SDK bundle output")
    try:
        output_directory.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ProtocolSdkBundleError(
            f"failed to create protocol SDK output directory: {output_directory}"
        ) from error
    output_directory = _safe_path(
        output_directory, "protocol SDK bundle output"
    ).resolve(strict=True)
    output_path = output_directory / artifact_name
    if is_link_or_junction(output_path):
        raise ProtocolSdkBundleError(
            f"protocol SDK output file is a symlink or junction: {output_path}"
        )

    try:
        atomic_write_bytes(output_path, bundle, "protocol SDK bundle")
    except ReleaseAtomicWriteError as error:
        raise ProtocolSdkBundleError(
            f"failed to write protocol SDK bundle: {output_path}"
        ) from error

    _, written_bundle = _read_bundle_file(output_path)
    if written_bundle != bundle:
        raise ProtocolSdkBundleError(
            "written protocol SDK bundle does not match the generated artifact"
        )
    return metadata
