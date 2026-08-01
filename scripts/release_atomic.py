"""Write release proof outputs without following predictable temporary paths."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from release_path_safety import (
    ReleasePathSafetyError,
    is_link_or_junction,
    reject_link_components,
)


class ReleaseAtomicWriteError(RuntimeError):
    """A release proof output cannot be written safely and atomically."""


def atomic_write_bytes(path: Path, contents: bytes, label: str) -> None:
    """Atomically replace one regular output using a random same-directory file."""

    absolute = path.absolute()
    try:
        parent = reject_link_components(absolute.parent, f"{label} parent")
        parent.mkdir(parents=True, exist_ok=True)
        parent = reject_link_components(parent, f"{label} parent")
    except (OSError, ReleasePathSafetyError) as error:
        raise ReleaseAtomicWriteError(
            f"cannot prepare {label} parent {absolute.parent}: {error}"
        ) from error
    if is_link_or_junction(parent) or not parent.is_dir():
        raise ReleaseAtomicWriteError(
            f"{label} parent must be a real directory: {parent}"
        )
    if absolute.exists() or is_link_or_junction(absolute):
        if is_link_or_junction(absolute) or not absolute.is_file():
            raise ReleaseAtomicWriteError(
                f"{label} output must be absent or a regular file: {absolute}"
            )

    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb",
            dir=parent,
            prefix=f".{absolute.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary = Path(stream.name)
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, absolute)
        temporary = None
    except OSError as error:
        raise ReleaseAtomicWriteError(
            f"cannot atomically write {label} {absolute}: {error}"
        ) from error
    finally:
        if temporary is not None:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
