using System;
using System.IO;
using System.Threading;
using System.Threading.Tasks;

namespace UnityAsset.SearchProtocol.Reference
{
    /// <summary>
    /// Opens one already-authenticated local transport stream for a protocol session.
    /// </summary>
    public interface IProtocolTransportAdapter
    {
        /// <summary>
        /// Opens a readable, writable stream. A successful <see cref="ProtocolSession.ConnectAsync"/>
        /// takes exclusive ownership of the returned stream and closes it on every terminal path.
        /// </summary>
        Task<Stream> ConnectAsync(CancellationToken cancellationToken);
    }

    /// <summary>
    /// Indicates that the remote endpoint rejected Bootstrap with a closed protocol code.
    /// </summary>
    public sealed class ProtocolBootstrapRejectedException : Exception
    {
        /// <summary>
        /// Creates a Bootstrap rejection with the remote endpoint's closed rejection code.
        /// </summary>
        public ProtocolBootstrapRejectedException(string code)
            : base($"Protocol bootstrap was rejected with code '{code}'.")
        {
            Code = code ?? throw new ArgumentNullException(nameof(code));
        }

        /// <summary>
        /// Gets the closed Bootstrap rejection code returned by the remote endpoint.
        /// </summary>
        public string Code { get; }
    }

    /// <summary>
    /// Owns one Bootstrap-verified transport stream and serializes business exchanges on it.
    /// </summary>
    public sealed class ProtocolSession : IDisposable
    {
        private readonly FramedProtocolStream framed;
        private readonly SemaphoreSlim exchangeGate = new SemaphoreSlim(1, 1);
        private readonly object stateGate = new object();
        private bool disposed;

        private ProtocolSession(FramedProtocolStream framed, ProtocolBinding binding)
        {
            this.framed = framed;
            Binding = binding;
        }

        /// <summary>
        /// Gets the project, daemon instance, query-policy, and business-revision binding negotiated by Bootstrap.
        /// </summary>
        public ProtocolBinding Binding { get; }

        /// <summary>
        /// Connects, performs Bootstrap V2, and returns an instance-bound session.
        /// The resulting session exclusively owns the stream returned by <paramref name="transport"/>
        /// and closes it when the session is disposed or Bootstrap fails.
        /// </summary>
        public static async Task<ProtocolSession> ConnectAsync(
            IProtocolTransportAdapter transport,
            ProjectId projectId,
            DaemonInstanceId daemonInstanceId,
            CancellationToken cancellationToken)
        {
            if (transport == null)
            {
                throw new ArgumentNullException(nameof(transport));
            }
            if (projectId == null)
            {
                throw new ArgumentNullException(nameof(projectId));
            }
            if (daemonInstanceId == null)
            {
                throw new ArgumentNullException(nameof(daemonInstanceId));
            }

            Stream stream = await transport.ConnectAsync(cancellationToken).ConfigureAwait(false);
            if (stream == null)
            {
                throw new InvalidOperationException("Protocol transport returned a null stream.");
            }
            FramedProtocolStream framed;
            try
            {
                framed = new FramedProtocolStream(stream, ownsStream: true);
            }
            catch
            {
                stream.Dispose();
                throw;
            }
            try
            {
                var hello = new BootstrapHelloV2(
                    ProtocolConstants.BootstrapVersion,
                    projectId,
                    daemonInstanceId,
                    new[] { ProtocolConstants.BusinessProtocolRevision });
                await framed.WritePayloadAsync(
                    BootstrapCodec.EncodeHello(hello),
                    FrameLimits.BootstrapMaxEncodedBytes,
                    cancellationToken).ConfigureAwait(false);
                byte[] payload = await framed.ReadPayloadAsync(
                    FrameLimits.BootstrapMaxEncodedBytes,
                    cancellationToken).ConfigureAwait(false);
                BootstrapReplyV2 reply = BootstrapCodec.DecodeReply(payload);
                BootstrapCodec.ValidateReplyFor(reply, hello);
                if (reply is BootstrapRejectedV2 rejected)
                {
                    throw new ProtocolBootstrapRejectedException(rejected.Code);
                }
                if (reply is not BootstrapAcceptedV2 accepted)
                {
                    throw new ProtocolValidationException("bootstrap reply has an unsupported result");
                }

                var binding = new ProtocolBinding(
                    accepted.SelectedRevision,
                    accepted.ProjectId,
                    accepted.DaemonInstanceId,
                    accepted.QueryPolicyId);
                return new ProtocolSession(framed, binding);
            }
            catch
            {
                framed.Dispose();
                throw;
            }
        }

        /// <summary>
        /// Performs one validated request/response exchange.
        /// Concurrent callers are serialized. Once this method has started I/O, any cancellation,
        /// framing, transport, or response-validation failure permanently poisons and closes the session.
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
            byte[] requestPayload = BusinessCodec.EncodeRequest(request);
            int requestLimit = FrameLimits.ForRequest(request.OperationKind);
            int responseLimit = FrameLimits.ForResponse(request.OperationKind);

            await exchangeGate.WaitAsync(cancellationToken).ConfigureAwait(false);
            try
            {
                ThrowIfDisposed();
                await framed.WritePayloadAsync(
                    requestPayload,
                    requestLimit,
                    cancellationToken).ConfigureAwait(false);
                byte[] responsePayload = await framed.ReadPayloadAsync(
                    responseLimit,
                    cancellationToken).ConfigureAwait(false);
                ResponseEnvelopeV1 response = BusinessCodec.DecodeResponse(responsePayload);
                response.ValidateFor(request);
                return response;
            }
            catch
            {
                framed.Abort();
                throw;
            }
            finally
            {
                exchangeGate.Release();
            }
        }

        /// <summary>
        /// Closes the owned transport stream and rejects active or queued exchanges.
        /// </summary>
        public void Dispose()
        {
            lock (stateGate)
            {
                if (disposed)
                {
                    return;
                }
                disposed = true;
            }
            framed.Dispose();
        }

        private void ThrowIfDisposed()
        {
            lock (stateGate)
            {
                if (disposed)
                {
                    throw new ObjectDisposedException(nameof(ProtocolSession));
                }
            }
        }
    }

    internal sealed class FramedProtocolStream : IDisposable
    {
        private readonly Stream stream;
        private readonly bool ownsStream;
        private readonly SemaphoreSlim readGate = new SemaphoreSlim(1, 1);
        private readonly SemaphoreSlim writeGate = new SemaphoreSlim(1, 1);
        private readonly object stateGate = new object();
        private bool disposed;
        private bool poisoned;

        internal FramedProtocolStream(Stream stream, bool ownsStream = false)
        {
            this.stream = stream ?? throw new ArgumentNullException(nameof(stream));
            if (!stream.CanRead || !stream.CanWrite)
            {
                throw new ArgumentException("Protocol stream must be readable and writable.", nameof(stream));
            }
            this.ownsStream = ownsStream;
        }

        internal async Task WritePayloadAsync(
            byte[] payload,
            int maximumEncodedBytes,
            CancellationToken cancellationToken)
        {
            ThrowIfUnavailable();
            byte[] frame = FrameCodec.Encode(payload, maximumEncodedBytes);
            await writeGate.WaitAsync(cancellationToken).ConfigureAwait(false);
            try
            {
                ThrowIfUnavailable();
                await stream.WriteAsync(frame, 0, frame.Length, cancellationToken).ConfigureAwait(false);
                await stream.FlushAsync(cancellationToken).ConfigureAwait(false);
            }
            catch
            {
                Poison();
                throw;
            }
            finally
            {
                writeGate.Release();
            }
        }

        internal async Task<byte[]> ReadPayloadAsync(int maximumEncodedBytes, CancellationToken cancellationToken)
        {
            ThrowIfUnavailable();
            FrameCodec.ValidateMaximum(maximumEncodedBytes);
            await readGate.WaitAsync(cancellationToken).ConfigureAwait(false);
            try
            {
                ThrowIfUnavailable();
                var header = new byte[4];
                await ReadExactlyAsync(header, cancellationToken).ConfigureAwait(false);
                uint declared = FrameCodec.ReadLength(header);
                if (declared > maximumEncodedBytes)
                {
                    throw new ProtocolValidationException(
                        $"frame declares {declared} encoded bytes; maximum is {maximumEncodedBytes}");
                }
                var payload = new byte[checked((int)declared)];
                await ReadExactlyAsync(payload, cancellationToken).ConfigureAwait(false);
                return payload;
            }
            catch
            {
                Poison();
                throw;
            }
            finally
            {
                readGate.Release();
            }
        }

        public void Dispose()
        {
            bool closeStream;
            lock (stateGate)
            {
                if (disposed)
                {
                    return;
                }
                disposed = true;
                closeStream = ownsStream;
            }
            if (closeStream)
            {
                stream.Dispose();
            }
        }

        internal void Abort()
        {
            Poison();
        }

        private async Task ReadExactlyAsync(byte[] buffer, CancellationToken cancellationToken)
        {
            int offset = 0;
            while (offset < buffer.Length)
            {
                int read = await stream.ReadAsync(
                    buffer,
                    offset,
                    buffer.Length - offset,
                    cancellationToken).ConfigureAwait(false);
                if (read == 0)
                {
                    throw new EndOfStreamException(
                        offset == 0 && buffer.Length == 4
                            ? "Protocol stream ended before a frame header was available."
                            : "Protocol stream ended before the declared frame payload was complete.");
                }
                offset += read;
            }
        }

        private void Poison()
        {
            bool closeStream;
            lock (stateGate)
            {
                closeStream = !poisoned;
                poisoned = true;
            }
            if (closeStream)
            {
                stream.Dispose();
            }
        }

        private void ThrowIfUnavailable()
        {
            lock (stateGate)
            {
                if (disposed)
                {
                    throw new ObjectDisposedException(nameof(FramedProtocolStream));
                }
                if (poisoned)
                {
                    throw new IOException("Protocol stream is poisoned after an incomplete or invalid exchange.");
                }
            }
        }
    }
}
