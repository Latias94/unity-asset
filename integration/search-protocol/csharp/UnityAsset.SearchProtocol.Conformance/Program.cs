using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;
using UnityAsset.SearchProtocol.Reference;

return await ConformanceProgram.RunAsync(args);

internal static class ConformanceProgram
{
    private static readonly string[] OperationNames =
    {
        "capabilities",
        "status",
        "search",
        "suggest",
        "references",
        "reindex_admit",
        "reindex_status",
        "reindex_wait",
        "reindex_cancel",
        "shutdown",
    };

    public static async Task<int> RunAsync(string[] args)
    {
        try
        {
            string fixtureRoot = ResolveFixtureRoot(args);
            Run(fixtureRoot);
            await AssertFramedStreamPoisoningAsync().ConfigureAwait(false);
            Console.WriteLine($"PASS: search protocol v1 fixtures conform ({fixtureRoot})");
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine($"FAIL: {error.Message}");
            return 1;
        }
    }

    private static void Run(string fixtureRoot)
    {
        string manifestPath = Path.Combine(fixtureRoot, "manifest.json");
        byte[] manifestBytes = ReadNonEmpty(manifestPath);
        FixtureManifest manifest = JsonSerializer.Deserialize<FixtureManifest>(manifestBytes, ManifestOptions())
            ?? throw new InvalidOperationException("Fixture manifest decoded to null.");

        Require(manifest.FixtureFormat == 1, "Unsupported fixture manifest format.");
        Require(manifest.ProtocolRevision == ProtocolConstants.BusinessProtocolRevision, "Manifest protocol revision mismatch.");
        Require(manifest.Valid.Count > 0, "Manifest has no valid fixtures.");
        Require(manifest.Invalid.Count > 0, "Manifest has no invalid fixtures.");

        var binding = new ProtocolBinding(
            manifest.ProtocolRevision,
            ProjectId.Parse(manifest.Binding.ProjectId),
            DaemonInstanceId.Parse(manifest.Binding.DaemonInstanceId),
            QueryPolicyId.Parse(manifest.Binding.QueryPolicyId));

        AssertCoverage(manifest);
        AssertManifestOwnsAllJson(fixtureRoot, manifest);

        foreach (FixtureEntry fixture in manifest.Valid)
        {
            byte[] payload = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, fixture.Path)));
            byte[] encoded;
            int maximum;

            switch (fixture.Kind)
            {
                case "bootstrap_hello":
                    BootstrapHelloV1 hello = BootstrapCodec.DecodeHello(payload);
                    encoded = BootstrapCodec.EncodeHello(hello);
                    maximum = FrameLimits.BootstrapMaxEncodedBytes;
                    break;
                case "bootstrap_reply":
                    BootstrapReplyV1 reply = BootstrapCodec.DecodeReply(payload);
                    BootstrapHelloV1 offeredHello = BootstrapCodec.DecodeHello(
                        TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, "bootstrap", "hello-v1.json"))));
                    BootstrapCodec.ValidateReplyFor(reply, offeredHello);
                    encoded = BootstrapCodec.EncodeReply(reply);
                    maximum = FrameLimits.BootstrapMaxEncodedBytes;
                    break;
                case "request":
                    RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(payload, binding);
                    Require(request.OperationKind == fixture.Operation, $"{fixture.Name}: request operation mismatch.");
                    encoded = BusinessCodec.EncodeRequest(request);
                    maximum = FrameLimits.ForRequest(request.OperationKind);
                    break;
                case "response":
                    string requestPath = fixture.Request
                        ?? throw new InvalidOperationException($"{fixture.Name}: response fixture has no request fixture.");
                    RequestEnvelopeV1 pairedRequest = BusinessCodec.DecodeRequest(
                        TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, requestPath))),
                        binding);
                    ResponseEnvelopeV1 response = BusinessCodec.DecodeResponse(payload);
                    response.ValidateFor(pairedRequest);
                    if (response.IsError)
                    {
                        Require(fixture.Name == "structured error response", $"{fixture.Name}: unexpected error outcome.");
                    }
                    else
                    {
                        Require(response.OperationKind == fixture.Operation, $"{fixture.Name}: response operation mismatch.");
                    }
                    encoded = BusinessCodec.EncodeResponse(response);
                    maximum = FrameLimits.ForResponse(pairedRequest.OperationKind);
                    break;
                default:
                    throw new InvalidOperationException($"{fixture.Name}: unknown fixture kind '{fixture.Kind}'.");
            }

            Require(payload.AsSpan().SequenceEqual(encoded), $"{fixture.Name}: canonical encode did not reproduce fixture bytes.");
            byte[] frame = FrameCodec.Encode(payload, maximum);
            Require(payload.AsSpan().SequenceEqual(FrameCodec.Decode(frame, maximum)), $"{fixture.Name}: frame round trip failed.");
        }

        foreach (FixtureEntry fixture in manifest.Invalid)
        {
            byte[] payload = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, fixture.Path)));
            Exception error = ExpectFailure(() => BusinessCodec.DecodeRequest(payload, binding), fixture.Name);
            Require(
                error.Message.Contains(fixture.ExpectedError!, StringComparison.OrdinalIgnoreCase),
                $"{fixture.Name}: rejection did not identify '{fixture.ExpectedError}': {error.Message}");
        }

        AssertFramingRejectsLengthMismatch();
        AssertFixedIdsRejectNonCanonicalHex();
        AssertJsonRequiresCanonicalEncoding(fixtureRoot, binding);
        AssertBootstrapRejectsBindingMismatches(fixtureRoot);
        AssertContractHardening(fixtureRoot, binding);
    }

    private static void AssertCoverage(FixtureManifest manifest)
    {
        var requestOperations = manifest.Valid
            .Where(entry => entry.Kind == "request")
            .Select(entry => entry.Operation)
            .ToHashSet(StringComparer.Ordinal);
        var responseOperations = manifest.Valid
            .Where(entry => entry.Kind == "response" && entry.Name != "structured error response")
            .Select(entry => entry.Operation)
            .ToHashSet(StringComparer.Ordinal);

        Require(requestOperations.SetEquals(OperationNames), "Request fixtures do not cover every v1 operation exactly once.");
        Require(responseOperations.SetEquals(OperationNames), "Response fixtures do not cover every v1 operation exactly once.");
        Require(manifest.Valid.Count(entry => entry.Name == "structured error response") == 1, "Structured error fixture is missing or duplicated.");
        Require(manifest.Valid.Any(entry => entry.Kind == "bootstrap_hello"), "Bootstrap hello fixture is missing.");
        Require(manifest.Valid.Count(entry => entry.Kind == "bootstrap_reply") == 2, "Bootstrap accepted/rejected fixtures are incomplete.");
    }

    private static void AssertManifestOwnsAllJson(string fixtureRoot, FixtureManifest manifest)
    {
        var listed = manifest.Valid.Concat(manifest.Invalid)
            .Select(entry => entry.Path.Replace('/', Path.DirectorySeparatorChar))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        foreach (string path in Directory.EnumerateFiles(fixtureRoot, "*.json", SearchOption.AllDirectories))
        {
            string relative = Path.GetRelativePath(fixtureRoot, path);
            if (!relative.Equals("manifest.json", StringComparison.OrdinalIgnoreCase))
            {
                Require(listed.Contains(relative), $"Unlisted JSON fixture: {relative}");
            }
        }
    }

    private static void AssertFramingRejectsLengthMismatch()
    {
        byte[] malformed = { 0, 0, 0, 2, (byte)'{', (byte)'}', (byte)' ' };
        ExpectFailure(() => FrameCodec.Decode(malformed, FrameLimits.BootstrapMaxEncodedBytes), "frame length mismatch");
    }

    private static void AssertFixedIdsRejectNonCanonicalHex()
    {
        string uppercase = "project-v1:" + new string('A', 64);
        ExpectFailure(() => ProjectId.Parse(uppercase), "uppercase fixed ID");
    }

    private static void AssertJsonRequiresCanonicalEncoding(string fixtureRoot, ProtocolBinding binding)
    {
        string request = ReadFixtureText(fixtureRoot, "requests/search-v1.json");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(request.Insert(1, " ")),
                binding),
            "non-canonical JSON whitespace");
    }

    private static void AssertBootstrapRejectsBindingMismatches(string fixtureRoot)
    {
        BootstrapHelloV1 hello = BootstrapCodec.DecodeHello(
            TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, "bootstrap", "hello-v1.json"))));
        BootstrapReplyV1 wrongProject = BootstrapNegotiator.Negotiate(
            hello,
            ProjectId.Parse("project-v1:" + new string('a', 64)),
            hello.DaemonInstanceId,
            new ushort[] { 1 });
        BootstrapReplyV1 wrongInstance = BootstrapNegotiator.Negotiate(
            hello,
            hello.ProjectId,
            DaemonInstanceId.Parse("daemon-v1:" + new string('b', 32)),
            new ushort[] { 1 });
        Require(
            wrongProject is BootstrapRejectedV1 projectRejected && projectRejected.Code == "project_mismatch",
            "Bootstrap negotiation did not reject a project mismatch.");
        Require(
            wrongInstance is BootstrapRejectedV1 instanceRejected && instanceRejected.Code == "instance_mismatch",
            "Bootstrap negotiation did not reject a daemon instance mismatch.");
    }

    private static void AssertContractHardening(string fixtureRoot, ProtocolBinding binding)
    {
        const string referenceBinding = "reference-query-v1:35aa0af0405db47e75d177436adb2fc23bef67f3046df24e030bc9ec1ff5c02e";
        const string generation = "blake3-v1:6666666666666666666666666666666666666666666666666666666666666666";
        string policy = binding.QueryPolicyId.Value;
        string referencesRequest = ReadFixtureText(fixtureRoot, "requests/references-v1.json");
        string cursor = $"\"cursor\":{{\"generation\":\"{generation}\",\"query_policy_id\":\"{policy}\",\"after_stable_id\":\"reference:page-1\",\"query_binding\":\"{referenceBinding}\"}}";
        string requestWithCursor = ReplaceExactly(
            referencesRequest,
            "\"limit\":25}}}",
            "\"limit\":25," + cursor + "}}}",
            "reference cursor insertion");
        BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(requestWithCursor), binding);

        string cursorPolicy = $"\"query_policy_id\":\"{policy}\",\"after_stable_id\":\"reference:page-1\"";
        string wrongCursorPolicy = "\"query_policy_id\":\"query-policy-v1:"
            + new string('5', 64)
            + "\",\"after_stable_id\":\"reference:page-1\"";
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    requestWithCursor,
                    cursorPolicy,
                    wrongCursorPolicy,
                    "cursor policy")),
                binding),
            "reference cursor policy binding");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    requestWithCursor,
                    referenceBinding,
                    "reference-query-v1:" + new string('0', 64),
                    "cursor query binding")),
                binding),
            "reference cursor query binding");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(referencesRequest, "\"limit\":25", "\"limit\":0", "zero reference limit")),
                binding),
            "zero reference limit");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(referencesRequest, "\"limit\":25", "\"limit\":501", "large reference limit")),
                binding),
            "large reference limit");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    referencesRequest,
                    "0123456789abcdef0123456789abcdef",
                    "0123456789ABCDEF0123456789ABCDEF",
                    "uppercase GUID")),
                binding),
            "uppercase reference GUID");

        AssertResponseRequestBindings(fixtureRoot, binding);
        AssertReferenceResponseBindings(fixtureRoot, binding, referenceBinding, generation, policy);
        AssertReindexSucceededState(fixtureRoot, binding);
        AssertCanonicalCoreValues(fixtureRoot, binding);
        AssertUnicodeScalarPathOrdering(binding);
        AssertStatusPathBudget(fixtureRoot);
        AssertErrorFrameBudget(fixtureRoot, binding);
        AssertReindexPublishWarningBudget(fixtureRoot);
    }

    private static void AssertResponseRequestBindings(string fixtureRoot, ProtocolBinding binding)
    {
        string searchRequestText = ReadFixtureText(fixtureRoot, "requests/search-v1.json");
        string searchResponseText = ReadFixtureText(fixtureRoot, "responses/search-v1.json");
        RequestEnvelopeV1 searchRequest = BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(searchRequestText), binding);
        ResponseEnvelopeV1 wrongQuery = BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(ReplaceExactly(
            searchResponseText,
            "\"query\":\"player controller\"",
            "\"query\":\"camera\"",
            "search response query")));
        ExpectFailure(() => wrongQuery.ValidateFor(searchRequest), "search response query binding");

        RequestEnvelopeV1 zeroSearchLimit = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReplaceExactly(searchRequestText, "\"limit\":25", "\"limit\":0", "search limit")),
            binding);
        ResponseEnvelopeV1 searchResponse = BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(searchResponseText));
        ExpectFailure(() => searchResponse.ValidateFor(zeroSearchLimit), "search response limit binding");

        string suggestRequestText = ReadFixtureText(fixtureRoot, "requests/suggest-v1.json");
        string suggestResponseText = ReadFixtureText(fixtureRoot, "responses/suggest-v1.json");
        RequestEnvelopeV1 suggestRequest = BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(suggestRequestText), binding);
        ResponseEnvelopeV1 wrongPrefix = BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(ReplaceExactly(
            suggestResponseText,
            "\"prefix\":\"play\"",
            "\"prefix\":\"camera\"",
            "suggest response prefix")));
        ExpectFailure(() => wrongPrefix.ValidateFor(suggestRequest), "suggest response prefix binding");

        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    suggestRequestText,
                    "\"limit\":10",
                    "\"limit\":0",
                    "zero suggest limit")),
                binding),
            "zero suggest limit");
        RequestEnvelopeV1 limitedSuggest = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReplaceExactly(
                suggestRequestText,
                "\"limit\":10",
                "\"limit\":1",
                "suggest response limit")),
            binding);
        ResponseEnvelopeV1 suggestResponse = BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(suggestResponseText));
        ExpectFailure(() => suggestResponse.ValidateFor(limitedSuggest), "suggest response limit binding");

        JsonObject oversizedSuggestions = ParseObjectNode(suggestResponseText);
        var values = new JsonArray();
        for (int index = 0; index < 8; index++)
        {
            values.Add(new string('x', 30 * 1024));
        }
        oversizedSuggestions["value"]!["response"]!["suggestions"] = values;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedSuggestions)),
            "suggest response JSON byte limit");

        JsonObject oversizedSearchHit = ParseObjectNode(searchResponseText);
        oversizedSearchHit["value"]!["response"]!["hits"]![0]!["name"] =
            new string('x', 10 * 1024 * 1024);
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedSearchHit)),
            "search hit JSON byte limit");

        JsonObject invalidGuid = ParseObjectNode(searchResponseText);
        invalidGuid["value"]!["response"]!["hits"]![0]!["guid"] = "not-a-guid";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(invalidGuid)),
            "search hit GUID");
    }

    private static void AssertStatusPathBudget(string fixtureRoot)
    {
        JsonObject oversizedStatus = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v1.json"));
        var roots = new JsonArray();
        for (int index = 0; index < 8; index++)
        {
            roots.Add("root-" + index + "/" + new string('x', 30 * 1024));
        }
        oversizedStatus["value"]!["response"]!["scan_roots"] = roots;

        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedStatus)),
            "status response path JSON byte limit");

        JsonObject oversizedFailure = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v1.json"));
        oversizedFailure["value"]!["response"]!["generation"]!["last_failure"] = new JsonObject
        {
            ["code"] = "index_build_failed",
            ["message"] = new string('x', 16 * 1024 + 1),
            ["retryable"] = false,
            ["failed_unix_ms"] = 1,
        };
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedFailure)),
            "generation failure message byte limit");

        JsonObject idleBuilding = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v1.json"));
        idleBuilding["value"]!["response"]!["generation"]!["building_revision"] =
            "blake3-v1:" + new string('b', 64);
        idleBuilding["value"]!["response"]!["indexing"] = false;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(idleBuilding)),
            "idle status building revision");
    }

    private static void AssertErrorFrameBudget(string fixtureRoot, ProtocolBinding binding)
    {
        RequestEnvelopeV1 shutdown = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/shutdown-v1.json")),
            binding);
        JsonObject errorNode = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/structured-error-v1.json"));
        errorNode["request_id"] = shutdown.RequestId.ToString();
        errorNode["value"]!["message"] = new string('x', 16 * 1024);
        ResponseEnvelopeV1 error = BusinessCodec.DecodeResponse(SerializeNode(errorNode));
        FrameCodec.EncodeResponse(error, shutdown);

        var details = new JsonObject();
        for (int index = 0; index < 64; index++)
        {
            details["detail-" + index.ToString("D2")] = new string('\u0001', 4 * 1024);
        }
        errorNode["value"]!["message"] = "failure";
        errorNode["value"]!["details"] = details;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(errorNode)),
            "API error aggregate JSON byte limit");
    }

    private static void AssertReindexPublishWarningBudget(string fixtureRoot)
    {
        JsonObject oversizedWarnings = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/reindex-admit-v1.json"));
        var warnings = new JsonArray();
        for (int index = 0; index < 64; index++)
        {
            warnings.Add(new string('x', 4 * 1024));
        }
        oversizedWarnings["value"]!["response"]!["admission"]!["evidence"]!["publish_warnings"] =
            warnings;

        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedWarnings)),
            "reindex publish warning aggregate JSON byte limit");
    }

    private static void AssertReferenceResponseBindings(
        string fixtureRoot,
        ProtocolBinding binding,
        string referenceBinding,
        string generation,
        string policy)
    {
        string requestText = ReplaceExactly(
            ReadFixtureText(fixtureRoot, "requests/references-v1.json"),
            "\"limit\":25",
            "\"limit\":1",
            "reference request limit");
        RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(requestText), binding);
        JsonObject responseRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v1.json"));
        JsonObject response = responseRoot["value"]!["response"]!.AsObject();
        response["request"]!["limit"] = 1;
        JsonObject coverage = response["coverage"]!.AsObject();
        coverage["returned"] = 2;
        coverage["total"] = 2;
        JsonArray hits = response["hits"]!.AsArray();
        hits.Add(hits[0]!.DeepClone());
        ResponseEnvelopeV1 tooManyReferences = BusinessCodec.DecodeResponse(SerializeNode(responseRoot));
        ExpectFailure(() => tooManyReferences.ValidateFor(request), "reference response limit binding");

        JsonObject diagnosticBytes = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v1.json"));
        diagnosticBytes["value"]!["response"]!["diagnostic_coverage"]!["serialized_bytes"] = 3;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(diagnosticBytes)),
            "reference diagnostic serialized byte accounting");

        JsonObject pagedRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v1.json"));
        JsonObject pagedCoverage = pagedRoot["value"]!["response"]!["coverage"]!.AsObject();
        pagedCoverage["truncated"] = true;
        pagedCoverage["next_cursor"] = new JsonObject
        {
            ["generation"] = generation,
            ["query_policy_id"] = policy,
            ["after_stable_id"] = "reference:page-1",
            ["query_binding"] = referenceBinding,
        };
        ResponseEnvelopeV1 pagedResponse = BusinessCodec.DecodeResponse(SerializeNode(pagedRoot));
        RequestEnvelopeV1 originalRequest = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/references-v1.json")),
            binding);
        pagedResponse.ValidateFor(originalRequest);

        string searchResponse = ReadFixtureText(fixtureRoot, "responses/search-v1.json");
        const string diagnostic = "{\"code\":\"duplicate_candidate_key\",\"stable_key\":\"asset:duplicate-suppressed\"}";
        string excessiveDiagnostics = ReplaceExactly(
            searchResponse,
            $"\"diagnostics\":[{diagnostic}]",
            "\"diagnostics\":[" + string.Join(",", Enumerable.Repeat("{\"code\":\"empty_query\"}", 4097)) + "]",
            "search diagnostics");
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(excessiveDiagnostics)),
            "search diagnostic count");
    }

    private static void AssertReindexSucceededState(string fixtureRoot, ProtocolBinding binding)
    {
        JsonObject operationRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/reindex-status-v1.json"));
        JsonObject running = operationRoot["value"]!["response"]!.AsObject();
        JsonObject admission = running["admission"]!.AsObject();
        JsonObject statusRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v1.json"));
        JsonObject status = statusRoot["value"]!["response"]!.AsObject();
        JsonObject activeGeneration = status["generation"]!["active"]!.AsObject();
        var completion = new JsonObject
        {
            ["protocol_revision"] = 1,
            ["disposition"] = "applied",
            ["transaction"] = admission["transaction"]!.DeepClone(),
            ["target_revision"] = "blake3-v1:" + new string('a', 64),
            ["generation"] = activeGeneration.DeepClone(),
            ["evidence"] = admission["evidence"]!.DeepClone(),
        };
        operationRoot["value"]!["response"] = new JsonObject
        {
            ["operation_id"] = running["operation_id"]!.DeepClone(),
            ["state"] = "succeeded",
            ["admission"] = admission.DeepClone(),
            ["completion"] = completion,
            ["status"] = status.DeepClone(),
        };
        byte[] succeededJson = SerializeNode(operationRoot);
        ResponseEnvelopeV1 succeeded = BusinessCodec.DecodeResponse(succeededJson);
        RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/reindex-status-v1.json")),
            binding);
        succeeded.ValidateFor(request);

        JsonObject queuedCompletion = ParseObjectNode(Encoding.UTF8.GetString(succeededJson));
        queuedCompletion["value"]!["response"]!["completion"]!["disposition"] = "queued";
        ExpectFailure(() => BusinessCodec.DecodeResponse(SerializeNode(queuedCompletion)), "queued succeeded completion");

        JsonObject indexing = ParseObjectNode(Encoding.UTF8.GetString(succeededJson));
        indexing["value"]!["response"]!["status"]!["indexing"] = true;
        ExpectFailure(() => BusinessCodec.DecodeResponse(SerializeNode(indexing)), "indexing succeeded status");

        JsonObject building = ParseObjectNode(Encoding.UTF8.GetString(succeededJson));
        building["value"]!["response"]!["status"]!["generation"]!["building_revision"] =
            "blake3-v1:" + new string('b', 64);
        ExpectFailure(() => BusinessCodec.DecodeResponse(SerializeNode(building)), "building succeeded status");
    }

    private static void AssertCanonicalCoreValues(string fixtureRoot, ProtocolBinding binding)
    {
        string response = ReadFixtureText(fixtureRoot, "responses/references-v1.json");
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(ReplaceExactly(
                response,
                "blake3-v1:" + new string('6', 64),
                "blake3-v1:" + new string('A', 64),
                "uppercase digest"))),
            "uppercase digest");
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(Encoding.UTF8.GetBytes(ReplaceExactly(
                response,
                "workspace-v1:00000000000000000000000000000001",
                "workspace-v1:A0000000000000000000000000000001",
                "uppercase workspace"))),
            "uppercase workspace ID");

        string reindex = ReadFixtureText(fixtureRoot, "requests/reindex-admit-v1.json");
        BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReplaceExactly(
                reindex,
                "Assets/Prefabs/Player.prefab",
                "1:/asset",
                "portable path")),
            binding);
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    reindex,
                    "Assets/Prefabs/Player.prefab",
                    "C:/asset",
                    "portable path")),
                binding),
            "ASCII-letter drive path");
    }

    private static void AssertUnicodeScalarPathOrdering(ProtocolBinding binding)
    {
        byte[] scalarOrdered = Encoding.UTF8.GetBytes(
            "{\"intent\":{\"protocol_revision\":1,\"scope\":{\"kind\":\"changed_paths\",\"paths\":[\"Assets/\\uE000\",\"Assets/\\uD800\\uDC00\"]}}}");
        BusinessCodec.CreateRequest(
            binding,
            RequestId.Parse("request-v1:81818181818181818181818181818181"),
            "reindex_admit",
            scalarOrdered);

        byte[] utf16Ordered = Encoding.UTF8.GetBytes(
            "{\"intent\":{\"protocol_revision\":1,\"scope\":{\"kind\":\"changed_paths\",\"paths\":[\"Assets/\\uD800\\uDC00\",\"Assets/\\uE000\"]}}}");
        ExpectFailure(
            () => BusinessCodec.CreateRequest(
                binding,
                RequestId.Parse("request-v1:82828282828282828282828282828282"),
                "reindex_admit",
                utf16Ordered),
            "UTF-16-only path ordering");
    }

    private static async Task AssertFramedStreamPoisoningAsync()
    {
        using (var stream = new MemoryStream(new byte[] { 0, 0, 0, 2 }))
        using (var framed = new FramedProtocolStream(stream))
        {
            await ExpectFailureAsync(
                () => framed.ReadPayloadAsync(1, CancellationToken.None),
                "oversized frame declaration").ConfigureAwait(false);
            await ExpectFailureAsync(
                () => framed.ReadPayloadAsync(1, CancellationToken.None),
                "oversized frame stream reuse").ConfigureAwait(false);
        }

        using var cancellation = new CancellationTokenSource();
        using var partial = new CancelAfterPartialReadStream(cancellation);
        using var cancelled = new FramedProtocolStream(partial);
        await ExpectFailureAsync(
            () => cancelled.ReadPayloadAsync(8, cancellation.Token),
            "cancelled partial frame").ConfigureAwait(false);
        await ExpectFailureAsync(
            () => cancelled.ReadPayloadAsync(8, CancellationToken.None),
            "cancelled frame stream reuse").ConfigureAwait(false);
    }

    private static JsonSerializerOptions ManifestOptions() => new()
    {
        PropertyNameCaseInsensitive = false,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
    };

    private static JsonSerializerOptions CanonicalOptions() => new()
    {
        Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
        TypeInfoResolver = new DefaultJsonTypeInfoResolver(),
        WriteIndented = false,
    };

    private static string ResolveFixtureRoot(string[] args)
    {
        if (args.Length > 0)
        {
            return Path.GetFullPath(args[0]);
        }

        for (DirectoryInfo? cursor = new(Environment.CurrentDirectory); cursor is not null; cursor = cursor.Parent)
        {
            string direct = Path.Combine(cursor.FullName, "fixtures", "manifest.json");
            if (File.Exists(direct))
            {
                return Path.GetDirectoryName(direct)!;
            }

            string repository = Path.Combine(cursor.FullName, "integration", "search-protocol", "fixtures", "manifest.json");
            if (File.Exists(repository))
            {
                return Path.GetDirectoryName(repository)!;
            }
        }

        throw new DirectoryNotFoundException("Could not find integration/search-protocol/fixtures. Pass the fixture directory as the first argument.");
    }

    private static byte[] ReadNonEmpty(string path)
    {
        byte[] bytes = File.ReadAllBytes(path);
        Require(bytes.Length > 0 && bytes.Any(value => !char.IsWhiteSpace((char)value)), $"Fixture is empty: {path}");
        return bytes;
    }

    private static string ReadFixtureText(string fixtureRoot, string relativePath)
    {
        return Encoding.UTF8.GetString(TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, relativePath))));
    }

    private static JsonObject ParseObjectNode(string json)
    {
        return JsonNode.Parse(json)?.AsObject()
            ?? throw new InvalidOperationException("Expected a JSON object while constructing a conformance case.");
    }

    private static byte[] SerializeNode(JsonNode node)
    {
        return Encoding.UTF8.GetBytes(node.ToJsonString(CanonicalOptions()));
    }

    private static string ReplaceExactly(string source, string oldValue, string newValue, string subject)
    {
        int offset = source.IndexOf(oldValue, StringComparison.Ordinal);
        Require(offset >= 0, $"Could not locate {subject} in its conformance fixture.");
        Require(
            source.IndexOf(oldValue, offset + oldValue.Length, StringComparison.Ordinal) < 0,
            $"Expected exactly one {subject} occurrence in its conformance fixture.");
        return source.Substring(0, offset) + newValue + source.Substring(offset + oldValue.Length);
    }

    private static byte[] TrimTerminalNewline(byte[] bytes)
    {
        int length = bytes.Length;
        while (length > 0 && (bytes[length - 1] == (byte)'\n' || bytes[length - 1] == (byte)'\r'))
        {
            length--;
        }
        return length == bytes.Length ? bytes : bytes[..length];
    }

    private static Exception ExpectFailure(Action action, string subject)
    {
        try
        {
            action();
        }
        catch (ProtocolValidationException error)
        {
            return error;
        }
        throw new InvalidOperationException($"Expected rejection did not occur: {subject}");
    }

    private static async Task<Exception> ExpectFailureAsync(Func<Task> action, string subject)
    {
        try
        {
            await action().ConfigureAwait(false);
        }
        catch (Exception error)
        {
            return error;
        }
        throw new InvalidOperationException($"Expected rejection did not occur: {subject}");
    }

    private static void Require(bool condition, string message)
    {
        if (!condition)
        {
            throw new InvalidOperationException(message);
        }
    }
}

internal sealed class CancelAfterPartialReadStream : Stream
{
    private readonly byte[] bytes = { 0, 0, 0, 2, (byte)'x' };
    private readonly CancellationTokenSource cancellation;
    private int offset;

    internal CancelAfterPartialReadStream(CancellationTokenSource cancellation)
    {
        this.cancellation = cancellation;
    }

    public override bool CanRead => true;

    public override bool CanSeek => false;

    public override bool CanWrite => true;

    public override long Length => throw new NotSupportedException();

    public override long Position
    {
        get => throw new NotSupportedException();
        set => throw new NotSupportedException();
    }

    public override void Flush()
    {
    }

    public override int Read(byte[] buffer, int bufferOffset, int count)
    {
        throw new NotSupportedException();
    }

    public override Task<int> ReadAsync(
        byte[] buffer,
        int bufferOffset,
        int count,
        CancellationToken cancellationToken)
    {
        if (cancellationToken.IsCancellationRequested)
        {
            return Task.FromCanceled<int>(cancellationToken);
        }
        int remaining = bytes.Length - offset;
        if (remaining == 0)
        {
            return Task.FromResult(0);
        }
        int copied = Math.Min(count, remaining);
        Buffer.BlockCopy(bytes, offset, buffer, bufferOffset, copied);
        offset += copied;
        if (offset == bytes.Length)
        {
            cancellation.Cancel();
        }
        return Task.FromResult(copied);
    }

    public override long Seek(long offset, SeekOrigin origin)
    {
        throw new NotSupportedException();
    }

    public override void SetLength(long value)
    {
        throw new NotSupportedException();
    }

    public override void Write(byte[] buffer, int bufferOffset, int count)
    {
    }
}

internal sealed class FixtureManifest
{
    [JsonPropertyName("fixture_format")]
    public int FixtureFormat { get; init; }

    [JsonPropertyName("protocol_revision")]
    public ushort ProtocolRevision { get; init; }

    [JsonPropertyName("binding")]
    public FixtureBinding Binding { get; init; } = new();

    [JsonPropertyName("valid")]
    public List<FixtureEntry> Valid { get; init; } = new();

    [JsonPropertyName("invalid")]
    public List<FixtureEntry> Invalid { get; init; } = new();
}

internal sealed class FixtureBinding
{
    [JsonPropertyName("project_id")]
    public string ProjectId { get; init; } = string.Empty;

    [JsonPropertyName("daemon_instance_id")]
    public string DaemonInstanceId { get; init; } = string.Empty;

    [JsonPropertyName("query_policy_id")]
    public string QueryPolicyId { get; init; } = string.Empty;
}

internal sealed class FixtureEntry
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = string.Empty;

    [JsonPropertyName("kind")]
    public string Kind { get; init; } = string.Empty;

    [JsonPropertyName("operation")]
    public string? Operation { get; init; }

    [JsonPropertyName("path")]
    public string Path { get; init; } = string.Empty;

    [JsonPropertyName("request")]
    public string? Request { get; init; }

    [JsonPropertyName("expected_error")]
    public string? ExpectedError { get; init; }
}
