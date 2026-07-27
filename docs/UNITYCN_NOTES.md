# UnityCN / Tuanjie Notes

This document records format behavior observed in external UnityCN/Tuanjie corpora. It contains no
project paths or proprietary assets.

## Version Strings

Observed forms include:

- `2022.3.48t6`
- `2022.3.48t6 (b281c1694403)`
- `2022.3.48f1c1`

`UnityVersion::parse_version` accepts `t*` channels and `f*c*` suffixes. A revision in parentheses
is ignored for version comparison and format heuristics.

## Bundle Header Flags

Modern UnityFS bundles may set:

- `0x200` for `BlockInfoNeedPaddingAtStart`;
- `0x40` for `BlocksAndDirectoryInfoCombined`;
- low bits such as `0x3` for the compression algorithm.

`0x00000243` has been observed in production data. Some regional-engine encryption flags overlap
with otherwise known bits depending on engine version, so flag interpretation must be
version-aware. The current writer structurally rejects encrypted layouts that it cannot reproduce;
it does not publish an unencrypted artifact under an encrypted source policy.

## Signed Path IDs

Binary object `path_id` is an `i64`. Negative values are valid and must survive parsing,
inspection, reference resolution, mutation guards, writing, and reopening without conversion to an
unsigned integer.

Inspect the exact object contract:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace inspect objects --input D:\Corpus > objects.json
```

One PowerShell summary for the binary object projection is:

```powershell
$objects = Get-Content -Raw objects.json | ConvertFrom-Json
$ids = @($objects | Where-Object { $_.format.kind -eq 'binary' } | ForEach-Object { [int64]$_.format.path_id })
$negative = @($ids | Where-Object { $_ -lt 0 }).Count
$zero = @($ids | Where-Object { $_ -eq 0 }).Count
$positive = @($ids | Where-Object { $_ -gt 0 }).Count
[pscustomobject]@{
    files_or_members = @($objects | ForEach-Object { $_.address.source } | Sort-Object -Unique).Count
    objects_total = $ids.Count
    negative = $negative
    zero = $zero
    positive = $positive
    min = ($ids | Measure-Object -Minimum).Minimum
    max = ($ids | Measure-Object -Maximum).Maximum
}
```

For very large corpora, use `WorkspaceInspector` directly and aggregate while consuming source
partitions instead of retaining every JSON object.

## Historical Corpus Observation

A superseded aggregate diagnostic captured the following neutral baseline:

- 30 sampled bundle assets all reported UnityFS flags `0x00000243`.
- Across those 30 assets: `objects_total=4552`, `negative=2253`, `zero=0`, `positive=2299`.
- The observed range was `-9213568037368421799..=9222975297749798082`.
- A 10-asset duplicate check reported `files_with_duplicates=0` and
  `duplicate_path_ids=0`.

These values characterize one corpus, not a format invariant. In particular, path IDs are only
object-local within a SerializedFile and must never be treated as workspace-global identity.
