//! Uses locally selected Rust expansions to establish the identities of written name tokens.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{FileDigest, ProjectPath, SourceRange, SourceUnitId};
use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::index::{PackagePublication, PackageSourceUnit, PackageSymbol};
use rift_protocol::producer::FactProducer;
use rift_protocol::read::{
    Digest, ExactKind, ExtensionKey, ExtensionValue, Extensions, SourceKind, Symbol, SymbolId,
    SymbolKind, TextRange,
};
use rift_syntax::{
    ByteRange, RustSyntaxProvider, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource,
    SyntaxSymbol,
};

use super::{PackageAnalyzer, bound, digest_of, file_digest, line_of, line_starts};
use crate::ExactPackageLimits;

/// A Rust macro invocation whose selected owner can be checked through a language server.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroSite {
    /// Exact original identifier of the macro to expand.
    pub name: SourceRange,
    /// Complete invocation in the original source.
    pub invocation: SourceRange,
}

/// One singleton selected Rust macro definition in an admitted package unit.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroDefinition<'source> {
    /// Path of the selected definition, which may differ from the invocation's path.
    pub path: &'source ProjectPath,
    /// Digest of the definition unit supplied to the language server.
    pub digest: FileDigest,
    /// Original name token reported by the server.
    pub range: SourceRange,
}

/// An emitted item shape and the independently confirmed original names it selected.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroItems<'source> {
    /// Exact original location of the invocation and its name.
    pub site: RustLspMacroSite,
    /// Original token named by the server's definition response at the macro invocation.
    pub definition: RustLspMacroDefinition<'source>,
    /// Parsed only to establish emitted kind and relative owner.
    pub expansion: &'source str,
    /// Unique same-source definitions that point to their exact original input token.
    pub names: &'source [SourceRange],
}

/// One source file's Rust expansion observations.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroFile<'source> {
    /// Path of the original source supplied to the language server.
    pub path: &'source ProjectPath,
    /// Digest of the original complete bytes.
    pub digest: FileDigest,
    /// Expansions tied to that source.
    pub expansions: &'source [RustLspMacroItems<'source>],
}

/// Counts authored declaration facets added under the existing limits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RustLspMacroAdmission {
    /// Newly published written name facets.
    pub admitted: usize,
    /// Otherwise verified names that did not fit the publication limits.
    pub dropped_at_limit: usize,
}

impl PackageAnalyzer {
    /// Finds direct module item Rust macro invocations with a bare written name.
    #[must_use]
    pub fn rust_lsp_macro_sites(
        path: &ProjectPath,
        source: &str,
        limits: SyntaxLimits,
    ) -> Vec<RustLspMacroSite> {
        if !source.contains('!') {
            return Vec::new();
        }
        let Some(document) = parse(path, source, limits) else {
            return Vec::new();
        };
        sites(&document).into_values().collect()
    }

    /// Finds exact original candidate names for definition checks in one macro invocation.
    ///
    /// The engine expansion supplies item shapes. Only a name with one original spelling
    /// and one emitted declaration is considered.
    #[must_use]
    pub fn rust_lsp_macro_name_sites(
        path: &ProjectPath,
        source: &str,
        invocation: SourceRange,
        expansion: &str,
        limits: SyntaxLimits,
    ) -> Vec<SourceRange> {
        candidates(path, source, invocation, expansion, limits)
            .into_keys()
            .collect()
    }

    /// Publishes verified original Rust macro input names under their retained source owner.
    ///
    /// Both the selected macro definition and the name must be confirmed by the engine in
    /// the original complete source. Expanded text provides the item's kind and owner only.
    /// Established declarations consume capacity first.
    /// # Errors
    /// Returns an error if an admitted declaration cannot be represented canonically.
    pub fn admit_rust_lsp_macro_items(
        publication: &mut PackagePublication,
        observed: &[RustLspMacroFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspMacroAdmission, RiftError> {
        if observed.is_empty() {
            return Ok(RustLspMacroAdmission::default());
        }
        if observed.len() > bound(limits.files_max()) {
            return Ok(RustLspMacroAdmission {
                admitted: 0,
                dropped_at_limit: observed.len(),
            });
        }
        let mut selected = BTreeMap::new();
        for file in observed {
            selected
                .entry(file.path.as_str())
                .and_modify(|row| *row = None)
                .or_insert(Some(file));
        }
        let mut output = BTreeMap::new();
        let objects = publication
            .objects
            .iter()
            .filter_map(|row| Some((row.id.as_ref()?, row)))
            .collect::<BTreeMap<_, _>>();
        let mut declarations = BTreeMap::<&str, Vec<_>>::new();
        for declaration in &publication.declarations {
            declarations
                .entry(declaration.unit.as_str())
                .or_default()
                .push(declaration);
        }
        let definitions =
            selected_macro_definitions(publication, observed, &declarations, limits.syntax());
        for unit in &publication.units {
            let Some(file) = selected.get(unit.path.0.as_str()).copied().flatten() else {
                continue;
            };
            if !same_source(publication, file.path, file.digest, unit) {
                continue;
            }
            let Some(existing) = declarations.get(unit.unit.as_str()) else {
                continue;
            };
            collect_file(
                publication,
                file,
                unit,
                existing,
                (&objects, &definitions),
                limits.syntax(),
                &mut output,
            );
        }
        let (objects, declarations, report) = records(publication, producer, limits, output)?;
        if report.admitted != 0 {
            publication.objects.extend(objects);
            publication
                .objects
                .sort_by(|left, right| left.id.cmp(&right.id));
            publication.declarations.extend(declarations);
            publication.declarations.sort_by(|left, right| {
                (&left.symbol, &left.unit, left.range.start, left.range.end).cmp(&(
                    &right.symbol,
                    &right.unit,
                    right.range.start,
                    right.range.end,
                ))
            });
        }
        Ok(report)
    }
}

fn parse(path: &ProjectPath, source: &str, limits: SyntaxLimits) -> Option<SyntaxDocument> {
    let document = parse_item_source(SyntaxSource { path, text: source }, limits)?;
    (!document.facts().has_errors()).then_some(document)
}

fn parse_item_source(source: SyntaxSource<'_>, limits: SyntaxLimits) -> Option<SyntaxDocument> {
    let document = RustSyntaxProvider::default().analyze(source, limits).ok()?;
    (document.facts().left_out_declaration_count() == 0).then_some(document)
}

fn range(value: ByteRange) -> Option<SourceRange> {
    SourceRange::new(value.start, value.end).ok()
}

fn bytes(value: SourceRange) -> ByteRange {
    ByteRange {
        start: value.start(),
        end: value.end(),
    }
}

fn text_at(source: &str, value: SourceRange) -> Option<&str> {
    source.get(usize::try_from(value.start()).ok()?..usize::try_from(value.end()).ok()?)
}

fn sites(document: &SyntaxDocument) -> BTreeMap<SourceRange, RustLspMacroSite> {
    let elements = document.syntax_elements();
    let mut sites = BTreeMap::new();
    for element in elements {
        if element.kind != "identifier" {
            continue;
        }
        let Some(invocation) = element.parent.and_then(|index| elements.get(index)) else {
            continue;
        };
        if invocation.kind != "macro_invocation" || invocation.range.start != element.range.start {
            continue;
        }
        let Some(mut scope) = invocation.parent.and_then(|index| elements.get(index)) else {
            continue;
        };
        if scope.kind == "expression_statement" {
            let Some(parent) = scope.parent.and_then(|index| elements.get(index)) else {
                continue;
            };
            scope = parent;
        }
        if scope.kind != "source_file"
            && !(scope.kind == "declaration_list"
                && scope
                    .parent
                    .and_then(|index| elements.get(index))
                    .is_some_and(|parent| parent.kind == "mod_item"))
        {
            continue;
        }
        let Some((name, invocation)) = range(element.range).zip(range(invocation.range)) else {
            continue;
        };
        sites.insert(invocation, RustLspMacroSite { name, invocation });
    }
    sites
}

#[derive(Debug, Clone, Copy)]
struct ItemKind {
    exact: &'static str,
    kinds: &'static [SymbolKind],
    child: bool,
}

fn item_kind(symbol: &SyntaxSymbol) -> Option<ItemKind> {
    let (exact, kinds, child): (&str, &[SymbolKind], _) = match symbol.syntax_element_kind? {
        "struct_item" if symbol.container.is_none() => ("struct", &[SymbolKind::Type], false),
        "enum_item" if symbol.container.is_none() => ("enum", &[SymbolKind::Type], false),
        "union_item" if symbol.container.is_none() => ("union_item", &[SymbolKind::Type], false),
        "field_declaration" if symbol.container.is_some() => (
            "field_declaration",
            &[SymbolKind::Value, SymbolKind::Member],
            true,
        ),
        "enum_variant" if symbol.container.is_some() => ("variant", &[SymbolKind::Variant], true),
        _ => return None,
    };
    Some(ItemKind {
        exact,
        kinds,
        child,
    })
}

struct Candidate {
    name: String,
    relative: Vec<String>,
    kind: ItemKind,
    visibility: Option<String>,
}

fn candidates(
    path: &ProjectPath,
    source: &str,
    invocation: SourceRange,
    expansion: &str,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Candidate> {
    let generated = SyntaxSource {
        path,
        text: expansion,
    };
    let Some(emitted) = parse_item_source(generated, limits) else {
        return BTreeMap::new();
    };
    // Other generated items may have unreadable bodies. Only individually checked
    // item declarations and their independently checked enclosing types supply names.
    let facts = emitted.facts();
    let invalid = if facts.has_errors() {
        let item_ranges = facts
            .symbols()
            .iter()
            .map(|symbol| symbol.item_range)
            .collect::<Vec<_>>();
        let Ok(invalid) =
            RustSyntaxProvider::default().incomplete_items(generated, limits, &item_ranges)
        else {
            return BTreeMap::new();
        };
        invalid
    } else {
        BTreeSet::new()
    };
    let clean = checked_emitted_items(&emitted, &invalid);
    let supported = |symbol: &SyntaxSymbol| {
        clean
            .contains(&(symbol.item_range, symbol.syntax_element_kind?))
            .then_some(item_kind(symbol))
            .flatten()
    };
    let types = facts
        .symbols()
        .iter()
        .filter_map(|symbol| (!supported(symbol)?.child).then_some(symbol.qualified_name.as_str()))
        .collect::<BTreeSet<_>>();
    let mut output = BTreeMap::new();
    for symbol in facts.symbols() {
        let Some(kind) = supported(symbol) else {
            continue;
        };
        if kind.child
            && !symbol
                .container
                .as_deref()
                .is_some_and(|owner| types.contains(owner))
        {
            continue;
        }
        let expected_parent = usize::from(kind.child);
        let relative = symbol
            .qualified_name
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if relative.len() != expected_parent + 1 || relative.last() != Some(&symbol.name) {
            continue;
        }
        let candidate = Candidate {
            name: symbol.name.clone(),
            relative,
            kind,
            visibility: symbol.visibility.clone(),
        };
        output
            .entry(symbol.name.as_str())
            .and_modify(|entry| *entry = None)
            .or_insert(Some(candidate));
    }

    let mut written = BTreeMap::new();
    if !rift_syntax::tokens(source, bytes(invocation), limits, |token| {
        let Some(position) = range(token) else {
            return false;
        };
        if let Some(name) = text_at(source, position)
            && output.contains_key(name)
        {
            written
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(position));
        }
        true
    }) {
        return BTreeMap::new();
    }
    output
        .into_iter()
        .filter_map(|(name, candidate)| Some((written.get(name).copied().flatten()?, candidate?)))
        .collect()
}

fn checked_emitted_items(
    emitted: &SyntaxDocument,
    invalid: &BTreeSet<ByteRange>,
) -> BTreeSet<(ByteRange, &'static str)> {
    let elements = emitted.syntax_elements();
    elements
        .iter()
        .filter_map(|row| {
            let parent = elements.get(row.parent?)?;
            let kind = match (row.kind, parent.kind) {
                ("struct_item" | "enum_item" | "union_item", "source_file") => row.kind,
                (
                    "field_declaration" | "enum_variant",
                    "field_declaration_list" | "enum_variant_list",
                ) if parent
                    .parent
                    .and_then(|i| elements.get(i))
                    .is_some_and(|owner| {
                        matches!(owner.kind, "struct_item" | "enum_item" | "union_item")
                            && owner.parent == Some(0)
                            && !invalid.contains(&owner.range)
                    }) =>
                {
                    row.kind
                }
                _ => return None,
            };
            (!row.has_error && !invalid.contains(&row.range)).then_some((row.range, kind))
        })
        .collect()
}

fn same_source(
    publication: &PackagePublication,
    path: &ProjectPath,
    digest: FileDigest,
    unit: &PackageSourceUnit,
) -> bool {
    let Ok(identity) = SourceUnitId::parse(unit.unit.as_str()) else {
        return false;
    };
    unit.source_complete
        && unit.language.name == "rust"
        && identity.source_owner() == Some(&publication.owner)
        && identity.key().as_str() == path.as_str()
        && digest == FileDigest::of(unit.source.as_bytes())
        && file_digest(digest) == unit.content_digest
}

struct Definition<'publication> {
    name: String,
    item_end: u64,
    unit: &'publication PackageSourceUnit,
    digest: FileDigest,
    identity: SymbolIdentity,
}

type Definitions<'publication> =
    BTreeMap<&'publication str, BTreeMap<SourceRange, Definition<'publication>>>;
type Objects<'publication> = BTreeMap<&'publication SymbolId, &'publication Symbol>;

fn selected_macro_definitions<'publication>(
    publication: &'publication PackagePublication,
    observed: &[RustLspMacroFile<'_>],
    declarations: &BTreeMap<&str, Vec<&PackageSymbol>>,
    limits: SyntaxLimits,
) -> Definitions<'publication> {
    let mut requested = BTreeMap::new();
    for file in observed {
        for observation in file.expansions.iter().take(limits.syntax_elements_max()) {
            let site = observation.definition;
            let digests = requested
                .entry(site.path.as_str())
                .or_insert_with(BTreeMap::new);
            digests
                .entry(site.range)
                .or_insert_with(BTreeSet::new)
                .insert(site.digest);
        }
    }
    let mut output = BTreeMap::new();
    for unit in &publication.units {
        let Some(names) = requested.get(unit.path.0.as_str()) else {
            continue;
        };
        let Ok(path) = ProjectPath::new(&unit.path.0) else {
            continue;
        };
        let digest = FileDigest::of(unit.source.as_bytes());
        if !same_source(publication, &path, digest, unit) {
            continue;
        }
        let Some(existing) = declarations.get(unit.unit.as_str()) else {
            continue;
        };
        let Some(source) = parse(&path, &unit.source, limits) else {
            continue;
        };
        let mut found = BTreeMap::new();
        for symbol in source.facts().symbols() {
            let Some(name) = symbol.name_range.and_then(range) else {
                continue;
            };
            if symbol.kind != "macro"
                || symbol.syntax_element_kind != Some("macro_definition")
                || !names
                    .get(&name)
                    .is_some_and(|digests| digests.contains(&digest))
            {
                continue;
            }
            let Some(retained) = retained_macro(existing, symbol) else {
                continue;
            };
            let Ok(identity) = SymbolIdentity::parse(retained.symbol.as_str()) else {
                continue;
            };
            if identity.owner() != &publication.owner
                || identity.language() != &unit.language
                || identity.qualified_path().last() != Some(&symbol.name)
            {
                continue;
            }
            found
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(Definition {
                    name: symbol.name.clone(),
                    item_end: symbol.item_range.end,
                    unit,
                    digest,
                    identity,
                }));
        }
        output.insert(
            unit.path.0.as_str(),
            found
                .into_iter()
                .filter_map(|(name, def)| Some((name, def?)))
                .collect(),
        );
    }
    output
}

struct Observed<'a> {
    unit: &'a PackageSourceUnit,
    owner: &'a PackageSymbol,
    name: Candidate,
    range: SourceRange,
    line: u64,
    invocation: SourceRange,
    definition: SourceRange,
    definition_unit: &'a PackageSourceUnit,
    expansion_digest: FileDigest,
    parent: SymbolId,
    symbol: SymbolId,
}

fn collect_file<'a>(
    publication: &PackagePublication,
    file: &RustLspMacroFile<'_>,
    unit: &'a PackageSourceUnit,
    existing: &[&'a PackageSymbol],
    inventory: (&Objects<'_>, &Definitions<'a>),
    limits: SyntaxLimits,
    output: &mut BTreeMap<SymbolId, Option<Observed<'a>>>,
) {
    let Some(original) = parse(file.path, &unit.source, limits) else {
        return;
    };
    let sites = sites(&original);
    let (objects, definitions) = inventory;
    let starts = line_starts(&unit.source);
    let mut work = limits.source_bytes_max();
    for observation in file.expansions.iter().take(limits.syntax_elements_max()) {
        if observation.names.len() > limits.syntax_elements_max() {
            continue;
        }
        let Some(site) = sites
            .get(&observation.site.invocation)
            .filter(|site| site.name == observation.site.name)
        else {
            continue;
        };
        let selected = observation.definition;
        let Some(definition) = definitions
            .get(selected.path.as_str())
            .and_then(|unit| unit.get(&selected.range))
        else {
            continue;
        };
        if selected.digest != definition.digest
            || text_at(&unit.source, site.name) != Some(definition.name.as_str())
        {
            continue;
        }
        let Some(left) = work.checked_sub(observation.expansion.len()) else {
            break;
        };
        work = left;
        let expansion_digest = FileDigest::of(observation.expansion.as_bytes());
        let Some(owner) = module_owner(&original, site.invocation, existing, objects) else {
            continue;
        };
        let Ok(module) = SymbolIdentity::parse(owner.symbol.as_str()) else {
            continue;
        };
        if module.owner() != &publication.owner || module.language() != &unit.language {
            continue;
        }
        if !selected_in_scope(definition, unit, site.invocation, &module) {
            continue;
        }
        let mut names = candidates(
            file.path,
            &unit.source,
            site.invocation,
            observation.expansion,
            limits,
        );
        let mut claimed = BTreeMap::new();
        for &range in observation.names {
            claimed
                .entry(range)
                .and_modify(|unique| *unique = false)
                .or_insert(true);
        }
        for (range, unique) in claimed {
            if range.start() <= site.name.end() || !unique {
                continue;
            }
            let Some(name) = names.remove(&range) else {
                continue;
            };
            let Some((symbol, parent)) = item_identity(&module, &name.relative) else {
                continue;
            };
            if objects.contains_key(&symbol) {
                continue;
            }
            output
                .entry(symbol.clone())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(Observed {
                    unit,
                    owner,
                    name,
                    range,
                    line: line_of(&starts, range.start()),
                    invocation: site.invocation,
                    definition: observation.definition.range,
                    definition_unit: definition.unit,
                    expansion_digest,
                    parent,
                    symbol,
                }));
        }
    }
}

fn selected_in_scope(
    definition: &Definition<'_>,
    invocation_unit: &PackageSourceUnit,
    invocation: SourceRange,
    module: &SymbolIdentity,
) -> bool {
    if definition.unit.unit == invocation_unit.unit {
        return definition.item_end <= invocation.start();
    }
    let Some((_, declaring_module)) = definition.identity.qualified_path().split_last() else {
        return false;
    };
    let invoking_module = module.qualified_path();
    !declaring_module.is_empty()
        && declaring_module.len() < invoking_module.len()
        && invoking_module.starts_with(declaring_module)
}

fn retained_macro<'publication>(
    existing: &[&'publication PackageSymbol],
    definition: &SyntaxSymbol,
) -> Option<&'publication PackageSymbol> {
    let mut matches = existing.iter().copied().filter(|declaration| {
        declaration.syntax_element_kind.as_deref() == Some("macro_definition")
            && declaration.range.start == definition.item_range.start
            && declaration.range.end == definition.item_range.end
            && declaration.origin.source_kind == SourceKind::Authored
    });
    let known = matches.next()?;
    matches.next().is_none().then_some(known)
}

fn module_owner<'a>(
    original: &SyntaxDocument,
    invocation: SourceRange,
    existing: &[&'a PackageSymbol],
    objects: &BTreeMap<&SymbolId, &Symbol>,
) -> Option<&'a PackageSymbol> {
    let nearest = original
        .facts()
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.kind == "module"
                && symbol
                    .body_range
                    .is_some_and(|body| body.encloses(bytes(invocation)))
        })
        .min_by_key(|symbol| symbol.item_range.end - symbol.item_range.start);
    let mut matches = existing.iter().copied().filter(|declaration| {
        if declaration.origin.source_kind != SourceKind::Authored
            || !objects
                .get(&declaration.symbol)
                .is_some_and(|object| object.kinds.contains(&SymbolKind::Module))
        {
            return false;
        }
        if let Some(nearest) = nearest {
            declaration.syntax_element_kind.as_deref() == Some("mod_item")
                && declaration.range.start == nearest.item_range.start
                && declaration.range.end == nearest.item_range.end
        } else {
            declaration.syntax_element_kind.as_deref() == Some("source_file")
        }
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn item_identity(module: &SymbolIdentity, relative: &[String]) -> Option<(SymbolId, SymbolId)> {
    let mut path = module.qualified_path().to_vec();
    path.extend_from_slice(relative);
    let symbol = SymbolIdentity::new(
        module.owner().clone(),
        module.language().clone(),
        path.clone(),
    )
    .ok()?;
    path.pop()?;
    let parent =
        SymbolIdentity::new(module.owner().clone(), module.language().clone(), path).ok()?;
    Some((
        SymbolId::from_identity(&symbol),
        SymbolId::from_identity(&parent),
    ))
}

fn records(
    publication: &PackagePublication,
    producer: &FactProducer,
    limits: ExactPackageLimits,
    output: BTreeMap<SymbolId, Option<Observed<'_>>>,
) -> Result<(Vec<Symbol>, Vec<PackageSymbol>, RustLspMacroAdmission), RiftError> {
    let mut known = publication
        .objects
        .iter()
        .filter_map(|row| row.id.clone())
        .collect::<BTreeSet<_>>();
    let mut declarations = Vec::new();
    let mut objects = Vec::new();
    let mut report = RustLspMacroAdmission::default();
    let symbols = bound(limits.publication().symbols);
    let mut remaining = symbols.saturating_sub(publication.objects.len()).min(
        symbols
            .min(limits.declarations_max())
            .saturating_sub(publication.declarations.len()),
    );
    let (mut bytes_left, record_max) = limits.retained_source().map_or(
        (
            u64::MAX,
            u64::from(rift_protocol::index::PACKAGE_SOURCE_BYTES_MAX),
        ),
        |limit| {
            (
                limit.total_bytes_max.saturating_sub(
                    super::rust_lsp_declarations::retained_source_bytes(publication),
                ),
                u64::from(limit.record_bytes_max),
            )
        },
    );
    for row in output.into_values().flatten() {
        if !known.contains(&row.parent) {
            continue;
        }
        let length = row.range.end() - row.range.start();
        if remaining == 0 || length > record_max || length > bytes_left {
            report.dropped_at_limit += 1;
            continue;
        }
        append(
            publication,
            producer,
            row,
            &mut declarations,
            &mut objects,
            &mut known,
        )?;
        remaining -= 1;
        bytes_left -= length;
    }
    report.admitted = declarations.len();
    Ok((objects, declarations, report))
}

fn append(
    publication: &PackagePublication,
    producer: &FactProducer,
    row: Observed<'_>,
    declarations: &mut Vec<PackageSymbol>,
    objects: &mut Vec<Symbol>,
    known: &mut BTreeSet<SymbolId>,
) -> Result<(), RiftError> {
    let mut declaration = PackageSymbol {
        symbol: row.symbol.clone(),
        origin: row.owner.origin.clone(),
        unit: row.unit.unit.clone(),
        range: TextRange {
            start: row.range.start(),
            end: row.range.end(),
        },
        syntax_element_kind: Some("identifier".to_owned()),
        line: row.line,
        signature_indices: Vec::new(),
        type_indices: Vec::new(),
        documentation_indices: Vec::new(),
        source: row.name.name.clone(),
        source_complete: true,
        public: row.name.visibility.as_deref() == Some("pub"),
        digest: Digest(String::new()),
    };
    declaration.digest = digest_of(&declaration, &publication.owner)?;
    known.insert(row.symbol.clone());
    let mut kinds = row.name.kind.kinds.to_vec();
    if declaration.public {
        kinds.push(SymbolKind::Public);
    }
    objects.push(Symbol {
        id: Some(row.symbol),
        language: row.unit.language.clone(),
        name: row.name.name,
        kind: ExactKind(row.name.kind.exact.to_owned()),
        kinds,
        origin: row.owner.origin.clone(),
        container: Some(row.parent),
        modifiers: Vec::new(),
        visibility: row.name.visibility,
        types: Vec::new(),
        signatures: Vec::new(),
        documentation: Vec::new(),
        extensions: Extensions(BTreeMap::from([(
            ExtensionKey("org.lsp.rust-macro-name".to_owned()),
            ExtensionValue {
                version: 1,
                data: serde_json::json!({
                    "producer": producer,
                    "invocation": [row.invocation.start(), row.invocation.end()],
                    "definition": [row.definition.start(), row.definition.end()],
                    "definition_unit": row.definition_unit.unit,
                    "expansion_digest": row.expansion_digest.as_bytes(),
                }),
            },
        )])),
        document_local: false,
    });
    declarations.push(declaration);
    Ok(())
}

#[cfg(test)]
mod tests {
    use rift_protocol::producer::{ProducerName, ProducerRevision};
    use rift_syntax::ShippedLanguage;

    use super::*;
    use crate::analyzer::fixture;

    const SOURCE: &str = r"
pub mod api {
    macro_rules! define_type {
        ($name:ident) => { pub struct $name { generated: u8 } };
    }
    define_type!(Produced);
    pub fn make() -> Produced { Produced { generated: 7 } }
}";
    const MANIFEST: &str = "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n";
    const EXPANSION: &str = "pub struct Produced { generated: u8 }\nfn damaged() { let =; }";

    #[test]
    fn local_expansion_keeps_the_authored_name_and_refuses_unproved_names() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let syntax = SyntaxLimits::DEFAULT;
        let digest = FileDigest::of(SOURCE.as_bytes());
        let sites = PackageAnalyzer::rust_lsp_macro_sites(&path, SOURCE, syntax);
        assert_eq!(sites.len(), 1);
        let site = sites[0];
        let names = PackageAnalyzer::rust_lsp_macro_name_sites(
            &path,
            SOURCE,
            site.invocation,
            EXPANSION,
            syntax,
        );
        assert_eq!(names.len(), 1);
        assert_eq!(text_at(SOURCE, names[0]), Some("Produced"));
        let definition = SOURCE.find("define_type").expect("macro is written");
        let definition =
            SourceRange::new(definition as u64, (definition + "define_type".len()) as u64)
                .expect("macro definition range");
        let observation = RustLspMacroItems {
            site,
            definition: RustLspMacroDefinition {
                path: &path,
                digest,
                range: definition,
            },
            expansion: EXPANSION,
            names: &names,
        };
        let base = authored_macro_fixture(vec![("src/lib.rs", SOURCE)]);
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("fixture producer"),
            revision: ProducerRevision::parse("1").expect("fixture producer revision"),
        };
        let apply = |publication: &mut PackagePublication, digest, observation, limits| {
            PackageAnalyzer::admit_rust_lsp_macro_items(
                publication,
                &[RustLspMacroFile {
                    path: &path,
                    digest,
                    expansions: &[observation],
                }],
                &producer,
                limits,
            )
            .expect("typed admission")
        };
        let limits = ExactPackageLimits::new(1024, 2 * 1024 * 1024);
        let mut accepted = base.clone();
        assert_eq!(
            apply(&mut accepted, digest, observation, limits).admitted,
            1
        );
        assert_authored_facet(&accepted, &base, names[0]);
        assert_eq!(
            apply(&mut accepted, digest, observation, limits).admitted,
            0
        );

        let mut refused = base.clone();
        assert_eq!(
            apply(&mut refused, FileDigest::of(b"other"), observation, limits).admitted,
            0
        );
        let mut refused_observation = |observation| {
            assert_eq!(apply(&mut refused, digest, observation, limits).admitted, 0);
        };
        let duplicates = [names[0], names[0]];
        let duplicate = RustLspMacroItems {
            names: &duplicates,
            ..observation
        };
        refused_observation(duplicate);
        let wrong_macro = RustLspMacroItems {
            definition: RustLspMacroDefinition {
                range: site.name,
                ..observation.definition
            },
            ..observation
        };
        refused_observation(wrong_macro);
        let malformed_owner = RustLspMacroItems {
            expansion: "pub struct Produced { generated: }",
            ..observation
        };
        refused_observation(malformed_owner);
        let bound = limits
            .with_declarations(u32::try_from(base.declarations.len()).expect("fixture bound"))
            .expect("positive bound");
        assert_eq!(
            apply(&mut refused, digest, observation, bound).dropped_at_limit,
            1
        );
        assert_eq!(refused, base);
    }
    #[test]
    fn selected_cross_file_macro_requires_the_exact_ancestor_definition() {
        const MACRO: &str = "macro_rules! declare_type { ($name:ident) => { pub struct $name; }; }";
        const INVOCATION: &str = "declare_type!(Produced);";
        let source = format!("{MACRO}\npub mod api; pub mod sibling;");
        let root = ProjectPath::new("src/lib.rs").expect("root path");
        let child = ProjectPath::new("src/api.rs").expect("invocation path");
        let sibling = ProjectPath::new("src/sibling.rs").expect("other module path");
        let limits = ExactPackageLimits::new(16, 2 * 1024 * 1024);
        let macro_name_start =
            u64::try_from(MACRO.find("declare_type").expect("macro name")).expect("offset fits");
        let macro_name =
            SourceRange::new(macro_name_start, macro_name_start + 12).expect("definition range");
        let site = PackageAnalyzer::rust_lsp_macro_sites(&child, INVOCATION, limits.syntax())[0];
        let expansion = "pub struct Produced;";
        let names = PackageAnalyzer::rust_lsp_macro_name_sites(
            &child,
            INVOCATION,
            site.invocation,
            expansion,
            limits.syntax(),
        );
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("1").expect("producer revision"),
        };
        let base = authored_macro_fixture(vec![
            ("src/lib.rs", &source),
            ("src/api.rs", INVOCATION),
            ("src/sibling.rs", MACRO),
        ]);
        for (path, digest, accepted) in [
            (&root, FileDigest::of(source.as_bytes()), 1),
            (&root, FileDigest::of(b"changed"), 0),
            (&sibling, FileDigest::of(MACRO.as_bytes()), 0),
        ] {
            let mut publication = base.clone();
            let report = PackageAnalyzer::admit_rust_lsp_macro_items(
                &mut publication,
                &[RustLspMacroFile {
                    path: &child,
                    digest: FileDigest::of(INVOCATION.as_bytes()),
                    expansions: &[RustLspMacroItems {
                        site,
                        definition: RustLspMacroDefinition {
                            path,
                            digest,
                            range: macro_name,
                        },
                        expansion,
                        names: &names,
                    }],
                }],
                &producer,
                limits,
            )
            .expect("bounded admission");
            assert_eq!(report.admitted, accepted);
            if accepted == 1 {
                assert_authored_facet(&publication, &base, names[0]);
            } else {
                assert_eq!(publication, base);
            }
        }
    }

    fn authored_macro_fixture(files: Vec<(&str, &str)>) -> PackagePublication {
        fixture::package_result_with_metadata(
            ShippedLanguage::Rust,
            files,
            None,
            Some(("Cargo.toml", MANIFEST)),
        )
        .expect("authored fixture")
        .publication()
        .clone()
    }

    fn assert_authored_facet(
        accepted: &PackagePublication,
        base: &PackagePublication,
        name: SourceRange,
    ) {
        let created = accepted
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with("/api/Produced"))
            .expect("authored name facet");
        assert_eq!(created.source, "Produced");
        assert_eq!(created.range.start, name.start());
        assert_eq!(created.range.end, name.end());
        assert!(created.signature_indices.is_empty());
        assert_eq!(accepted.edges, base.edges);
        accepted.validate_facts().expect("validated package facts");
        assert!(
            accepted
                .declarations
                .iter()
                .all(|row| !row.symbol.as_str().ends_with("/Produced/generated"))
        );
        let owner = accepted
            .objects
            .iter()
            .find(|row| row.id.as_ref() == Some(&created.symbol))
            .expect("type object");
        assert!(
            owner
                .container
                .as_ref()
                .expect("module container")
                .as_str()
                .ends_with("/api")
        );
    }
}
