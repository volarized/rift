use std::path::Path;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Language, NodesParams, NodesResult};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

use super::super::nodes::{
    NodesPage, NodesSelector, PageBounds, UnitSource, position_page, symbol_page, unit_page,
};
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
        if request.revision.is_some() {
            return Err(invalid(
                "revision",
                "revision cannot accompany a captured view",
            ));
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
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let view = request.view.as_ref().map_or("", CapturedViewId::as_str);
        let bounds = PageBounds::served(self.index.results_max());
        let index = self.index.as_ref();
        let answer = match NodesSelector::of(request, view)? {
            NodesSelector::Position {
                path,
                position,
                cursor,
            } => self.position_nodes(&path, position, cursor, view)?,
            NodesSelector::Symbol { id, key, offset } => {
                symbol_page(index, (id, &key), (offset, view), bounds)?
                    .ok_or(SymbolUnavailableReason::InsufficientCoverage)
            }
        };
        Ok(answer.map(|page| NodesResult {
            symbol: page.symbol,
            nodes: page.nodes,
            node_cursor: page.node_cursor,
            revision: metadata.revision,
            view: None,
            warnings: metadata.warnings,
        }))
    }

    /// The captured nodes of the declaration or unit holding `position`.
    fn position_nodes(
        &self,
        path: &rift_core::ProjectPath,
        position: u64,
        cursor: Option<&str>,
        view: &str,
    ) -> Result<Result<NodesPage, SymbolUnavailableReason>, RiftError> {
        let bounds = PageBounds::served(self.index.results_max());
        if let Some(file) = self.index.file(path) {
            inside(file.source(), position)?;
            return Ok(position_page(
                self.index.as_ref(),
                (path, position),
                (cursor, view),
                bounds,
            )?
            .ok_or(SymbolUnavailableReason::InsufficientCoverage));
        }
        let Some(text) = self.index.source(path) else {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        };
        inside(text, position)?;
        let unit = super::super::file_id(path);
        Ok(Ok(unit_page(
            UnitSource {
                unit: &unit,
                language: &Language::text(),
                source: text,
            },
            None,
            cursor,
            view,
            bounds,
        )?))
    }
}

/// Refuses a `position` at or past the captured source's byte length.
fn inside(source: &str, position: u64) -> Result<(), RiftError> {
    if position >= u64::try_from(source.len()).unwrap_or(u64::MAX) {
        return Err(invalid(
            "position",
            "position must be inside captured source",
        ));
    }
    Ok(())
}
