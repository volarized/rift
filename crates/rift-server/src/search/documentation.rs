//! Documentation projection over the request's captured project and package sources.

use rift_index::{DocumentationProjection, DocumentationProjectionTarget};
use rift_protocol::documentation::{
    DocumentationContentIdentity, DocumentationSource, DocumentationSourceIdentity,
    DocumentationStage, DocumentationWarning, DocumentationWarningKind,
};
use rift_protocol::read::TextRange;

use super::{
    DocumentIdentity, FusedCandidate, ParsedQuery, Path, PathMatcher, ProjectPath, RankingInput,
    ReadError, ReadFault, ReadWarning, Resolution, ResolvedCandidate, SearchHit, SearchHitTarget,
    SearchParamsTarget, SearchScope, WorkspaceIndex, includes, matched_fields, query_line,
    resolve_candidate, text_range,
};

/// Metadata held for one search, without another copy of source content.
pub(super) struct SearchDocumentation<'a> {
    projection: DocumentationProjection<'a>,
    index: &'a WorkspaceIndex,
    resolution: Resolution<'a>,
}

impl<'a> SearchDocumentation<'a> {
    pub(super) fn new(
        index: &'a WorkspaceIndex,
        scope: SearchScope,
        resolution: Resolution<'a>,
    ) -> Result<Self, ReadError> {
        let mut projection = DocumentationProjection::default();
        if scope != SearchScope::Global {
            projection
                .extend_collection(index.documentation())
                .map_err(ReadFault::documentation)?;
        }
        if let Some(extra) = resolution.force_include {
            projection
                .extend_collection(extra.documentation())
                .map_err(ReadFault::documentation)?;
        }
        if let Some(packages) = resolution.packages {
            for package in packages.packages() {
                projection
                    .extend_collection(package.documentation())
                    .map_err(ReadFault::documentation)?;
            }
        }
        Ok(Self {
            projection,
            index,
            resolution,
        })
    }

    pub(super) fn admits(
        &self,
        identity: &DocumentIdentity,
        matcher: Option<&PathMatcher>,
        root: &Path,
    ) -> Option<bool> {
        let source = self.projection.document_source(identity)?;
        Some(match &source.source {
            DocumentationSourceIdentity::Package { .. } => true,
            DocumentationSourceIdentity::Project { path } => {
                let Ok(path) = ProjectPath::new(path.0.as_str()) else {
                    return Some(false);
                };
                includes(matcher, root, &path)
                    || self
                        .resolution
                        .force_include
                        .is_some_and(|extra| extra.documentation().source(source).is_some())
            }
        })
    }

    pub(super) fn project(
        &self,
        inputs: &[RankingInput],
        target: SearchParamsTarget,
        query: &ParsedQuery,
    ) -> Result<Vec<RankingInput>, ReadError> {
        let target = match target {
            SearchParamsTarget::Documentation => DocumentationProjectionTarget::Documentation,
            SearchParamsTarget::All => DocumentationProjectionTarget::All,
            _ => return Ok(inputs.to_vec()),
        };
        self.projection
            .project(inputs, target, |identity, source| {
                let range = self.document_range(identity)?;
                let facts = self.projection.source(source)?;
                let content = captured_content(self.index, self.resolution, source, facts)?;
                let start = usize::try_from(range.start).ok()?;
                let end = usize::try_from(range.end).ok()?;
                let (_, found, _) = query_line(content.get(start..end)?, query)?;
                Some(TextRange {
                    start: range.start.checked_add(found.start)?,
                    end: range.start.checked_add(found.end)?,
                })
            })
            .map_err(ReadFault::documentation)
    }

    fn document_range(&self, identity: &DocumentIdentity) -> Option<TextRange> {
        if let Some(range) = self.projection.document_range(identity) {
            return Some(range.clone());
        }
        let range = match resolve_candidate(self.index, self.resolution, identity)? {
            ResolvedCandidate::Declaration(_, found) => found.symbol.range,
            ResolvedCandidate::Package(found) => found.matched.symbol.range,
            ResolvedCandidate::SourceFile(_) | ResolvedCandidate::TextFile(_) => return None,
        };
        Some(text_range(range))
    }

    pub(super) fn hit(&self, candidate: &FusedCandidate) -> Option<SearchHit> {
        let documentation = self.projection.hit(candidate.identity())?;
        let (path, unit) = match &documentation.block.source.source {
            DocumentationSourceIdentity::Project { path } => (Some(path.clone()), None),
            DocumentationSourceIdentity::Package { unit } => (None, Some(unit.clone())),
        };
        Some(SearchHit {
            range: Some(documentation.block.range.clone()),
            line: Some(documentation.block.line),
            hit: SearchHitTarget::Documentation {
                documentation: Box::new(documentation),
            },
            score: Some(candidate.score()),
            matched_by: matched_fields(candidate),
            source: None,
            path,
            unit,
            traversal_path: None,
            distance: None,
            change: None,
        })
    }
}

/// Source lookup uses the already selected package identity and never acquires bytes.
fn captured_content<'a>(
    index: &'a WorkspaceIndex,
    resolution: Resolution<'a>,
    identity: &DocumentationContentIdentity,
    source: &DocumentationSource,
) -> Option<&'a str> {
    match &identity.source {
        DocumentationSourceIdentity::Project { .. } => index
            .documentation_content(identity)
            .or_else(|| resolution.force_include?.documentation_content(identity)),
        DocumentationSourceIdentity::Package { .. } => resolution
            .packages?
            .package(source.origin.package.as_ref()?)?
            .documentation_content(identity),
    }
}

/// Copies excerpts only for the returned page, within one response byte bound.
pub(super) fn populate_sources(
    results: &mut [SearchHit],
    index: &WorkspaceIndex,
    resolution: Resolution<'_>,
) -> Vec<ReadWarning> {
    let mut remaining = rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
    let mut warnings = Vec::new();
    for hit in results {
        let SearchHitTarget::Documentation { documentation } = &hit.hit else {
            continue;
        };
        let Some(content) = captured_content(
            index,
            resolution,
            &documentation.block.source,
            &documentation.source,
        ) else {
            warn(
                &mut warnings,
                &documentation.block.source,
                DocumentationWarningKind::SourceUnavailable,
            );
            continue;
        };
        let range = &documentation.block.range;
        let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end))
        else {
            continue;
        };
        let Some(exact) = content.get(start..end) else {
            warn(
                &mut warnings,
                &documentation.block.source,
                DocumentationWarningKind::SourceTruncated,
            );
            continue;
        };
        let mut end = exact.len().min(remaining);
        while !exact.is_char_boundary(end) {
            end -= 1;
        }
        if end != 0 {
            hit.source = Some(exact[..end].to_owned());
        }
        if end < exact.len() {
            warn(
                &mut warnings,
                &documentation.block.source,
                DocumentationWarningKind::LimitExceeded,
            );
        }
        remaining -= end;
    }
    warnings
}

fn warn(
    warnings: &mut Vec<ReadWarning>,
    source: &DocumentationContentIdentity,
    kind: DocumentationWarningKind,
) {
    if let Some(warning) = warnings.iter_mut().find_map(|warning| match warning {
        ReadWarning::Documentation { warning }
            if warning.source == *source && warning.kind == kind =>
        {
            Some(warning)
        }
        _ => None,
    }) {
        warning.count += 1;
    } else if warnings.len() < rift_protocol::documentation::DOCUMENTATION_WARNINGS_MAX as usize {
        warnings.push(ReadWarning::Documentation {
            warning: DocumentationWarning {
                source: source.clone(),
                stage: DocumentationStage::Index,
                kind,
                count: 1,
            },
        });
    }
}
