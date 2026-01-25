# UnityCN / Tuanjie Notes

This document records format quirks observed in UnityCN/Tuanjie projects so we can keep parsing/edit behavior predictable without hardcoding any project paths or bundling proprietary assets.

## Version Strings

UnityCN/Tuanjie projects may use version strings like:

- `2022.3.48t6` (Tuanjie channel)
- `2022.3.48t6 (b281c1694403)` (revision suffix, as seen in `ProjectSettings/ProjectVersion.txt`)
- `2022.3.48f1c1` (UnityCN extra suffix after the type number)

Rust-side notes:

- `UnityVersion::parse_version(...)` is expected to accept `t*` channels and `f*c*` suffixes.
- Revision suffixes in parentheses should be ignored for comparisons/heuristics.

## Bundle Header Flags

Modern UnityFS bundles may set:

- `0x200` for `BlockInfoNeedPaddingAtStart` (new flag set)
- `0x40` for `BlocksAndDirectoryInfoCombined`
- low bits (`0x3`) for compression type (e.g. LZ4HC)

Example seen in the wild: `flags=0x00000243`.

Important:
- UnityCN encryption uses bits that can overlap with `0x200` depending on engine version.
- When saving, we strip encryption flags (UnityPy parity) because we do not re-encrypt outputs.

## “Resource IDs” / Path IDs

Some bundles contain objects whose `path_id` values are negative 64-bit integers (still valid per the format; treat as `i64`).

Implications:
- Parsing/edit/save must preserve these values exactly.
- Any downstream feature that assumes `path_id >= 0` (e.g. converting to `u64`) may drop coverage and should be treated as best-effort.

