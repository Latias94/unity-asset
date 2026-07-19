//! Deterministic, budgeted streamed-resource allocation.
//!
//! A resource plan borrows the bytes owned by the caller and performs all arithmetic and shape
//! validation before it touches an [`crate::artifact::ArtifactBatch`]. Preparing the plan then
//! creates one exact streamed-resource artifact. The artifact batch owns the allocation budget
//! and transaction, so failed encoding never leaves a partially appended CAB behind.

mod allocation;
mod encoder;

pub use allocation::{
    StreamedResourceAllocation, StreamedResourceAllocationIter, StreamedResourceExtent,
    StreamedResourceFlags, StreamedResourcePlan, StreamedResourcePlanError,
};
pub use encoder::{DeclaredStreamedResource, PreparedStreamedResource, StreamedResourceError};

#[cfg(test)]
mod tests;
