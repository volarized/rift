//! Admitted dependency facts retained by the original query selection.

use std::collections::BTreeMap;
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};
use rift_protocol::read::{GetSymbolInclude, SourceUnitId, SymbolId};
use rift_protocol::symbol_read::{
    CapturedView, GetSymbolParams, GetSymbolResult, SymbolUnavailableReason,
};
use rift_search::query::QueryFacts;

use super::{
    CapturedMetadata, LocalSymbolMaterial, LocalSymbolViewInput, invalid, material_error,
    unavailable,
};

impl LocalSymbolViewInput {
    /// Retains admitted dependency declarations and exact source bytes with this selection.
    ///
    /// # Errors
    /// Refuses local identities, mismatched source ownership or invalid declaration ranges.
    pub fn with_package_facts(self, facts: QueryFacts) -> Result<Self, RiftError> {
        validate(&facts)?;
        if facts.symbols.is_empty()
            && facts.sources.is_empty()
            && facts.relationships.is_empty()
            && facts.relationship_records.is_empty()
            && facts.occurrences.is_empty()
        {
            return Ok(self);
        }
        let mut combined = self
            .material
            .package_facts
            .as_deref()
            .cloned()
            .unwrap_or_default();
        crate::engine_read::merge_package_facts(&mut combined, facts)?;
        Ok(self.with_package_facts_arc(Some(Arc::new(combined))))
    }

    /// Dependency facts captured by this immutable selection.
    #[must_use]
    pub fn package_facts(&self) -> Option<Arc<QueryFacts>> {
        self.material.package_facts.clone()
    }

    pub(super) fn with_package_facts_arc(mut self, facts: Option<Arc<QueryFacts>>) -> Self {
        let previous = self
            .material
            .package_facts
            .as_deref()
            .map_or(0, retained_bytes);
        let added = facts.as_deref().map_or(0, retained_bytes);
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(previous)
            .saturating_add(added);
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            metadata: Arc::clone(&self.material.metadata),
            history: self.material.history.clone(),
            traversal: self.material.traversal.clone(),
            query_result: self.material.query_result.clone(),
            package_facts: facts,
            provider_selection: self.material.provider_selection.clone(),
        });
        self
    }
}

fn retained_bytes(facts: &QueryFacts) -> usize {
    crate::engine_read::retained_package_bytes(facts)
        .saturating_add(std::mem::size_of::<QueryFacts>())
        .saturating_add(2 * std::mem::size_of::<usize>())
}

fn validate(facts: &QueryFacts) -> Result<(), RiftError> {
    facts.validate_providers()?;
    if !facts.nodes.is_empty()
        || !facts.histories.is_empty()
        || !facts.documentation_sources.is_empty()
        || !facts.documentation_blocks.is_empty()
        || !facts.documentation_links.is_empty()
        || !facts.documentation_references.is_empty()
        || !facts.documentation_reference_candidates.is_empty()
        || !facts.unavailable_families.is_empty()
    {
        return Err(invalid(
            "query",
            "dependency facts contain an uncaptured domain family",
        ));
    }
    let mut sources = BTreeMap::new();
    for source in &facts.sources {
        let (owner, _) = parse_source_unit_identity(source.unit.as_str())?;
        if !owner.as_ref().is_some_and(released_owner) {
            return Err(invalid(
                "unit",
                "dependency facts require an exact package or runtime owner",
            ));
        }
        if sources
            .insert(&source.unit, source)
            .is_some_and(|previous: &rift_search::query::QuerySource| previous.text != source.text)
        {
            return Err(invalid(
                "unit",
                "dependency source bytes disagree within the selection",
            ));
        }
    }
    for (symbol_index, symbol) in facts.symbols.iter().enumerate() {
        if let Some(id) = &symbol.symbol.id {
            let identity = SymbolIdentity::parse(id.as_str())?;
            let synthetic = symbol.declarations.is_empty()
                && symbol.symbol.origin.source_kind == rift_protocol::read::SourceKind::Synthetic
                && symbol.symbol.origin.location.is_none()
                && symbol.symbol.origin.package.is_none()
                && symbol.symbol.origin.runtime.is_none();
            if !released_owner(identity.owner())
                || identity.language() != &symbol.symbol.language
                || (!synthetic && &origin_owner(&symbol.symbol.origin)? != identity.owner())
            {
                return Err(invalid(
                    "id",
                    "dependency symbol ownership or language differs from its identity",
                ));
            }
        } else {
            let owner = origin_owner(&symbol.symbol.origin)?;
            let captured = facts.providers.iter().any(|snapshot| {
                snapshot.context.owner == owner
                    && snapshot.records.iter().any(|record| {
                        record.symbol_index == symbol_index && !record.declarations.is_empty()
                    })
            });
            if !captured {
                return Err(invalid(
                    "query",
                    "dependency declaration has no captured provider association",
                ));
            }
        }
        for declaration in &symbol.declarations {
            if !declaration.is_valid_for(&symbol.symbol) {
                return Err(invalid(
                    "query",
                    "dependency declaration associations are invalid",
                ));
            }
            let unit = declaration.unit.as_ref().ok_or_else(|| {
                invalid(
                    "unit",
                    "dependency declarations require their original source unit",
                )
            })?;
            let (owner, _) = parse_source_unit_identity(unit.as_str())?;
            if owner.as_ref() != Some(&origin_owner(&declaration.origin)?)
                || declaration.path.is_some()
            {
                return Err(invalid(
                    "unit",
                    "dependency declarations require an exact physical source owner",
                ));
            }
            if let Some(text) = sources.get(unit).and_then(|source| source.text.as_deref()) {
                declaration_text(text, &declaration.range)?;
            }
        }
    }
    Ok(())
}

fn released_owner(owner: &SymbolOwner) -> bool {
    matches!(
        owner,
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    )
}

fn origin_owner(origin: &rift_protocol::read::SymbolOrigin) -> Result<SymbolOwner, RiftError> {
    if origin.source_kind != rift_protocol::read::SourceKind::Synthetic {
        match origin.location {
            Some(rift_protocol::read::SourceLocationKind::Dependency)
                if origin.runtime.is_none() =>
            {
                return origin
                    .package
                    .as_ref()
                    .ok_or_else(|| {
                        invalid(
                            "origin",
                            "dependency origin requires its complete package owner",
                        )
                    })?
                    .owner();
            }
            Some(rift_protocol::read::SourceLocationKind::Stdlib) if origin.package.is_none() => {
                return origin
                    .runtime
                    .as_ref()
                    .ok_or_else(|| invalid("origin", "runtime origin requires its exact version"))?
                    .owner();
            }
            _ => {}
        }
    }
    Err(invalid(
        "origin",
        "dependency facts require an exact package or runtime origin",
    ))
}

fn declaration_text<'text>(
    text: &'text str,
    range: &rift_protocol::read::TextRange,
) -> Result<&'text str, RiftError> {
    let start = usize::try_from(range.start)
        .map_err(|error| material_error("dependency declaration byte range", &error))?;
    let end = usize::try_from(range.end)
        .map_err(|error| material_error("dependency declaration byte range", &error))?;
    text.get(start..end).ok_or_else(|| {
        invalid(
            "range",
            "dependency declaration range must contain complete UTF-8 bytes",
        )
    })
}

impl LocalSymbolMaterial {
    pub(super) fn admits_package_symbol(&self, id: &SymbolId) -> bool {
        self.package_facts.as_ref().is_some_and(|facts| {
            facts
                .symbols
                .iter()
                .any(|symbol| symbol.symbol.id.as_ref() == Some(id))
        })
    }

    pub(super) fn package_source_text(&self, unit: &SourceUnitId) -> Option<Option<&str>> {
        self.package_facts
            .as_ref()?
            .sources
            .iter()
            .find(|source| source.unit == *unit)
            .map(|source| source.text.as_deref())
    }

    pub(super) fn package_symbol(
        &self,
        request: &GetSymbolParams,
        view: &CapturedView,
        offset: usize,
    ) -> Result<(GetSymbolResult, Option<usize>), RiftError> {
        let Some(facts) = self.package_facts.as_ref() else {
            return Ok((
                unavailable(
                    request,
                    Some(view.clone()),
                    SymbolUnavailableReason::ViewContext,
                ),
                None,
            ));
        };
        let Some(selected) = facts
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.id.as_ref() == Some(&request.id))
        else {
            return Ok((
                unavailable(
                    request,
                    Some(view.clone()),
                    SymbolUnavailableReason::ViewContext,
                ),
                None,
            ));
        };
        if request.include.iter().any(|include| {
            matches!(
                include,
                GetSymbolInclude::History | GetSymbolInclude::Documentation
            )
        }) {
            return Ok((
                unavailable(
                    request,
                    Some(view.clone()),
                    SymbolUnavailableReason::InsufficientCoverage,
                ),
                None,
            ));
        }
        let limit = super::super::accepted_limit(request.declaration_limit)?;
        let mut declarations = selected
            .declarations
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        for declaration in &mut declarations {
            if request.include.contains(&GetSymbolInclude::Source) {
                let text = declaration
                    .unit
                    .as_ref()
                    .and_then(|unit| self.package_source_text(unit))
                    .flatten();
                let Some(text) = text else {
                    return Ok((
                        unavailable(
                            request,
                            Some(view.clone()),
                            SymbolUnavailableReason::InsufficientCoverage,
                        ),
                        None,
                    ));
                };
                declaration.source = Some(declaration_text(text, &declaration.range)?.to_owned());
                declaration.source_complete = true;
            } else {
                declaration.source = None;
                declaration.source_complete = false;
            }
        }
        let next = offset.saturating_add(declarations.len());
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        Ok((
            GetSymbolResult::Found {
                symbol: Box::new(selected.symbol.clone()),
                view: view.clone(),
                declarations,
                declaration_cursor: None,
                history: None,
                documentation: None,
                warnings: metadata.warnings,
            },
            (next < selected.declarations.len()).then_some(next),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::LocalSymbolViews;
    use super::*;
    use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
    use rift_protocol::symbol_read::CapturedViewId;
    use rift_search::query::{QuerySource, QuerySymbol};
    use std::time::SystemTime;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn facts(input: &LocalSymbolViewInput, original: &SymbolId) -> Result<QueryFacts, RiftError> {
        let key = rift_core::SymbolId::new(original.as_str())?;
        let mut symbol = input
            .material
            .index
            .object_by_identity(&key)?
            .expect("fixture object")
            .to_protocol_symbol();
        let package = rift_core::PackageIdentity {
            manager: "npm".to_owned(),
            registry: "npmjs.org".to_owned(),
            name: "beacon".to_owned(),
            version: "1.0.0".to_owned(),
        };
        let origin = rift_core::SourceLocation::Dependency {
            package: package.clone(),
        };
        let identity = SymbolIdentity::new(
            package.owner()?,
            symbol.language.clone(),
            vec!["beacon".to_owned(), "open".to_owned()],
        )?;
        symbol.id = Some(SymbolId::parse(&identity.wire_identity())?);
        symbol.origin = serde_json::from_value(
            serde_json::json!({"location":"dependency","source_kind":"authored","package":package}),
        )
        .expect("fixture origin");
        let (mut declarations, _) = input
            .material
            .index
            .declarations_by_identity_page(&key, 0, 1_000)?;
        let mut sources = BTreeMap::new();
        for declaration in &mut declarations {
            let path = declaration.path.take().expect("fixture local declaration");
            let path = rift_core::ProjectPath::new(path.0)?;
            let text = input
                .material
                .index
                .source(&path)
                .expect("captured fixture bytes");
            let unit =
                SourceUnitId(rift_core::SourceUnitId::for_origin(&origin, &path)?.to_string());
            sources.insert(unit.clone(), text.to_owned());
            declaration.unit = Some(unit);
            declaration.origin = symbol.origin.clone();
            declaration.source = Some(declaration_text(text, &declaration.range)?.to_owned());
            declaration.source_complete = true;
            declaration.node = None;
        }
        Ok(QueryFacts {
            symbols: vec![QuerySymbol {
                symbol,
                declarations,
            }],
            sources: sources
                .into_iter()
                .map(|(unit, text)| QuerySource {
                    unit,
                    digest: None,
                    text: Some(text),
                })
                .collect(),
            ..QueryFacts::default()
        })
    }

    fn request(id: SymbolId, view: CapturedViewId) -> GetSymbolParams {
        GetSymbolParams {
            id,
            view: Some(view),
            include: vec![GetSymbolInclude::Source],
            rev: None,
            declaration_cursor: None,
            declaration_limit: 1,
        }
    }

    #[test]
    fn retained_package_reads_keep_original_bytes_bindings_and_expiry() -> TestResult {
        let (directory, reads, local) = super::super::tests::fixture(true);
        let input = reads.local_search_view_input(true)?;
        let facts = facts(&input, &local)?;
        let id = facts.symbols[0]
            .symbol
            .id
            .clone()
            .expect("package identity");
        let unit = facts.sources[0].unit.clone();
        let original = facts.sources[0].text.clone().expect("package bytes");
        let input = input
            .with_package_facts(facts)?
            .with_query_result(vec![1, 2, 3]);
        let input = input.with_sources(&reads.index)?;
        let store = LocalSymbolViews::new(2 << 20, [31; 32]);
        let now = SystemTime::now();
        let view = store
            .capture_at(input, now)?
            .expect("admitted package material");
        let mut params = request(id.clone(), view.id.clone());
        let first = store.resolve_at(&params, reads.index.root(), reads.index.owner(), now)?;
        assert!(first.is_valid_for(&id));
        let GetSymbolResult::Found {
            declaration_cursor: Some(cursor),
            view: answered,
            ..
        } = &first
        else {
            panic!("package bindings require continuation");
        };
        assert_eq!(answered, &view);
        params.declaration_cursor = Some(cursor.clone());
        assert!(
            store
                .resolve_at(&params, reads.index.root(), reads.index.owner(), now)?
                .is_valid_for(&id)
        );
        std::fs::remove_file(directory.path().join("index.js"))?;
        std::fs::remove_file(directory.path().join("index.d.ts"))?;
        let params = request(id.clone(), view.id.clone());
        let mut repeated =
            store.resolve_at(&params, reads.index.root(), reads.index.owner(), now)?;
        let mut expected = first.clone();
        for result in [&mut repeated, &mut expected] {
            if let GetSymbolResult::Found {
                declaration_cursor, ..
            } = result
            {
                assert!(declaration_cursor.is_some());
                *declaration_cursor = None;
            }
        }
        assert_eq!(repeated, expected);
        let source = GetSourceParams {
            unit,
            range: None,
            rev: None,
            view: Some(view.id.clone()),
        };
        let GetSourceResult::Found {
            source,
            view: source_view,
            ..
        } = store.get_source(&source, reads.index.root(), reads.index.owner())?
        else {
            panic!("captured package bytes");
        };
        assert_eq!(source.text, original);
        assert_eq!(source_view, view);
        let selected = store
            .selected_input(&view.id, reads.index.root(), reads.index.owner())
            .expect("retained input");
        let derived = store
            .capture_at(selected, now + std::time::Duration::from_secs(10))?
            .expect("derived retained input");
        assert_eq!(derived.expires_at, view.expires_at);
        let mut foreign = params.clone();
        foreign.id = SymbolId::parse(&id.0.replace("@1.0.0", "@2.0.0"))?;
        assert!(matches!(
            store.resolve_at(&foreign, reads.index.root(), reads.index.owner(), now)?,
            GetSymbolResult::Unavailable {
                reason: SymbolUnavailableReason::ViewContext,
                ..
            }
        ));
        assert!(matches!(
            store.resolve_at(
                &params,
                reads.index.root(),
                reads.index.owner(),
                now + super::super::VIEW_LIFETIME
            )?,
            GetSymbolResult::Unavailable {
                reason: SymbolUnavailableReason::ViewExpired,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn package_material_refuses_owner_range_and_byte_conflicts() -> TestResult {
        let (_directory, reads, local) = super::super::tests::fixture(true);
        let input = reads.local_search_view_input(true)?;
        let original = facts(&input, &local)?;
        let mut wrong = original.clone();
        wrong.symbols[0]
            .symbol
            .origin
            .package
            .as_mut()
            .expect("package owner")
            .version = "2.0.0".to_owned();
        assert!(
            reads
                .local_search_view_input(true)?
                .with_package_facts(wrong)
                .is_err()
        );
        let mut wrong = original.clone();
        wrong.symbols[0].declarations[0].range.end = u64::MAX;
        assert!(
            reads
                .local_search_view_input(true)?
                .with_package_facts(wrong)
                .is_err()
        );
        let input = input.with_package_facts(original.clone())?;
        let mut changed = original;
        changed.sources[0]
            .text
            .as_mut()
            .expect("selected source")
            .push_str("// changed");
        assert!(input.with_package_facts(changed).is_err());
        Ok(())
    }
    #[test]
    fn retained_source_less_synthetic_symbol_preserves_absent_origin() -> TestResult {
        let (_directory, reads, local) = super::super::tests::fixture(true);
        let input = reads.local_search_view_input(true)?;
        let mut captured = facts(&input, &local)?;
        captured.sources.clear();
        captured.symbols[0].declarations.clear();
        captured.symbols[0].symbol.origin =
            serde_json::from_value(serde_json::json!({"source_kind":"synthetic"}))?;
        let id = captured.symbols[0]
            .symbol
            .id
            .clone()
            .expect("canonical package identity");
        let original = captured.symbols[0].symbol.clone();
        validate(&captured)?;
        let mut invalid_origin = captured.clone();
        invalid_origin.symbols[0].symbol.origin.source_kind =
            rift_protocol::read::SourceKind::Authored;
        assert!(validate(&invalid_origin).is_err());
        let input = input.with_package_facts(captured)?;
        let store = LocalSymbolViews::new(2 << 20, [41; 32]);
        let now = SystemTime::now();
        let view = store
            .capture_at(input, now)?
            .expect("retained synthetic symbol");
        let answer = store.resolve_at(
            &request(id.clone(), view.id.clone()),
            reads.index.root(),
            reads.index.owner(),
            now,
        )?;
        assert!(answer.is_valid_for(&id));
        let GetSymbolResult::Found {
            symbol,
            declarations,
            view: answered,
            ..
        } = answer
        else {
            panic!("captured synthetic symbol");
        };
        assert_eq!(*symbol, original);
        assert!(declarations.is_empty());
        assert_eq!(answered, view);
        Ok(())
    }
}
