//! Strict allocation-free media layout declarations.

use std::collections::TryReserveError;
use std::fmt;

use indexmap::IndexMap;
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadBudgetDomainToken, BudgetError, UnityValue, vec_allocation_bytes,
};

/// Owned media bytes whose exact `Vec` allocation belongs to one load-budget domain.
///
/// This proof is intentionally not cloneable: cloning the backing would allocate storage that has
/// not been charged. Prepared codecs consume the proof and reject a different budget domain rather
/// than charging the same allocation again.
pub struct BudgetedMediaBytes {
    bytes: Vec<u8>,
    domain: AssetLoadBudgetDomainToken,
    resource: &'static str,
}

impl BudgetedMediaBytes {
    /// Accounts an existing caller-owned `Vec` before it enters media preparation.
    pub fn from_vec(
        bytes: Vec<u8>,
        resource: &'static str,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetError> {
        budget.consume_bytes(media_vec_allocation_bytes(bytes.capacity(), resource)?)?;
        Ok(Self {
            bytes,
            domain: budget.domain_token(),
            resource,
        })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Verifies that this proof was minted by `budget`.
    pub fn validate_budget(&self, budget: &AssetLoadBudget) -> Result<(), BudgetError> {
        self.domain.validate(budget, self.resource)
    }

    /// Consumes the proof after validating the owning budget domain.
    pub fn into_vec(self, budget: &AssetLoadBudget) -> Result<Vec<u8>, BudgetError> {
        self.validate_budget(budget)?;
        Ok(self.bytes)
    }
}

impl AsRef<[u8]> for BudgetedMediaBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for BudgetedMediaBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BudgetedMediaBytes")
            .field("bytes", &self.bytes.len())
            .field("capacity", &self.bytes.capacity())
            .field("resource", &self.resource)
            .finish_non_exhaustive()
    }
}

fn media_vec_allocation_bytes(capacity: usize, resource: &'static str) -> Result<u64, BudgetError> {
    vec_allocation_bytes::<u8>(capacity).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

/// A borrowed reference to non-empty Unity stream data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDataRef<'a> {
    path: &'a str,
    offset: u64,
    size: u64,
}

impl<'a> StreamDataRef<'a> {
    pub fn new(path: &'a str, offset: u64, size: u64) -> Result<Self, MediaInspectionError> {
        if path.trim().is_empty() {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "stream.path",
                reason: "stream path must not be empty",
            });
        }
        if path.chars().any(char::is_control) {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "stream.path",
                reason: "stream path must not contain control characters",
            });
        }
        if size == 0 {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "stream.size",
                reason: "stream size must be non-zero",
            });
        }
        offset
            .checked_add(size)
            .ok_or(MediaInspectionError::StreamRangeOverflow { offset, size })?;
        Ok(Self { path, offset, size })
    }

    #[must_use]
    pub const fn path(self) -> &'a str {
        self.path
    }

    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn size(self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn end(self) -> u64 {
        self.offset + self.size
    }
}

/// The unique non-empty encoded payload selected from one Unity media object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPayloadRef<'a> {
    /// Media bytes are embedded in the inspected Unity object.
    Embedded(EmbeddedMediaRef<'a>),
    /// Media bytes live in an external streamed resource.
    Streamed(StreamDataRef<'a>),
}

/// Borrowed evidence for one validated embedded bytes field.
#[derive(Clone, Copy)]
pub struct EmbeddedMediaRef<'a> {
    backing: EmbeddedMediaBacking<'a>,
    byte_len: usize,
}

#[cfg_attr(not(any(feature = "audio", feature = "texture")), allow(dead_code))]
#[derive(Clone, Copy)]
enum EmbeddedMediaBacking<'a> {
    Bytes(&'a [u8]),
    Array(&'a [UnityValue]),
}

impl<'a> EmbeddedMediaRef<'a> {
    #[cfg_attr(not(any(feature = "audio", feature = "texture")), allow(dead_code))]
    pub(crate) fn inspect(
        value: &'a UnityValue,
        field: &'static str,
        invalid_array_reason: &'static str,
        invalid_shape_reason: &'static str,
    ) -> Result<Self, MediaInspectionError> {
        let (backing, byte_len) = match value {
            UnityValue::Bytes(bytes) => (EmbeddedMediaBacking::Bytes(bytes), bytes.len()),
            UnityValue::Array(items) => {
                if items.iter().any(|item| {
                    item.as_u64()
                        .and_then(|value| u8::try_from(value).ok())
                        .is_none()
                }) {
                    return Err(MediaInspectionError::InvalidDescriptor {
                        field,
                        reason: invalid_array_reason,
                    });
                }
                (EmbeddedMediaBacking::Array(items), items.len())
            }
            _ => {
                return Err(MediaInspectionError::InvalidDescriptor {
                    field,
                    reason: invalid_shape_reason,
                });
            }
        };
        Ok(Self { backing, byte_len })
    }

    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }

    pub fn materialize(
        self,
        resource: &'static str,
        budget: &mut AssetLoadBudget,
    ) -> Result<BudgetedMediaBytes, EmbeddedMediaError> {
        let minimum = u64::try_from(self.byte_len)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(minimum)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(self.byte_len).map_err(|source| {
            EmbeddedMediaError::Allocation {
                resource,
                requested: self.byte_len,
                source,
            }
        })?;
        let retained = media_vec_allocation_bytes(bytes.capacity(), resource)?;
        budget.check_bytes(retained)?;
        match self.backing {
            EmbeddedMediaBacking::Bytes(source) => bytes.extend_from_slice(source),
            EmbeddedMediaBacking::Array(values) => {
                for value in values {
                    let byte = value
                        .as_u64()
                        .and_then(|value| u8::try_from(value).ok())
                        .ok_or(EmbeddedMediaError::EvidenceChanged)?;
                    bytes.push(byte);
                }
            }
        }
        if bytes.len() != self.byte_len {
            return Err(EmbeddedMediaError::EvidenceChanged);
        }
        BudgetedMediaBytes::from_vec(bytes, resource, budget).map_err(Into::into)
    }
}

impl fmt::Debug for EmbeddedMediaRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backing = match self.backing {
            EmbeddedMediaBacking::Bytes(_) => "bytes",
            EmbeddedMediaBacking::Array(_) => "array",
        };
        formatter
            .debug_struct("EmbeddedMediaRef")
            .field("backing", &backing)
            .field("byte_len", &self.byte_len)
            .finish()
    }
}

impl PartialEq for EmbeddedMediaRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.byte_len == other.byte_len
    }
}

impl Eq for EmbeddedMediaRef<'_> {}

#[derive(Debug, Error)]
pub enum EmbeddedMediaError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} bytes for {resource}: {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("embedded media no longer matches its inspection evidence")]
    EvidenceChanged,
}

/// TypeTree field selected by the AudioClip streamed-resource precedence rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioClipResourceField {
    Resource,
    StreamData,
}

impl AudioClipResourceField {
    #[must_use]
    pub const fn field_name(self) -> &'static str {
        match self {
            Self::Resource => "m_Resource",
            Self::StreamData => "m_StreamData",
        }
    }
}

/// Allocation-free AudioClip streamed-resource selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClipResourceRef<'a> {
    field: AudioClipResourceField,
    stream: StreamDataRef<'a>,
}

impl<'a> AudioClipResourceRef<'a> {
    #[must_use]
    pub const fn field(self) -> AudioClipResourceField {
        self.field
    }

    #[must_use]
    pub const fn stream(self) -> StreamDataRef<'a> {
        self.stream
    }
}

/// Applies the repository's explicit primary/fallback AudioClip field rule.
pub fn classify_audio_clip_resource(
    properties: &IndexMap<String, UnityValue>,
) -> Result<Option<AudioClipResourceRef<'_>>, MediaInspectionError> {
    if let Some(stream) = stream_data_candidate(
        properties.get("m_Resource"),
        StreamDataShape {
            field: "m_Resource",
            path: "m_Source",
            offset: "m_Offset",
            size: "m_Size",
        },
    )? {
        return Ok(Some(AudioClipResourceRef {
            field: AudioClipResourceField::Resource,
            stream,
        }));
    }
    Ok(stream_data_candidate(
        properties.get("m_StreamData"),
        StreamDataShape {
            field: "m_StreamData",
            path: "path",
            offset: "offset",
            size: "size",
        },
    )?
    .map(|stream| AudioClipResourceRef {
        field: AudioClipResourceField::StreamData,
        stream,
    }))
}

#[derive(Clone, Copy)]
pub(crate) struct StreamDataShape {
    field: &'static str,
    path: &'static str,
    offset: &'static str,
    size: &'static str,
}

impl StreamDataShape {
    #[cfg(feature = "texture")]
    pub(crate) const UNITY_STREAM_DATA: Self = Self {
        field: "m_StreamData",
        path: "path",
        offset: "offset",
        size: "size",
    };
}

pub(crate) fn stream_data_candidate<'a>(
    value: Option<&'a UnityValue>,
    shape: StreamDataShape,
) -> Result<Option<StreamDataRef<'a>>, MediaInspectionError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let UnityValue::Object(fields) = value else {
        return Err(MediaInspectionError::InvalidDescriptor {
            field: shape.field,
            reason: "stream declaration must be an object",
        });
    };
    let path = fields.get(shape.path).and_then(UnityValue::as_str).ok_or(
        MediaInspectionError::InvalidDescriptor {
            field: shape.path,
            reason: "field must be a string",
        },
    )?;
    let offset = fields
        .get(shape.offset)
        .and_then(UnityValue::as_u64)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: shape.offset,
            reason: "field must be an unsigned integer",
        })?;
    let size = fields.get(shape.size).and_then(UnityValue::as_u64).ok_or(
        MediaInspectionError::InvalidDescriptor {
            field: shape.size,
            reason: "field must be an unsigned integer",
        },
    )?;
    StreamDataRef::new(path, offset, size).map(Some)
}

impl<'a> MediaPayloadRef<'a> {
    pub fn classify(
        embedded: Option<EmbeddedMediaRef<'a>>,
        stream: Option<StreamDataRef<'a>>,
    ) -> Result<Self, MediaInspectionError> {
        match (embedded, stream) {
            (None, None) => Err(MediaInspectionError::MissingPayload),
            (None, Some(stream)) => Ok(Self::Streamed(stream)),
            (Some(embedded), None) if embedded.byte_len() != 0 => Ok(Self::Embedded(embedded)),
            (Some(_), None) => Err(MediaInspectionError::MissingPayload),
            (Some(embedded), Some(stream)) if embedded.byte_len() == 0 => {
                Ok(Self::Streamed(stream))
            }
            (Some(_), Some(_)) => Err(MediaInspectionError::AmbiguousPayload),
        }
    }

    #[must_use]
    pub const fn embedded_byte_len(self) -> Option<usize> {
        match self {
            Self::Embedded(embedded) => Some(embedded.byte_len()),
            Self::Streamed(_) => None,
        }
    }

    #[must_use]
    pub const fn embedded(self) -> Option<EmbeddedMediaRef<'a>> {
        match self {
            Self::Embedded(embedded) => Some(embedded),
            Self::Streamed(_) => None,
        }
    }

    #[must_use]
    pub const fn stream(self) -> Option<StreamDataRef<'a>> {
        match self {
            Self::Embedded(_) => None,
            Self::Streamed(stream) => Some(stream),
        }
    }
}

/// Failures while deriving a strict media layout from trusted schema evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MediaInspectionError {
    #[error("object class {actual} is not the expected media class {expected}")]
    NotApplicable { expected: i32, actual: i32 },
    #[error("the object has no TypeTree evidence")]
    TypeTreeUnavailable,
    #[error("invalid media descriptor field {field}: {reason}")]
    InvalidDescriptor {
        field: &'static str,
        reason: &'static str,
    },
    #[error("{family} encoding {value} has no prepared artifact implementation in this build")]
    UnsupportedEncoding { family: &'static str, value: i32 },
    #[error("{family} layout {layout} has no strict prepared artifact implementation")]
    UnsupportedLayout {
        family: &'static str,
        layout: &'static str,
    },
    #[error("media object has no non-empty embedded or streamed payload")]
    MissingPayload,
    #[error(
        "media object has simultaneous embedded and streamed payloads without a precedence rule"
    )]
    AmbiguousPayload,
    #[error("streamed media range {offset}+{size} overflows u64")]
    StreamRangeOverflow { offset: u64, size: u64 },
    #[error("raw media layout {layout} is not supported without versioned corpus evidence")]
    UnsupportedRawLayout { layout: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_range_rejects_empty_zero_and_overflow() {
        assert!(matches!(
            StreamDataRef::new("", 0, 1),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "stream.path",
                ..
            })
        ));
        assert!(matches!(
            StreamDataRef::new("a.resS", 0, 0),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "stream.size",
                ..
            })
        ));
        assert_eq!(
            StreamDataRef::new("a.resS", u64::MAX, 1),
            Err(MediaInspectionError::StreamRangeOverflow {
                offset: u64::MAX,
                size: 1,
            })
        );
    }

    #[test]
    fn payload_classifier_requires_exactly_one_non_empty_payload() {
        let stream = StreamDataRef::new("a.resS", 2, 3).unwrap();
        let empty_value = UnityValue::Bytes(Vec::new());
        let empty =
            EmbeddedMediaRef::inspect(&empty_value, "test", "invalid array", "invalid shape")
                .unwrap();
        let embedded_value = UnityValue::Bytes(vec![0; 4]);
        let embedded =
            EmbeddedMediaRef::inspect(&embedded_value, "test", "invalid array", "invalid shape")
                .unwrap();
        assert_eq!(
            MediaPayloadRef::classify(None, None),
            Err(MediaInspectionError::MissingPayload)
        );
        assert_eq!(
            MediaPayloadRef::classify(Some(embedded), Some(stream)),
            Err(MediaInspectionError::AmbiguousPayload)
        );
        assert_eq!(
            MediaPayloadRef::classify(None, Some(stream)).unwrap(),
            MediaPayloadRef::Streamed(stream)
        );
        assert_eq!(
            MediaPayloadRef::classify(Some(empty), Some(stream)).unwrap(),
            MediaPayloadRef::Streamed(stream)
        );
        assert_eq!(
            MediaPayloadRef::classify(Some(empty), None),
            Err(MediaInspectionError::MissingPayload)
        );
        let payload = MediaPayloadRef::classify(Some(embedded), None).unwrap();
        assert_eq!(payload.embedded_byte_len(), Some(4));
    }

    #[test]
    fn embedded_media_materialization_charges_actual_capacity() {
        let value = UnityValue::Array(vec![
            UnityValue::Integer(1),
            UnityValue::Integer(2),
            UnityValue::Integer(3),
            UnityValue::Integer(4),
        ]);
        let embedded =
            EmbeddedMediaRef::inspect(&value, "test", "invalid array", "invalid shape").unwrap();
        let mut measurement = AssetLoadBudget::default();
        assert_eq!(
            embedded
                .materialize("test embedded media", &mut measurement)
                .unwrap()
                .as_bytes(),
            [1, 2, 3, 4]
        );
        let retained = measurement.usage().bytes;
        assert!(retained >= 4);

        let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: retained,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();
        assert!(
            embedded
                .materialize("test embedded media", &mut exact)
                .is_ok()
        );
        assert_eq!(exact.usage().bytes, retained);

        let mut one_short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_bytes: retained - 1,
            ..unity_asset_core::AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            embedded.materialize("test embedded media", &mut one_short),
            Err(EmbeddedMediaError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn budgeted_media_bytes_reject_a_different_budget_domain() {
        let mut first = AssetLoadBudget::default();
        let bytes = BudgetedMediaBytes::from_vec(vec![1, 2, 3, 4], "test media source", &mut first)
            .unwrap();
        let second = AssetLoadBudget::default();

        assert!(matches!(
            bytes.into_vec(&second),
            Err(BudgetError::DomainMismatch {
                resource: "test media source"
            })
        ));
    }
}
