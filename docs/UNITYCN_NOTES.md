# UnityCN / Tuanjie Notes

This document records format quirks observed in UnityCN/Tuanjie projects so we can keep parsing/edit behavior predictable without hardcoding any project paths or bundling proprietary assets.

## Version Strings

UnityCN/Tuanjie projects may use version strings like:

- `2022.3.48t6` (Tuanjie channel)
- `2022.3.48t6 (b281c1694403)` (revision suffix, as seen in `ProjectSettings/ProjectVersion.txt`)
- `2022.3.48f1c1` (UnityCN-style suffix; UnityPy treats this as an unknown/custom channel `f1c` with number `1`)

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

How to scan (no hardcoded paths):

```
cargo run -p unity-asset-cli --bin unity-asset -- stats-pathid --input <path-or-dir> --kind bundle --limit 50
# include duplicate checks (slower):
cargo run -p unity-asset-cli --bin unity-asset -- stats-pathid --input <path-or-dir> --kind bundle --limit 50 --check-duplicates
```

Observed (external UnityCN project corpus, sample):

- `stats --kind bundle --summary --limit 30` reported `UnityFS flags=0x00000243` for all 30 scanned bundle assets.
- `stats-pathid --kind bundle --limit 30`:
  - `files_scanned=30`, `objects_total=4552`
  - `negative=2253`, `zero=0`, `positive=2299`
  - `min=-9213568037368421799`, `max=9222975297749798082`
- `stats-pathid --kind bundle --limit 10 --check-duplicates`:
  - `files_with_duplicates=0`, `duplicate_path_ids=0`
