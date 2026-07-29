using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using System.Text.Encodings.Web;
using System.Text.Json;

namespace UnityAsset.SearchProtocol.Reference
{
    internal static class StrictJson
    {
        private const int MaximumCollectionEntries = 1_000_000;
        private const int MaximumObjectMembers = 1_000_000;

        internal static readonly JsonWriterOptions WriterOptions = new JsonWriterOptions
        {
            Encoder = JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
            Indented = false,
            SkipValidation = false,
        };

        internal static JsonElement ParseObject(byte[] utf8Json, string path)
        {
            if (utf8Json == null)
            {
                throw new ArgumentNullException(nameof(utf8Json));
            }
            if (utf8Json.Length == 0)
            {
                throw new ProtocolValidationException($"{path} JSON must not be empty");
            }
            if (utf8Json.Length >= 3 && utf8Json[0] == 0xef && utf8Json[1] == 0xbb && utf8Json[2] == 0xbf)
            {
                throw new ProtocolValidationException($"{path} JSON must not contain a UTF-8 BOM");
            }

            try
            {
                using JsonDocument document = JsonDocument.Parse(
                    utf8Json,
                    new JsonDocumentOptions
                    {
                        AllowTrailingCommas = false,
                        CommentHandling = JsonCommentHandling.Disallow,
                        MaxDepth = 32,
                    });
                JsonElement root = document.RootElement;
                RequireKind(root, JsonValueKind.Object, path);
                int collectionEntries = 0;
                int objectMembers = 0;
                ValidateStructuralLimits(root, path, ref collectionEntries, ref objectMembers);
                return root.Clone();
            }
            catch (JsonException error)
            {
                throw new ProtocolValidationException($"{path} contains invalid JSON: {error.Message}", error);
            }
        }

        internal static void Properties(JsonElement element, string path, params string[] expected)
        {
            RequireKind(element, JsonValueKind.Object, path);
            int expectedIndex = 0;
            foreach (JsonProperty property in element.EnumerateObject())
            {
                while (expectedIndex < expected.Length
                    && IsOptional(expected[expectedIndex])
                    && !Name(expected[expectedIndex]).Equals(property.Name, StringComparison.Ordinal))
                {
                    expectedIndex++;
                }

                if (expectedIndex >= expected.Length
                    || !Name(expected[expectedIndex]).Equals(property.Name, StringComparison.Ordinal))
                {
                    throw new ProtocolValidationException(
                        $"{path} has an unknown, duplicate, or non-canonical property '{property.Name}'");
                }
                expectedIndex++;
            }

            while (expectedIndex < expected.Length && IsOptional(expected[expectedIndex]))
            {
                expectedIndex++;
            }
            if (expectedIndex != expected.Length)
            {
                throw new ProtocolValidationException($"{path} is missing required property '{Name(expected[expectedIndex])}'");
            }
        }

        internal static void RequireEncodedLimit(byte[] utf8Json, int maximum, string path)
        {
            if (utf8Json == null)
            {
                throw new ArgumentNullException(nameof(utf8Json));
            }
            if (utf8Json.Length > maximum)
            {
                throw new ProtocolValidationException(
                    $"{path} contains {utf8Json.Length} encoded bytes; maximum is {maximum}");
            }
        }

        internal static JsonElement Required(JsonElement element, string name, string path)
        {
            if (!element.TryGetProperty(name, out JsonElement value))
            {
                throw new ProtocolValidationException($"{path} is missing required property '{name}'");
            }
            return value;
        }

        internal static bool Optional(JsonElement element, string name, out JsonElement value)
        {
            return element.TryGetProperty(name, out value);
        }

        internal static string String(JsonElement element, string path, int maximumUtf8Bytes = int.MaxValue, bool allowEmpty = true)
        {
            RequireKind(element, JsonValueKind.String, path);
            string value = element.GetString()!;
            if (!allowEmpty && value.Length == 0)
            {
                throw new ProtocolValidationException($"{path} must not be empty");
            }
            int byteCount = Encoding.UTF8.GetByteCount(value);
            if (byteCount > maximumUtf8Bytes)
            {
                throw new ProtocolValidationException($"{path} exceeds {maximumUtf8Bytes} UTF-8 bytes");
            }
            return value;
        }

        internal static ushort UInt16(JsonElement element, string path)
        {
            if (element.ValueKind != JsonValueKind.Number || !element.TryGetUInt16(out ushort value))
            {
                throw new ProtocolValidationException($"{path} must be an unsigned 16-bit integer");
            }
            return value;
        }

        internal static uint UInt32(JsonElement element, string path)
        {
            if (element.ValueKind != JsonValueKind.Number || !element.TryGetUInt32(out uint value))
            {
                throw new ProtocolValidationException($"{path} must be an unsigned 32-bit integer");
            }
            return value;
        }

        internal static ulong UInt64(JsonElement element, string path)
        {
            if (element.ValueKind != JsonValueKind.Number || !element.TryGetUInt64(out ulong value))
            {
                throw new ProtocolValidationException($"{path} must be an unsigned 64-bit integer");
            }
            return value;
        }

        internal static long Int64(JsonElement element, string path)
        {
            if (element.ValueKind != JsonValueKind.Number || !element.TryGetInt64(out long value))
            {
                throw new ProtocolValidationException($"{path} must be a signed 64-bit integer");
            }
            return value;
        }

        internal static bool Boolean(JsonElement element, string path)
        {
            if (element.ValueKind == JsonValueKind.True)
            {
                return true;
            }
            if (element.ValueKind == JsonValueKind.False)
            {
                return false;
            }
            throw new ProtocolValidationException($"{path} must be a boolean");
        }

        internal static string Enum(JsonElement element, string path, params string[] allowed)
        {
            string value = String(element, path, allowEmpty: false);
            if (!allowed.Contains(value, StringComparer.Ordinal))
            {
                throw new ProtocolValidationException($"{path} contains unsupported value '{value}'");
            }
            return value;
        }

        internal static JsonElement[] Array(JsonElement element, string path, int maximum = int.MaxValue, bool allowEmpty = true)
        {
            RequireKind(element, JsonValueKind.Array, path);
            JsonElement[] values = element.EnumerateArray().ToArray();
            if (!allowEmpty && values.Length == 0)
            {
                throw new ProtocolValidationException($"{path} must not be empty");
            }
            if (values.Length > maximum)
            {
                throw new ProtocolValidationException($"{path} exceeds {maximum} entries");
            }
            return values;
        }

        internal static void RequireRevision(JsonElement element, string path)
        {
            ushort revision = UInt16(element, path);
            if (revision != ProtocolConstants.BusinessProtocolRevision)
            {
                throw new ProtocolValidationException(
                    $"{path} protocol revision mismatch: expected {ProtocolConstants.BusinessProtocolRevision}, got {revision}");
            }
        }

        internal static void RequireKind(JsonElement element, JsonValueKind kind, string path)
        {
            if (element.ValueKind != kind)
            {
                throw new ProtocolValidationException($"{path} must be a JSON {kind.ToString().ToLowerInvariant()}");
            }
        }

        internal static void FixedHex(string value, string prefix, int hexCharacters, string path, bool lowercaseOnly)
        {
            if (!value.StartsWith(prefix, StringComparison.Ordinal) || value.Length != prefix.Length + hexCharacters)
            {
                throw new ProtocolValidationException($"{path} has an invalid version prefix or encoded length");
            }
            bool anyNonZero = false;
            for (int index = prefix.Length; index < value.Length; index++)
            {
                char character = value[index];
                bool digit = character >= '0' && character <= '9';
                bool lower = character >= 'a' && character <= 'f';
                bool upper = character >= 'A' && character <= 'F';
                if (!(digit || lower || (!lowercaseOnly && upper)))
                {
                    throw new ProtocolValidationException($"{path} has an invalid hexadecimal payload");
                }
                anyNonZero |= character != '0';
            }
            if (prefix == "workspace-v1:" && !anyNonZero)
            {
                throw new ProtocolValidationException($"{path} must not be the zero workspace ID");
            }
        }

        internal static void PortablePath(
            JsonElement element,
            string path,
            bool requireRelative = false,
            int maximumUtf8Bytes = 32 * 1024,
            bool rejectControlCharacters = false)
        {
            string value = String(element, path, maximumUtf8Bytes, allowEmpty: false);
            if (value.IndexOf('\\') >= 0 || value.IndexOf('\0') >= 0)
            {
                throw new ProtocolValidationException($"{path} must use forward slashes and contain no NUL bytes");
            }
            if (!requireRelative)
            {
                return;
            }
            bool hasDrivePrefix = value.Length > 1 && value[1] == ':';
            if (value.StartsWith("/", StringComparison.Ordinal) || hasDrivePrefix)
            {
                throw new ProtocolValidationException($"{path} must be relative");
            }
            string[] components = value.Split('/');
            if (components.Any(component => component.Length == 0 || component == "." || component == ".."))
            {
                throw new ProtocolValidationException($"{path} contains an invalid relative component");
            }
            if (rejectControlCharacters && value.Any(char.IsControl))
            {
                throw new ProtocolValidationException($"{path} contains a control character");
            }
        }

        internal static int CompareUnicodeScalarOrdinal(string left, string right, string path)
        {
            ValidateUnicodeScalarString(left, path);
            ValidateUnicodeScalarString(right, path);

            int leftIndex = 0;
            int rightIndex = 0;
            while (leftIndex < left.Length && rightIndex < right.Length)
            {
                uint leftScalar = ReadUnicodeScalar(left, ref leftIndex);
                uint rightScalar = ReadUnicodeScalar(right, ref rightIndex);
                if (leftScalar != rightScalar)
                {
                    return leftScalar < rightScalar ? -1 : 1;
                }
            }
            return leftIndex == left.Length
                ? rightIndex == right.Length ? 0 : -1
                : 1;
        }

        internal static void ValidateUnicodeScalarString(string value, string path)
        {
            int index = 0;
            while (index < value.Length)
            {
                char character = value[index++];
                if (char.IsHighSurrogate(character))
                {
                    if (index >= value.Length || !char.IsLowSurrogate(value[index]))
                    {
                        throw new ProtocolValidationException($"{path} contains an unpaired UTF-16 surrogate");
                    }
                    index++;
                }
                else if (char.IsLowSurrogate(character))
                {
                    throw new ProtocolValidationException($"{path} contains an unpaired UTF-16 surrogate");
                }
            }
        }

        private static uint ReadUnicodeScalar(string value, ref int index)
        {
            char first = value[index++];
            if (!char.IsHighSurrogate(first))
            {
                return first;
            }
            char second = value[index++];
            return checked(0x10000u
                + ((uint)(first - 0xd800) << 10)
                + (uint)(second - 0xdc00));
        }

        private static void ValidateStructuralLimits(
            JsonElement element,
            string path,
            ref int collectionEntries,
            ref int objectMembers)
        {
            if (element.ValueKind == JsonValueKind.Object)
            {
                foreach (JsonProperty property in element.EnumerateObject())
                {
                    objectMembers = checked(objectMembers + 1);
                    if (objectMembers > MaximumObjectMembers)
                    {
                        throw new ProtocolValidationException(
                            $"{path} JSON exceeds {MaximumObjectMembers} total object members");
                    }
                    ValidateStructuralLimits(
                        property.Value,
                        path + "." + property.Name,
                        ref collectionEntries,
                        ref objectMembers);
                }
            }
            else if (element.ValueKind == JsonValueKind.Array)
            {
                int count = element.GetArrayLength();
                collectionEntries = checked(collectionEntries + count);
                if (collectionEntries > MaximumCollectionEntries)
                {
                    throw new ProtocolValidationException(
                        $"{path} JSON exceeds {MaximumCollectionEntries} total collection entries");
                }
                for (int index = 0; index < count; index++)
                {
                    ValidateStructuralLimits(
                        element[index],
                        $"{path}[{index}]",
                        ref collectionEntries,
                        ref objectMembers);
                }
            }
        }

        private static bool IsOptional(string specification)
        {
            return specification.EndsWith("?", StringComparison.Ordinal);
        }

        private static string Name(string specification)
        {
            return IsOptional(specification) ? specification.Substring(0, specification.Length - 1) : specification;
        }
    }
}
