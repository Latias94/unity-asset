//! Canonical TypeTree writing adapters.

mod output;
mod primitives;
mod template;
mod writer;

#[cfg(test)]
mod characterization;
#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use template::{TemplateRewriteStats, rewrite_object};
pub(crate) use writer::validate_value;
