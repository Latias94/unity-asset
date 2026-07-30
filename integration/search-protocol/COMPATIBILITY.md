# Search Protocol Compatibility

## Compatibility Matrix

| Artifact | Target/runtime | Bootstrap | Business revision | Fixture format | Status |
| --- | --- | ---: | ---: | ---: | --- |
| `unity-asset-search-protocol` Rust crate | Workspace Rust toolchain | 2 | 2 | 2 | Source of truth |
| `UnityAsset.SearchProtocol.Reference` | `netstandard2.0` | 2 | 2 | 2 | Reference implementation |
| `UnityAsset.SearchProtocol.Conformance` | `net8.0` or newer SDK | 2 | 2 | 2 | Executable fixture verification |
| External Unity editor plugin | Plugin-selected .NET/Unity runtime with `netstandard2.0` support | 2 | 2 | 2 | Transport implementation owned externally |
| Frozen business archive | No runtime | N/A | 1 | 2 | Immutable and unsupported |

## Revision Policy

Bootstrap version and business revision are separate compatibility domains.

Bootstrap version 2 is closed. It supersedes the unreleased version 1 draft by adding the accepted reply's required query-policy identity. Adding another bootstrap field, result, or rejection code requires a new bootstrap version unless the Rust contract deliberately defines compatible optional data.

Business revision 1 is frozen and archived. It is not advertised during bootstrap, decoded by the current Rust or C# DTOs, or served by the daemon. `frozen-business-v1.json` inventories every archived request, response, and invalid request by canonical encoded length and SHA-256 digest; both Rust and C# conformance pin the inventory digest itself. In particular, `responses/status-v1.json` remains the original pre-lifecycle shape.

Business revision 2 is current and closed:

- unknown fields and enum variants are rejected;
- request and response operation kinds must match;
- nested protocol revisions must equal the envelope revision;
- fixed-width identifiers retain their v1 prefix and encoded width;
- query-policy, project, daemon instance, request, and operation bindings are exact.

Any incompatible JSON shape, semantic invariant, operation, identifier encoding, or frame-limit change requires a new business revision. Current peers advertise only revision 2 and reject revision-1-only peers with `no_common_revision`. They must not silently fall back after an accepted revision starts exchanging business frames.

## Fixture Policy

Fixture format 2 describes an expected peer binding, canonical current messages, expected-invalid messages, and a pinned frozen-revision inventory. Fixture format changes are independent of wire revision changes, but a fixture update must remain synchronized with the Rust wire contract.

For a new business revision:

1. Keep the existing revision fixtures immutable.
2. Add a complete request/response fixture set for the new revision.
3. Add success, rejection, structured-error, and cross-revision negative cases.
4. Extend the reference codec for the new current revision without changing archived fixture bytes.
5. Update this matrix and run the conformance runner.

## Platform Boundary

The reference library intentionally does not select named pipes, Unix domain sockets, TCP, process spawning, or Unity editor APIs. Those choices belong to the external plugin and daemon launcher. `IProtocolTransportAdapter` is the only connection boundary; framed JSON behavior above the returned `Stream` remains portable and testable.
