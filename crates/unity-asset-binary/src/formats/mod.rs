//! Layered "formats" namespace (UnityPy-aligned).
//!
//! This provides a stable-ish import surface to reduce top-level re-exports.

pub mod bundle {
    pub use crate::bundle::*;
}

pub mod serialized {
    pub use crate::asset::*;
}

pub mod web {
    pub use crate::webfile::*;
}
