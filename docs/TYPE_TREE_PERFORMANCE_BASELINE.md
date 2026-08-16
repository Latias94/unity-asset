# TypeTree Traversal Performance Baseline

This document records the deterministic performance contract and the opt-in runtime sample for
the U3 semantic traversal replacement. It is a characterization artifact, not a claim that wall
clock results are portable across machines.

## Code Points

- `3d514982f30bb2a80a185e0f866b8abe67b4838c` (`3d51498`) is the repository HEAD before the
  U3 semantic traversal replacement. Superseded paths were not reconstructed or rerun, so
  pre-replacement CPU, allocation, and RSS values are **not available**. No speedup percentage is
  claimed.
- The replacement baseline below was measured from the U3 working tree on
  `2026-07-17T18:43:22+08:00`.
- The deterministic contract lives in
  `crates/unity-asset-write/src/typetree/characterization.rs` and is the CI authority. This document
  records the observed values and explains its ceilings.

## Fixtures

| Fixture | Shape | Purpose |
|---|---|---|
| `representative` | `UInt64`, one external PPtr, aligned TypelessData, 64 `SInt32` values, and a two-entry string-to-`UInt16` map | Mixed semantic and alignment coverage with one emitted reference |
| `generated-large` | 256 KiB `UInt8`, 64 KiB `SInt8`, and 65,536 `SInt32` values | Bulk-path and zero-materialization guard over 589,824 payload bytes |
| `adversarial-wide-deep` | 256 aligned/scalar fields, 64 empty numeric sequences, and a 48-record chain | Per-node overhead, empty-run behavior, and depth accounting |

Every fixture runs through write, strict read, skip, PPtr scan, and no-op template rewrite. All
adapters must consume the same schema extent. The no-op rewrite must reproduce the exact original
bytes and report every byte as preserved.

## Metric Semantics

- `wire_bytes` is the exact input or output extent charged by an adapter.
- `owned_bytes` is storage explicitly retained by the traversal adapter. It does not use a global
  allocator hook.
- `peak_owned_surrogate_bytes` is the operation's monotonic byte-budget usage minus traversed wire
  bytes. For rewrite it subtracts both input and output wire extents. It is a deterministic upper
  surrogate for retained plus temporary budgeted storage, not a sampled allocator peak.
- `unity_values_materialized`, `bulk_runs`, `bulk_bytes`, `scalar_element_ops`, `node_visits`, and
  `members` come from `TypeTreeTraversalStats`.
- `scalar_element_ops` counts primitive values decoded or compared element by element. A borrowed
  `SInt8`/`UInt8` byte slice is one bulk run with zero scalar element operations.
- Decompression bytes, open files, and file-system I/O are **N/A** for these in-memory TypeTree
  fixtures. Those resources belong to container and Prepared Artifact baselines, not U3 traversal.

## Deterministic Observations

The following values were produced by the debug-profile contract test. Rewrite has separate input
and output statistics because its budget spans both phases and its action plan.

| Fixture | Adapter | Wire B | Owned B | Budget B | Owned surrogate B | Nodes | Members | Bulk runs / B | Scalar ops | Values | Depth |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| representative | write | 324 | 584 | 908 | 584 | 78 | 77 | 1 / 256 | 5 | 0 | 3 |
| representative | read | 324 | 5,888 | 6,212 | 5,888 | 78 | 77 | 1 / 256 | 69 | 77 | 3 |
| representative | skip | 324 | 0 | 324 | 0 | 78 | 77 | 1 / 256 | 5 | 0 | 3 |
| representative | scan | 324 | 16 | 340 | 16 | 78 | 78 | 1 / 256 | 5 | 0 | 3 |
| generated-large | write | 589,836 | 1,048,592 | 1,638,428 | 1,048,592 | 393,220 | 393,219 | 66 / 589,824 | 0 | 0 | 2 |
| generated-large | read | 589,836 | 5,046,635 | 5,636,471 | 5,046,635 | 393,220 | 393,219 | 3 / 589,824 | 65,536 | 65,539 | 2 |
| generated-large | skip | 589,836 | 0 | 589,836 | 0 | 393,220 | 393,219 | 3 / 589,824 | 0 | 0 | 2 |
| generated-large | scan | 589,836 | 0 | 589,836 | 0 | 393,220 | 393,219 | 3 / 589,824 | 0 | 0 | 2 |
| adversarial-wide-deep | write | 1,284 | 2,048 | 3,332 | 2,048 | 370 | 369 | 0 / 0 | 257 | 0 | 49 |
| adversarial-wide-deep | read | 1,284 | 45,591 | 46,875 | 45,591 | 370 | 369 | 64 / 0 | 257 | 369 | 49 |
| adversarial-wide-deep | skip | 1,284 | 0 | 1,284 | 0 | 370 | 369 | 64 / 0 | 257 | 0 | 49 |
| adversarial-wide-deep | scan | 1,284 | 0 | 1,284 | 0 | 370 | 369 | 64 / 0 | 257 | 0 | 49 |

| Fixture | Rewrite input owned / values | Input bulk runs / B | Input scalar ops | Output owned B | Total budget B | Owned surrogate B | Preserved B |
|---|---:|---:|---:|---:|---:|---:|---:|
| representative | 0 / 0 | 1 / 256 | 69 | 584 | 2,000 | 1,352 | 324 |
| generated-large | 0 / 0 | 3 / 589,824 | 65,536 | 1,048,592 | 2,228,648 | 1,048,976 | 589,836 |
| adversarial-wide-deep | 0 / 0 | 64 / 0 | 257 | 2,048 | 29,192 | 26,624 | 1,284 |

The large-fixture guard is intentionally stronger than a ratio check: skip, scan, and rewrite
input must retain zero bytes and materialize zero `UnityValue` nodes; all 589,824 payload bytes must
remain on bulk paths. Read and rewrite comparison report the 65,536 element decodes required by the
`SInt32` payload, while direct `UInt8`/`SInt8` byte slices and skip/scan add no scalar work.

## Regression Ceilings

Wire extent, node visits, members, observed depth, and scalar work are exact fixture contracts.
Bulk bytes are a minimum. Owned and byte-budget ceilings are rounded above the current
observation by roughly 5% to 12%, so a representation change has limited room but allocator noise
does not affect CI.

| Fixture | Write owned / budget max B | Read owned / budget max B | Scan owned / budget max B | Rewrite input / output owned max B | Rewrite budget / surrogate max B |
|---|---:|---:|---:|---:|---:|
| representative | 640 / 1,024 | 6,400 / 6,656 | 32 / 384 | 0 / 640 | 2,560 / 1,856 |
| generated-large | 1,100,000 / 1,720,000 | 5,300,000 / 5,920,000 | 0 / 589,836 | 0 / 1,100,000 | 2,350,000 / 1,102,000 |
| adversarial-wide-deep | 2,304 / 3,584 | 48,000 / 49,000 | 0 / 1,284 | 0 / 2,304 | 32,000 / 28,000 |

An intentional fixture or representation change must update the test thresholds and this artifact
in the same change, with the changed ownership model explained. Wall-clock variation is never a
reason to loosen these deterministic ceilings.

## Runtime Sample

Measurement host:

- Windows 11 Pro `10.0.26200`, x86-64 MSVC
- Intel Core i9-13900KF, 24 cores / 32 logical processors
- 68,400,455,680 bytes physical memory
- `rustc 1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6
- `cargo-nextest 0.9.138`
- release profile, sampled at `2026-07-17T22:12:12+08:00`, 100 iterations per fixture and
  adapter, one ignored test process per adapter

| Fixture | Adapter | Wall MiB/s | Process CPU ns | CPU MiB/s | Peak RSS before / after / growth B |
|---|---:|---:|---:|---:|---:|
| representative | read | 109.532 | 0 | N/A | 11,055,104 / 11,100,160 / 45,056 |
| representative | skip | 463.950 | 0 | N/A | 11,063,296 / 11,067,392 / 4,096 |
| representative | scan | 562.824 | 0 | N/A | 11,063,296 / 11,071,488 / 8,192 |
| representative | write | 201.955 | 0 | N/A | 11,091,968 / 11,091,968 / 0 |
| representative | rewrite | 322.537 | 0 | N/A | 11,087,872 / 11,104,256 / 16,384 |
| generated-large | read | 456.803 | 125,000,000 | 450.009 | 12,619,776 / 17,375,232 / 4,755,456 |
| generated-large | skip | 5,160,655.450 | 0 | N/A | 12,537,856 / 12,537,856 / 0 |
| generated-large | scan | 4,327,011.108 | 0 | N/A | 12,566,528 / 12,566,528 / 0 |
| generated-large | write | 1,023.746 | 46,875,000 | 1,200.024 | 11,116,544 / 12,578,816 / 1,462,272 |
| generated-large | rewrite | 3,497.216 | 31,250,000 | 3,600.073 | 12,632,064 / 13,254,656 / 622,592 |
| adversarial-wide-deep | read | 19.351 | 15,625,000 | 7.837 | 17,375,232 / 17,375,232 / 0 |
| adversarial-wide-deep | skip | 225.967 | 0 | N/A | 12,537,856 / 12,537,856 / 0 |
| adversarial-wide-deep | scan | 241.237 | 0 | N/A | 12,566,528 / 12,566,528 / 0 |
| adversarial-wide-deep | write | 85.428 | 0 | N/A | 12,578,816 / 12,578,816 / 0 |
| adversarial-wide-deep | rewrite | 91.005 | 15,625,000 | 15.674 | 13,254,656 / 13,254,656 / 0 |

Skip and scan validate and advance contiguous bulk slices without touching each payload byte, so
their reported throughput is logical cursor throughput, not memory bandwidth. Rewrite counts both
input and output wire bytes; its JSON `last_iteration` statistics merge the two phases, while the
deterministic table above keeps them separate. Windows CPU and RSS are read in process through
`GetProcessTimes` and `GetProcessMemoryInfo`; no sampling subprocess runs inside either snapshot.
Windows process CPU time is quantized at approximately 15.625 ms, so short samples may report zero
or coarse CPU throughput. Peak RSS is the process high-water mark. The before snapshot is taken
after fixture construction and any input encoding required by the target adapter, and growth is
the additional high-water mark observed during that adapter. These measurements are diagnostic
output only and have no pass/fail threshold.

## Reproduction

Run the deterministic contract in the normal test profile:

```powershell
cargo nextest run -p unity-asset-write typetree_characterization_contract
```

Emit the opt-in release sample as versioned JSON lines:

```powershell
$env:UNITY_ASSET_TYPETREE_SAMPLE_ITERATIONS = '100'
cargo nextest run --release -p unity-asset-write typetree_characterization_sample_ `
  --run-ignored ignored-only --no-capture
```

The sampler emits `unity-asset.typetree-characterization.v1`. CPU or RSS fields may be `null` on
platforms where the test-only process sampler has no supported source.
