using System.Net;
using System.Net.Sockets;
using System.Text.Json;
using UnityAsset.SearchProtocol.Reference;

internal static class LiveDaemonConformance
{
    private static readonly string[] ExpectedOperations =
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

    internal static async Task RunAsync(string[] args)
    {
        Require(args.Length == 3, "Real-daemon mode requires relay port, project ID, and daemon instance ID.");
        Require(
            int.TryParse(args[0], out int relayPort) && relayPort is > 0 and <= ushort.MaxValue,
            "Real-daemon relay port is invalid.");
        ProjectId projectId = ProjectId.Parse(args[1]);
        DaemonInstanceId daemonInstanceId = DaemonInstanceId.Parse(args[2]);
        using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        using ProtocolSession session = await ProtocolSession.ConnectAsync(
            new LoopbackProtocolTransportAdapter(relayPort),
            projectId,
            daemonInstanceId,
            cancellation.Token).ConfigureAwait(false);

        var observedOperations = new HashSet<string>(StringComparer.Ordinal);
        int requestOrdinal = 1;
        async Task<JsonElement> ExchangeAsync(string operation, object payload)
        {
            RequestId requestId = RequestId.Parse($"request-v1:{requestOrdinal++:x32}");
            RequestEnvelopeV1 request = BusinessCodec.CreateRequest(
                session.Binding,
                requestId,
                operation,
                JsonSerializer.SerializeToUtf8Bytes(payload));
            ResponseEnvelopeV1 response = await session.ExchangeAsync(
                request,
                cancellation.Token).ConfigureAwait(false);
            Require(!response.IsError, $"Real daemon returned an error for {operation}: {response.Value}");
            Require(
                string.Equals(response.OperationKind, operation, StringComparison.Ordinal),
                $"Real daemon returned {response.OperationKind} for {operation}.");
            observedOperations.Add(operation);
            return response.Value.GetProperty("response").Clone();
        }

        JsonElement capabilities = await ExchangeAsync("capabilities", new { }).ConfigureAwait(false);
        Require(capabilities.GetProperty("daemon_version").GetString() is { Length: > 0 }, "Capabilities omitted daemon version.");

        JsonElement initialStatus = await ExchangeAsync("status", new { }).ConfigureAwait(false);
        Require(
            initialStatus.GetProperty("daemon").GetProperty("lifecycle").GetString() == "serving",
            "Real daemon was not serving after Bootstrap.");

        JsonElement admission = await ExchangeAsync(
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
        string operationId = admission.GetProperty("operation_id").GetString()
            ?? throw new InvalidOperationException("Reindex admission omitted its operation ID.");

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
            observedOperations.Count == ExpectedOperations.Length
                && observedOperations.SetEquals(ExpectedOperations),
            "Public C# session did not reach every real daemon operation exactly once.");
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

internal sealed class LoopbackProtocolTransportAdapter : IProtocolTransportAdapter
{
    private readonly int port;

    internal LoopbackProtocolTransportAdapter(int port)
    {
        this.port = port;
    }

    public async Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        var socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp)
        {
            NoDelay = true,
        };
        try
        {
            await socket.ConnectAsync(
                new IPEndPoint(IPAddress.Loopback, port),
                cancellationToken).ConfigureAwait(false);
            return new NetworkStream(socket, ownsSocket: true);
        }
        catch
        {
            socket.Dispose();
            throw;
        }
    }
}
