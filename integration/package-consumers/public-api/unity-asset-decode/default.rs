//! Public API contract for the feature-free `unity-asset-decode` package.

pub use unity_asset_decode::{
    descriptor::{
        CanonicalMediaExtension, MediaContainer, MediaDescriptor, MediaDimensions, MediaEncoding,
        MediaFamily, MediaMime, MediaOutputEstimate, PreparedAudioSourceKind, UnityTextureEncoding,
    },
    media::{
        BudgetedMediaBytes, EmbeddedMediaRef, MediaInspectionError, MediaPayloadRef, StreamDataRef,
    },
};
