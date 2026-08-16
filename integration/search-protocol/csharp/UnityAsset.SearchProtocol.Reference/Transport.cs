using System;
using System.IO;
using System.Net;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace UnityAsset.SearchProtocol.Reference
{
    /// <summary>
    /// Describes one capability-authenticated loopback HTTP daemon instance.
    /// </summary>
    public sealed class LoopbackEndpointDescriptor
    {
        private const int DescriptorVersion = 2;
        private const int MaximumDescriptorBytes = 512;
        private const string RequestPath = "/v1/request";
        private readonly byte[] canonicalBytes;
        private readonly string capability;
        private readonly string hostHeader;
        private readonly Func<byte[]?> readCurrentCanonicalDescriptor;
        private readonly Uri requestUri;

        private LoopbackEndpointDescriptor(
            Func<byte[]?> readCurrentCanonicalDescriptor,
            ProtocolBinding binding,
            ushort port,
            string capability,
            uint serverPid,
            byte[] canonicalBytes)
        {
            this.readCurrentCanonicalDescriptor = readCurrentCanonicalDescriptor;
            Binding = binding;
            Port = port;
            this.capability = capability;
            hostHeader = $"127.0.0.1:{port}";
            requestUri = new Uri($"http://{hostHeader}{RequestPath}", UriKind.Absolute);
            ServerPid = serverPid;
            this.canonicalBytes = canonicalBytes;
        }

        /// <summary>
        /// Gets the binding that every request to this daemon instance must carry.
        /// </summary>
        public ProtocolBinding Binding { get; }

        /// <summary>
        /// Gets the operating-system-selected loopback port.
        /// </summary>
        public ushort Port { get; }

        /// <summary>
        /// Gets the daemon process identifier for diagnostics only.
        /// </summary>
        public uint ServerPid { get; }

        internal string Capability => capability;

        internal Uri RequestUri => requestUri;

        internal string HostHeader => hostHeader;

        /// <summary>
        /// Creates a descriptor from canonical bytes supplied by a caller-owned authority boundary.
        /// The callback must perform trusted discovery and any platform-specific bounded stable-read,
        /// no-follow, ownership, and permission checks. It must return the exact current bytes on
        /// every invocation, or <see langword="null"/> when the descriptor is absent. This SDK
        /// validates protocol data but does not authorize filesystem paths. Concurrent exchanges
        /// may invoke the callback concurrently.
        /// </summary>
        public static LoopbackEndpointDescriptor ReadFromSource(
            Func<byte[]?> readCurrentCanonicalDescriptor,
            ProjectId expectedProjectId,
            QueryPolicyId expectedQueryPolicyId)
        {
            if (readCurrentCanonicalDescriptor == null)
            {
                throw new ArgumentNullException(nameof(readCurrentCanonicalDescriptor));
            }
            if (expectedProjectId == null)
            {
                throw new ArgumentNullException(nameof(expectedProjectId));
            }
            if (expectedQueryPolicyId == null)
            {
                throw new ArgumentNullException(nameof(expectedQueryPolicyId));
            }

            byte[] encoded = ReadDescriptorBytes(readCurrentCanonicalDescriptor)
                ?? throw new ProtocolValidationException(
                    "the caller-owned endpoint descriptor source returned no current descriptor");
            JsonElement root = StrictJson.ParseObject(encoded, "loopback endpoint descriptor");
            StrictJson.Properties(
                root,
                "loopback endpoint descriptor",
                "descriptor_version",
                "project_id",
                "daemon_instance_id",
                "port",
                "capability",
                "business_protocol_revision",
                "query_policy_id",
                "server_pid");

            ushort descriptorVersion = StrictJson.UInt16(
                root.GetProperty("descriptor_version"),
                "loopback endpoint descriptor.descriptor_version");
            if (descriptorVersion != DescriptorVersion)
            {
                throw new ProtocolValidationException(
                    $"unsupported loopback endpoint descriptor version {descriptorVersion}");
            }

            ushort port = StrictJson.UInt16(
                root.GetProperty("port"),
                "loopback endpoint descriptor.port");
            if (port == 0)
            {
                throw new ProtocolValidationException("loopback endpoint descriptor.port must not be zero");
            }
            string capability = StrictJson.String(
                root.GetProperty("capability"),
                "loopback endpoint descriptor.capability",
                maximumUtf8Bytes: 64,
                allowEmpty: false);
            ValidateCapability(capability);
            ushort businessRevision = StrictJson.UInt16(
                root.GetProperty("business_protocol_revision"),
                "loopback endpoint descriptor.business_protocol_revision");
            if (businessRevision != ProtocolConstants.BusinessProtocolRevision)
            {
                throw new ProtocolValidationException(
                    "business protocol revision mismatch: expected "
                    + $"{ProtocolConstants.BusinessProtocolRevision}, got {businessRevision}");
            }
            uint serverPid = StrictJson.UInt32(
                root.GetProperty("server_pid"),
                "loopback endpoint descriptor.server_pid");
            if (serverPid == 0)
            {
                throw new ProtocolValidationException("loopback endpoint descriptor.server_pid must not be zero");
            }

            string projectIdValue = StrictJson.String(
                root.GetProperty("project_id"),
                "loopback endpoint descriptor.project_id");
            string daemonInstanceIdValue = StrictJson.String(
                root.GetProperty("daemon_instance_id"),
                "loopback endpoint descriptor.daemon_instance_id");
            string queryPolicyIdValue = StrictJson.String(
                root.GetProperty("query_policy_id"),
                "loopback endpoint descriptor.query_policy_id");
            ValidateNonZeroIdPayload(projectIdValue, "project_id");
            ValidateNonZeroIdPayload(daemonInstanceIdValue, "daemon_instance_id");
            ValidateNonZeroIdPayload(queryPolicyIdValue, "query_policy_id");

            var binding = new ProtocolBinding(
                businessRevision,
                ProjectId.Parse(projectIdValue),
                DaemonInstanceId.Parse(daemonInstanceIdValue),
                QueryPolicyId.Parse(queryPolicyIdValue));
            if (!binding.ProjectId.Equals(expectedProjectId))
            {
                throw new ProtocolValidationException(
                    "loopback endpoint descriptor.project_id does not match the expected project");
            }
            if (!binding.QueryPolicyId.Equals(expectedQueryPolicyId))
            {
                throw new ProtocolValidationException(
                    "loopback endpoint descriptor.query_policy_id does not match the expected query policy");
            }
            return new LoopbackEndpointDescriptor(
                readCurrentCanonicalDescriptor,
                binding,
                port,
                capability,
                serverPid,
                encoded);
        }

        internal void RequireUnchanged()
        {
            byte[]? current = ReadCurrentDescriptorForComparison();
            if (current == null || !canonicalBytes.AsSpan().SequenceEqual(current))
            {
                throw new ProtocolEndpointChangedException();
            }
        }

        internal void RequireUnchangedOrMissingAfterShutdown()
        {
            byte[]? current = ReadCurrentDescriptorForComparison();
            if (current != null && !canonicalBytes.AsSpan().SequenceEqual(current))
            {
                throw new ProtocolEndpointChangedException();
            }
        }

        private byte[]? ReadCurrentDescriptorForComparison()
        {
            try
            {
                return ReadDescriptorBytes(readCurrentCanonicalDescriptor);
            }
            catch (ProtocolValidationException error)
            {
                throw new ProtocolEndpointChangedException(error);
            }
        }

        private static byte[]? ReadDescriptorBytes(Func<byte[]?> source)
        {
            byte[]? encoded = source();
            if (encoded == null)
            {
                return null;
            }
            if (encoded.Length > MaximumDescriptorBytes)
            {
                throw new ProtocolValidationException(
                    $"loopback endpoint descriptor contains {encoded.Length} bytes; maximum is {MaximumDescriptorBytes}");
            }
            return (byte[])encoded.Clone();
        }

        private static void ValidateCapability(string value)
        {
            if (value.Length != 64)
            {
                throw new ProtocolValidationException(
                    "loopback endpoint descriptor.capability must contain 64 hexadecimal characters");
            }
            bool anyNonZero = false;
            foreach (char character in value)
            {
                bool valid = (character >= '0' && character <= '9')
                    || (character >= 'a' && character <= 'f');
                if (!valid)
                {
                    throw new ProtocolValidationException(
                        "loopback endpoint descriptor.capability must be lowercase hexadecimal");
                }
                anyNonZero |= character != '0';
            }
            if (!anyNonZero)
            {
                throw new ProtocolValidationException(
                    "loopback endpoint descriptor.capability must not be all zeroes");
            }
        }

        private static void ValidateNonZeroIdPayload(string value, string field)
        {
            int separator = value.IndexOf(':');
            bool anyNonZero = false;
            for (int index = separator + 1; index < value.Length; index++)
            {
                anyNonZero |= value[index] != '0';
            }
            if (!anyNonZero)
            {
                throw new ProtocolValidationException(
                    $"loopback endpoint descriptor.{field} must not be all zeroes");
            }
        }
    }

    /// <summary>
    /// Exchanges canonical business-protocol JSON with one caller-authorized loopback HTTP daemon.
    /// </summary>
    public sealed class ProtocolHttpClient : IDisposable
    {
        private const int ReadBufferBytes = 16 * 1024;
        private const int DefaultRequestTimeoutMilliseconds = 60_000;
        private const int ServerWaitResponseMarginMilliseconds = 2_000;
        private readonly LoopbackEndpointDescriptor endpoint;
        private readonly HttpClient client;
        private int disposed;

        private ProtocolHttpClient(LoopbackEndpointDescriptor endpoint)
        {
            this.endpoint = endpoint ?? throw new ArgumentNullException(nameof(endpoint));
            client = new HttpClient(CreateHandler(), disposeHandler: true)
            {
                Timeout = System.Threading.Timeout.InfiniteTimeSpan,
            };
        }

        /// <summary>
        /// Gets the descriptor-derived request binding.
        /// </summary>
        public ProtocolBinding Binding => endpoint.Binding;

        /// <summary>
        /// Opens a client for a caller-authorized endpoint descriptor.
        /// </summary>
        public static ProtocolHttpClient Open(LoopbackEndpointDescriptor endpoint)
        {
            return new ProtocolHttpClient(endpoint);
        }

        /// <summary>
        /// Performs one capability-authenticated HTTP exchange and validates the canonical response.
        /// </summary>
        public async Task<ResponseEnvelopeV1> ExchangeAsync(
            RequestEnvelopeV1 request,
            CancellationToken cancellationToken)
        {
            if (request == null)
            {
                throw new ArgumentNullException(nameof(request));
            }
            ThrowIfDisposed();
            request.ValidateBinding(Binding);
            endpoint.RequireUnchanged();

            byte[] requestPayload = BusinessCodec.EncodeRequest(request);

            ResponseEnvelopeV1? response = null;
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            deadline.CancelAfter(OperationTimeout(request));
            try
            {
                response = await ExchangeCoreAsync(
                    request,
                    requestPayload,
                    deadline.Token).ConfigureAwait(false);
                return response;
            }
            finally
            {
                if (IsAcceptedShutdown(request, response))
                {
                    endpoint.RequireUnchangedOrMissingAfterShutdown();
                }
                else
                {
                    endpoint.RequireUnchanged();
                }
            }
        }

        internal static TimeSpan OperationTimeout(RequestEnvelopeV1 request)
        {
            int milliseconds = DefaultRequestTimeoutMilliseconds;
            if (string.Equals(request.OperationKind, "reindex_wait", StringComparison.Ordinal))
            {
                uint waitMilliseconds = request.Operation
                    .GetProperty("request")
                    .GetProperty("timeout_ms")
                    .GetUInt32();
                milliseconds = Math.Max(
                    milliseconds,
                    checked((int)waitMilliseconds + ServerWaitResponseMarginMilliseconds));
            }
            return TimeSpan.FromMilliseconds(milliseconds);
        }

        private static bool IsAcceptedShutdown(
            RequestEnvelopeV1 request,
            ResponseEnvelopeV1? response)
        {
            return string.Equals(request.OperationKind, "shutdown", StringComparison.Ordinal)
                && response != null
                && !response.IsError
                && response.Value.GetProperty("response").GetProperty("accepted").GetBoolean();
        }

        private async Task<ResponseEnvelopeV1> ExchangeCoreAsync(
            RequestEnvelopeV1 request,
            byte[] requestPayload,
            CancellationToken cancellationToken)
        {
            using var message = new HttpRequestMessage(HttpMethod.Post, endpoint.RequestUri)
            {
                Version = HttpVersion.Version11,
                Content = new ByteArrayContent(requestPayload),
            };
            message.Headers.Host = endpoint.HostHeader;
            message.Headers.Authorization = new AuthenticationHeaderValue("Bearer", endpoint.Capability);
            message.Headers.ConnectionClose = true;
            message.Content.Headers.ContentType = new MediaTypeHeaderValue("application/json");

            using HttpResponseMessage response = await client.SendAsync(
                message,
                HttpCompletionOption.ResponseHeadersRead,
                cancellationToken).ConfigureAwait(false);
            if (response.StatusCode != HttpStatusCode.OK)
            {
                throw new ProtocolHttpException(response.StatusCode);
            }
            if (response.Content.Headers.ContentType?.ToString() != "application/json")
            {
                throw new ProtocolValidationException(
                    "loopback HTTP response must use application/json without parameters");
            }
            if (response.Content.Headers.ContentEncoding.Count != 0)
            {
                throw new ProtocolValidationException(
                    "loopback HTTP response must not use content encoding");
            }

            byte[] responsePayload = await ReadBoundedAsync(
                response.Content,
                ProtocolLimits.ForResponse(request.OperationKind),
                cancellationToken).ConfigureAwait(false);
            ResponseEnvelopeV1 envelope = BusinessCodec.DecodeResponse(responsePayload);
            envelope.ValidateFor(request);
            return envelope;
        }

        public void Dispose()
        {
            if (Interlocked.Exchange(ref disposed, 1) == 0)
            {
                client.Dispose();
            }
        }

        internal static HttpClientHandler CreateHandler()
        {
            return new HttpClientHandler
            {
                AllowAutoRedirect = false,
                AutomaticDecompression = DecompressionMethods.None,
                UseCookies = false,
                UseDefaultCredentials = false,
                UseProxy = false,
            };
        }

        private static async Task<byte[]> ReadBoundedAsync(
            HttpContent content,
            int maximumBytes,
            CancellationToken cancellationToken)
        {
            long? declared = content.Headers.ContentLength;
            if (declared.HasValue && declared.Value > maximumBytes)
            {
                throw new ProtocolValidationException(
                    $"HTTP response declares {declared.Value} bytes; maximum is {maximumBytes}");
            }

            using Stream stream = await content.ReadAsStreamAsync().ConfigureAwait(false);
            using var output = new MemoryStream(
                declared.HasValue ? checked((int)declared.Value) : Math.Min(maximumBytes, ReadBufferBytes));
            var buffer = new byte[ReadBufferBytes];
            while (true)
            {
                int read = await stream.ReadAsync(
                    buffer,
                    0,
                    Math.Min(buffer.Length, maximumBytes - checked((int)output.Length) + 1),
                    cancellationToken).ConfigureAwait(false);
                if (read == 0)
                {
                    if (output.Length == output.Capacity)
                    {
                        return output.GetBuffer();
                    }
                    return output.ToArray();
                }
                if (output.Length + read > maximumBytes)
                {
                    throw new ProtocolValidationException(
                        $"HTTP response exceeds {maximumBytes} encoded bytes");
                }
                output.Write(buffer, 0, read);
            }
        }

        private void ThrowIfDisposed()
        {
            if (Volatile.Read(ref disposed) != 0)
            {
                throw new ObjectDisposedException(nameof(ProtocolHttpClient));
            }
        }
    }

    /// <summary>
    /// Indicates that the caller-owned endpoint descriptor generation changed during an exchange.
    /// </summary>
    public sealed class ProtocolEndpointChangedException : IOException
    {
        public ProtocolEndpointChangedException()
            : base("Loopback endpoint descriptor changed during the HTTP exchange.")
        {
        }

        public ProtocolEndpointChangedException(Exception innerException)
            : base("Loopback endpoint descriptor changed during the HTTP exchange.", innerException)
        {
        }
    }

    /// <summary>
    /// Indicates that the loopback HTTP boundary rejected a request before business dispatch.
    /// </summary>
    public sealed class ProtocolHttpException : IOException
    {
        public ProtocolHttpException(HttpStatusCode statusCode)
            : base($"Loopback HTTP request failed with status {(int)statusCode} ({statusCode}).")
        {
            StatusCode = statusCode;
        }

        public HttpStatusCode StatusCode { get; }
    }
}
