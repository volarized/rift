//! The exact `get_symbol` read of one project symbol at the revision a service serves.

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{ReadWarning, SymbolHistory};
use rift_protocol::symbol_read::{
    GetSymbolParams, GetSymbolResult, SymbolAlternative, SymbolDeclaration, SymbolMatchContext,
    SymbolNotFound, SymbolUnavailableReason,
};

use rift_index::{WorkspaceIndex, WorkspaceSymbolView};

use super::ReadService;

impl ReadService {
    /// Reads one project symbol by its canonical identity at the revision this service
    /// serves.
    ///
    /// An unready publication answers `index_preparing`. The caller selects declarations with `page`
    /// and pins the revision for later pages.
    ///
    /// # Errors
    ///
    /// Returns `read_revision_unserved` for a `revision` this service does not serve,
    /// `read_invalid` for a malformed identity, page or `max_results`, and the index or history read failure.
    pub fn exact_symbol(
        &self,
        request: &GetSymbolParams,
        ready: bool,
    ) -> Result<GetSymbolResult, RiftError> {
        self.read_exact_symbol(request, ready, false)
    }

    /// Reads one symbol from the complete publication a request accepted as current.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [Self::exact_symbol].
    pub fn exact_current_symbol(
        &self,
        request: &GetSymbolParams,
    ) -> Result<GetSymbolResult, RiftError> {
        self.read_exact_symbol(request, true, self.revision().is_none())
    }

    fn read_exact_symbol(
        &self,
        request: &GetSymbolParams,
        ready: bool,
        verified_current: bool,
    ) -> Result<GetSymbolResult, RiftError> {
        let identity = SymbolIdentity::parse(request.id.as_str())?;
        let limit = super::accepted_max_results(
            request.max_results,
            rift_protocol::symbol_read::GET_SYMBOL_MAX_RESULTS,
        )?;
        self.accept_revision(request.revision.as_ref())?;
        let revision = self.served_revision()?;
        let pagination = rift_protocol::read::Pagination::new(request.page, request.max_results, 0);
        super::accept_page(request.page)?;
        let offset = pagination.offset();
        let warnings = self.warnings();
        if !ready {
            return Ok(unavailable(
                request,
                SymbolUnavailableReason::IndexPreparing,
                warnings,
            ));
        }
        let key = rift_core::SymbolId::new(request.id.as_str())?;
        let direct = (verified_current
            && request.revision.is_none()
            && request.include.is_empty()
            && offset == 0)
            .then(|| self.index.exact_relative_singleton(&key))
            .flatten();
        let (readable, direct_declaration) = match direct {
            Some((readable, declaration)) => {
                rift_tracing::debug!(
                    component = "index",
                    operation = "read.symbol.current_direct",
                    "read exact current symbol from its admitted file"
                );
                (Some(readable), Some(declaration))
            }
            None => (self.index.object_by_identity(&key)?, None),
        };
        let Some(readable) = readable else {
            let index = self.index.symbol_view()?;
            if !covers_exact_identity(&index, &identity) {
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
            page: request.page,
            offset,
            limit,
        };
        self.found_symbol(request, &readable, &key, page, warnings, direct_declaration)
    }

    /// The answer for a symbol the index holds: its facts, one page of its declarations, and
    /// the projections the request names.
    fn found_symbol(
        &self,
        request: &GetSymbolParams,
        readable: &rift_index::ReadableSymbol,
        key: &rift_core::SymbolId,
        page: DeclarationPage,
        mut warnings: Vec<ReadWarning>,
        direct_declaration: Option<SymbolDeclaration>,
    ) -> Result<GetSymbolResult, RiftError> {
        let symbol = readable.to_protocol_symbol();
        if !symbol.name_is_public() {
            let (bindings, _) = self.index.symbols_by_identity_page(key, 0, 1)?;
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
        let history = if request.include.history {
            let Some(history) = self.exact_history(request, key)? else {
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
        let (declarations, total) = match direct_declaration {
            Some(declaration) => (vec![declaration], 1),
            None => declarations_page(&self.index, key, page.offset, page.limit)?,
        };
        let documentation = request
            .include
            .documentation
            .then(|| exact_documentation(&self.index, request));
        warnings.extend(super::symbol_disagreement_warning(readable.assembled()));
        Ok(GetSymbolResult::Found {
            symbol: Box::new(symbol),
            declarations,
            pagination: rift_protocol::read::Pagination::new(
                page.page,
                page.limit as u64,
                total as u64,
            ),
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
        request: &GetSymbolParams,
        key: &rift_core::SymbolId,
    ) -> Result<Option<SymbolHistory>, RiftError> {
        let (bindings, _) =
            self.index
                .symbols_by_identity_page(key, 0, self.index.results_max())?;
        if bindings.is_empty() {
            return Ok(None);
        }
        let mut timelines = self.symbol_timelines()?;
        timelines
            .symbol_timeline(&self.index, &bindings, request.id.clone())
            .map(Some)
    }
}

fn covers_exact_identity(index: &WorkspaceSymbolView, identity: &SymbolIdentity) -> bool {
    if !index.identity_coverage_complete(identity.language()) {
        return false;
    }
    if identity.language().name != "javascript" || !identity.owner().is_local() {
        return true;
    }
    let Some(namespace) = identity.qualified_path().first() else {
        return false;
    };
    if namespace.starts_with("./") {
        return true;
    }
    // Standalone module anchors name their physical path. Only admitted package
    // namespace anchors can establish absence inside a bare JavaScript namespace.
    let Ok(namespace) = SymbolIdentity::new(
        identity.owner().clone(),
        identity.language().clone(),
        vec![namespace.clone()],
    ) else {
        return false;
    };
    let prefix = format!("{}/", namespace.wire_identity());
    index
        .objects()
        .any(|(candidate, _)| candidate.as_str().starts_with(&prefix))
}

/// The bounded documentation that refers to the requested symbol.
fn exact_documentation(
    index: &WorkspaceIndex,
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
    page: u64,
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

fn declarations_page(
    index: &WorkspaceIndex,
    key: &rift_core::SymbolId,
    offset: usize,
    max_results: usize,
) -> Result<(Vec<rift_protocol::symbol_read::SymbolDeclaration>, usize), RiftError> {
    let mut declarations = Vec::new();
    let mut total = 0;
    for start in (0..max_results).step_by(index.results_max()) {
        let count = (max_results - start).min(index.results_max());
        let (page, count_total) =
            index.declarations_by_identity_page(key, offset.saturating_add(start), count)?;
        total = count_total;
        let end = page.len() < count;
        declarations.extend(page);
        if end {
            break;
        }
    }
    Ok((declarations, total))
}
