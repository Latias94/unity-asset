using System;
using System.IO;
using System.Threading;
using System.Threading.Tasks;

namespace UnityAsset.SearchProtocol.Reference
{
    public interface IProtocolTransportAdapter
    {
        Task<Stream> ConnectAsync(CancellationToken cancellationToken);
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
