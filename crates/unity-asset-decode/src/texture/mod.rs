//! Strict Unity Texture2D inspection, decoding, and prepared PNG output.
//!
//! [`Texture2DLayout`] binds TypeTree fields to the owning serialized-file context. Prepare
//! [`PreparedTexturePng`] from that layout and caller-budgeted media bytes; owned Texture2D
//! carriers, generic image exporters, and context-free decode helpers are intentionally absent.

mod decoders;
mod formats;
mod inspection;
pub(crate) mod prepared;

pub use formats::TextureFormat;
pub use inspection::{MediaInspectionContext, Texture2DLayout};
pub use prepared::{PreparedTexturePng, TexturePreparationError};
