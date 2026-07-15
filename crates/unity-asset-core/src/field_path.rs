use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::bounded::{BoundedString, BoundedVec};

const MAX_FIELD_PATH_SEGMENTS: usize = 512;
const MAX_FIELD_NAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(Vec<FieldPathSegment>);

impl FieldPath {
    #[must_use]
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    pub fn push_field(mut self, name: impl Into<String>) -> Result<Self, FieldPathError> {
        self.ensure_capacity()?;
        self.0.push(FieldPathSegment::field(name)?);
        Ok(self)
    }

    pub fn push_index(mut self, index: u32) -> Result<Self, FieldPathError> {
        self.ensure_capacity()?;
        self.0.push(FieldPathSegment::Index(index));
        Ok(self)
    }

    #[must_use]
    pub fn segments(&self) -> &[FieldPathSegment] {
        &self.0
    }

    fn ensure_capacity(&self) -> Result<(), FieldPathError> {
        if self.0.len() == MAX_FIELD_PATH_SEGMENTS {
            Err(FieldPathError::TooManySegments {
                maximum: MAX_FIELD_PATH_SEGMENTS,
            })
        } else {
            Ok(())
        }
    }
}

impl Default for FieldPath {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("$")?;
        for segment in &self.0 {
            match segment {
                FieldPathSegment::Field(name) => write!(formatter, ".{name}")?,
                FieldPathSegment::Index(index) => write!(formatter, "[{index}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldPathSegment {
    Field(String),
    Index(u32),
}

impl FieldPathSegment {
    pub fn field(name: impl Into<String>) -> Result<Self, FieldPathError> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_FIELD_NAME_BYTES || name.contains('\0') {
            return Err(FieldPathError::InvalidField(name));
        }
        Ok(Self::Field(name))
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FieldPathSegmentRef<'a> {
    Field { name: &'a str },
    Index { index: u32 },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FieldPathSegmentWire {
    Field {
        name: BoundedString<MAX_FIELD_NAME_BYTES>,
    },
    Index {
        index: u32,
    },
}

impl Serialize for FieldPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.0.iter().map(|segment| match segment {
            FieldPathSegment::Field(name) => FieldPathSegmentRef::Field { name },
            FieldPathSegment::Index(index) => FieldPathSegmentRef::Index { index: *index },
        }))
    }
}

impl<'de> Deserialize<'de> for FieldPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire =
            BoundedVec::<FieldPathSegmentWire, MAX_FIELD_PATH_SEGMENTS>::deserialize(deserializer)?;
        let mut path = Self::root();
        for segment in wire.into_vec() {
            path = match segment {
                FieldPathSegmentWire::Field { name } => path
                    .push_field(name.into_string())
                    .map_err(serde::de::Error::custom)?,
                FieldPathSegmentWire::Index { index } => {
                    path.push_index(index).map_err(serde::de::Error::custom)?
                }
            };
        }
        Ok(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FieldPathError {
    #[error("field path segment is empty or contains NUL: {0:?}")]
    InvalidField(String),
    #[error("field path exceeds the maximum of {maximum} segments")]
    TooManySegments { maximum: usize },
}
