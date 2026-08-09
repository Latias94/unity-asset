//! Strict Unity AudioClip inspection and prepared standard-container output.
//!
//! [`AudioClipLayout`] owns TypeTree field interpretation. Pass its resolved, caller-budgeted
//! payload to [`PreparedAudioSource::prepare`] and publish the prepared bytes. The crate no
//! longer exposes owned AudioClip carriers or unbudgeted decode helpers.

mod formats;
mod fsb5;
mod inspection;
mod ogg;
mod prepared;

pub use formats::AudioCompressionFormat;
pub use fsb5::MAX_VORBIS_SETUP_PACKET_BYTES;
pub use inspection::AudioClipLayout;
pub use prepared::{AudioSourceError, PreparedAudioSource};
