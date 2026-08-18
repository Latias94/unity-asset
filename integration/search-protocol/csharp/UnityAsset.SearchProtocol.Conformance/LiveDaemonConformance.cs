using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;
using UnityAsset.SearchProtocol.Reference;

internal static class LiveDaemonConformance
{
    internal static async Task RunAsync(string[] args)
    {
        Require(
            args.Length == 3,
            "Real-daemon mode requires a descriptor path, expected project ID, and expected query-policy ID.");
        string descriptorPath = Path.GetFullPath(args[0]);
        byte[]? ReadCurrentDescriptor()
        {
            try
            {
                using var descriptor = new FileStream(
                    descriptorPath,
                    FileMode.Open,
                    FileAccess.Read,
                    FileShare.Read | FileShare.Delete);
                using var encoded = new MemoryStream();
                descriptor.CopyTo(encoded);
                return encoded.ToArray();
            }
            catch (Exception error) when (
                error is FileNotFoundException
                || error is DirectoryNotFoundException)
            {
                return null;
            }
        }

        LoopbackEndpointDescriptor endpoint = LoopbackEndpointDescriptor.ReadFromSource(
            ReadCurrentDescriptor,
            ProjectId.Parse(args[1]),
            QueryPolicyId.Parse(args[2]));
        using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        await AssertHttpBoundaryRejectsUnauthenticatedAndBrowserRequestsAsync(
            endpoint,
            cancellation.Token).ConfigureAwait(false);
        using ProtocolHttpClient client = ProtocolHttpClient.Open(endpoint);

        var observedOperations = new HashSet<string>(StringComparer.Ordinal);
        int requestOrdinal = 1;
        async Task<ResponseEnvelopeV1> ExchangeResponseAsync(string operation, object payload)
        {
            RequestId requestId = RequestId.Parse($"request-v1:{requestOrdinal++:x32}");
            RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
                client.Binding,
                requestId,
                operation,
                JsonSerializer.SerializeToUtf8Bytes(payload));
            ResponseEnvelopeV1 response = await client.ExchangeAsync(
                request,
                cancellation.Token).ConfigureAwait(false);
            observedOperations.Add(operation);
            return response;
        }

        async Task<JsonElement> ExchangeAsync(string operation, object payload)
        {
            ResponseEnvelopeV1 response = await ExchangeResponseAsync(operation, payload)
                .ConfigureAwait(false);
            Require(!response.IsError, $"Real daemon returned an error for {operation}: {response.Value}");
            Require(
                string.Equals(response.OperationKind, operation, StringComparison.Ordinal),
                $"Real daemon returned {response.OperationKind} for {operation}.");
            return response.Value.GetProperty("response").Clone();
        }

        ResponseEnvelopeV1 capabilitiesResponse = await ExchangeResponseAsync("capabilities", new { })
            .ConfigureAwait(false);
        Require(!capabilitiesResponse.IsError, "Real daemon rejected the capabilities request.");
        SearchCapabilities capabilitySet = capabilitiesResponse.ReadSearchCapabilities();
        Require(
            capabilitySet.ProtocolRevision == ProtocolConstants.BusinessProtocolRevision
                && capabilitySet.BackgroundReindexDiscovery,
            "Real daemon did not advertise background reindex discovery.");
        JsonElement capabilities = capabilitiesResponse.Value.GetProperty("response");
        Require(capabilities.GetProperty("daemon_version").GetString() is { Length: > 0 }, "Capabilities omitted daemon version.");

        ResponseEnvelopeV1 initialStatusResponse = await ExchangeResponseAsync("status", new { })
            .ConfigureAwait(false);
        Require(!initialStatusResponse.IsError, "Real daemon rejected the initial status request.");
        JsonElement initialStatus = initialStatusResponse.Value.GetProperty("response");
        Require(
            initialStatus.GetProperty("daemon").GetProperty("lifecycle").GetString() == "serving",
            "Real daemon was not serving after endpoint discovery.");

        ResponseEnvelopeV1 admissionResponse = await ExchangeResponseAsync(
            "reindex_admit",
            new
            {
                intent = new
                {
                    protocol_revision = ProtocolConstants.BusinessProtocolRevision,
                    scope = new { kind = "full" },
                },
                idempotency_key = "csharp-live-daemon-v1",
            }).ConfigureAwait(false);
        Require(!admissionResponse.IsError, $"Real daemon rejected reindex admission: {admissionResponse.Value}");
        string operationId = admissionResponse.ReadReindexOperationId().Value;

        JsonElement operationStatus = await ExchangeAsync(
            "reindex_status",
            new { operation_id = operationId }).ConfigureAwait(false);
        string state = operationStatus.GetProperty("state").GetString() ?? string.Empty;
        Require(
            state is "queued" or "coalesced" or "running" or "succeeded",
            $"Freshly admitted reindex reported impossible state '{state}'.");
        Require(
            operationStatus.TryGetProperty("admission", out JsonElement statusAdmission)
                && statusAdmission.ValueKind == JsonValueKind.Object,
            "Reindex status omitted admission evidence.");

        IReadOnlyList<BackgroundReindexOperation> backgroundOperations =
            initialStatusResponse.ReadBackgroundReindexOperations();
        BackgroundReindexOperation? backgroundOperation = backgroundOperations.FirstOrDefault(
            operation => operation.Origin == BackgroundReindexOrigin.Startup);
        Require(
            backgroundOperation is not null,
            "Real daemon status did not expose its startup reindex operation.");
        JsonElement backgroundStatus = await ExchangeAsync(
            "reindex_status",
            new { operation_id = backgroundOperation!.OperationId.Value }).ConfigureAwait(false);
        Require(
            backgroundStatus.GetProperty("operation_id").GetString() == backgroundOperation.OperationId.Value,
            "Background reindex discovery returned an operation ID that could not be queried.");

        ResponseEnvelopeV1 forbiddenCancel = await ExchangeResponseAsync(
            "reindex_cancel",
            new { operation_id = backgroundOperation.OperationId.Value }).ConfigureAwait(false);
        Require(forbiddenCancel.IsError, "Real daemon allowed a client to cancel a background reindex operation.");
        Require(
            forbiddenCancel.ReadApiErrorCode() == ApiErrorCode.OperationControlForbidden,
            "Real daemon returned the wrong error when cancelling a background reindex operation.");

        JsonElement completed = await ExchangeAsync(
            "reindex_wait",
            new { operation_id = operationId, timeout_ms = 20_000 }).ConfigureAwait(false);
        Require(completed.GetProperty("state").GetString() == "succeeded", "Reindex did not succeed.");

        JsonElement cancelled = await ExchangeAsync(
            "reindex_cancel",
            new { operation_id = operationId }).ConfigureAwait(false);
        Require(
            cancelled.GetProperty("state").GetString() == "succeeded"
                && !cancelled.GetProperty("cancelled").GetBoolean(),
            "Cancelling a completed reindex did not preserve its terminal result.");

        JsonElement search = await ExchangeAsync(
            "search",
            new { query = "AgentBeacon", limit = 10 }).ConfigureAwait(false);
        Require(
            ArrayContainsObject(
                search.GetProperty("hits"),
                hit => hit.GetProperty("name").GetString() == "AgentBeacon"
                    && hit.GetProperty("path").GetString() == "Assets/Owner.prefab"),
            "C# search did not observe the real indexed asset.");

        JsonElement suggest = await ExchangeAsync(
            "suggest",
            new { prefix = "in:Assets/", limit = 10 }).ConfigureAwait(false);
        Require(
            ArrayContainsString(suggest.GetProperty("suggestions"), "in:Assets/"),
            "C# suggest did not observe the indexed Assets path.");

        JsonElement references = await ExchangeAsync(
            "references",
            new
            {
                direction = "incoming",
                selector = new { kind = "guid", guid = "0123456789abcdef0123456789abcdef", file_id = 100 },
                limit = 10,
            }).ConfigureAwait(false);
        Require(
            ArrayContainsObject(
                references.GetProperty("hits"),
                hit => hit.GetProperty("source_path").GetString() == "Assets/Owner.prefab"),
            "C# references did not observe the real indexed reference edge.");

        JsonElement shutdown = await ExchangeAsync(
            "shutdown",
            new { drain_timeout_ms = 5_000 }).ConfigureAwait(false);
        Require(shutdown.GetProperty("accepted").GetBoolean(), "Real daemon rejected structured shutdown.");
        Require(
            observedOperations.Count == ConformanceOperations.All.Count
                && observedOperations.SetEquals(ConformanceOperations.All),
            "Public C# HTTP client did not reach every real daemon operation.");
    }

    private static async Task AssertHttpBoundaryRejectsUnauthenticatedAndBrowserRequestsAsync(
        LoopbackEndpointDescriptor endpoint,
        CancellationToken cancellationToken)
    {
        using HttpClientHandler handler = ProtocolHttpClient.CreateHandler();
        using var client = new HttpClient(handler);
        using (var unauthenticated = BoundaryRequest(endpoint))
        using (HttpResponseMessage response = await client.SendAsync(
            unauthenticated,
            HttpCompletionOption.ResponseHeadersRead,
            cancellationToken).ConfigureAwait(false))
        {
            Require(
                response.StatusCode == HttpStatusCode.Unauthorized,
                "Loopback HTTP boundary parsed an unauthenticated request.");
        }

        using (var browserRequest = BoundaryRequest(endpoint))
        {
            browserRequest.Headers.Authorization = new AuthenticationHeaderValue(
                "Bearer",
                endpoint.Capability);
            browserRequest.Headers.TryAddWithoutValidation("Origin", "https://example.invalid");
            using HttpResponseMessage response = await client.SendAsync(
                browserRequest,
                HttpCompletionOption.ResponseHeadersRead,
                cancellationToken).ConfigureAwait(false);
            Require(
                response.StatusCode == HttpStatusCode.Forbidden,
                "Loopback HTTP boundary accepted a browser-origin request.");
        }
    }

    private static HttpRequestMessage BoundaryRequest(LoopbackEndpointDescriptor endpoint)
    {
        var request = new HttpRequestMessage(HttpMethod.Post, endpoint.RequestUri)
        {
            Version = HttpVersion.Version11,
            Content = new ByteArrayContent(new byte[] { (byte)'{' }),
        };
        request.Headers.Host = endpoint.HostHeader;
        request.Headers.ConnectionClose = true;
        request.Content.Headers.ContentType = new MediaTypeHeaderValue("application/json");
        return request;
    }

    private static bool ArrayContainsString(JsonElement array, string expected)
    {
        foreach (JsonElement value in array.EnumerateArray())
        {
            if (string.Equals(value.GetString(), expected, StringComparison.Ordinal))
            {
                return true;
            }
        }
        return false;
    }

    private static bool ArrayContainsObject(JsonElement array, Func<JsonElement, bool> predicate)
    {
        foreach (JsonElement value in array.EnumerateArray())
        {
            if (predicate(value))
            {
                return true;
            }
        }
        return false;
    }

    private static void Require(bool condition, string message)
    {
        if (!condition)
        {
            throw new InvalidOperationException(message);
        }
    }
}
