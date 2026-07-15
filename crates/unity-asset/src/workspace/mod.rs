//! Revisioned ownership and source-resolution foundation.

mod source_catalog;

pub use source_catalog::{
    CatalogError, PhysicalOrigin, PhysicalOriginError, SourceCatalog, SourceDescriptor,
    SourceLocationKind,
};
