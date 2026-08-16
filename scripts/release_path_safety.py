"""Shared path-safety policy for release inputs and portable archives."""

from __future__ import annotations

import unicodedata
from pathlib import Path
from typing import Iterable


WINDOWS_RESERVED_COMPONENTS = frozenset(
    {
        "AUX",
        "CLOCK$",
        "CON",
        "CONIN$",
        "CONOUT$",
        "NUL",
        "PRN",
    }
)
WINDOWS_RESERVED_DEVICE_DIGITS = frozenset("123456789¹²³")


class ReleasePathSafetyError(RuntimeError):
    """A release path contains an unsafe indirection component."""


def is_link_or_junction(path: Path) -> bool:
    return path.is_symlink() or bool(getattr(path, "is_junction", lambda: False)())


def reject_link_components(path: Path, label: str) -> Path:
    absolute = path.absolute()
    current = Path(absolute.anchor)
    for component in absolute.parts[1:]:
        current /= component
        if not (current.exists() or is_link_or_junction(current)):
            break
        if is_link_or_junction(current):
            raise ReleasePathSafetyError(
                f"{label} path contains a symlink or junction: {current}"
            )
    return absolute


def is_windows_reserved_component(component: str) -> bool:
    """Return whether one filename component aliases a Windows device name."""

    stem = component.split(".", 1)[0].upper()
    return stem in WINDOWS_RESERVED_COMPONENTS or (
        len(stem) == 4
        and stem[:3] in {"COM", "LPT"}
        and stem[3] in WINDOWS_RESERVED_DEVICE_DIGITS
    )


def portable_path_component_key(component: str, label: str) -> str:
    """Validate one portable path component and return its alias key."""

    if (
        not component
        or component in {".", ".."}
        or "/" in component
        or "\\" in component
        or ":" in component
        or component.endswith((".", " "))
        or any(ord(character) < 32 for character in component)
        or is_windows_reserved_component(component)
    ):
        raise ReleasePathSafetyError(
            f"{label} contains a non-portable component: {component!r}"
        )
    return unicodedata.normalize("NFC", component).casefold()


def portable_path_alias_key(components: Iterable[str], label: str) -> tuple[str, ...]:
    """Return the NFC/case-insensitive identity of a portable relative path."""

    parts = tuple(components)
    if not parts:
        raise ReleasePathSafetyError(f"{label} must contain at least one component")
    return tuple(portable_path_component_key(component, label) for component in parts)
