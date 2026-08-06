# Frozen Search Storage V1 Fixture

This fixture is an immutable compatibility input. Do not regenerate it with the current writer.

- Producer commit: `57e05d5ae74a498b3706b5b433be53cf3a37481d`
- Producer command: `cargo run -p unity-asset-search-index --example reindex_project -- <PROJECT_ROOT> <PRIVATE_INDEX_BASE>`
- Rust: `rustc 1.97.1 (8bab26f4f 2026-07-14)`
- Target: `x86_64-pc-windows-msvc`
- Capture platform: Windows 11 Pro 10.0.26200
- Tantivy: `0.25.0`
- Tantivy crate checksum: `502915c7381c5cb2d2781503962610cb880ad8f1a0ca95df1bae645d5ebf2545`
- Capture date: `2026-08-05`

The fixture contains three YAML assets, three search documents, and two reference documents. One
source deliberately uses the non-canonical YAML anchor `01`. Storage V1 preserves that spelling;
the current reader must degrade only that address instead of rejecting the complete generation.

The historical reference payload filename is `reference-payload-v2.jsonl`, while each contained
payload has `contract_version: 1`. The filename is part of the captured storage contract.

The checked-in store excludes machine-bound or mutable namespace state: `binding.v1`, the writer
lease, staging directories, quarantine entries, ACLs, timestamps, and absolute paths. Tests create
a fresh private index binding and copy only the frozen activation and immutable generation.

The Rust test source owns the exact file inventory, lengths, SHA-256 digests, logical identities,
and query identities. A current writer must never update those expected values automatically.
