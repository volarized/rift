use std::path::Path;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{NodesParams, NodesResult};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

use super::{CapturedMetadata, LocalSymbolMaterial, LocalSymbolViews, invalid, material_error};

impl LocalSymbolViews {
    /// Reads nodes from the requested retained publication and its captured framework context.
    /// Expected view refusal remains an outcome; it never selects current source.
    ///
    /// # Errors
    /// Returns registered request, syntax or captured metadata failures.
    pub fn nodes(
        &self,
        request: &NodesParams,
        view: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<Result<(NodesResult, CapturedView), SymbolUnavailableReason>, RiftError> {
        if request.rev.is_some() {
            return Err(invalid("rev", "rev cannot accompany a captured view"));
        }
        if request
            .view
            .as_ref()
            .is_some_and(|selected| selected != view)
        {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        let held = match self.held_view_at(view, root, owner, SystemTime::now()) {
            Ok(held) => held,
            Err(reason) => return Ok(Err(reason)),
        };
        Ok(held.material.nodes(request)?.map(|mut nodes| {
            nodes.view = Some(held.view.clone());
            (nodes, held.view)
        }))
    }
}

impl LocalSymbolMaterial {
    fn nodes(
        &self,
        request: &NodesParams,
    ) -> Result<Result<NodesResult, SymbolUnavailableReason>, RiftError> {
        let path = rift_core::ProjectPath::new(&request.path.0)?;
        let Some(file) = self.index.file(&path) else {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        };
        if request.position >= u64::try_from(file.source().len()).unwrap_or(u64::MAX) {
            return Err(invalid(
                "position",
                "position must be inside captured source",
            ));
        }
        let Some(document) = self.index.syntax(&path)? else {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        };
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let declarations: Vec<_> = (0..document.symbols().len())
            .map(|position| {
                self.index
                    .declaration_address(&path, position)
                    .map(|(identity, ordinal)| (identity.clone(), ordinal))
            })
            .collect();
        let (matched, places) =
            super::super::covering_nodes(&document, &declarations, request.position);
        let nodes = matched
            .iter()
            .zip(places)
            .map(|(node, place)| super::super::wire_node(file, node, place, &declarations))
            .collect();
        let source = matched
            .iter()
            .map(|node| super::super::excerpt(file, node.range))
            .collect();
        Ok(Ok(NodesResult {
            nodes,
            source,
            view: None,
            warnings: metadata.warnings,
        }))
    }
}
