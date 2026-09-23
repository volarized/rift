//! Bounded exact-symbol documentation context, loaded after symbol pagination.

use rift_protocol::documentation::{
    DOCUMENTATION_EXCERPT_BYTES_MAX, DOCUMENTATION_SYMBOL_REFERENCES_MAX,
    DocumentationContentIdentity, DocumentationContext, DocumentationHit,
    DocumentationReferenceHit, DocumentationStage, DocumentationWarning, DocumentationWarningKind,
};
use rift_protocol::read::SymbolId;

use super::DocumentationCollection;

/// Reads documentation by exact declaration identity from the published reverse index.
///
/// At most 32 references and 16 KiB of total excerpt bytes are returned. Missing or cut
/// captured content leaves metadata intact and emits one typed warning per source.
/// The callback reads already-captured content; this function performs no source acquisition.
#[must_use]
pub fn documentation_context<'content>(
    collection: &DocumentationCollection,
    symbol: &SymbolId,
    content: impl FnMut(&DocumentationContentIdentity) -> Option<&'content str>,
) -> DocumentationContext {
    documentation_context_with_budget(
        collection,
        symbol,
        content,
        &mut (DOCUMENTATION_EXCERPT_BYTES_MAX as usize),
    )
}

/// Reads exact-symbol references using the caller's remaining response excerpt budget.
///
/// Each context also enforces the per-symbol excerpt and reference bounds. The caller
/// shares `bytes_left` across paginated hits and carries the returned typed warnings.
#[must_use]
pub fn documentation_context_with_budget<'content>(
    collection: &DocumentationCollection,
    symbol: &SymbolId,
    mut content: impl FnMut(&DocumentationContentIdentity) -> Option<&'content str>,
    bytes_left: &mut usize,
) -> DocumentationContext {
    let mut result = DocumentationContext {
        documentation_revision: collection.index().documentation_revision.clone(),
        references: Vec::new(),
        truncated: false,
        warnings: Vec::new(),
    };
    let mut context_bytes_left = (*bytes_left).min(DOCUMENTATION_EXCERPT_BYTES_MAX as usize);
    let initial_bytes = context_bytes_left;
    for reference in collection.references_to(symbol) {
        if result.references.len() == DOCUMENTATION_SYMBOL_REFERENCES_MAX as usize {
            result.truncated = true;
            if let Some(block) = collection.block(&reference.block) {
                warn(
                    &mut result,
                    &block.source,
                    DocumentationWarningKind::LimitExceeded,
                );
            }
            break;
        }
        let Some(block) = collection.block(&reference.block) else {
            continue;
        };
        let Some(source) = collection.source(&block.source) else {
            continue;
        };
        let excerpt = read_excerpt(
            content(&block.source),
            block,
            &mut context_bytes_left,
            &mut result,
        );
        result.references.push(DocumentationReferenceHit {
            reference: reference.clone(),
            documentation: DocumentationHit {
                block: block.clone(),
                source: source.clone(),
                documentation_revision: result.documentation_revision.clone(),
            },
            excerpt,
        });
    }
    *bytes_left -= initial_bytes - context_bytes_left;
    result
}

fn read_excerpt(
    content: Option<&str>,
    block: &rift_protocol::documentation::DocumentationBlock,
    bytes_left: &mut usize,
    result: &mut DocumentationContext,
) -> Option<String> {
    let Some(content) = content else {
        warn(
            result,
            &block.source,
            DocumentationWarningKind::SourceUnavailable,
        );
        return None;
    };
    let Ok(exact) = super::input::slice(content, &block.range) else {
        warn(
            result,
            &block.source,
            DocumentationWarningKind::SourceTruncated,
        );
        return None;
    };
    if exact.len() <= *bytes_left {
        *bytes_left -= exact.len();
        return Some(exact.to_owned());
    }
    result.truncated = true;
    warn(
        result,
        &block.source,
        DocumentationWarningKind::LimitExceeded,
    );
    let mut end = *bytes_left;
    while end > 0 && !exact.is_char_boundary(end) {
        end -= 1;
    }
    *bytes_left -= end;
    (end > 0).then(|| exact[..end].to_owned())
}

fn warn(
    result: &mut DocumentationContext,
    source: &DocumentationContentIdentity,
    kind: DocumentationWarningKind,
) {
    if let Some(warning) = result
        .warnings
        .iter_mut()
        .find(|warning| warning.source == *source && warning.kind == kind)
    {
        warning.count += 1;
        return;
    }
    result.warnings.push(DocumentationWarning {
        source: source.clone(),
        stage: DocumentationStage::Index,
        kind,
        count: 1,
    });
}
