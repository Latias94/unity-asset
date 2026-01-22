//! Unity WebFile rebuild/save support (UnityPy parity).

mod edits;
mod writer;

pub use edits::WebFileEdits;
pub use writer::{WebFilePacker, WebFileWriter};
