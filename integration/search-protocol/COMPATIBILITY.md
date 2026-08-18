# Search Protocol Compatibility

## Compatibility Matrix

| Artifact | Target/runtime | Local transport | Business revision | Fixture format | Status |
| --- | --- | --- | ---: | ---: | --- |
| `unity-asset-search-protocol` Rust crate | Workspace Rust toolchain | Transport-neutral JSON | 1 | 1 | Source of truth |
| `UnityAsset.SearchProtocol.Reference` | `netstandard2.0` | Capability-authenticated loopback HTTP | 1 | 1 | Reference implementation |
| `UnityAsset.SearchProtocol.Conformance` | `net8.0` or newer SDK | Real loopback HTTP daemon | 1 | 1 | Executable verification |
| External Unity editor plugin | Plugin-selected .NET/Unity runtime with `netstandard2.0` support | Reference HTTP client | 1 | 1 | Process lifecycle owned externally |

## Revision Policy

Business revision 1 is the first published contract and is current:

- unknown fields and enum variants are rejected;
- `status` exposes at most one retained operation for each background origin;
- search hits contain structured UTF-8 highlight ranges but no pre-rendered HTML;
- request and response operation kinds must match;
- nested protocol revisions must equal the envelope revision;
- fixed-width identifiers retain their v1 prefix and encoded width;
- query-policy, project, daemon instance, request, and operation bindings are exact.

Any incompatible JSON shape, semantic invariant, operation, identifier encoding, or body-limit change after the 0.3.0 release requires a new business revision. The private endpoint descriptor states the one installed business revision; clients reject a different revision instead of negotiating or falling back.

The public SDK contains `schema/business-v1.schema.json`. This Draft 2020-12 document describes structural JSON shape only. Canonical encoding, byte budgets, request/response identity binding, and lifecycle state-machine rules remain the Rust/C# conformance authority.

## Fixture Policy

Fixture format 1 describes an expected peer binding, canonical current messages, and expected-invalid messages. Fixture format changes are independent of wire revision changes, but a fixture update must remain synchronized with the Rust wire contract.

For a new business revision:

1. Freeze the last published revision fixtures before replacing the current set.
2. Add a complete request/response fixture set for the new revision.
3. Add success, structured-error, and cross-revision negative cases.
4. Extend the reference codec without changing published archived fixture bytes.
5. Update this matrix and run the conformance runner.

## Platform Boundary

The reference library accepts canonical descriptor bytes from a caller-owned authority source and
speaks only capability-authenticated HTTP/1.1 to the descriptor's IPv4 loopback port. It validates
the expected project/query-policy binding and exact descriptor generation, but does not claim to
prove filesystem ownership or permissions. It does not select named pipes, Unix domain sockets,
public interfaces, proxies, process spawning, or Unity editor APIs. Secure descriptor discovery,
daemon lifecycle, and editor integration remain external concerns.
