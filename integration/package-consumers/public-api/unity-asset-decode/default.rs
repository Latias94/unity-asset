//! Public API contract for the feature-free `unity-asset-decode` package.

pub use unity_asset_decode::{
    descriptor::{
        CanonicalMediaExtension, MediaContainer, MediaDescriptor, MediaDimensions, MediaEncoding,
        MediaFamily, MediaMime, MediaOutputEstimate, PreparedAudioSourceKind, UnityTextureEncoding,
    },
    media::{
        AudioClipResourceField, AudioClipResourceRef, BudgetedMediaBytes, EmbeddedMediaRef,
        MediaInspectionError, MediaPayloadRef, StreamDataRef, classify_audio_clip_resource,
    },
};
