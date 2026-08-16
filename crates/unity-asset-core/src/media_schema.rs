//! Allocation-free media field-shape interpretation shared by schema consumers.

use indexmap::IndexMap;
use thiserror::Error;

use crate::UnityValue;

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

/// Borrowed fields from one structurally and semantically valid Unity stream declaration.
///
/// This type proves the shared stream contract: a non-blank control-free path, a non-zero size,
/// and an in-range `offset + size`. Codecs remain responsible for platform-specific layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamDataDeclaration<'a> {
    path: &'a str,
    offset: u64,
    size: u64,
}

impl<'a> StreamDataDeclaration<'a> {
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
}

/// Allocation-free AudioClip streamed-resource field selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClipResourceSelection<'a> {
    field: AudioClipResourceField,
    declaration: StreamDataDeclaration<'a>,
}

impl<'a> AudioClipResourceSelection<'a> {
    #[must_use]
    pub const fn field(self) -> AudioClipResourceField {
        self.field
    }

    #[must_use]
    pub const fn declaration(self) -> StreamDataDeclaration<'a> {
        self.declaration
    }
}

/// Invalid AudioClip streamed-resource fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AudioClipResourceShapeError {
    #[error("invalid AudioClip resource field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("AudioClip streamed-resource range {offset}+{size} overflows u64")]
    StreamRangeOverflow { offset: u64, size: u64 },
}

/// Applies the repository's explicit primary/fallback AudioClip field-shape rule.
///
/// `m_Resource` is authoritative when active. A missing or canonical inactive primary
/// declaration falls back to `m_StreamData`; a malformed primary declaration never does.
pub fn classify_audio_clip_resource(
    properties: &IndexMap<String, UnityValue>,
) -> Result<Option<AudioClipResourceSelection<'_>>, AudioClipResourceShapeError> {
    if let Some(declaration) = classify_stream_declaration(
        properties.get("m_Resource"),
        StreamDataShape {
            field: "m_Resource",
            path: "m_Source",
            offset: "m_Offset",
            size: "m_Size",
        },
    )? {
        return Ok(Some(AudioClipResourceSelection {
            field: AudioClipResourceField::Resource,
            declaration,
        }));
    }

    classify_stream_declaration(
        properties.get("m_StreamData"),
        StreamDataShape {
            field: "m_StreamData",
            path: "path",
            offset: "offset",
            size: "size",
        },
    )
    .map(|declaration| {
        declaration.map(|declaration| AudioClipResourceSelection {
            field: AudioClipResourceField::StreamData,
            declaration,
        })
    })
}

#[derive(Clone, Copy)]
struct StreamDataShape {
    field: &'static str,
    path: &'static str,
    offset: &'static str,
    size: &'static str,
}

fn classify_stream_declaration<'a>(
    value: Option<&'a UnityValue>,
    shape: StreamDataShape,
) -> Result<Option<StreamDataDeclaration<'a>>, AudioClipResourceShapeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let UnityValue::Object(fields) = value else {
        return Err(AudioClipResourceShapeError::InvalidField {
            field: shape.field,
            reason: "stream declaration must be an object",
        });
    };
    let path = fields.get(shape.path).and_then(UnityValue::as_str).ok_or(
        AudioClipResourceShapeError::InvalidField {
            field: shape.path,
            reason: "field must be a string",
        },
    )?;
    let offset = fields
        .get(shape.offset)
        .and_then(UnityValue::as_u64)
        .ok_or(AudioClipResourceShapeError::InvalidField {
            field: shape.offset,
            reason: "field must be an unsigned integer",
        })?;
    let size = fields.get(shape.size).and_then(UnityValue::as_u64).ok_or(
        AudioClipResourceShapeError::InvalidField {
            field: shape.size,
            reason: "field must be an unsigned integer",
        },
    )?;

    if path.is_empty() && offset == 0 && size == 0 {
        return Ok(None);
    }

    if path.trim().is_empty() {
        return Err(AudioClipResourceShapeError::InvalidField {
            field: "stream.path",
            reason: "stream path must not be empty",
        });
    }
    if path.chars().any(char::is_control) {
        return Err(AudioClipResourceShapeError::InvalidField {
            field: "stream.path",
            reason: "stream path must not contain control characters",
        });
    }
    if size == 0 {
        return Err(AudioClipResourceShapeError::InvalidField {
            field: "stream.size",
            reason: "stream size must be non-zero",
        });
    }
    offset
        .checked_add(size)
        .ok_or(AudioClipResourceShapeError::StreamRangeOverflow { offset, size })?;

    Ok(Some(StreamDataDeclaration { path, offset, size }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(path: &str, offset: u64, size: u64) -> UnityValue {
        UnityValue::Object(IndexMap::from([
            ("m_Source".to_owned(), UnityValue::String(path.to_owned())),
            ("m_Offset".to_owned(), UnityValue::from(offset)),
            ("m_Size".to_owned(), UnityValue::from(size)),
        ]))
    }

    fn stream_data(path: &str, offset: u64, size: u64) -> UnityValue {
        UnityValue::Object(IndexMap::from([
            ("path".to_owned(), UnityValue::String(path.to_owned())),
            ("offset".to_owned(), UnityValue::from(offset)),
            ("size".to_owned(), UnityValue::from(size)),
        ]))
    }

    #[test]
    fn primary_resource_is_authoritative() {
        let properties = IndexMap::from([
            (
                "m_Resource".to_owned(),
                resource("archive:/CAB-a/CAB-a.resS", 4, 8),
            ),
            ("m_StreamData".to_owned(), UnityValue::Float(1.0)),
        ]);

        let selection = classify_audio_clip_resource(&properties).unwrap().unwrap();

        assert_eq!(selection.field(), AudioClipResourceField::Resource);
        assert_eq!(selection.declaration().path(), "archive:/CAB-a/CAB-a.resS");
        assert_eq!(selection.declaration().offset(), 4);
        assert_eq!(selection.declaration().size(), 8);
    }

    #[test]
    fn inactive_primary_uses_compatibility_fallback() {
        let properties = IndexMap::from([
            ("m_Resource".to_owned(), resource("", 0, 0)),
            (
                "m_StreamData".to_owned(),
                stream_data("archive:/CAB-a/CAB-a.resS", 4, 8),
            ),
        ]);

        let selection = classify_audio_clip_resource(&properties).unwrap().unwrap();

        assert_eq!(selection.field(), AudioClipResourceField::StreamData);
        assert_eq!(selection.declaration().offset(), 4);
        assert_eq!(selection.declaration().size(), 8);
    }

    #[test]
    fn absent_and_inactive_declarations_select_nothing() {
        assert_eq!(
            classify_audio_clip_resource(&IndexMap::new()).unwrap(),
            None
        );
        let properties = IndexMap::from([
            ("m_Resource".to_owned(), resource("", 0, 0)),
            ("m_StreamData".to_owned(), stream_data("", 0, 0)),
        ]);
        assert_eq!(classify_audio_clip_resource(&properties).unwrap(), None);
    }

    #[test]
    fn malformed_primary_never_falls_back() {
        let properties = IndexMap::from([
            ("m_Resource".to_owned(), UnityValue::Float(1.0)),
            (
                "m_StreamData".to_owned(),
                stream_data("archive:/CAB-a/CAB-a.resS", 4, 8),
            ),
        ]);

        assert_eq!(
            classify_audio_clip_resource(&properties),
            Err(AudioClipResourceShapeError::InvalidField {
                field: "m_Resource",
                reason: "stream declaration must be an object",
            })
        );
    }

    #[test]
    fn declaration_fields_reject_wrong_and_negative_numeric_types() {
        let cases = [
            (
                "m_Source",
                UnityValue::Float(1.0),
                AudioClipResourceShapeError::InvalidField {
                    field: "m_Source",
                    reason: "field must be a string",
                },
            ),
            (
                "m_Offset",
                UnityValue::from(-1_i64),
                AudioClipResourceShapeError::InvalidField {
                    field: "m_Offset",
                    reason: "field must be an unsigned integer",
                },
            ),
            (
                "m_Size",
                UnityValue::Float(1.0),
                AudioClipResourceShapeError::InvalidField {
                    field: "m_Size",
                    reason: "field must be an unsigned integer",
                },
            ),
        ];

        for (field, value, expected) in cases {
            let UnityValue::Object(mut declaration) = resource("a.resS", 0, 1) else {
                unreachable!();
            };
            declaration.insert(field.to_owned(), value);
            let properties =
                IndexMap::from([("m_Resource".to_owned(), UnityValue::Object(declaration))]);
            assert_eq!(classify_audio_clip_resource(&properties), Err(expected));
        }
    }

    #[test]
    fn active_declarations_enforce_the_shared_stream_contract() {
        let cases = [
            (
                "",
                0,
                1,
                AudioClipResourceShapeError::InvalidField {
                    field: "stream.path",
                    reason: "stream path must not be empty",
                },
            ),
            (
                "   ",
                0,
                1,
                AudioClipResourceShapeError::InvalidField {
                    field: "stream.path",
                    reason: "stream path must not be empty",
                },
            ),
            (
                "bad\npath",
                0,
                1,
                AudioClipResourceShapeError::InvalidField {
                    field: "stream.path",
                    reason: "stream path must not contain control characters",
                },
            ),
            (
                "a.resS",
                0,
                0,
                AudioClipResourceShapeError::InvalidField {
                    field: "stream.size",
                    reason: "stream size must be non-zero",
                },
            ),
            (
                "a.resS",
                u64::MAX,
                1,
                AudioClipResourceShapeError::StreamRangeOverflow {
                    offset: u64::MAX,
                    size: 1,
                },
            ),
        ];

        for (path, offset, size, expected) in cases {
            let properties =
                IndexMap::from([("m_Resource".to_owned(), resource(path, offset, size))]);
            assert_eq!(classify_audio_clip_resource(&properties), Err(expected));
        }
    }
}
