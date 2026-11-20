//! Capability-gated LSP implementation and type hierarchy edges.

use lsp_types::request::{
    GotoImplementation, GotoImplementationParams, Request, TypeHierarchyPrepare,
    TypeHierarchySubtypes, TypeHierarchySupertypes,
};
use lsp_types::{
    GotoDefinitionResponse, Location, PartialResultParams, Position, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
    WorkDoneProgressParams,
};
use rift_core::ProjectPath;
use rift_error::RiftError;

use super::{EngineSession, require};

/// Initialize retains the advertised edge capabilities absent from the dependency model.
#[derive(Debug)]
pub(super) enum EdgeInitialize {}

impl Request for EdgeInitialize {
    type Params = lsp_types::InitializeParams;
    type Result = serde_json::Value;
    const METHOD: &'static str = <lsp_types::request::Initialize as Request>::METHOD;
}

impl EngineSession {
    /// Reads implementation locations when the engine advertises that edge capability.
    ///
    /// # Errors
    /// Returns the capability refusal or the bounded exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves a pending response for the next exchange to discard.
    pub async fn implementation_edges(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Vec<Location>, RiftError> {
        require(
            self.capabilities.edges.implementation,
            GotoImplementation::METHOD,
        )?;
        let params = GotoImplementationParams {
            text_document_position_params: self.position_params(path, position)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let response = self.request::<GotoImplementation>(params).await?;
        Ok(definition_locations(response))
    }

    /// Prepares the type hierarchy items used to read inheritance edges.
    ///
    /// # Errors
    /// Returns the capability refusal or the bounded exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves a pending response for the next exchange to discard.
    pub async fn prepare_type_edges(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Vec<TypeHierarchyItem>, RiftError> {
        require(
            self.capabilities.edges.type_hierarchy,
            TypeHierarchyPrepare::METHOD,
        )?;
        let params = TypeHierarchyPrepareParams {
            text_document_position_params: self.position_params(path, position)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        Ok(self
            .request::<TypeHierarchyPrepare>(params)
            .await?
            .unwrap_or_default())
    }

    /// Reads the immediate supertypes of one prepared type hierarchy item.
    ///
    /// # Errors
    /// Returns the capability refusal or the bounded exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves a pending response for the next exchange to discard.
    pub async fn supertype_edges(
        &mut self,
        item: TypeHierarchyItem,
    ) -> Result<Vec<TypeHierarchyItem>, RiftError> {
        require(
            self.capabilities.edges.type_hierarchy,
            TypeHierarchySupertypes::METHOD,
        )?;
        let params = TypeHierarchySupertypesParams {
            item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        Ok(self
            .request::<TypeHierarchySupertypes>(params)
            .await?
            .unwrap_or_default())
    }

    /// Reads the immediate subtypes of one prepared type hierarchy item.
    ///
    /// # Errors
    /// Returns the capability refusal or the bounded exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves a pending response for the next exchange to discard.
    pub async fn subtype_edges(
        &mut self,
        item: TypeHierarchyItem,
    ) -> Result<Vec<TypeHierarchyItem>, RiftError> {
        require(
            self.capabilities.edges.type_hierarchy,
            TypeHierarchySubtypes::METHOD,
        )?;
        let params = TypeHierarchySubtypesParams {
            item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        Ok(self
            .request::<TypeHierarchySubtypes>(params)
            .await?
            .unwrap_or_default())
    }
}

/// The declaration selection ranges shared by definition response forms.
pub(super) fn definition_locations(response: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match response {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(locations)) => locations
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
    }
}
