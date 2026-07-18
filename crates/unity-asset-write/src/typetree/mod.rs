//! Canonical TypeTree writing adapters.

mod output;
mod primitives;
mod template;
mod writer;

#[cfg(test)]
mod characterization;
#[cfg(test)]
mod test_support;

pub(crate) use template::rewrite_object;
