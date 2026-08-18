# Unity Asset Search Protocol Integration

This directory is the language-neutral integration surface for `unity-asset-search-protocol` business revision 1. It contains canonical JSON fixtures, a published structural JSON Schema, a Unity-independent C# reference codec and HTTP client, and an executable conformance runner.

The Rust crate remains the source of truth for the protocol. These artifacts make the contract reviewable and testable by clients that cannot link Rust directly.

## Layout

```text
fixtures/
  manifest.json              Current fixture inventory and expected peer binding
  requests/                  Canonical revision 1 requests
  responses/                 Canonical revision 1 responses
  invalid/                   Binding and cross-revision rejection cases
schema/
  business-v1.schema.json    Structural business revision 1 shape schema
csharp/
  UnityAsset.SearchProtocol.Reference/    netstandard2.0 codec and HTTP client
  UnityAsset.SearchProtocol.Conformance/  net8.0 fixture and live-daemon runner
  UnityAsset.SearchProtocol.ExternalConsumer/  netstandard2.0 public API smoke consumer
```

## Transport And Wire Contract

Every exchange is one capability-authenticated HTTP/1.1 `POST /v1/request` on IPv4 loopback. The request and response bodies are complete canonical UTF-8 JSON documents. HTTP owns framing and content length; the protocol codec rejects trailing JSON, non-canonical encodings, and bodies above the operation-specific limit.

The private `endpoint.v2.json` descriptor supplies the operating-system-selected port, ephemeral bearer capability, project ID, daemon instance ID, query-policy ID, and current business revision. Clients reject descriptors for another revision and revalidate the exact descriptor around each exchange. There is no transport session or revision negotiation.

Business revision 1 contains these operations:

- `capabilities`
- `status`
- `search`
- `suggest`
- `references`
- `reindex_admit`
- `reindex_status`
- `reindex_wait`
- `reindex_cancel`
- `shutdown`

Request and response envelopes bind every exchange to a protocol revision, request ID, project ID, daemon instance ID, and query-policy ID. A response is valid only in the context of its originating request.

The current codec accepts and emits only business revision 1. This first published contract uses numeric nonzero YAML `file_id` object selectors, requires a typed `source_object` on every reference hit, makes structured UTF-8 highlight ranges authoritative, exposes bounded background reindex operations, forbids cancelling those internal operations, and reports the first process-lifetime task failure as `daemon.process_failure`.

## Canonical JSON And Limits

Fixture payloads are compact JSON with Rust/Serde field order. A terminal CR/LF belongs to the repository text file and is not part of the HTTP body. Canonical encoders:

- emit object fields in contract order;
- omit only absent optional fields;
- emit fixed IDs with their exact v1 prefix and lowercase hexadecimal payload;
- emit map entries in Unicode scalar-value ordinal key order;
- emit integers without alternate number spellings.

Decoders reject unknown, duplicate, and non-canonical-order properties. They also validate byte/count limits, portable paths, lifecycle state invariants, process-failure evidence, nested protocol revisions, query-policy bindings, reference request echoes, and reindex operation IDs.

Reference cursors bind the generation and query-policy ID and carry a SHA-256 binding over the direction plus canonical selector JSON. Aggregate limits are measured from canonical JSON rather than source strings. Search responses allow at most 10 MiB of hit JSON, 4 MiB of diagnostic JSON, and 15 MiB for the complete operation response. Other operations retain their smaller operation-specific request and response limits.

## C# Reference Client

`UnityAsset.SearchProtocol.Reference` targets `netstandard2.0`, uses no Unity API, and depends on `System.Text.Json` 8.0.5. It exposes:

- strict fixed-ID parsers;
- business envelope construction, encoding, decoding, and request/response validation;
- strict `endpoint.v2.json` validation from a caller-owned descriptor source without exposing the
  bearer capability;
- `ProtocolHttpClient` as the bounded capability-authenticated loopback HTTP adapter.

Domain operation payloads remain schema-validated JSON instead of becoming a second public object graph. `RequestEnvelopeV1` and `ResponseEnvelopeV1` use `ProtocolConstants.BusinessProtocolRevision`, which is 1.

`ProtocolHttpClient` derives the loopback URI and exact `Host` value internally, disables proxies, redirects, cookies, decompression, and connection reuse, sends the capability only as a bearer header, bounds response streaming by operation, and validates every response against its request.

```csharp
LoopbackEndpointDescriptor endpoint = LoopbackEndpointDescriptor.ReadFromSource(
    readCurrentCanonicalDescriptor,
    expectedProjectId,
    expectedQueryPolicyId);
using ProtocolHttpClient client = ProtocolHttpClient.Open(endpoint);

RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
    client.Binding,
    RequestId.Parse(requestId),
    "search",
    Encoding.UTF8.GetBytes("{\"query\":\"player\",\"limit\":25}"));

ResponseEnvelopeV1 response = await client.ExchangeAsync(request, cancellationToken);
```

The external Unity plugin owns daemon process lifecycle, secure descriptor discovery and stable
reads, and editor-thread integration. The reference package validates canonical descriptor bytes,
expected project/query-policy bindings, and exact descriptor generation changes; it does not claim
to prove filesystem ownership or permissions. The plugin does not implement a native pipe or socket
adapter. Low-level codec consumers must call `ResponseEnvelopeV1.ValidateFor(request)` before
trusting a parsed response; `ProtocolHttpClient.ExchangeAsync` performs this validation
automatically.

## Conformance

Run the focused suite from the repository root:

```text
dotnet build integration/search-protocol/csharp/UnityAsset.SearchProtocol.ExternalConsumer/UnityAsset.SearchProtocol.ExternalConsumer.csproj --configuration Release
dotnet run --project integration/search-protocol/csharp/UnityAsset.SearchProtocol.Conformance/UnityAsset.SearchProtocol.Conformance.csproj -- integration/search-protocol/fixtures
```

The runner verifies that:

- every listed fixture is non-empty and every fixture JSON document is listed;
- all ten revision 1 request and response operations are covered;
- decode followed by canonical encode reproduces every valid fixture byte-for-byte;
- protocol revision, project, daemon instance, query-policy, and request bindings are exact;
- exact-limit JSON documents succeed and one-byte-over documents fail;
- reference hits cover anchored and unanchored YAML `source_object` selectors;
- status fixtures cover semantic drift, configuration drift, cleanup recovery, and process failure;
- the public HTTP client uses a validated private descriptor and exposes structured success/error payloads;
- an external `netstandard2.0` consumer compiles without `InternalsVisibleTo` access;
- the published business schema is valid UTF-8 JSON using Draft 2020-12;

The JSON Schema describes structural shape only: types, required fields, closed object properties, enums, identifier patterns, and bounded collections. It does not replace the Rust/C# validators for canonical property order, duplicate-key rejection, UTF-8 byte budgets, HTTP body limits, request/response bindings, or lifecycle invariants. The SDK bundle includes the schema alongside the reference codec and fixtures.

See [COMPATIBILITY.md](COMPATIBILITY.md) for revision and runtime policy.
