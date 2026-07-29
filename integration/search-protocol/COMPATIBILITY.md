# Search Protocol Compatibility

## Compatibility Matrix

| Artifact | Target/runtime | Bootstrap | Business revision | Fixture format | Status |
| --- | --- | ---: | ---: | ---: | --- |
| `unity-asset-search-protocol` Rust crate | Workspace Rust toolchain | 1 | 1 | 1 | Source of truth |
| `UnityAsset.SearchProtocol.Reference` | `netstandard2.0` | 1 | 1 | 1 | Reference implementation |
| `UnityAsset.SearchProtocol.Conformance` | `net8.0` or newer SDK | 1 | 1 | 1 | Executable fixture verification |
| External Unity editor plugin | Plugin-selected .NET/Unity runtime with `netstandard2.0` support | 1 | 1 | 1 | Transport implementation owned externally |

## Revision Policy

Bootstrap version and business revision are separate compatibility domains.

Bootstrap version 1 is closed. Adding a bootstrap field, result, or rejection code requires a new bootstrap version unless the Rust contract is deliberately changed to define compatible optional data.

Business revision 1 is also closed:

- unknown fields and enum variants are rejected;
- request and response operation kinds must match;
- nested protocol revisions must equal the envelope revision;
- fixed-width identifiers retain their v1 prefix and encoded width;
- query-policy, project, daemon instance, request, and operation bindings are exact.

Any incompatible JSON shape, semantic invariant, operation, identifier encoding, or frame-limit change requires a new business revision. Peers advertise all revisions they implement during bootstrap and select the highest common value. They must not silently fall back after an accepted revision starts exchanging business frames.

## Fixture Policy

Fixture format 1 describes an inventory, an expected peer binding, canonical valid messages, and expected-invalid messages. Fixture format changes are independent of wire revision changes, but a fixture update must remain synchronized with the Rust wire contract.

For a new business revision:

1. Keep the existing revision fixtures immutable.
2. Add a complete request/response fixture set for the new revision.
3. Add success, rejection, structured-error, and cross-revision negative cases.
4. Extend the reference codec without weakening revision 1 validation.
5. Update this matrix and run the conformance runner.

## Platform Boundary

The reference library intentionally does not select named pipes, Unix domain sockets, TCP, process spawning, or Unity editor APIs. Those choices belong to the external plugin and daemon launcher. `IProtocolTransportAdapter` is the only connection boundary; framed JSON behavior above the returned `Stream` remains portable and testable.
