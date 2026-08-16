use thiserror::Error;

use crate::artifact::{
    ArtifactBatch, ArtifactBatchDeclaration, ArtifactBuildError, ArtifactBuildFailurePhase,
    ArtifactHandle, LogicalArtifactName, OutputSlot, StreamedResourceExtentInspection,
};

use super::allocation::{
    ExtentPayload, StreamedResourceFlags, StreamedResourcePlan, StreamedResourcePlanError,
};

/// A resource plan whose sidecar output name has been declared in an artifact batch.
pub struct DeclaredStreamedResource<'extent, 'payload> {
    plan: StreamedResourcePlan<'extent, 'payload>,
    slot: OutputSlot,
}

impl<'extent, 'payload> DeclaredStreamedResource<'extent, 'payload> {
    #[must_use]
    pub const fn output_slot(&self) -> OutputSlot {
        self.slot
    }

    #[must_use]
    pub const fn flags(&self) -> StreamedResourceFlags {
        self.plan.flags()
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.plan.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.plan.is_empty()
    }

    #[must_use]
    pub const fn extent_count(&self) -> usize {
        self.plan.extent_count()
    }

    /// Encodes the sidecar and binds it to the previously declared output atomically.
    pub fn prepare(
        self,
        batch: &mut ArtifactBatch<'_, '_>,
    ) -> Result<PreparedStreamedResource, StreamedResourceError> {
        let prepared = self.plan.prepare(batch)?;
        batch.bind_output(self.slot, prepared.handle)?;
        Ok(PreparedStreamedResource {
            handle: prepared.handle,
            output_slot: Some(self.slot),
            flags: prepared.flags,
            length: prepared.length,
            extent_count: prepared.extent_count,
        })
    }
}

/// The exact sidecar artifact produced by a streamed-resource plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedStreamedResource {
    handle: ArtifactHandle,
    output_slot: Option<OutputSlot>,
    flags: StreamedResourceFlags,
    length: u64,
    extent_count: usize,
}

impl PreparedStreamedResource {
    #[must_use]
    pub const fn handle(self) -> ArtifactHandle {
        self.handle
    }

    #[must_use]
    pub const fn output_slot(self) -> Option<OutputSlot> {
        self.output_slot
    }

    #[must_use]
    pub const fn flags(self) -> StreamedResourceFlags {
        self.flags
    }

    #[must_use]
    pub const fn len(self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.length == 0
    }

    #[must_use]
    pub const fn extent_count(self) -> usize {
        self.extent_count
    }
}

#[derive(Debug, Error)]
pub enum StreamedResourceError {
    #[error(transparent)]
    Plan(#[from] StreamedResourcePlanError),
    #[error(transparent)]
    Artifact(Box<ArtifactBuildError>),
}

impl StreamedResourceError {
    /// Reports the artifact-build stage in which resource preparation failed.
    #[must_use]
    pub const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::Artifact(error) => error.failure_phase(),
            Self::Plan(_) => ArtifactBuildFailurePhase::Encoding,
        }
    }
}

impl From<ArtifactBuildError> for StreamedResourceError {
    fn from(error: ArtifactBuildError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl<'extent, 'payload> StreamedResourcePlan<'extent, 'payload> {
    /// Declares a named sidecar output before the artifact namespace is sealed.
    pub fn declare_output(
        self,
        declaration: &mut ArtifactBatchDeclaration<'_, '_>,
        name: LogicalArtifactName,
    ) -> Result<DeclaredStreamedResource<'extent, 'payload>, StreamedResourceError> {
        let slot = declaration.declare_output(name)?;
        Ok(DeclaredStreamedResource { plan: self, slot })
    }

    /// Encodes the plan as an unbound artifact, for use as an embedded container dependency.
    pub fn prepare(
        &self,
        batch: &mut ArtifactBatch<'_, '_>,
    ) -> Result<PreparedStreamedResource, StreamedResourceError> {
        let mut layout = batch.streamed_resource_layout_builder()?;
        for (extent, allocation) in self.extents().iter().zip(self.allocations()) {
            layout.push(StreamedResourceExtentInspection::new(
                extent.payload_digest(),
                allocation.offset(),
                u64::from(allocation.size()),
                allocation.alignment(),
            ))?;
        }

        let handle = batch.prepare_streamed_resource(layout, |encoder| {
            for (extent, allocation) in self.extents().iter().zip(self.allocations()) {
                let padding = allocation.padding_before();
                if padding != 0 {
                    let length = usize::try_from(padding).map_err(|_| {
                        ArtifactBuildError::ArithmeticOverflow {
                            resource: "streamed resource padding length",
                        }
                    })?;
                    let mut writer = encoder.generated_chunk_writer()?;
                    writer.resize_zero(length)?;
                    let payload = encoder.finish_generated_chunk(writer)?;
                    encoder.push_payload_full(&payload)?;
                }

                match extent.payload() {
                    ExtentPayload::Generated(bytes) => {
                        if !bytes.is_empty() {
                            let mut writer = encoder.generated_chunk_writer()?;
                            writer.extend_from_slice(bytes)?;
                            let payload = encoder.finish_generated_chunk(writer)?;
                            encoder.push_payload_full(&payload)?;
                        }
                    }
                    ExtentPayload::Artifact(payload) => {
                        if !payload.is_empty() {
                            encoder.push_payload_full(payload)?;
                        }
                    }
                    ExtentPayload::ArtifactRange { payload, range } => {
                        if !range.is_empty() {
                            encoder.push_payload_range(payload, range.clone())?;
                        }
                    }
                }
            }
            Ok::<(), ArtifactBuildError>(())
        })?;

        Ok(PreparedStreamedResource {
            handle,
            output_slot: None,
            flags: self.flags(),
            length: self.len(),
            extent_count: self.extent_count(),
        })
    }
}
