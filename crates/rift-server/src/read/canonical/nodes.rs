use std::path::Path;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{NodesParams, NodesResult, SymbolId};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

use super::super::nodes::{
    NodesPage, NodesSelector, PageBounds, declared_syntax, file_node, located_node, symbol_page,
    text_file_node,
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
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let view = request.view.as_ref().map_or("", CapturedViewId::as_str);
        let answer = match NodesSelector::of(request, view)? {
            NodesSelector::Position { path, position } => self.position_nodes(&path, position)?,
            NodesSelector::Symbol { id, key, offset } => {
                let results_max = self.index.results_max();
                let (declarations, _) =
                    self.index.symbols_by_identity_page(&key, 0, results_max)?;
                if declarations.is_empty() {
                    return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
                }
                Ok(symbol_page(
                    &declarations,
                    |path| declared_syntax(path, self.index.syntax(path)),
                    (id, view),
                    offset,
                    PageBounds::served(results_max),
                )?)
            }
        };
        Ok(answer.map(|page| NodesResult {
            nodes: page.nodes,
            node_cursor: page.node_cursor,
            view: None,
            warnings: metadata.warnings,
        }))
    }

    /// The captured nodes covering `position`, from the file node inward.
    fn position_nodes(
        &self,
        path: &rift_core::ProjectPath,
        position: u64,
    ) -> Result<Result<NodesPage, SymbolUnavailableReason>, RiftError> {
        let Some(file) = self.index.file(path) else {
            let Some(text) = self.index.source(path) else {
                return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
            };
            inside(text, position)?;
            return Ok(Ok(NodesPage {
                nodes: vec![text_file_node(path, text)],
                node_cursor: None,
            }));
        };
        inside(file.source(), position)?;
        let document = declared_syntax(path, self.index.syntax(path))?;
        let mut nodes = vec![file_node(file)];
        nodes.extend(document.nodes_at(position).into_iter().map(|node| {
            let of = self
                .index
                .symbol_identity(path, node.range)
                .map(|identity| SymbolId(identity.as_str().to_owned()));
            located_node(file, node.kind, node.range, of)
        }));
        Ok(Ok(NodesPage {
            nodes,
            node_cursor: None,
        }))
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
