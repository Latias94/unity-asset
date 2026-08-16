using System.Collections.Frozen;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using UnityAsset.SearchProtocol.Reference;

return await ConformanceProgram.RunAsync(args);

internal static class ConformanceProgram
{
    private const string FrozenBusinessV1InventorySha256 =
        "13cf5971f83e9a608c504582a36c442e79a982c9eb9dbad8d447a41c7694022a";
    private const string FrozenBusinessV2InventorySha256 =
        "6891e3190d36396e546989a0f55ac97766902aa37289993b5f4709ffa3ccf776";
    private const string FrozenBusinessV3InventorySha256 =
        "5774a6331cf7f560d389b86bd268639672304d4ad638dd9d8a5a6053b49a9d7a";
    private const string FrozenBusinessV4InventorySha256 =
        "43a825a10cf984122d4c6fd4f8d6d33e9d9a09cdbce17711924df46fa21b00c7";

    public static async Task<int> RunAsync(string[] args)
    {
        try
        {
            if (args.Length > 0 && string.Equals(args[0], "--real-daemon-http", StringComparison.Ordinal))
            {
                await LiveDaemonConformance.RunAsync(args[1..]).ConfigureAwait(false);
                Console.WriteLine("PASS: public C# HTTP client reached every real daemon operation");
                return 0;
            }

            string fixtureRoot = ResolveFixtureRoot(args);
            Run(fixtureRoot);
            Console.WriteLine($"PASS: search protocol v5 fixtures conform; business v1, v2, v3, and v4 remain frozen ({fixtureRoot})");
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

        Require(manifest.FixtureFormat == 3, "Unsupported fixture manifest format.");
        Require(manifest.ProtocolRevision == ProtocolConstants.BusinessProtocolRevision, "Manifest protocol revision mismatch.");
        Require(manifest.Valid.Count > 0, "Manifest has no valid fixtures.");
        Require(manifest.Invalid.Count > 0, "Manifest has no invalid fixtures.");

        var binding = new ProtocolBinding(
            manifest.ProtocolRevision,
            ProjectId.Parse(manifest.Binding.ProjectId),
            DaemonInstanceId.Parse(manifest.Binding.DaemonInstanceId),
            QueryPolicyId.Parse(manifest.Binding.QueryPolicyId));

        IReadOnlyList<FrozenBusinessInventory> frozen = AssertFrozenBusiness(
            fixtureRoot,
            manifest.FrozenInventories);
        AssertCoverage(manifest);
        AssertRevisionFiveFixtureSemantics(fixtureRoot);
        AssertManifestOwnsAllJson(fixtureRoot, manifest, frozen);
        AssertPublishedSchema(fixtureRoot);

        foreach (FixtureEntry fixture in manifest.Valid)
        {
            byte[] payload = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, fixture.Path)));
            byte[] encoded;
            if (fixture.Kind == "request")
            {
                RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(payload, binding);
                Require(request.OperationKind == fixture.Operation, $"{fixture.Name}: request operation mismatch.");
                encoded = BusinessCodec.EncodeRequest(request);
            }
            else if (fixture.Kind == "response")
            {
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
            }
            else
            {
                throw new InvalidOperationException(
                    $"{fixture.Name}: unknown fixture kind '{fixture.Kind}'.");
            }

            Require(payload.AsSpan().SequenceEqual(encoded), $"{fixture.Name}: canonical encode did not reproduce fixture bytes.");
        }

        foreach (FixtureEntry fixture in manifest.Invalid)
        {
            if (fixture.Kind != "request")
            {
                throw new InvalidOperationException(
                    $"{fixture.Name}: unknown invalid fixture kind '{fixture.Kind}'.");
            }
            byte[] payload = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, fixture.Path)));
            Exception error = ExpectFailure(
                () => BusinessCodec.DecodeRequest(payload, binding),
                fixture.Name);
            Require(
                error.Message.Contains(fixture.ExpectedError!, StringComparison.OrdinalIgnoreCase),
                $"{fixture.Name}: rejection did not identify '{fixture.ExpectedError}': {error.Message}");
        }

        AssertFixedIdsRejectNonCanonicalHex();
        AssertLoopbackEndpointDescriptorContract(binding);
        AssertHttpOperationTimeouts(fixtureRoot, binding);
        AssertJsonRequiresCanonicalEncoding(fixtureRoot, binding);
        AssertPortablePathSecondCharacterColonSemantics();
        AssertContractHardening(fixtureRoot, binding);
    }

    private static void AssertLoopbackEndpointDescriptorContract(ProtocolBinding binding)
    {
        string projectIdValue = binding.ProjectId.Value;
        string daemonInstanceIdValue = binding.DaemonInstanceId.Value;
        string queryPolicyIdValue = binding.QueryPolicyId.Value;
        string descriptorJson =
            "{\"descriptor_version\":2,"
            + $"\"project_id\":\"{projectIdValue}\","
            + $"\"daemon_instance_id\":\"{daemonInstanceIdValue}\","
            + "\"port\":42424,"
            + $"\"capability\":\"{new string('5', 64)}\","
            + $"\"business_protocol_revision\":{binding.ProtocolRevision},"
            + $"\"query_policy_id\":\"{queryPolicyIdValue}\","
            + "\"server_pid\":42}";
        byte[]? currentDescriptor = Encoding.UTF8.GetBytes(descriptorJson);
        byte[]? ReadCurrentDescriptor() => currentDescriptor;

        LoopbackEndpointDescriptor descriptor = LoopbackEndpointDescriptor.ReadFromSource(
            ReadCurrentDescriptor,
            binding.ProjectId,
            binding.QueryPolicyId);
        descriptor.RequireUnchanged();

        currentDescriptor = null;
        descriptor.RequireUnchangedOrMissingAfterShutdown();

        currentDescriptor = Encoding.UTF8.GetBytes(ReplaceExactly(
            descriptorJson,
            new string('5', 64),
            new string('6', 64),
            "endpoint capability"));
        ExpectEndpointChanged(
            descriptor.RequireUnchangedOrMissingAfterShutdown,
            "shutdown endpoint replacement");

        currentDescriptor = Encoding.UTF8.GetBytes(descriptorJson);
        ExpectFailure(
            () => LoopbackEndpointDescriptor.ReadFromSource(
                ReadCurrentDescriptor,
                ProjectId.Parse(
                    "project-v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                binding.QueryPolicyId),
            "unexpected endpoint project");
        ExpectFailure(
            () => LoopbackEndpointDescriptor.ReadFromSource(
                ReadCurrentDescriptor,
                binding.ProjectId,
                QueryPolicyId.Parse(
                    "query-policy-v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
            "unexpected endpoint query policy");

        foreach ((string field, string value) in new[]
        {
            ("project_id", projectIdValue),
            ("daemon_instance_id", daemonInstanceIdValue),
            ("query_policy_id", queryPolicyIdValue),
        })
        {
            int separator = value.IndexOf(':');
            string zeroValue = value[..(separator + 1)] + new string('0', value.Length - separator - 1);
            currentDescriptor = Encoding.UTF8.GetBytes(ReplaceExactly(
                descriptorJson,
                value,
                zeroValue,
                "endpoint " + field));
            ExpectFailure(
                () => LoopbackEndpointDescriptor.ReadFromSource(
                    ReadCurrentDescriptor,
                    binding.ProjectId,
                    binding.QueryPolicyId),
                "zero endpoint " + field);
        }
    }

    private static void AssertHttpOperationTimeouts(
        string fixtureRoot,
        ProtocolBinding binding)
    {
        RequestEnvelopeV1 status = BusinessCodec.DecodeRequest(
            TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, "requests/status-v5.json"))),
            binding);
        Require(
            ProtocolHttpClient.OperationTimeout(status) == TimeSpan.FromSeconds(60),
            "ordinary HTTP operation deadline changed");

        string reindexWait = ReadFixtureText(
            fixtureRoot,
            "requests/reindex-wait-v5.json").Replace(
                "\"timeout_ms\":30000",
                "\"timeout_ms\":300000",
                StringComparison.Ordinal);
        RequestEnvelopeV1 wait = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(reindexWait),
            binding);
        Require(
            ProtocolHttpClient.OperationTimeout(wait) == TimeSpan.FromSeconds(302),
            "reindex-wait HTTP deadline lost its response margin");
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

        Require(requestOperations.SetEquals(ConformanceOperations.All), "Request fixtures do not cover every current operation.");
        Require(responseOperations.SetEquals(ConformanceOperations.All), "Response fixtures do not cover every current operation.");
        Require(manifest.Valid.Count(entry => entry.Name == "structured error response") == 1, "Structured error fixture is missing or duplicated.");
        Require(manifest.Valid.Count(entry => entry.Name == "unanchored YAML document references response") == 1, "Unanchored YAML document fixture is missing or duplicated.");
        Require(manifest.Valid.Count(entry => entry.Name == "semantics-stale status response") == 1, "Semantics-stale status fixture is missing or duplicated.");
        Require(manifest.Valid.Count(entry => entry.Name == "configuration-stale status response") == 1, "Configuration-stale status fixture is missing or duplicated.");
        Require(manifest.Valid.Count(entry => entry.Name == "recovery-required status response") == 1, "Recovery-required status fixture is missing or duplicated.");
    }

    private static void AssertRevisionFiveFixtureSemantics(string fixtureRoot)
    {
        JsonObject references = ReadSuccessResponse(fixtureRoot, "responses/references-v5.json");
        JsonObject referenceHit = references["hits"]!.AsArray()[0]!.AsObject();
        JsonObject sourceObject = referenceHit["source_object"]!.AsObject();
        JsonObject sourceSelector = sourceObject["selector"]!.AsObject();
        Require(sourceObject["kind"]!.GetValue<string>() == "yaml", "Reference fixture source_object is not typed YAML.");
        Require(sourceSelector["kind"]!.GetValue<string>() == "file_id", "Reference fixture source_object is not file-ID anchored.");
        Require(sourceSelector["file_id"]!.GetValue<long>() == 1001, "Reference fixture source_object file ID changed.");

        JsonObject unanchored = ReadSuccessResponse(
            fixtureRoot,
            "responses/references-unanchored-document-v5.json");
        JsonObject unanchoredHit = unanchored["hits"]!.AsArray()[0]!.AsObject();
        JsonObject unanchoredSelector = unanchoredHit["source_object"]!["selector"]!.AsObject();
        Require(unanchoredSelector["kind"]!.GetValue<string>() == "unanchored", "Unanchored fixture selector kind changed.");
        Require(unanchoredSelector["document_index"]!.GetValue<uint>() == 0, "Unanchored fixture document index changed.");
        Require(!unanchoredHit["location"]!.AsObject().ContainsKey("file_id"), "Unanchored fixture leaked a legacy location file_id.");
        foreach (JsonNode? contextNode in unanchoredHit["contexts"]!.AsArray())
        {
            Require(!contextNode!.AsObject().ContainsKey("doc_file_id"), "Unanchored fixture leaked a legacy context doc_file_id.");
        }

        JsonObject semanticsStale = ReadSuccessResponse(
            fixtureRoot,
            "responses/status-semantics-stale-v5.json");
        JsonObject semanticsGeneration = semanticsStale["generation"]!["active"]!.AsObject();
        Require(!semanticsGeneration["semantics_current"]!.GetValue<bool>(), "Semantics-stale fixture does not mark semantics stale.");
        Require(semanticsGeneration["configuration_current"]!.GetValue<bool>(), "Semantics-stale fixture also changed configuration identity.");
        Require(semanticsGeneration["stale"]!.GetValue<bool>(), "Semantics-stale fixture does not mark the generation stale.");
        Require(semanticsStale["daemon"]!["freshness"]!.GetValue<string>() == "stale", "Semantics-stale fixture daemon freshness is inconsistent.");

        JsonObject configurationStale = ReadSuccessResponse(
            fixtureRoot,
            "responses/status-configuration-stale-v5.json");
        JsonObject configurationGeneration = configurationStale["generation"]!["active"]!.AsObject();
        Require(configurationGeneration["semantics_current"]!.GetValue<bool>(), "Configuration-stale fixture also changed semantic identity.");
        Require(!configurationGeneration["configuration_current"]!.GetValue<bool>(), "Configuration-stale fixture does not mark configuration stale.");
        Require(configurationGeneration["stale"]!.GetValue<bool>(), "Configuration-stale fixture does not mark the generation stale.");
        Require(configurationStale["daemon"]!["freshness"]!.GetValue<string>() == "stale", "Configuration-stale fixture daemon freshness is inconsistent.");

        JsonObject recoveryRequired = ReadSuccessResponse(
            fixtureRoot,
            "responses/status-recovery-required-v5.json");
        JsonObject maintenance = recoveryRequired["daemon"]!["generation_maintenance"]!.AsObject();
        Require(maintenance["state"]!.GetValue<string>() == "recovery_required", "Recovery fixture does not expose recovery_required.");
        Require(!string.IsNullOrWhiteSpace(maintenance["last_cleanup_failure"]!.GetValue<string>()), "Recovery fixture has no cleanup failure evidence.");

        ResponseEnvelopeV1 statusResponse = BusinessCodec.DecodeResponse(
            TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, "responses", "status-v5.json"))));
        Require(statusResponse.ReadDaemonProcessFailure() is null, "Healthy status exposed a daemon process failure.");
        SearchCapabilities capabilities = statusResponse.ReadSearchCapabilities();
        Require(
            capabilities.ProtocolRevision == ProtocolConstants.BusinessProtocolRevision
                && capabilities.BackgroundReindexDiscovery,
            "Revision 5 status fixture does not advertise background reindex discovery.");
        IReadOnlyList<BackgroundReindexOperation> operations =
            statusResponse.ReadBackgroundReindexOperations();
        Require(operations.Count > 0, "Revision 5 status fixture has no discoverable background operation.");
        Require(
            operations.Select(operation => operation.Origin).SequenceEqual(new[]
            {
                BackgroundReindexOrigin.Startup,
                BackgroundReindexOrigin.Watcher,
                BackgroundReindexOrigin.WatcherOverflow,
                BackgroundReindexOrigin.Timer,
                BackgroundReindexOrigin.SemanticUpgrade,
            }),
            "Revision 5 status fixture changed the canonical background operation order.");

        ResponseEnvelopeV1 failedStatusResponse = BusinessCodec.DecodeResponse(
            TrimTerminalNewline(ReadNonEmpty(
                Path.Combine(fixtureRoot, "responses", "status-process-failure-v5.json"))));
        DaemonProcessFailure processFailure = failedStatusResponse.ReadDaemonProcessFailure()
            ?? throw new InvalidOperationException("Process-failure status omitted its failure evidence.");
        Require(
            processFailure.Component == DaemonProcessComponent.ReindexCoordinator
                && processFailure.Cause == "reindex coordinator panicked",
            "Process-failure status changed its structured failure evidence.");

        ResponseEnvelopeV1 structuredError = BusinessCodec.DecodeResponse(
            TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, "responses", "structured-error-v5.json"))));
        _ = structuredError.ReadApiErrorCode();
    }

    private static JsonObject ReadSuccessResponse(string fixtureRoot, string relativePath)
    {
        return ParseObjectNode(ReadFixtureText(fixtureRoot, relativePath))["value"]!["response"]!.AsObject();
    }

    private static void AssertManifestOwnsAllJson(
        string fixtureRoot,
        FixtureManifest manifest,
        IReadOnlyList<FrozenBusinessInventory> frozen)
    {
        var listed = manifest.Valid.Concat(manifest.Invalid)
            .Select(entry => entry.Path.Replace('/', Path.DirectorySeparatorChar))
            .ToHashSet(StringComparer.OrdinalIgnoreCase);
        foreach (FrozenInventoryReference reference in manifest.FrozenInventories)
        {
            listed.Add(reference.Path.Replace('/', Path.DirectorySeparatorChar));
        }
        foreach (FrozenFixture fixture in frozen.SelectMany(inventory => inventory.Files))
        {
            listed.Add(fixture.Path.Replace('/', Path.DirectorySeparatorChar));
        }
        foreach (string path in Directory.EnumerateFiles(fixtureRoot, "*.json", SearchOption.AllDirectories))
        {
            string relative = Path.GetRelativePath(fixtureRoot, path);
            if (!relative.Equals("manifest.json", StringComparison.OrdinalIgnoreCase))
            {
                Require(listed.Contains(relative), $"Unlisted JSON fixture: {relative}");
            }
        }
    }

    private static void AssertPublishedSchema(string fixtureRoot)
    {
        string schemaRoot = Path.GetFullPath(Path.Combine(fixtureRoot, "..", "schema"));
        string path = Path.Combine(schemaRoot, "business-v5.schema.json");
        byte[] bytes = ReadNonEmpty(path);
        string text;
        try
        {
            text = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true)
                .GetString(bytes);
        }
        catch (DecoderFallbackException error)
        {
            throw new InvalidOperationException($"Schema is not valid UTF-8: {path}", error);
        }

        using JsonDocument document = JsonDocument.Parse(text);
        JsonElement root = document.RootElement;
        Require(root.ValueKind == JsonValueKind.Object, $"Schema root is not an object: {path}");
        Require(
            root.TryGetProperty("$schema", out JsonElement schema)
                && schema.ValueKind == JsonValueKind.String
                && schema.GetString() == "https://json-schema.org/draft/2020-12/schema",
            $"Schema does not declare Draft 2020-12: {path}");
        Require(
            root.TryGetProperty("$id", out JsonElement id)
                && id.ValueKind == JsonValueKind.String
                && !string.IsNullOrWhiteSpace(id.GetString()),
            $"Schema does not declare a stable $id: {path}");
        Require(
            root.TryGetProperty("$defs", out JsonElement definitions)
                && definitions.ValueKind == JsonValueKind.Object
                && definitions.EnumerateObject().Any(),
            $"Schema does not expose reusable definitions: {path}");
    }

    private static IReadOnlyList<FrozenBusinessInventory> AssertFrozenBusiness(
        string fixtureRoot,
        IReadOnlyList<FrozenInventoryReference> references)
    {
        var expectedDigests = new Dictionary<ushort, string>
        {
            [1] = FrozenBusinessV1InventorySha256,
            [2] = FrozenBusinessV2InventorySha256,
            [3] = FrozenBusinessV3InventorySha256,
            [4] = FrozenBusinessV4InventorySha256,
        };
        Require(references.Count == expectedDigests.Count, "Frozen business inventory set is incomplete.");
        var inventories = new List<FrozenBusinessInventory>(references.Count);
        ushort previousRevision = 0;
        foreach (FrozenInventoryReference reference in references)
        {
            Require(reference.BusinessRevision > previousRevision, "Frozen business revisions must be strictly sorted.");
            previousRevision = reference.BusinessRevision;
            Require(
                expectedDigests.TryGetValue(reference.BusinessRevision, out string? expectedDigest),
                $"Unexpected frozen business revision {reference.BusinessRevision}.");
            Require(
                string.Equals(reference.Sha256, expectedDigest, StringComparison.Ordinal),
                $"Frozen business v{reference.BusinessRevision} inventory digest reference changed.");

            byte[] inventoryBytes = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, reference.Path)));
            Require(
                string.Equals(Sha256(inventoryBytes), expectedDigest, StringComparison.Ordinal),
                $"Frozen business v{reference.BusinessRevision} inventory changed.");
            FrozenBusinessInventory inventory = JsonSerializer.Deserialize<FrozenBusinessInventory>(
                inventoryBytes,
                ManifestOptions()) ?? throw new InvalidOperationException("Frozen business inventory decoded to null.");
            Require(inventory.InventoryFormat == 1, "Unsupported frozen inventory format.");
            Require(inventory.BusinessRevision == reference.BusinessRevision, "Frozen inventory business revision mismatch.");
            Require(inventory.Files.Count > 0, "Frozen business inventory is empty.");

            string suffix = $"-v{reference.BusinessRevision}.json";
            string? previous = null;
            var inventoried = new HashSet<string>(StringComparer.Ordinal);
            foreach (FrozenFixture fixture in inventory.Files)
            {
                Require(
                    previous is null || string.CompareOrdinal(previous, fixture.Path) < 0,
                    "Frozen business inventory paths must be strictly sorted.");
                previous = fixture.Path;
                Require(
                    fixture.Path.EndsWith(suffix, StringComparison.Ordinal)
                        && (fixture.Path.StartsWith("requests/", StringComparison.Ordinal)
                            || fixture.Path.StartsWith("responses/", StringComparison.Ordinal)
                            || fixture.Path.StartsWith("invalid/request-", StringComparison.Ordinal)),
                    $"Unexpected frozen business fixture path: {fixture.Path}");
                Require(inventoried.Add(fixture.Path), $"Duplicate frozen fixture: {fixture.Path}");

                byte[] payload = TrimTerminalNewline(ReadNonEmpty(Path.Combine(fixtureRoot, fixture.Path)));
                Require(payload.Length == fixture.EncodedBytes, $"{fixture.Path}: encoded byte length changed.");
                Require(
                    string.Equals(Sha256(payload), fixture.Sha256, StringComparison.Ordinal),
                    $"{fixture.Path}: frozen business v{reference.BusinessRevision} bytes changed.");
            }

            var archived = Directory.EnumerateFiles(Path.Combine(fixtureRoot, "requests"), $"*-v{reference.BusinessRevision}.json")
                .Concat(Directory.EnumerateFiles(Path.Combine(fixtureRoot, "responses"), $"*-v{reference.BusinessRevision}.json"))
                .Concat(Directory.EnumerateFiles(Path.Combine(fixtureRoot, "invalid"), $"request-*-v{reference.BusinessRevision}.json"))
                .Select(path => Path.GetRelativePath(fixtureRoot, path).Replace(Path.DirectorySeparatorChar, '/'))
                .ToHashSet(StringComparer.Ordinal);
            Require(
                inventoried.SetEquals(archived),
                $"Frozen business v{reference.BusinessRevision} inventory is incomplete or owns extra files.");
            inventories.Add(inventory);
        }
        return inventories;
    }

    private static void AssertFixedIdsRejectNonCanonicalHex()
    {
        string uppercase = "project-v1:" + new string('A', 64);
        ExpectFailure(() => ProjectId.Parse(uppercase), "uppercase fixed ID");
    }

    private static void AssertJsonRequiresCanonicalEncoding(string fixtureRoot, ProtocolBinding binding)
    {
        string request = ReadFixtureText(fixtureRoot, "requests/search-v5.json");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(request.Insert(1, " ")),
                binding),
            "non-canonical JSON whitespace");

        const string writerInput =
            "{\"unicode\":\"A\\u3000B\\uE000\\uD800\\uDC00\",\"controls\":\"\\u0000\\b\\t\\n\\f\\r\\u001F\",\"signed_zero\":-0}";
        const string contractCanonical =
            "{\"unicode\":\"A\u3000B\uE000\U00010000\",\"controls\":\"\\u0000\\b\\t\\n\\f\\r\\u001f\",\"signed_zero\":0}";
        using JsonDocument writerDocument = JsonDocument.Parse(writerInput);
        byte[] writerEncoded = CanonicalJson.Write(writer => writerDocument.RootElement.WriteTo(writer));
        Require(
            writerEncoded.AsSpan().SequenceEqual(Encoding.UTF8.GetBytes(contractCanonical)),
            "C# contract JSON writer does not match Rust contract string and integer bytes.");
        StrictJson.ParseObject(Encoding.UTF8.GetBytes(contractCanonical), "contract canonical probe");
        ExpectFailure(
            () => StrictJson.ParseObject(Encoding.UTF8.GetBytes(writerInput), "non-canonical contract probe"),
            "escaped Unicode, uppercase control escape, and signed integer zero");
        ExpectFailure(
            () => StrictJson.ParseObject(
                Encoding.UTF8.GetBytes("{\"value\":1.5}"),
                "floating-point protocol probe"),
            "floating-point number outside the integer-only protocol");

        const string canonicalSearchPayload =
            "{\"query\":\"ideographic\u3000space\\u001f\",\"limit\":1}";
        RequestEnvelopeV1 canonicalSearch = BusinessCodec.CreateRequest(
            binding,
            RequestId.Parse("request-v1:91919191919191919191919191919191"),
            "search",
            Encoding.UTF8.GetBytes(canonicalSearchPayload));
        Require(
            Encoding.UTF8.GetString(BusinessCodec.EncodeRequest(canonicalSearch))
                .Contains("\"query\":\"ideographic\u3000space\\u001f\"", StringComparison.Ordinal),
            "Business request encoding did not preserve serde_json Unicode and control escaping.");

        const string signedZeroSelector =
            "{\"direction\":\"outgoing\",\"selector\":{\"kind\":\"guid\",\"guid\":\"0123456789abcdef0123456789abcdef\",\"file_id\":-0},\"limit\":1}";
        ExpectFailure(
            () => BusinessCodec.CreateRequest(
                binding,
                RequestId.Parse("request-v1:92929292929292929292929292929292"),
                "references",
                Encoding.UTF8.GetBytes(signedZeroSelector)),
            "signed -0 selector file ID");

        const string unicodeSelector =
            "{\"kind\":\"object\",\"address\":{\"kind\":\"yaml\",\"version\":2,\"source\":{\"version\":1,\"outer_path\":\"Assets/ideographic\u3000space.prefab\",\"members\":[]},\"selector\":{\"kind\":\"file_id\",\"file_id\":1001}}}";
        const string queryBinding =
            "reference-query-v2:611e0c3023cefa5fe3fb52669b57d4eb8a04b0c86a1286ce341a3bf40be8f2d1";
        string cursorRequest =
            "{\"direction\":\"outgoing\",\"selector\":"
            + unicodeSelector
            + ",\"limit\":1,\"cursor\":{\"generation\":\"blake3-v1:"
            + new string('6', 64)
            + "\",\"query_policy_id\":\""
            + binding.QueryPolicyId.Value
            + "\",\"after_stable_id\":\"reference:unicode\",\"query_binding\":\""
            + queryBinding
            + "\"}}";
        BusinessCodec.CreateRequest(
            binding,
            RequestId.Parse("request-v1:93939393939393939393939393939393"),
            "references",
            Encoding.UTF8.GetBytes(cursorRequest));
    }

    private static void AssertPortablePathSecondCharacterColonSemantics()
    {
        using JsonDocument digitPrefix = JsonDocument.Parse("\"1:relative/path\"");
        ExpectFailure(
            () => StrictJson.PortablePath(
                digitPrefix.RootElement,
                "portable path second-character colon",
                requireRelative: true,
                rejectControlCharacters: true),
            "second-character colon prefix");
    }

    private static string Sha256(byte[] payload)
    {
        using SHA256 sha256 = SHA256.Create();
        return Convert.ToHexString(sha256.ComputeHash(payload)).ToLowerInvariant();
    }

    private static void AssertContractHardening(string fixtureRoot, ProtocolBinding binding)
    {
        const string referenceBinding = "reference-query-v2:d9f13e496b1348267125438d3cef749cbe2f1ef0f31c2b133accdb842f3dca9f";
        const string generation = "blake3-v1:6666666666666666666666666666666666666666666666666666666666666666";
        string policy = binding.QueryPolicyId.Value;
        string referencesRequest = ReadFixtureText(fixtureRoot, "requests/references-v5.json");
        string yamlObjectRequest = ReadFixtureText(fixtureRoot, "requests/references-yaml-object-v5.json");
        BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(yamlObjectRequest), binding);
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    yamlObjectRequest,
                    "\"file_id\":1001",
                    "\"file_id\":0",
                    "zero YAML file ID")),
                binding),
            "zero YAML file ID");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    yamlObjectRequest,
                    "\"kind\":\"yaml\",\"version\":2",
                    "\"kind\":\"yaml\",\"version\":1",
                    "legacy YAML address version")),
                binding),
            "legacy YAML address version");
        ExpectFailure(
            () => BusinessCodec.DecodeRequest(
                Encoding.UTF8.GetBytes(ReplaceExactly(
                    yamlObjectRequest,
                    "\"selector\":{\"kind\":\"file_id\",\"file_id\":1001}",
                    "\"selector\":{\"kind\":\"anchored\",\"anchor\":\"1001\"}",
                    "legacy YAML selector")),
                binding),
            "legacy YAML selector");
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
                    "reference-query-v2:" + new string('0', 64),
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
        AssertReindexFailedState(fixtureRoot, binding);
        AssertCanonicalCoreValues(fixtureRoot, binding);
        AssertUnicodeScalarPathOrdering(binding);
        AssertStatusPathBudget(fixtureRoot);
        AssertBackgroundReindexOperationContract(fixtureRoot);
        AssertOperationControlForbiddenCode(fixtureRoot);
        AssertErrorJsonBudget(fixtureRoot, binding);
        AssertReindexPublishWarningBudget(fixtureRoot);
    }

    private static void AssertResponseRequestBindings(string fixtureRoot, ProtocolBinding binding)
    {
        string searchRequestText = ReadFixtureText(fixtureRoot, "requests/search-v5.json");
        string searchResponseText = ReadFixtureText(fixtureRoot, "responses/search-v5.json");
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

        string suggestRequestText = ReadFixtureText(fixtureRoot, "requests/suggest-v5.json");
        string suggestResponseText = ReadFixtureText(fixtureRoot, "responses/suggest-v5.json");
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
        JsonObject oversizedStatus = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        var roots = new JsonArray();
        for (int index = 0; index < 8; index++)
        {
            roots.Add("root-" + index + "/" + new string('x', 30 * 1024));
        }
        oversizedStatus["value"]!["response"]!["scan_roots"] = roots;

        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(oversizedStatus)),
            "status response path JSON byte limit");

        JsonObject oversizedFailure = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
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

        JsonObject idleBuilding = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        idleBuilding["value"]!["response"]!["generation"]!["building_revision"] =
            "blake3-v1:" + new string('b', 64);
        idleBuilding["value"]!["response"]!["indexing"] = false;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(idleBuilding)),
            "idle status building revision");

        JsonObject unavailableActive = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        unavailableActive["value"]!["response"]!["daemon"]!["serving"] = "unavailable";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(unavailableActive)),
            "active generation serving availability");

        JsonObject staleCurrent = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        staleCurrent["value"]!["response"]!["daemon"]!["freshness"] = "stale";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(staleCurrent)),
            "generation freshness evidence");

        JsonObject recoveryWithoutFailure = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        recoveryWithoutFailure["value"]!["response"]!["daemon"]!["generation_maintenance"]!["state"] =
            "recovery_required";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(recoveryWithoutFailure)),
            "generation maintenance failure evidence");

        JsonObject cleanWithFailure = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        cleanWithFailure["value"]!["response"]!["daemon"]!["generation_maintenance"]!["last_cleanup_failure"] =
            "staging cleanup failed";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(cleanWithFailure)),
            "clean generation maintenance cannot retain failure evidence");

        JsonObject retryingWithoutFailure = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        retryingWithoutFailure["value"]!["response"]!["daemon"]!["watcher"]!["state"] = "retrying";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(retryingWithoutFailure)),
            "watcher retry failure evidence");

        JsonObject disabledTimerWithDeadline = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        disabledTimerWithDeadline["value"]!["response"]!["daemon"]!["timer"]!["state"] = "disabled";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(disabledTimerWithDeadline)),
            "disabled timer next run");

        JsonObject failureWhileServing = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/status-process-failure-v5.json"));
        failureWhileServing["value"]!["response"]!["daemon"]!["lifecycle"] = "serving";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(failureWhileServing)),
            "process failure serving lifecycle");

        JsonObject mismatchedWatcherFailure = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/status-process-failure-v5.json"));
        JsonObject failedDaemon = mismatchedWatcherFailure["value"]!["response"]!["daemon"]!.AsObject();
        failedDaemon["process_failure"]!["component"] = "filesystem_watcher";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(mismatchedWatcherFailure)),
            "process failure component evidence");
    }

    private static void AssertBackgroundReindexOperationContract(string fixtureRoot)
    {
        JsonObject status = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonArray operations = status["value"]!["response"]!["daemon"]!["background_reindex_operations"]!.AsArray();
        Require(operations.Count > 0, "Background-operation hardening requires a non-empty status fixture.");

        JsonObject lost = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        lost["value"]!["response"]!["daemon"]!["background_reindex_operations"]![0]!["state"] = "lost";
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(lost)),
            "lost background operation");

        JsonObject duplicateOrigin = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonArray duplicateOriginOperations = duplicateOrigin["value"]!["response"]!["daemon"]!["background_reindex_operations"]!.AsArray();
        JsonObject duplicated = JsonNode.Parse(duplicateOriginOperations[0]!.ToJsonString())!.AsObject();
        duplicated["operation_id"] = "operation-v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        duplicateOriginOperations.Insert(1, duplicated);
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(duplicateOrigin)),
            "duplicate background operation origin");

        JsonObject duplicateId = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonArray duplicateIdOperations = duplicateId["value"]!["response"]!["daemon"]!["background_reindex_operations"]!.AsArray();
        JsonObject second = JsonNode.Parse(duplicateIdOperations[0]!.ToJsonString())!.AsObject();
        second["origin"] = "watcher";
        duplicateIdOperations.Insert(1, second);
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(duplicateId)),
            "duplicate background operation ID");

        JsonObject unsorted = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonArray unsortedOperations = unsorted["value"]!["response"]!["daemon"]!["background_reindex_operations"]!.AsArray();
        JsonObject earlier = JsonNode.Parse(unsortedOperations[0]!.ToJsonString())!.AsObject();
        earlier["origin"] = "startup";
        earlier["operation_id"] = "operation-v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        unsortedOperations[0]!["origin"] = "watcher";
        unsortedOperations.Insert(1, earlier);
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(unsorted)),
            "unsorted background operation origins");

        JsonObject tooMany = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonArray tooManyOperations = tooMany["value"]!["response"]!["daemon"]!["background_reindex_operations"]!.AsArray();
        tooManyOperations.Clear();
        for (int index = 0; index < 6; index++)
        {
            tooManyOperations.Add(BackgroundOperation("startup", (char)('a' + index), "queued"));
        }
        Exception tooManyError = ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(tooMany)),
            "too many background operations");
        Require(
            tooManyError.Message.EndsWith("exceeds 5 entries", StringComparison.Ordinal),
            "Background-operation count hardening did not exercise the explicit entry limit.");
    }

    private static JsonObject BackgroundOperation(string origin, char idDigit, string state)
    {
        return new JsonObject
        {
            ["origin"] = origin,
            ["operation_id"] = "operation-v1:" + new string(idDigit, 32),
            ["state"] = state,
        };
    }

    private static void AssertOperationControlForbiddenCode(string fixtureRoot)
    {
        JsonObject error = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/structured-error-v5.json"));
        ResponseEnvelopeV1 response = BusinessCodec.DecodeResponse(SerializeNode(error));
        Require(
            response.ReadApiErrorCode() == ApiErrorCode.OperationControlForbidden,
            "C# structured errors did not project operation_control_forbidden.");
    }

    private static void AssertErrorJsonBudget(string fixtureRoot, ProtocolBinding binding)
    {
        RequestEnvelopeV1 shutdown = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/shutdown-v5.json")),
            binding);
        JsonObject errorNode = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/structured-error-v5.json"));
        errorNode["request_id"] = shutdown.RequestId.ToString();
        errorNode["value"]!["message"] = new string('x', 16 * 1024);
        ResponseEnvelopeV1 error = BusinessCodec.DecodeResponse(SerializeNode(errorNode));
        error.ValidateFor(shutdown);
        BusinessCodec.EncodeResponse(error);

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
            ReadFixtureText(fixtureRoot, "responses/reindex-admit-v5.json"));
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
            ReadFixtureText(fixtureRoot, "requests/references-v5.json"),
            "\"limit\":25",
            "\"limit\":1",
            "reference request limit");
        RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(Encoding.UTF8.GetBytes(requestText), binding);

        JsonObject missingSourceObject = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        missingSourceObject["value"]!["response"]!["hits"]![0]!.AsObject().Remove("source_object");
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(missingSourceObject)),
            "reference source_object requirement");

        JsonObject mismatchedLocation = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        mismatchedLocation["value"]!["response"]!["hits"]![0]!["location"]!["file_id"] = 1002;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(mismatchedLocation)),
            "reference source_object location binding");

        JsonObject mismatchedContext = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        mismatchedContext["value"]!["response"]!["hits"]![0]!["contexts"]![0]!["doc_file_id"] = 1002;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(mismatchedContext)),
            "reference source_object context binding");

        JsonObject unanchoredWithLegacyFileId = ParseObjectNode(
            ReadFixtureText(fixtureRoot, "responses/references-unanchored-document-v5.json"));
        unanchoredWithLegacyFileId["value"]!["response"]!["hits"]![0]!["location"]!["file_id"] = 1;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(unanchoredWithLegacyFileId)),
            "unanchored reference legacy file_id");

        JsonObject responseRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        JsonObject response = responseRoot["value"]!["response"]!.AsObject();
        response["request"]!["limit"] = 1;
        JsonObject coverage = response["coverage"]!.AsObject();
        coverage["returned"] = 2;
        coverage["total"] = 2;
        JsonArray hits = response["hits"]!.AsArray();
        hits.Add(hits[0]!.DeepClone());
        ResponseEnvelopeV1 tooManyReferences = BusinessCodec.DecodeResponse(SerializeNode(responseRoot));
        ExpectFailure(() => tooManyReferences.ValidateFor(request), "reference response limit binding");

        JsonObject diagnosticBytes = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        diagnosticBytes["value"]!["response"]!["diagnostic_coverage"]!["serialized_bytes"] = 3;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(diagnosticBytes)),
            "reference diagnostic serialized byte accounting");

        JsonObject diagnosticRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
        JsonObject diagnosticResponse = diagnosticRoot["value"]!["response"]!.AsObject();
        var diagnosticAddress = new JsonObject
        {
            ["kind"] = "yaml",
            ["version"] = 2,
            ["source"] = new JsonObject
            {
                ["version"] = 1,
                ["outer_path"] = "Assets/Prefabs/Player.prefab",
                ["members"] = new JsonArray(),
            },
            ["selector"] = new JsonObject
            {
                ["kind"] = "file_id",
                ["file_id"] = -1,
            },
        };
        var diagnostics = new JsonArray
        {
            new JsonObject
            {
                ["version"] = (int)ProtocolConstants.CoreDiagnosticVersion,
                ["severity"] = "warning",
                ["code"] = "YAML_REFERENCE_TEST",
                ["message"] = "fixture\u3000diagnostic\u001f",
                ["address"] = diagnosticAddress,
                ["field_path"] = null,
            },
        };
        diagnosticResponse["diagnostics"] = diagnostics;
        JsonObject diagnosticCoverage = diagnosticResponse["diagnostic_coverage"]!.AsObject();
        diagnosticCoverage["returned"] = 1;
        diagnosticCoverage["total"] = 1;
        byte[] serializedDiagnostics = SerializeNode(diagnostics);
        Require(
            Encoding.UTF8.GetString(serializedDiagnostics)
                .Contains("\"message\":\"fixture\u3000diagnostic\\u001f\"", StringComparison.Ordinal),
            "Diagnostic byte accounting did not use Rust contract JSON string bytes.");
        diagnosticCoverage["serialized_bytes"] = serializedDiagnostics.Length;
        BusinessCodec.DecodeResponse(SerializeNode(diagnosticRoot));

        diagnostics[0]!["version"] = 1;
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(diagnosticRoot)),
            "legacy diagnostic version");

        JsonObject pagedRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/references-v5.json"));
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
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/references-v5.json")),
            binding);
        pagedResponse.ValidateFor(originalRequest);

        string searchResponse = ReadFixtureText(fixtureRoot, "responses/search-v5.json");
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
        JsonObject operationRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/reindex-status-v5.json"));
        JsonObject running = operationRoot["value"]!["response"]!.AsObject();
        JsonObject admission = running["admission"]!.AsObject();
        JsonObject statusRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        JsonObject status = statusRoot["value"]!["response"]!.AsObject();
        JsonObject activeGeneration = status["generation"]!["active"]!.AsObject();
        var completion = new JsonObject
        {
            ["protocol_revision"] = (int)ProtocolConstants.BusinessProtocolRevision,
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
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/reindex-status-v5.json")),
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

    private static void AssertReindexFailedState(string fixtureRoot, ProtocolBinding binding)
    {
        JsonObject failedRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/reindex-wait-v5.json"));
        RequestEnvelopeV1 request = BusinessCodec.DecodeRequest(
            Encoding.UTF8.GetBytes(ReadFixtureText(fixtureRoot, "requests/reindex-wait-v5.json")),
            binding);
        BusinessCodec.DecodeResponse(SerializeNode(failedRoot)).ValidateFor(request);

        JsonObject contradictory = failedRoot.DeepClone().AsObject();
        JsonObject statusRoot = ParseObjectNode(ReadFixtureText(fixtureRoot, "responses/status-v5.json"));
        contradictory["value"]!["response"]!["status"] =
            statusRoot["value"]!["response"]!.DeepClone();
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(contradictory)),
            "failed reindex status snapshot");

        JsonObject wrongPolicy = failedRoot.DeepClone().AsObject();
        wrongPolicy["value"]!["response"]!["error"]!["query_policy_id"] =
            "query-policy-v1:" + new string('5', 64);
        ExpectFailure(
            () => BusinessCodec.DecodeResponse(SerializeNode(wrongPolicy)),
            "failed reindex query policy binding");
    }

    private static void AssertCanonicalCoreValues(string fixtureRoot, ProtocolBinding binding)
    {
        string response = ReadFixtureText(fixtureRoot, "responses/references-v5.json");
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

    }

    private static void AssertUnicodeScalarPathOrdering(ProtocolBinding binding)
    {
        byte[] scalarOrdered = Encoding.UTF8.GetBytes(
            $"{{\"intent\":{{\"protocol_revision\":{ProtocolConstants.BusinessProtocolRevision},\"scope\":{{\"kind\":\"changed_paths\",\"paths\":[\"Assets/\uE000\",\"Assets/\U00010000\"]}}}}}}");
        BusinessCodec.CreateRequest(
            binding,
            RequestId.Parse("request-v1:81818181818181818181818181818181"),
            "reindex_admit",
            scalarOrdered);

        byte[] utf16Ordered = Encoding.UTF8.GetBytes(
            $"{{\"intent\":{{\"protocol_revision\":{ProtocolConstants.BusinessProtocolRevision},\"scope\":{{\"kind\":\"changed_paths\",\"paths\":[\"Assets/\U00010000\",\"Assets/\uE000\"]}}}}}}");
        ExpectFailure(
            () => BusinessCodec.CreateRequest(
                binding,
                RequestId.Parse("request-v1:82828282828282828282828282828282"),
                "reindex_admit",
                utf16Ordered),
            "UTF-16-only path ordering");
    }

    private static JsonSerializerOptions ManifestOptions() => new()
    {
        PropertyNameCaseInsensitive = false,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
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
        return CanonicalJson.Write(writer => node.WriteTo(writer));
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

    private static void ExpectEndpointChanged(Action action, string subject)
    {
        try
        {
            action();
        }
        catch (ProtocolEndpointChangedException)
        {
            return;
        }
        throw new InvalidOperationException($"Expected endpoint replacement was not detected: {subject}");
    }

    private static void Require(bool condition, string message)
    {
        if (!condition)
        {
            throw new InvalidOperationException(message);
        }
    }
}

internal static class ConformanceOperations
{
    internal static IReadOnlySet<string> All { get; } = new[]
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
    }.ToFrozenSet(StringComparer.Ordinal);
}

internal sealed class FixtureManifest
{
    [JsonPropertyName("fixture_format")]
    public int FixtureFormat { get; init; }

    [JsonPropertyName("protocol_revision")]
    public ushort ProtocolRevision { get; init; }

    [JsonPropertyName("frozen_inventories")]
    public List<FrozenInventoryReference> FrozenInventories { get; init; } = new();

    [JsonPropertyName("binding")]
    public FixtureBinding Binding { get; init; } = new();

    [JsonPropertyName("valid")]
    public List<FixtureEntry> Valid { get; init; } = new();

    [JsonPropertyName("invalid")]
    public List<FixtureEntry> Invalid { get; init; } = new();
}

internal sealed class FrozenInventoryReference
{
    [JsonPropertyName("business_revision")]
    public ushort BusinessRevision { get; init; }

    [JsonPropertyName("path")]
    public string Path { get; init; } = string.Empty;

    [JsonPropertyName("sha256")]
    public string Sha256 { get; init; } = string.Empty;
}

internal sealed class FrozenBusinessInventory
{
    [JsonPropertyName("inventory_format")]
    public int InventoryFormat { get; init; }

    [JsonPropertyName("business_revision")]
    public ushort BusinessRevision { get; init; }

    [JsonPropertyName("files")]
    public List<FrozenFixture> Files { get; init; } = new();
}

internal sealed class FrozenFixture
{
    [JsonPropertyName("path")]
    public string Path { get; init; } = string.Empty;

    [JsonPropertyName("encoded_bytes")]
    public int EncodedBytes { get; init; }

    [JsonPropertyName("sha256")]
    public string Sha256 { get; init; } = string.Empty;
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
