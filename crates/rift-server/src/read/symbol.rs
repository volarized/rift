//! The exact `get_symbol` read of one project symbol at the revision a service serves.

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{GetSymbolInclude, ReadWarning, SymbolHistory};
use rift_protocol::symbol_read::{
    GetSymbolParams, GetSymbolResult, SymbolAlternative, SymbolMatchContext, SymbolNotFound,
    SymbolUnavailableReason,
};

use rift_index::WorkspaceSymbolView;

use super::ReadService;

impl ReadService {
    /// Reads one project symbol by its canonical identity at the revision this service
    /// serves.
    ///
    /// An unready publication answers `index_preparing`. A `declaration_cursor` resumes a
    /// preceding read of the same symbol at the same revision; the service holds nothing
    /// between pages.
    ///
    /// # Errors
    ///
    /// Returns `read_revision_unserved` for a `revision` this service does not serve,
    /// `read_invalid` for a malformed identity, limit, or a cursor minted for another symbol
    /// or revision, and the index or history read failure.
    pub fn exact_symbol(
        &self,
        request: &GetSymbolParams,
        ready: bool,
    ) -> Result<GetSymbolResult, RiftError> {
        let identity = SymbolIdentity::parse(request.id.as_str())?;
        let limit = super::accepted_limit(request.declaration_limit)?.min(self.index.results_max());
        self.accept_revision(request.revision.as_ref())?;
        let revision = self.served_revision()?;
        let served = revision.to_string();
        let offset = request
            .declaration_cursor
            .as_deref()
            .map_or(Ok(0), |cursor| {
                super::cursor::offset(cursor, "declaration_cursor", request.id.as_str(), &served)
            })?;
        let warnings = self.warnings();
        if !ready {
            return Ok(unavailable(
                request,
                SymbolUnavailableReason::IndexPreparing,
                warnings,
            ));
        }
        let key = rift_core::SymbolId::new(request.id.as_str())?;
        let index = self.index.symbol_view()?;
        let Some(readable) = index.object_by_identity(&key)? else {
            if !index.identity_coverage_complete(identity.language()) {
                return Ok(unavailable(
                    request,
                    SymbolUnavailableReason::InsufficientCoverage,
                    warnings,
                ));
            }
            let (alternatives, detail) = symbol_alternatives(&index, &identity)?;
            return Ok(GetSymbolResult::Missing {
                symbol_not_found: SymbolNotFound {
                    id: request.id.clone(),
                    alternatives,
                    detail,
                },
                revision,
                warnings,
            });
        };
        let page = DeclarationPage {
            revision,
            served,
            offset,
            limit,
        };
        self.found_symbol(request, &index, &readable, &key, page, warnings)
    }

    /// The answer for a symbol the index holds: its facts, one page of its declarations, and
    /// the projections the request names.
    fn found_symbol(
        &self,
        request: &GetSymbolParams,
        index: &WorkspaceSymbolView,
        readable: &rift_index::ReadableSymbol,
        key: &rift_core::SymbolId,
        page: DeclarationPage,
        mut warnings: Vec<ReadWarning>,
    ) -> Result<GetSymbolResult, RiftError> {
        let symbol = readable.to_protocol_symbol();
        if !symbol.name_is_public() {
            let (bindings, _) = index.symbols_by_identity_page(key, 0, 1)?;
            warnings.push(bindings.first().copied().map_or_else(
                || super::object_unavailable_at(readable.facts().qualified_name(), None, None),
                super::object_unavailable,
            ));
            return Ok(unavailable(
                request,
                SymbolUnavailableReason::InsufficientCoverage,
                warnings,
            ));
        }
        let history = if request.include.contains(&GetSymbolInclude::History) {
            let Some(history) = self.exact_history(index, request, key)? else {
                return Ok(unavailable(
                    request,
                    SymbolUnavailableReason::InsufficientCoverage,
                    warnings,
                ));
            };
            Some(history)
        } else {
            None
        };
        let (declarations, total) =
            index.declarations_by_identity_page(key, page.offset, page.limit)?;
        let next = page.offset.saturating_add(declarations.len());
        let documentation = request
            .include
            .contains(&GetSymbolInclude::Documentation)
            .then(|| exact_documentation(index, request));
        warnings.extend(super::symbol_disagreement_warning(readable.assembled()));
        Ok(GetSymbolResult::Found {
            symbol: Box::new(symbol),
            declarations,
            declaration_cursor: (next < total)
                .then(|| super::cursor::mint(next, request.id.as_str(), &page.served)),
            revision: page.revision,
            history,
            documentation,
            warnings,
        })
    }

    /// The version-control timeline of the symbol `key` binds, absent when no declaration
    /// binds it.
    fn exact_history(
        &self,
        index: &WorkspaceSymbolView,
        request: &GetSymbolParams,
        key: &rift_core::SymbolId,
    ) -> Result<Option<SymbolHistory>, RiftError> {
        let (bindings, _) = index.symbols_by_identity_page(key, 0, self.index.results_max())?;
        if bindings.is_empty() {
            return Ok(None);
        }
        let mut timelines = self.symbol_timelines()?;
        timelines
            .symbol_timeline(&self.index, &bindings, request.id.clone())
            .map(Some)
    }
}

/// The bounded documentation that refers to the requested symbol.
fn exact_documentation(
    index: &WorkspaceSymbolView,
    request: &GetSymbolParams,
) -> rift_protocol::documentation::DocumentationContext {
    let mut bytes_left = rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
    rift_index::documentation_context_with_budget(
        index.documentation(),
        &request.id,
        |source| index.documentation_content(source),
        &mut bytes_left,
    )
}

/// Symbols structurally near a missing identity, or the work bound that stopped the
/// search for them.
fn symbol_alternatives(
    index: &WorkspaceSymbolView,
    identity: &SymbolIdentity,
) -> Result<(Vec<SymbolAlternative>, Option<String>), RiftError> {
    let keys = match crate::alternatives::captured_symbols(index, identity) {
        Ok(keys) => keys,
        Err(error)
            if error.slug() == errors::server::read_symbol_alternatives_unavailable::SLUG =>
        {
            return Ok((Vec::new(), Some(error.to_string())));
        }
        Err(error) => return Err(error),
    };
    let mut alternatives = Vec::with_capacity(keys.len());
    for key in keys {
        let key_value = rift_core::SymbolId::new(key.as_str())?;
        let Some(symbol) = index.object_by_identity(&key_value)? else {
            continue;
        };
        let name = symbol.facts().qualified_name();
        if !rift_protocol::read::symbol_name_is_public(name) {
            continue;
        }
        alternatives.push(SymbolAlternative {
            id: key,
            name: name.to_owned(),
            match_context: vec![
                SymbolMatchContext::Owner,
                SymbolMatchContext::Language,
                SymbolMatchContext::Name,
            ],
        });
    }
    Ok((alternatives, None))
}

/// Where one page of a symbol's declarations starts, how many it holds, and the revision the
/// read serves.
struct DeclarationPage {
    revision: rift_protocol::revision::Revision,
    served: String,
    offset: usize,
    limit: usize,
}

fn unavailable(
    request: &GetSymbolParams,
    reason: SymbolUnavailableReason,
    warnings: Vec<ReadWarning>,
) -> GetSymbolResult {
    GetSymbolResult::Unavailable {
        id: request.id.clone(),
        revision: None,
        reason,
        warnings,
    }
}

#[cfg(test)]
mod tests;
