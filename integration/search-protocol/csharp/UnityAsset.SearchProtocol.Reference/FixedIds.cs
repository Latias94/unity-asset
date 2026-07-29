using System;

namespace UnityAsset.SearchProtocol.Reference
{
    public abstract class FixedId : IEquatable<FixedId>
    {
        protected FixedId(string value)
        {
            Value = value;
        }

        public string Value { get; }

        public bool Equals(FixedId? other)
        {
            return other != null
                && GetType() == other.GetType()
                && string.Equals(Value, other.Value, StringComparison.Ordinal);
        }

        public override bool Equals(object? obj)
        {
            return Equals(obj as FixedId);
        }

        public override int GetHashCode()
        {
            return (GetType().GetHashCode() * 397) ^ StringComparer.Ordinal.GetHashCode(Value);
        }

        public override string ToString()
        {
            return Value;
        }

        protected static string Validate(string value, string prefix, int byteLength)
        {
            if (value == null)
            {
                throw new ArgumentNullException(nameof(value));
            }

            string fullPrefix = prefix + ":";
            int expectedLength = fullPrefix.Length + (byteLength * 2);
            if (!value.StartsWith(fullPrefix, StringComparison.Ordinal))
            {
                throw new ProtocolValidationException($"invalid fixed ID prefix; expected '{fullPrefix}'");
            }
            if (value.Length != expectedLength)
            {
                throw new ProtocolValidationException($"invalid fixed ID length; expected {expectedLength}, got {value.Length}");
            }
            for (int index = fullPrefix.Length; index < value.Length; index++)
            {
                char character = value[index];
                if (!((character >= '0' && character <= '9') || (character >= 'a' && character <= 'f')))
                {
                    throw new ProtocolValidationException("fixed ID payload must be lowercase hexadecimal");
                }
            }
            return value;
        }
    }

    public sealed class ProjectId : FixedId
    {
        private ProjectId(string value)
            : base(value)
        {
        }

        public static ProjectId Parse(string value) => new(Validate(value, "project-v1", 32));
    }

    public sealed class DaemonInstanceId : FixedId
    {
        private DaemonInstanceId(string value)
            : base(value)
        {
        }

        public static DaemonInstanceId Parse(string value) => new(Validate(value, "daemon-v1", 16));
    }

    public sealed class RequestId : FixedId
    {
        private RequestId(string value)
            : base(value)
        {
        }

        public static RequestId Parse(string value) => new(Validate(value, "request-v1", 16));
    }

    public sealed class OperationId : FixedId
    {
        private OperationId(string value)
            : base(value)
        {
        }

        public static OperationId Parse(string value) => new(Validate(value, "operation-v1", 16));
    }

    public sealed class QueryPolicyId : FixedId
    {
        private QueryPolicyId(string value)
            : base(value)
        {
        }

        public static QueryPolicyId Parse(string value) => new(Validate(value, "query-policy-v1", 32));
    }
}
