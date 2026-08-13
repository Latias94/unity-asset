# Unity Asset Search Protocol Integration

This directory is the language-neutral integration surface for current `unity-asset-search-protocol` business revision 5. It contains canonical JSON fixtures, published structural JSON Schemas, a Unity-independent C# reference codec, and an executable conformance runner. Business revisions 1, 2, 3, and 4 are retained only as byte-frozen archives and are not implemented by the current runtime.

The Rust crate remains the source of truth for the protocol. These artifacts make the wire contract reviewable and testable by clients that cannot link Rust directly.

## Layout

```text
fixtures/
  manifest.json              Current fixture inventory and expected peer binding
  frozen-business-v1.json    Digests for the archived revision 1 byte set
  frozen-business-v2.json    Digests for the archived revision 2 byte set
  frozen-business-v3.json    Digests for the archived revision 3 byte set
  frozen-business-v4.json    Digests for the archived revision 4 byte set
  bootstrap/                 Hello, accepted, and rejected bootstrap messages
  requests/                  Archived v1/v2/v3/v4 and current v5 requests
  responses/                 Archived v1/v2/v3/v4 and current v5 responses
  invalid/                   Bootstrap, binding, and cross-revision rejection cases
schema/
  bootstrap-v2.schema.json   Structural Bootstrap V2 shape schema
  business-v5.schema.json    Structural business revision 5 shape schema
csharp/
  UnityAsset.SearchProtocol.Reference/    netstandard2.0 codec library
  UnityAsset.SearchProtocol.Conformance/  net8.0 fixture runner
  UnityAsset.SearchProtocol.ExternalConsumer/  netstandard2.0 public API smoke consumer
```

## Wire Contract

Every message is UTF-8 JSON preceded by a four-byte unsigned big-endian payload length. The length excludes the header. Decoders reject truncated headers, length mismatches, trailing frame bytes, and payloads that exceed the operation-specific limit.

Bootstrap is revision-independent and uses `bootstrap_version: 2`. A current client sends `BootstrapHelloV2` with a project ID, daemon instance ID, and `supported_revisions: [5]`. The list is still structurally bounded, non-empty, unique, and strictly increasing so a future coordinated release can negotiate another revision without changing Bootstrap V2. An accepted reply supplies the nonzero query-policy identity required to construct the first business request. The daemon otherwise returns one of these closed rejection codes:

- `project_mismatch`
- `instance_mismatch`
- `no_common_revision`

Business revision 5 contains these operations:

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

The current codec accepts and emits only business revision 5. It does not decode archived revision 1, 2, 3, or 4 fixtures, and peers that do not offer revision 5 receive `no_common_revision` during Bootstrap V2. Revision 2 added daemon lifecycle evidence to `status` and the closed `idempotency_conflict` API error code. Revision 3 adopted numeric nonzero YAML `file_id` object selectors and rejected the former string-anchor wire shape. Revision 4 added a required typed `source_object` to every reference hit. Revision 5 removes pre-rendered HTML highlight fields, adds structured UTF-8 ranges as the sole highlight representation, exposes a bounded list of background reindex operations, and makes cancellation of those internal operations explicitly forbidden. Anchored binary/YAML hits must mirror that address in `location.file_id` and each context `doc_file_id`; an unanchored YAML document uses `selector.kind: "unanchored"` plus `document_index` and omits those legacy numeric fields.

## Canonical JSON

Fixture payloads are compact JSON with Rust/Serde field order. A terminal CR/LF belongs to the repository text file and is not part of the framed payload. Canonical encoders:

- emit object fields in contract order;
- omit only absent optional fields; required collections and maps remain present even when empty;
- emit fixed IDs with their exact v1 prefix and lowercase hexadecimal payload;
- emit map entries in Unicode scalar-value ordinal key order;
- emit integers without alternate number spellings.

Decoders reject unknown, duplicate, and non-canonical-order properties. They also validate byte/count limits, portable paths, lifecycle state invariants, nested protocol revisions, query-policy bindings, reference request echoes, and reindex operation IDs.

Reference cursors bind the generation and query-policy ID and carry a SHA-256 binding over the direction plus canonical selector JSON. `coverage.complete` describes analysis/projection completeness, while `coverage.truncated` independently describes result pagination; complete coverage may therefore still return a `next_cursor`. A succeeded reindex operation must publish an applied or already-applied generation, report no active build, and identify the same generation in its completion receipt and status snapshot.

Aggregate limits are measured from canonical JSON, not from unescaped source strings. Search responses allow at most 10 MiB of hit JSON, 4 MiB of diagnostic JSON, and 15 MiB for the complete operation response; producers retain the largest ranked hit prefix that fits. Suggestion arrays and status path fields each have their own aggregate limits below the 256 KiB response frame. Reindex receipts retain the largest publish-warning prefix that fits their 64-entry, 4 KiB-per-entry, and 224 KiB aggregate limits, followed by an explicit omission warning when necessary. API errors and persisted generation-failure messages share a 16 KiB UTF-8 message limit.

## C# Reference Codec

`UnityAsset.SearchProtocol.Reference` targets `netstandard2.0`, uses no Unity API, and depends on `System.Text.Json` 8.0.5. It exposes:

- strict fixed-ID parsers;
- bootstrap encode, decode, and negotiation;
- business envelope encode, decode, construction, and request/response validation;
- bounded frame encode/decode helpers;
- `IProtocolTransportAdapter` as the platform connection boundary;
- `ProtocolSession` as the public Bootstrap V2 and sequential exchange owner.

Domain operation payloads are retained as schema-validated JSON rather than duplicated as a second public object model. This keeps the reference implementation aligned with the Rust DTOs while still rejecting invalid nested data. Use `BusinessCodec.CreateRequest`, `CreateSuccessResponse`, or `CreateErrorResponse` to construct outbound envelopes from operation payload JSON.

The C# `RequestEnvelopeV1` and `ResponseEnvelopeV1` class names are retained to avoid a mechanical source rename. Their wire revision always comes from `ProtocolConstants.BusinessProtocolRevision`, which is 5; the suffix does not imply revision 1 compatibility.

The asynchronous framed stream is deliberately internal. `ProtocolSession.ConnectAsync` sends the current Bootstrap V2 hello, validates the accepted project and daemon instance, retains the negotiated `ProtocolBinding`, and reports a rejected handshake as `ProtocolBootstrapRejectedException` with its closed rejection code. A successful session exclusively owns the adapter's returned `Stream`; callers must not reuse that stream after passing it to the session. `ExchangeAsync` permits one sequential request/response exchange at a time and validates the response against its request. Once an exchange starts I/O, cancellation, truncated I/O, or an invalid frame permanently poisons and closes the connection; later exchanges fail without writing another request. `ResponseEnvelopeV1.Value` exposes the validated success payload or structured error payload as `JsonElement`.

```csharp
using ProtocolSession session = await ProtocolSession.ConnectAsync(
    new EditorTransportAdapter(),
    ProjectId.Parse(projectId),
    DaemonInstanceId.Parse(instanceId),
    cancellationToken);

RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
    session.Binding,
    RequestId.Parse(requestId),
    "search",
    Encoding.UTF8.GetBytes("{\"query\":\"player\",\"limit\":25}"));

ResponseEnvelopeV1 response = await session.ExchangeAsync(request, cancellationToken);
```

The external Unity plugin owns the platform transport implementation, endpoint discovery, process lifecycle, and editor-thread integration. Its adapter only needs to return a connected, readable, writable `Stream`:

```csharp
public sealed class EditorTransportAdapter : IProtocolTransportAdapter
{
    public Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        // Open the platform-specific local IPC endpoint here.
        throw new NotImplementedException();
    }
}
```

Low-level codec consumers must call `ResponseEnvelopeV1.ValidateFor(request)` before trusting a parsed response. `ProtocolSession.ExchangeAsync` performs this validation automatically.

## Conformance

Run the focused suite from the repository root:

```text
dotnet build integration/search-protocol/csharp/UnityAsset.SearchProtocol.ExternalConsumer/UnityAsset.SearchProtocol.ExternalConsumer.csproj --configuration Release
dotnet run --project integration/search-protocol/csharp/UnityAsset.SearchProtocol.Conformance/UnityAsset.SearchProtocol.Conformance.csproj -- integration/search-protocol/fixtures
```

The runner verifies that:

- every listed fixture is non-empty and every JSON fixture is listed;
- all ten revision 5 request and response operations are covered;
- reference hits exercise typed anchored and unanchored YAML `source_object` selectors, including the absence of legacy file IDs for `document_index` identities;
- status fixtures cover semantic drift, configuration drift, and generation cleanup recovery evidence;
- bootstrap hello/accepted/rejected and a structured error are covered;
- decode followed by canonical encode reproduces every valid fixture byte-for-byte;
- every fixture survives bounded frame round-trip;
- empty, duplicate, unsorted, incompatible, and wrong-version bootstrap inputs are rejected;
- protocol revision, project, and daemon instance mismatches are rejected;
- exact-limit frames succeed and one-byte-over frames fail;
- the public transport-neutral session owns its stream, serializes concurrent exchanges, permanently poisons incomplete or invalid exchanges, exposes success/error payloads, and preserves structured rejection codes;
- an external `netstandard2.0` consumer compiles against representative fixed-ID, bootstrap, business codec, framing, transport adapter, session, and response payload interfaces without `InternalsVisibleTo` access;
- both published structural schemas are present, valid UTF-8 JSON, and declare Draft 2020-12 with reusable definitions;
- every archived business revision 1, 2, 3, and 4 fixture still matches its frozen inventory digest and per-file SHA-256 value.

The JSON Schemas describe structural shape only: types, required fields, closed object properties, enums, identifier patterns, and bounded collections. They deliberately do not replace the Rust/C# validators for canonical property order, duplicate-key rejection, UTF-8 byte budgets, framed limits, cross-message bindings, or lifecycle invariants. The SDK bundle includes both schema entrypoints alongside the reference codec and fixtures.

See [COMPATIBILITY.md](COMPATIBILITY.md) for revision and runtime policy.
