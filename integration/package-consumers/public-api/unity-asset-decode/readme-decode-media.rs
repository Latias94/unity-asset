//! README-promised audio and advanced texture API contract.

pub use unity_asset_decode::{
    audio::{AudioClipLayout, AudioCompressionFormat, PreparedAudioSource},
    descriptor::{MediaDescriptor, MediaOutputEstimate},
    media::{BudgetedMediaBytes, MediaInspectionError, MediaPayloadRef},
    texture::{
        MediaInspectionContext, PreparedTexturePng, Texture2DLayout, TextureFormat,
        TexturePreparationError,
    },
};
