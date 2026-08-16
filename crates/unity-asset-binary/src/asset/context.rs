//! Serialized object context proven by an owning SerializedFile.

use super::format::{MetadataField, SerializedFileFormat};
use crate::reader::ByteOrder;

/// Raw Unity build target retained without collapsing unknown values.
///
/// Values can only be minted while parsing or inspecting a SerializedFile. Consumers may inspect
/// the raw value, but cannot manufacture platform evidence from an arbitrary integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuildTarget(i32);

impl BuildTarget {
    /// Unity editor/no-player target.
    pub const NO_TARGET: Self = Self(-2);
    /// Xbox 360 player target.
    pub const XBOX_360: Self = Self(11);
    /// Nintendo Switch player target.
    pub const SWITCH: Self = Self(38);
    /// Nintendo Switch 2 player target.
    pub const SWITCH_2: Self = Self(48);

    pub(super) const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Returns the exact signed value stored in SerializedFile metadata.
    #[must_use]
    pub const fn raw(self) -> i32 {
        self.0
    }
}

/// Whether the SerializedFile format physically stored a target-platform field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetPlatformEvidence {
    /// The format predates the target-platform metadata field.
    Absent,
    /// The field was present and its exact raw value was retained.
    Present(BuildTarget),
}

impl TargetPlatformEvidence {
    const fn from_wire(format: SerializedFileFormat, raw: i32) -> Self {
        if format.has_metadata_field(MetadataField::TargetPlatform) {
            Self::Present(BuildTarget::from_raw(raw))
        } else {
            Self::Absent
        }
    }

    /// Returns the proven target when the field was present.
    #[must_use]
    pub const fn target(self) -> Option<BuildTarget> {
        match self {
            Self::Absent => None,
            Self::Present(target) => Some(target),
        }
    }
}

/// File-owned evidence required to interpret one serialized object's wire representation.
///
/// The constructor is intentionally private to the binary asset module. A context must originate
/// from a validated [`super::SerializedFile`] or [`super::SerializedFileInspection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedObjectContext {
    format: SerializedFileFormat,
    byte_order: ByteOrder,
    target_platform: TargetPlatformEvidence,
}

impl SerializedObjectContext {
    pub(super) const fn from_wire(
        format: SerializedFileFormat,
        byte_order: ByteOrder,
        target_platform: i32,
    ) -> Self {
        Self {
            format,
            byte_order,
            target_platform: TargetPlatformEvidence::from_wire(format, target_platform),
        }
    }

    /// Returns the validated SerializedFile format capabilities.
    #[must_use]
    pub const fn format(self) -> SerializedFileFormat {
        self.format
    }

    /// Returns the byte order declared by the owning SerializedFile.
    #[must_use]
    pub const fn byte_order(self) -> ByteOrder {
        self.byte_order
    }

    /// Returns explicit presence/absence evidence for target-platform metadata.
    #[must_use]
    pub const fn target_platform(self) -> TargetPlatformEvidence {
        self.target_platform
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_preserves_unknown_targets_and_field_absence() {
        let modern = SerializedObjectContext::from_wire(
            SerializedFileFormat::new(22).unwrap(),
            ByteOrder::Little,
            3716,
        );
        assert_eq!(
            modern.target_platform(),
            TargetPlatformEvidence::Present(BuildTarget::from_raw(3716))
        );

        let legacy = SerializedObjectContext::from_wire(
            SerializedFileFormat::new(5).unwrap(),
            ByteOrder::Big,
            0,
        );
        assert_eq!(legacy.target_platform(), TargetPlatformEvidence::Absent);
    }
}
