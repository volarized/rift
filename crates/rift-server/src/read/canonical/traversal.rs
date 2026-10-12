use super::{
    CapturedMetadata, CapturedView, CapturedViewId, HeldView, LocalSymbolMaterial,
    LocalSymbolViews, SymbolUnavailableReason, invalid, material_error,
};
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::ReadWarning;
use rift_protocol::search::{MatchedField, SearchHit, SearchHitTarget, SearchTraversal};
use std::path::Path;
use std::time::SystemTime;

type CapturedTraversal =
    Result<(rift_index::TraversalWalk, CapturedView, Option<ReadWarning>), SymbolUnavailableReason>;

type CapturedTraversalHits =
    Result<(Vec<SearchHit>, CapturedView, Vec<ReadWarning>), SymbolUnavailableReason>;

impl LocalSymbolViews {
    /// Copies documentation excerpts from the original captured view for one returned page.
    ///
    /// # Errors
    ///
    /// Returns invalid view spelling; expiry and context remain unavailable outcomes.
    pub fn populate_sources(
        &self,
        results: &mut [SearchHit],
        view: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<Result<Vec<ReadWarning>, SymbolUnavailableReason>, RiftError> {
        let selected = CapturedViewId::parse(view.as_str())?;
        let held = match self.held_view_at(&selected, root, owner, SystemTime::now()) {
            Ok(held) => held,
            Err(reason) => return Ok(Err(reason)),
        };
        if results
            .iter()
            .any(|hit| hit.view.as_ref().is_some_and(|view| view != &held.view))
        {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        Ok(Ok(crate::search::populate_sources_from(
            results,
            |identity| held.material.index.documentation_content(identity),
        )))
    }
    /// Walks the retained graph with the engine rows captured for this exact request.
    /// Missing rows or changed request context remain an unavailable outcome.
    ///
    /// # Errors
    /// Returns registered request or captured metadata failures.
    pub fn traversal(
        &self,
        request: &SearchTraversal,
        view: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<CapturedTraversal, RiftError> {
        let (held, walk) = match self.held_traversal(request, view, root, owner)? {
            Ok(captured) => captured,
            Err(reason) => return Ok(Err(reason)),
        };
        let warning = held
            .material
            .traversal
            .as_ref()
            .and_then(|(_, references)| references.analysis_unavailable().cloned());
        Ok(Ok((walk, held.view, warning)))
    }

    /// Projects reached objects and their first source binding from the retained publication.
    /// The returned view keeps its original expiry and workspace context.
    ///
    /// # Errors
    /// Returns registered request, source binding or captured metadata failures.
    pub fn traversal_hits(
        &self,
        request: &SearchTraversal,
        view: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
        include_source: bool,
    ) -> Result<CapturedTraversalHits, RiftError> {
        let (held, walk) = match self.held_traversal(request, view, root, owner)? {
            Ok(captured) => captured,
            Err(reason) => return Ok(Err(reason)),
        };
        let metadata: CapturedMetadata = serde_json::from_slice(&held.material.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let mut warnings = metadata.warnings;
        if let Some((_, references)) = &held.material.traversal {
            warnings.extend(references.warnings());
        }
        warnings.extend(crate::traversal::relationship_coverage_missing(request));
        if walk.truncated {
            warnings.push(crate::traversal::traversal_truncation_warning());
        }
        let mut hits = Vec::with_capacity(walk.discovered.len());
        for (identity, path) in walk.discovered {
            let Some(hit) = held.material.traversal_hit(
                &identity,
                path,
                &held.view,
                include_source,
                &mut warnings,
            )?
            else {
                return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
            };
            hits.push(hit);
        }
        Ok(Ok((hits, held.view, warnings)))
    }

    fn held_traversal(
        &self,
        request: &SearchTraversal,
        view: &CapturedViewId,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<Result<(HeldView, rift_index::TraversalWalk), SymbolUnavailableReason>, RiftError>
    {
        crate::traversal::validate_traversal(request)?;
        let seed = request
            .seed
            .as_ref()
            .ok_or_else(|| invalid("seed", "a traversal starts at seed"))?;
        let identity = SymbolIdentity::parse(seed.as_str())?;
        if identity.owner() != owner {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        let held = match self.held_view_at(view, root, owner, SystemTime::now()) {
            Ok(held) => held,
            Err(reason) => return Ok(Err(reason)),
        };
        let Some((captured, references)) = &held.material.traversal else {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        };
        let encoded = serde_json::to_vec(request)
            .map_err(|error| material_error("read traversal", &error))?;
        if captured.as_ref() != encoded.as_slice() {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        let seed = rift_core::SymbolId::new(seed.0.clone())?;
        if held.material.index.object_by_identity(&seed)?.is_none() {
            return Ok(Err(SymbolUnavailableReason::InsufficientCoverage));
        }
        let walk = crate::traversal::walk_traversal_with_references(
            held.material.index.relationships(),
            &seed,
            request,
            crate::traversal::TRAVERSAL_NODES_MAX,
            references,
        );
        Ok(Ok((held, walk)))
    }
}

impl LocalSymbolMaterial {
    fn traversal_hit(
        &self,
        identity: &rift_core::SymbolId,
        path: Vec<rift_protocol::search::GraphHop>,
        view: &CapturedView,
        include_source: bool,
        warnings: &mut Vec<ReadWarning>,
    ) -> Result<Option<SearchHit>, RiftError> {
        let Some(readable) = self.index.object_by_identity(identity)? else {
            return Ok(self
                .traversal
                .as_ref()
                .and_then(|(_, references)| references.package_declaration(identity))
                .map(|declaration| {
                    let mut hit = crate::traversal::package_traversal_hit(declaration, path);
                    hit.view = Some(view.clone());
                    hit
                }));
        };
        let symbol = readable.to_protocol_symbol();
        if !symbol.name_is_public() {
            return Ok(None);
        }
        let (declarations, _) = self.index.declarations_by_identity_page(identity, 0, 1)?;
        let declaration = declarations.into_iter().next();
        let source = if include_source {
            let (bindings, _) = self.index.symbols_by_identity_page(identity, 0, 1)?;
            bindings
                .first()
                .map(|matched| super::super::excerpt(matched.file, matched.symbol.range))
        } else {
            None
        };
        warnings.extend(super::super::symbol_disagreement_warning(
            readable.assembled(),
        ));
        let distance = u64::try_from(path.len()).unwrap_or(u64::MAX);
        Ok(Some(SearchHit {
            view: Some(view.clone()),
            hit: SearchHitTarget::Symbol {
                symbol: Box::new(symbol),
            },
            score: Some(crate::traversal::distance_score(distance)),
            matched_by: vec![MatchedField::Relationship],
            source,
            range: declaration.as_ref().map(|binding| binding.range.clone()),
            line: declaration.as_ref().map(|binding| binding.line),
            path: declaration
                .as_ref()
                .and_then(|binding| binding.path.clone()),
            unit: declaration.and_then(|binding| binding.unit),
            traversal_path: Some(path),
            distance: Some(distance),
            change: None,
        }))
    }
}
