# ADR 0001: Project-Bound Local Search Daemon

- Status: Accepted
- Date: 2026-07-30
- Supersedes: the localhost HTTP and bearer-token decisions previously recorded in this ADR
- Superseded in part by: ADR 0005's HTTP capability and transport decisions

## Context

Unity projects need an IDE-like search surface across asset metadata, scene and prefab hierarchy,
script symbols, and Unity object references. Reopening and reparsing source files for every query is
too slow, but the derived index must not become authoritative for asset bytes or workspace state.

The original daemon exposed a loopback HTTP API and protected only mutation routes with a
long-lived bearer token. That boundary was unsuitable for local multi-user machines, proxy-aware
clients, Unity integrations, and automated agents. It also split one product contract across HTTP
routes, CLI-specific behavior, and index implementation types.

## Decision

### One Derived Read-Model Owner

Run one single-writer daemon per project identity. The daemon owns indexing, immutable Search
Generation publication, query execution, operation tracking, startup reconciliation, independent
periodic reconciliation, and watcher supervision. File watching reduces latency but is not the
recovery mechanism.

Authoritative workspace mutation remains in-process through `unity-asset`. IPC cannot represent a
live Workspace View, `PreparedChange`, naked `ChangeSet`, file handle, or mutation plan. A daemon
filesystem reindex request is an intent to rebuild a derived view, not authority over source bytes.

### Deterministic Project And Storage Identity

Resolve an explicit Unity project root without following a root link and derive `ProjectIdentityV1`
from stable platform directory identity. The default index and runtime roots are deterministic and
outside the project tree.

Unix roots are private to the effective UID. On Windows, stable product/runtime/cache parents use
the user SID with only create-child, traverse, attribute-read, synchronization, and control-read
rights so the same user can reopen them after a new logon. Each security-context child uses a
protected, non-inheriting logon-context DACL. Exact Windows execution-context equality is enforced
at the IPC boundary; filesystem ACLs retain normal Windows privilege and integrity dominance and
do not claim symmetric isolation between medium and elevated processes in one logon session.

### Principal-Scoped IPC

Use operating-system local IPC only. There is no network listener, URL selection, proxy behavior,
bearer token, or fallback transport.

- Linux and macOS use a short Unix-domain socket in a private runtime namespace. Both peers verify
  endpoint ownership and the peer effective UID.
- Windows creates one random single-use first-instance pipe slot, publishes only its fixed-width ID
  through a crash-atomic volatile rendezvous, and rotates to a new slot before returning each
  accepted stream. Every pipe is created directly with the final protected client DACL and remote
  clients rejected. Client rights permit framed I/O but exclude pipe-instance creation, ACL/owner
  mutation, and deletion; no broad bootstrap DACL or post-create tightening exists. A Tokio/Mio
  pipe object is never reused across sessions. Both peers compare the complete
  `SecurityContextIdV1`; the client also binds the operating-system-reported server PID to a
  stable process-start identity and process-token snapshot, then verifies unchanged publication
  evidence. A descriptor-self-declared executable identity is deliberately excluded because it
  adds no authority inside the same-principal namespace and breaks across atomic binary upgrades.

One `EndpointClaimV1` acquires the project lease before daemon initialization and owns stale
retirement, server binding, crash-atomic runtime binding/rendezvous/descriptor publication,
conditional cleanup, and lease lifetime. It publishes a canonical `EndpointDescriptorV1` last.
The descriptor contains project, daemon instance, process-start, and security-context identity
evidence plus the bootstrap version. It is non-secret rendezvous metadata and contains no source
path, asset, query, result, or mutation content.

### One Strict Wire Contract

Use bounded framed JSON rather than HTTP over IPC. A four-byte big-endian length prefixes every
UTF-8 JSON message and is validated before payload allocation. A frozen bootstrap negotiates the
business revision while binding project and daemon instance identities. Each connection then
performs sequential request/response exchanges with at most one request in flight.

`unity-asset-search-protocol` owns the closed operation enum, DTO validation, errors, cursors,
capabilities, operation lifecycle, and canonical cross-language fixtures. The Rust CLI and the C#
reference adapter consume the same contract. Malformed, oversized, unsupported, mismatched, or
pipelined requests fail before a second domain dispatch.

Reindex admission is connection-independent. It returns an epoch-bound operation ID and supports
bounded status, wait, idempotent retry, and cancellation of only exclusive queued work. Terminal,
expired, and prior-daemon lost states are explicit.

### Tiered Immutable Search Generations

Indexing remains tiered so cold start and incremental work stay bounded:

- Tier 0: asset identity, path, type, labels, timestamps, and size.
- Tier 1: YAML names, hierarchy, components, selected visible fields, script terms, and reference
  edges.
- Tier 2: bounded binary enrichment, TypeTree reference facts, and container paths where evidence
  is available.

Queries never reopen Unity sources for enrichment. Every result is answered from one immutable
Search Generation. Generation heads are the sole durable activation and freshness authority; a
corrupt latest head fails closed. A valid stale generation may remain queryable while readiness
reports the desired revision and rebuild state.

## Consequences

Benefits:

- Interactive latency is independent of reparsing source files for each query.
- Every human, Unity, and agent client receives the same typed contract and structured failures.
- Local project data is not exposed on a TCP listener or authorized by a reusable secret.
- Process replacement, stale discovery, protocol mismatch, and daemon restart have explicit
  identity and lifecycle outcomes.
- Workspace authority and the replaceable search read model remain separated.

Costs:

- Unix and Windows require separate endpoint, peer-identity, and private-root implementations.
- Windows pipe security requires crash-atomic single-use slot rotation and explicit tests across
  integrity, elevation, restricted-token, AppContainer, replacement, and cancellation contexts.
- Search storage and semantic identities require migration and full-rebuild policy.
- The external Unity plugin must ship a platform IPC adapter and a daemon with a compatible
  protocol revision.

## Alternatives Considered

1. Loopback HTTP with bearer tokens: rejected because local TCP is broader than the intended trust
   boundary, clients can inherit proxy behavior, and token possession is not execution-principal
   identity.
2. HTTP over Unix sockets or named pipes: rejected because HTTP adds routes, methods, headers, and
   compatibility surface without benefiting a finite local protocol.
3. Pure in-editor scanning: rejected because it couples indexing cost to Unity's editor thread and
   cannot provide predictable large-project query latency.
4. Repeated text scanning: useful as a fallback diagnostic, but it lacks stable object identity,
   ranking, generation consistency, and reference context.
5. A daemon-owned workspace transaction API: rejected because serialized requests cannot carry the
   revision-bound handles and proofs required for authoritative mutation.
