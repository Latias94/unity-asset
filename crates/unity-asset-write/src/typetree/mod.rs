//! TypeTree-driven writing utilities (UnityPy parity target).
//!
//! This module is intentionally split into small submodules so the write pipeline doesn't grow
//! into a monolithic `lib.rs`.

mod context;
mod primitives;
mod referenced_object;
mod writer;

pub use context::TypeTreeWriteContext;
pub use writer::{TypeTreeWriteOptions, TypeTreeWriter};
