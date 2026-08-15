# Search Protocol Compatibility

## Compatibility Matrix

| Artifact | Target/runtime | Local transport | Business revision | Fixture format | Status |
| --- | --- | --- | ---: | ---: | --- |
| `unity-asset-search-protocol` Rust crate | Workspace Rust toolchain | Transport-neutral JSON | 5 | 3 | Source of truth |
| `UnityAsset.SearchProtocol.Reference` | `netstandard2.0` | Capability-authenticated loopback HTTP | 5 | 3 | Reference implementation |
| `UnityAsset.SearchProtocol.Conformance` | `net8.0` or newer SDK | Real loopback HTTP daemon | 5 | 3 | Executable verification |
| External Unity editor plugin | Plugin-selected .NET/Unity runtime with `netstandard2.0` support | Reference HTTP client | 5 | 3 | Process lifecycle owned externally |
| Frozen business archives | No runtime | N/A | 1, 2, 3, and 4 | 3 | Immutable and unsupported |

## Revision Policy

Business revisions 1, 2, 3, and 4 are frozen and archived. None is decoded by the current Rust or C# DTOs or served by the daemon. Each archived revision has a canonical inventory that pins every request, response, and invalid request by encoded length and SHA-256 digest.

Business revision 5 is current and closed:

- unknown fields and enum variants are rejected;
- `status` exposes at most one retained operation for each background origin;
- search hits contain structured UTF-8 highlight ranges but no pre-rendered HTML;
- request and response operation kinds must match;
- nested protocol revisions must equal the envelope revision;
- fixed-width identifiers retain their v1 prefix and encoded width;
- query-policy, project, daemon instance, request, and operation bindings are exact.

Any incompatible JSON shape, semantic invariant, operation, identifier encoding, or body-limit change requires another business revision after revision 5 is released. The private endpoint descriptor states the one installed business revision; clients reject a different revision instead of negotiating or falling back.

The public SDK contains `schema/business-v5.schema.json`. This Draft 2020-12 document describes structural JSON shape only. Canonical encoding, byte budgets, request/response identity binding, and lifecycle state-machine rules remain the Rust/C# conformance authority.

## Fixture Policy

Fixture format 3 describes an expected peer binding, canonical current messages, expected-invalid messages, and an ordered set of pinned frozen-revision inventories. Fixture format changes are independent of wire revision changes, but a fixture update must remain synchronized with the Rust wire contract.

For a new business revision:

1. Keep the existing revision fixtures immutable.
2. Add a complete request/response fixture set for the new revision.
3. Add success, structured-error, and cross-revision negative cases.
4. Extend the reference codec without changing archived fixture bytes.
5. Update this matrix and run the conformance runner.

## Platform Boundary

The reference library accepts canonical descriptor bytes from a caller-owned authority source and
speaks only capability-authenticated HTTP/1.1 to the descriptor's IPv4 loopback port. It validates
the expected project/query-policy binding and exact descriptor generation, but does not claim to
prove filesystem ownership or permissions. It does not select named pipes, Unix domain sockets,
public interfaces, proxies, process spawning, or Unity editor APIs. Secure descriptor discovery,
daemon lifecycle, and editor integration remain external concerns.
