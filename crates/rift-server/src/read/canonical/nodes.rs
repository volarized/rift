use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Extensions, Node, NodesParams, NodesResult, SymbolId};
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
        let Some(matched) = self.index.nodes(&path, request.position)? else {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        };
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let nodes = matched
            .iter()
            .map(|node| {
                let language = file.syntax().language();
                Node {
                    id: super::super::node_id(file, node.range),
                    symbol: self
                        .index
                        .symbol_identity(&path, node.range)
                        .map(|identity| SymbolId(identity.as_str().to_owned())),
                    unit: super::super::file_id(file.path()),
                    language: language.clone(),
                    kind: super::super::wire_kind(node.kind),
                    facets: super::super::language_provider(language).node_facets(node.kind),
                    range: super::super::text_range(node.range),
                    regions: Vec::new(),
                    parent: None,
                    extensions: Extensions(BTreeMap::new()),
                }
            })
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
