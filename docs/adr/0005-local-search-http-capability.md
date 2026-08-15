# ADR 0005: Local Search HTTP Capability Boundary

- Status: Accepted
- Date: 2026-08-15
- Supersedes: the principal-scoped IPC and framed-stream decisions in ADR 0001
- Supersedes: the native transport-adapter decision in ADR 0003

## Context

The project still needs one long-lived, single-writer search process per project. That process owns
warm query state, immutable Search Generation publication, watcher supervision, reconciliation,
and asynchronous reindex operations. Replacing it with one process per command would duplicate
expensive state, weaken continuous freshness, and complicate writer ownership across CLI, Unity,
and agent clients.

The existing transport coupled those product requirements to a stronger and substantially more
expensive security contract: platform-specific endpoint discovery, Unix peer credentials, Windows
named-pipe slot rotation and impersonation, exact Windows execution-context identity, four-byte
framed sessions, and a principal check around every inbound frame. That contract
is not portable as a true per-message sender proof on every supported platform. It also forces the
external Unity plugin to implement native endpoint and peer-verification adapters even though the
search daemon exposes only a replaceable derived read model and never owns Workspace mutation.

The earlier HTTP implementation is not an acceptable fallback. It left read routes anonymous,
used a long-lived project credential, exposed route-specific contracts, and allowed ordinary HTTP
client proxy behavior. A replacement must address those defects without preserving the custom IPC
stack.

## Decision

### Keep The Long-Lived Search Owner

Retain one single-writer search host per project identity. It continues to own indexing, immutable
generation publication, query execution, operation tracking, watcher supervision, startup
reconciliation, periodic reconciliation, and graceful lifecycle management. Authoritative Workspace
mutation remains in-process through `unity-asset`.

### Use One Stateless Loopback HTTP Boundary

Expose the closed search protocol through one versioned HTTP request endpoint bound only to the IPv4
loopback address. The server binds an operating-system-selected port before publishing discovery.
It does not accept a configurable host, public interface, hostname, remote address, or fallback
transport.

Each HTTP request carries one complete versioned `RequestEnvelope` and receives one complete
`ResponseEnvelope`. HTTP owns framing and message length; the protocol continues to own bounded JSON,
closed operation kinds, project identity, daemon instance identity, query-policy identity, request
identity, response binding, and semantic validation. There is no transport session, implicit
connection state, pipelining contract, or connection-bound authorization state.

### Authorize Every Request With An Ephemeral Capability

Generate a fresh 256-bit random capability for every daemon process start. Publish it only inside the
user-private endpoint descriptor after the listener is bound and the daemon is ready to serve. The
capability is never reused across daemon instances, persisted as a project-wide credential, rotated
in place, written to logs, included in errors, or exposed through status and diagnostics.

Every operation, including capabilities, status, search, reindex, and shutdown, requires the same
bearer capability. The daemon compares the fixed-size credential without secret-dependent early
exit. Missing, malformed, or incorrect credentials fail before parsing or dispatching the business
request.

The endpoint descriptor is a secret capability document rather than non-secret rendezvous metadata.
It contains the loopback port, capability, project identity, daemon instance identity, current
business revision, query-policy identity, and diagnostic process ID. Publication remains atomic and
lease-bound. Readers use the private runtime authority, reject unexpected file identity or permission
changes, and revalidate the descriptor generation around connection establishment. The Rust client
owns that platform authority directly. Cross-language reference clients accept canonical descriptor
bytes plus expected project/query-policy bindings from a caller-owned authority source; they do not
claim to prove filesystem ownership or permissions themselves.

### Define The Local Trust Boundary Honestly

The supported authorization boundary is the local operating-system user that owns the private
runtime directory. A correctly published live daemon rejects requests from remote clients and from
local clients that do not possess the capability. It does not claim to distinguish mutually hostile
processes already executing as the same user, prevent deliberate capability delegation, or resist
an administrator, debugger, or malware that can read the user's files or process memory.

Plain loopback HTTP also does not cryptographically authenticate the server before the client sends
the bearer capability. If a daemon crashes while a stale descriptor remains and another local user
actively binds the same numeric port, that process could receive the capability before descriptor
revalidation detects the stale generation. Lease ownership, private atomic descriptor publication,
withdrawal-before-drain, and generation revalidation close ordinary replacement races but do not
eliminate that active port-squatting case. This residual risk is accepted for a derived read model
that cannot mutate Workspace state. If cross-user server authenticity becomes a product requirement,
the next design must pin an ephemeral TLS public key in the private descriptor; it must not add a
custom challenge protocol.

Exact Windows logon, integrity, elevation, restricted-token, and AppContainer equality is no longer
part of the search transport contract. The descriptor's server PID is diagnostic metadata only; it
is not authorization. No request depends on Unix peer credentials, Windows pipe impersonation, or
per-message operating-system principal evidence.

### Harden The Standard HTTP Surface

The server and clients enforce the following boundary:

- bind only `127.0.0.1` and use a random operating-system-assigned port;
- authenticate every request before business JSON parsing or dispatch;
- reject browser-originated requests and unexpected `Host` values;
- disable CORS and client proxy discovery for local daemon traffic;
- apply an explicit request-body deadline and operation-specific client deadlines;
- enforce bounded request and response JSON before domain decoding or allocation growth;
- apply class-specific dispatch concurrency limits inside the transport-neutral search service;
- never place the capability in a URL, query string, trace span, diagnostic, or process argument;
- publish discovery only after the service is ready and withdraw it before shutdown draining;
- preserve project, daemon instance, request, query-policy, and response bindings in every exchange.

### Keep MCP And CLI As Adapters

The HTTP boundary is not the domain model. A transport-neutral search service owns operation dispatch
and lifecycle semantics. The Rust CLI, the C# reference client, and any MCP server are adapters over
that service contract.

An MCP adapter may expose stdio for client-owned local processes and Streamable HTTP for clients that
support it. It must translate typed MCP tools to the same search service operations and may start or
attach to the search host. MCP types, tool descriptions, and model-facing concerns do not enter the
search index, protocol, or Workspace crates.

### Delete The Superseded Transport

Delete rather than deprecate:

- `VerifiedFramedTransportV1` and the public raw-stream/session model;
- Unix-domain-socket and Windows named-pipe search transports;
- Windows single-use pipe rendezvous and per-message impersonation;
- transport-owned process and exact security-context authorization;
- four-byte search framing and Bootstrap session negotiation;
- C# native transport-adapter requirements and the transport-neutral `Stream` session facade;
- CI, release, fixtures, package consumers, and tests that exist only to prove the removed transport.

Private filesystem roots, daemon lease ownership, atomic descriptor publication, immutable Search
Generations, lifecycle supervision, operation retention, typed protocol validation, and C# semantic
conformance remain.

## Consequences

Benefits:

- clients use standard cross-platform HTTP and no longer ship platform-native IPC security code;
- transport security matches the actual derived-read-model authority and an explicit same-user trust
  boundary;
- request authorization, framing, body limits, deadlines, and concurrency use mature HTTP runtime
  components;
- MCP stdio and Streamable HTTP become thin integrations rather than new core protocols;
- the daemon, CLI, Unity plugin, C# reference, CI, and release pipeline lose a large shared platform
  compatibility surface.

Costs:

- the private endpoint descriptor becomes secret material and must retain strict filesystem handling;
- loopback TCP is shared by local users, so the capability and private descriptor are mandatory;
- plain loopback HTTP does not prove server identity before the first credential-bearing request;
- same-user malicious processes are explicitly outside the authorization boundary;
- the Rust, C#, CLI, daemon, fixtures, SDK bundle, and external Unity plugin require one coordinated
  breaking release;
- HTTP dependencies return to the daemon and client, but only behind a single finite protocol seam.

## Alternatives Considered

1. Keep principal-scoped IPC and adopt `interprocess`: rejected because a socket abstraction removes
   listener plumbing but cannot provide a uniform per-message operating-system principal or eliminate
   endpoint, lifecycle, protocol, and cross-language complexity.
2. Use MCP stdio as the only search process: rejected because one process per client would duplicate
   watcher, reconciliation, operation, cache, and writer state. Stdio remains appropriate for a thin
   MCP adapter that attaches to the shared search host.
3. Put HTTP over Unix sockets and named pipes: rejected because it retains native client adapters and
   most endpoint complexity while losing the cross-platform benefit of loopback HTTP.
4. Keep both HTTP and legacy IPC during migration: rejected because this project is pre-1.0, the
   transports have different trust and lifecycle models, and parallel compatibility would preserve
   the architecture being removed.
5. Move search into the Unity Editor: rejected because indexing and reconciliation must remain outside
   the Editor thread and must be reusable by CLI and agent clients.
