//! Workspace-bound execution runtime for extraction representations.

use unity_asset_core::AssetLoadBudget;

use super::contract::RepresentationContract;
use super::prepared::{PreparedRepresentation, RepresentationPreparationError};
use super::reservation::{ExtractionReservationError, trusted_working_set};
#[cfg(feature = "decode")]
use crate::workspace::StreamedResourceResolver;
#[cfg(feature = "decode")]
use crate::workspace::WorkspaceSource;
use crate::workspace::{WorkspaceError, WorkspaceView};

/// Owns the source snapshot required to prove streamed representation inputs.
pub(in crate::extraction) struct RepresentationRuntimeContext {
    #[cfg(feature = "decode")]
    stream_sources: Option<Vec<WorkspaceSource>>,
}

impl RepresentationRuntimeContext {
    pub(in crate::extraction) fn load<'contract>(
        view: &dyn WorkspaceView,
        contracts: impl IntoIterator<Item = &'contract RepresentationContract>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        #[cfg(feature = "decode")]
        {
            let stream_sources = contracts
                .into_iter()
                .any(RepresentationContract::requires_stream_resolution)
                .then(|| view.sources(budget))
                .transpose()?;
            Ok(Self { stream_sources })
        }
        #[cfg(not(feature = "decode"))]
        {
            let _ = (view, contracts, budget);
            Ok(Self {})
        }
    }

    pub(in crate::extraction) fn bind<'view, 'context>(
        &'context self,
        view: &'view dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<RepresentationRuntime<'view, 'context>, WorkspaceError> {
        #[cfg(feature = "decode")]
        let stream_resolver = self
            .stream_sources
            .as_deref()
            .map(|sources| StreamedResourceResolver::new(view, sources, budget))
            .transpose()?;
        #[cfg(not(feature = "decode"))]
        let _ = budget;
        Ok(RepresentationRuntime {
            view,
            #[cfg(feature = "decode")]
            stream_resolver,
            #[cfg(not(feature = "decode"))]
            marker: std::marker::PhantomData,
        })
    }
}

/// Opaque execution owner for representation proof and preparation.
pub(in crate::extraction) struct RepresentationRuntime<'view, 'context> {
    view: &'view dyn WorkspaceView,
    #[cfg(feature = "decode")]
    stream_resolver: Option<StreamedResourceResolver<'view, 'context>>,
    #[cfg(not(feature = "decode"))]
    marker: std::marker::PhantomData<&'context ()>,
}

impl RepresentationRuntime<'_, '_> {
    pub(in crate::extraction) fn trusted_working_set(
        &self,
        address: &unity_asset_core::ObjectAddress,
        contract: &RepresentationContract,
        budget: &mut AssetLoadBudget,
    ) -> Result<u64, ExtractionReservationError> {
        trusted_working_set(
            self.view,
            address,
            contract,
            #[cfg(feature = "decode")]
            self.stream_resolver.as_ref(),
            budget,
        )
    }

    pub(in crate::extraction) fn prepare(
        &self,
        address: &unity_asset_core::ObjectAddress,
        contract: &RepresentationContract,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedRepresentation, RepresentationPreparationError> {
        PreparedRepresentation::prepare(self.view, address, contract, budget)
    }
}
