//! Uses locally selected Rust expansions to establish the identities of written name tokens.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{FileDigest, ProjectPath, SourceRange, SourceUnitId};
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::{PackagePublication, PackageSourceUnit, PackageSymbol};
use rift_protocol::producer::FactProducer;
use rift_protocol::read::{
    Digest, ExactKind, ExtensionKey, ExtensionValue, Extensions, SourceKind,
    SourceUnitId as PublishedUnitId, Symbol, SymbolId, SymbolKind, TextRange,
};
use rift_protocol::revision::Revision;
use rift_syntax::{
    ByteRange, RustSyntaxProvider, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource,
    SyntaxSymbol,
};

use super::{PackageAnalyzer, bound, digest_of, file_digest, line_of, line_starts};
use crate::ExactPackageLimits;

mod associated_members;
mod constants;
mod foreign_declarations;
mod variant_fields;

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

/// An independently admitted dependency unit addressed by a selected Rust macro definition.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroDependency<'source> {
    /// Normalized locator that the caller assigned to the checked engine source.
    pub locator: &'source ProjectPath,
    /// Package-relative path in the dependency's retained publication.
    pub unit: &'source ProjectPath,
    /// Publication establishing the dependency's package identity and exported declaration.
    pub publication: &'source PackagePublication,
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

/// Macro-tagged native declaration evidence for one opened Rust unit.
#[derive(Debug, Clone, Copy)]
pub struct RustLspForeignDeclarationFile<'source> {
    /// Selected complete macro expansions in the same opened source bytes.
    pub macros: RustLspMacroFile<'source>,
    /// Exact original byte range and whether the engine labeled a function rather than a parameter.
    pub declarations: &'source [(SourceRange, bool)],
}

/// Macro-tagged native constant evidence for one opened Rust unit.
#[derive(Debug, Clone, Copy)]
pub struct RustLspMacroConstantFile<'source> {
    /// Selected complete macro expansions tied to the original source.
    pub macros: RustLspMacroFile<'source>,
    /// Exact original byte range and whether the native constant is associated.
    pub declarations: &'source [(SourceRange, bool)],
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
        macro_sites(path, source, limits, false)
    }

    /// Finds module-item macro sites for an independently selected dependency.
    ///
    /// Both a bare import and the terminal name of a qualified invocation can supply a
    /// definition position. The caller still has to verify its selected dependency source.
    #[must_use]
    pub fn rust_lsp_dependency_macro_sites(
        path: &ProjectPath,
        source: &str,
        limits: SyntaxLimits,
    ) -> Vec<RustLspMacroSite> {
        macro_sites(path, source, limits, true)
    }

    /// Finds Rust macro invocations inside a checked inherent implementation.
    ///
    /// An independently selected dependency definition and exact authored name remain
    /// necessary before any member can be published.
    #[must_use]
    pub fn rust_lsp_dependency_macro_member_sites(
        path: &ProjectPath,
        source: &str,
        limits: SyntaxLimits,
    ) -> Vec<RustLspMacroSite> {
        let scopes = associated_members::scopes(source, limits);
        if scopes.is_empty() || !source.contains('!') {
            return Vec::new();
        }
        let Some(document) = parse(path, source, limits) else {
            return Vec::new();
        };
        sites(&document, true, Some(&scopes))
            .into_values()
            .collect()
    }

    /// Finds exact written names in supported original Rust method signatures.
    ///
    /// The emitted signatures supply names and shapes. Definition checks must still
    /// establish the written member and its parameters at their own source positions.
    #[must_use]
    pub fn rust_lsp_macro_member_name_sites(
        path: &ProjectPath,
        source: &str,
        invocation: SourceRange,
        expansion: &str,
        limits: SyntaxLimits,
    ) -> Vec<SourceRange> {
        associated_members::candidates(path, source, invocation, expansion, limits)
            .into_keys()
            .collect()
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
        candidates(path, source, invocation, expansion, limits, false)
            .into_keys()
            .collect()
    }

    /// Finds exact written variant field names after their original enum and variant names
    /// are confirmed. A caller can reserve its remaining definition checks for these fields.
    #[must_use]
    pub fn rust_lsp_macro_variant_field_sites(
        path: &ProjectPath,
        source: &str,
        invocation: SourceRange,
        expansion: &str,
        limits: SyntaxLimits,
        confirmed: &[SourceRange],
    ) -> Vec<SourceRange> {
        if confirmed.is_empty() || confirmed.len() > limits.syntax_elements_max() {
            return Vec::new();
        }
        let names = unique_claims(confirmed);
        candidates(path, source, invocation, expansion, limits, true)
            .into_iter()
            .filter_map(|(range, candidate)| {
                candidate.variant_owner?.confirmed(&names).then_some(range)
            })
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
        Self::admit_rust_lsp_macro_items_with_dependencies(
            publication,
            observed,
            &[],
            producer,
            limits,
        )
    }

    /// Adds verified authored expansion names with checked dependency definition sources.
    ///
    /// Dependency locators identify exact selected units. Their exported macro and the written
    /// name in the consuming unit are checked separately. Local admissions retain capacity first.
    /// # Errors
    /// Returns an error if an admitted declaration cannot be represented canonically.
    pub fn admit_rust_lsp_macro_items_with_dependencies(
        publication: &mut PackagePublication,
        observed: &[RustLspMacroFile<'_>],
        dependencies: &[RustLspMacroDependency<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspMacroAdmission, RiftError> {
        Self::admit_rust_lsp_expansions(
            publication,
            ExpansionFiles::Items(observed),
            dependencies,
            producer,
            limits,
        )
    }

    /// Adds checked original foreign signatures and parameters from selected local Rust macros.
    ///
    /// The original signature must independently match a complete emitted ABI signature.
    /// Exact same-source semantic declaration roles confirm the function and its written
    /// parameters. Call this after established macro and local declaration admissions.
    ///
    /// # Errors
    /// Returns an error if an admitted declaration cannot be represented canonically.
    pub fn admit_rust_lsp_foreign_declarations(
        publication: &mut PackagePublication,
        observed: &[RustLspForeignDeclarationFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspMacroAdmission, RiftError> {
        Self::admit_rust_lsp_expansions(
            publication,
            ExpansionFiles::Foreign(observed),
            &[],
            producer,
            limits,
        )
    }

    /// Publishes written constants in checked module-item expansions of selected local macros.
    ///
    /// Native declaration roles, original tokens and complete emitted declarations must agree.
    /// Associated constants also require a retained nominal owner in the same authored unit.
    /// Call this after established declaration admissions.
    ///
    /// # Errors
    /// Returns an error if an admitted declaration cannot be represented canonically.
    pub fn admit_rust_lsp_macro_constants(
        publication: &mut PackagePublication,
        observed: &[RustLspMacroConstantFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspMacroAdmission, RiftError> {
        Self::admit_rust_lsp_expansions(
            publication,
            ExpansionFiles::Constants(observed),
            &[],
            producer,
            limits,
        )
    }

    fn admit_rust_lsp_expansions(
        publication: &mut PackagePublication,
        observed: ExpansionFiles<'_>,
        dependencies: &[RustLspMacroDependency<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspMacroAdmission, RiftError> {
        if observed.len() == 0 {
            return Ok(RustLspMacroAdmission::default());
        }
        if observed.len() > bound(limits.files_max()) {
            return Ok(RustLspMacroAdmission {
                admitted: 0,
                dropped_at_limit: observed.len(),
            });
        }
        let mut selected = BTreeMap::new();
        for (file, tokens) in observed.files() {
            selected
                .entry(file.path.as_str())
                .and_modify(|row| *row = None)
                .or_insert(Some((file, tokens)));
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
        let definitions = selected_macro_definitions(
            publication,
            selected.values().flatten().map(|(file, _)| *file),
            &declarations,
            dependencies,
            limits,
        );
        let imports = match observed {
            ExpansionFiles::Items(_) | ExpansionFiles::Constants(_) => {
                foreign_declarations::ImportedOwners::default()
            }
            ExpansionFiles::Foreign(_) => foreign_declarations::ImportedOwners::capture(
                publication,
                &declarations,
                limits.syntax(),
            ),
        };
        for unit in &publication.units {
            let Some((file, tokens)) = selected.get(unit.path.0.as_str()).copied().flatten() else {
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
                (
                    &objects,
                    &definitions,
                    ExpansionLabels::new(tokens, &imports, limits.syntax()),
                ),
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

type DeclarationTokens<'source> = &'source [(SourceRange, bool)];

#[derive(Clone, Copy)]
enum ExpansionFiles<'source> {
    Items(&'source [RustLspMacroFile<'source>]),
    Foreign(&'source [RustLspForeignDeclarationFile<'source>]),
    Constants(&'source [RustLspMacroConstantFile<'source>]),
}

#[derive(Clone, Copy)]
enum ExpansionTokens<'source> {
    Definitions,
    Foreign(DeclarationTokens<'source>),
    Constants(DeclarationTokens<'source>),
}

impl<'source> ExpansionFiles<'source> {
    fn len(&self) -> usize {
        match self {
            Self::Items(files) => files.len(),
            Self::Foreign(files) => files.len(),
            Self::Constants(files) => files.len(),
        }
    }

    fn files(
        &self,
    ) -> impl Iterator<Item = (&'source RustLspMacroFile<'source>, ExpansionTokens<'source>)> {
        let (items, foreign, constants) = match self {
            Self::Items(files) => (*files, &[][..], &[][..]),
            Self::Foreign(files) => (&[][..], *files, &[][..]),
            Self::Constants(files) => (&[][..], &[][..], *files),
        };
        items
            .iter()
            .map(|file| (file, ExpansionTokens::Definitions))
            .chain(
                foreign
                    .iter()
                    .map(|file| (&file.macros, ExpansionTokens::Foreign(file.declarations))),
            )
            .chain(
                constants
                    .iter()
                    .map(|file| (&file.macros, ExpansionTokens::Constants(file.declarations))),
            )
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

fn macro_sites(
    path: &ProjectPath,
    source: &str,
    limits: SyntaxLimits,
    qualified: bool,
) -> Vec<RustLspMacroSite> {
    if !source.contains('!') {
        return Vec::new();
    }
    let Some(document) = parse(path, source, limits) else {
        return Vec::new();
    };
    sites(&document, qualified, None).into_values().collect()
}

fn sites(
    document: &SyntaxDocument,
    qualified: bool,
    member_scopes: Option<&BTreeSet<ByteRange>>,
) -> BTreeMap<SourceRange, RustLspMacroSite> {
    let elements = document.syntax_elements();
    let mut sites = BTreeMap::new();
    for element in elements {
        if element.kind != "identifier" {
            continue;
        }
        let Some(parent) = element.parent.and_then(|index| elements.get(index)) else {
            continue;
        };
        let (invocation, head_start) = if qualified
            && parent.kind == "scoped_identifier"
            && parent.range.end == element.range.end
        {
            let Some(invocation) = parent.parent.and_then(|index| elements.get(index)) else {
                continue;
            };
            (invocation, parent.range.start)
        } else {
            (parent, element.range.start)
        };
        if invocation.kind != "macro_invocation" || invocation.range.start != head_start {
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
        if !site_scope(elements, scope, member_scopes) {
            continue;
        }
        let Some((name, invocation)) = range(element.range).zip(range(invocation.range)) else {
            continue;
        };
        sites.insert(invocation, RustLspMacroSite { name, invocation });
    }
    sites
}

fn site_scope(
    elements: &[rift_syntax::SyntaxElement],
    scope: &rift_syntax::SyntaxElement,
    member_scopes: Option<&BTreeSet<ByteRange>>,
) -> bool {
    if scope.kind == "source_file" {
        return member_scopes.is_none();
    }
    if scope.kind != "declaration_list" {
        return false;
    }
    let Some(owner) = scope.parent.and_then(|index| elements.get(index)) else {
        return false;
    };
    let Some(scopes) = member_scopes else {
        return owner.kind == "mod_item";
    };
    if owner.kind != "impl_item" || !scopes.contains(&owner.range) || owner.has_error {
        return false;
    }
    let Some(lexical) = owner.parent.and_then(|index| elements.get(index)) else {
        return false;
    };
    lexical.kind == "source_file"
        || (lexical.kind == "declaration_list"
            && lexical
                .parent
                .and_then(|index| elements.get(index))
                .is_some_and(|parent| parent.kind == "mod_item"))
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
    variant_owner: Option<VariantOwner>,
    member_owner: Option<SourceRange>,
}

struct VariantOwner {
    enumeration: SourceRange,
    name: SourceRange,
}

impl VariantOwner {
    fn confirmed(&self, names: &BTreeMap<SourceRange, bool>) -> bool {
        names.get(&self.enumeration) == Some(&true) && names.get(&self.name) == Some(&true)
    }
}

fn unique_claims(ranges: &[SourceRange]) -> BTreeMap<SourceRange, bool> {
    let mut claims = BTreeMap::new();
    for &range in ranges {
        claims
            .entry(range)
            .and_modify(|unique| *unique = false)
            .or_insert(true);
    }
    claims
}

fn candidates(
    path: &ProjectPath,
    source: &str,
    invocation: SourceRange,
    expansion: &str,
    limits: SyntaxLimits,
    include_variant_fields: bool,
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
    let output = primary_candidates(&emitted, &invalid);
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
    let mut output = output
        .into_iter()
        .filter_map(|(name, candidate)| Some((written.get(name).copied().flatten()?, candidate?)))
        .collect();
    if include_variant_fields {
        variant_fields::extend(
            path,
            &emitted,
            &invalid,
            source,
            invocation,
            limits,
            &mut output,
        );
    }
    output
}

fn primary_candidates<'source>(
    emitted: &'source SyntaxDocument,
    invalid: &BTreeSet<ByteRange>,
) -> BTreeMap<&'source str, Option<Candidate>> {
    let facts = emitted.facts();
    let clean = checked_emitted_items(emitted, invalid);
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
            variant_owner: None,
            member_owner: None,
        };
        output
            .entry(symbol.name.as_str())
            .and_modify(|entry| *entry = None)
            .or_insert(Some(candidate));
    }
    output
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
    revision: &'publication Revision,
}

type Definitions<'publication> =
    BTreeMap<&'publication str, BTreeMap<SourceRange, Definition<'publication>>>;
type Objects<'publication> = BTreeMap<&'publication SymbolId, &'publication Symbol>;
type DefinitionRequests<'source> =
    BTreeMap<&'source str, BTreeMap<SourceRange, BTreeSet<FileDigest>>>;
type Observations<'source> = BTreeMap<(bool, SymbolId), Option<Observed<'source>>>;

fn selected_macro_definitions<'publication, 'source>(
    publication: &'publication PackagePublication,
    observed: impl Iterator<Item = &'source RustLspMacroFile<'source>>,
    declarations: &BTreeMap<&str, Vec<&PackageSymbol>>,
    dependencies: &[RustLspMacroDependency<'publication>],
    limits: ExactPackageLimits,
) -> Definitions<'publication> {
    let syntax = limits.syntax();
    let mut requested = DefinitionRequests::new();
    for file in observed {
        for observation in file.expansions.iter().take(syntax.syntax_elements_max()) {
            let site = observation.definition;
            let digests = requested.entry(site.path.as_str()).or_default();
            digests.entry(site.range).or_default().insert(site.digest);
        }
    }
    let mut output = BTreeMap::new();
    for unit in &publication.units {
        let Some(names) = requested.get(unit.path.0.as_str()) else {
            continue;
        };
        let Some(existing) = declarations.get(unit.unit.as_str()) else {
            continue;
        };
        output.insert(
            unit.path.0.as_str(),
            unit_definitions(publication, unit, existing, names, syntax),
        );
    }
    if !dependencies.is_empty() && dependencies.len() <= bound(limits.files_max()) {
        dependency_definitions(publication, dependencies, &requested, syntax, &mut output);
    }
    output
}

fn dependency_definitions<'publication>(
    primary: &PackagePublication,
    dependencies: &[RustLspMacroDependency<'publication>],
    requested: &DefinitionRequests<'_>,
    limits: SyntaxLimits,
    output: &mut Definitions<'publication>,
) {
    let mut selected = BTreeMap::new();
    for dependency in dependencies {
        let locator = dependency.locator.as_str();
        if requested.contains_key(locator) {
            selected
                .entry(locator)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(dependency));
        }
    }
    let local_paths = primary
        .units
        .iter()
        .map(|unit| unit.path.0.as_str())
        .collect::<BTreeSet<_>>();
    for (locator, dependency) in selected {
        let Some(dependency) = dependency else {
            continue;
        };
        let publication = dependency.publication;
        if publication.owner == primary.owner
            || local_paths.contains(locator)
            || !matches!(&publication.owner, SymbolOwner::Package { manager, .. } if manager == "cargo")
            || !publication.revision.answers_for(&publication.owner)
            || publication.revision.release_version().is_none()
        {
            continue;
        }
        let mut units = publication
            .units
            .iter()
            .filter(|unit| unit.path.0 == dependency.unit.as_str());
        let (Some(unit), None) = (units.next(), units.next()) else {
            continue;
        };
        let Some(names) = requested.get(locator) else {
            continue;
        };
        let declarations = publication
            .declarations
            .iter()
            .filter(|row| row.public && row.unit == unit.unit)
            .collect::<Vec<_>>();
        output.insert(
            locator,
            unit_definitions(publication, unit, &declarations, names, limits),
        );
    }
}

fn unit_definitions<'publication>(
    publication: &'publication PackagePublication,
    unit: &'publication PackageSourceUnit,
    existing: &[&PackageSymbol],
    names: &BTreeMap<SourceRange, BTreeSet<FileDigest>>,
    limits: SyntaxLimits,
) -> BTreeMap<SourceRange, Definition<'publication>> {
    let Ok(path) = ProjectPath::new(&unit.path.0) else {
        return BTreeMap::new();
    };
    let digest = FileDigest::of(unit.source.as_bytes());
    if !same_source(publication, &path, digest, unit) {
        return BTreeMap::new();
    }
    let Some(source) = parse(&path, &unit.source, limits) else {
        return BTreeMap::new();
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
                revision: &publication.revision,
            }));
    }
    found
        .into_iter()
        .filter_map(|(name, definition)| Some((name, definition?)))
        .collect()
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
    dependency_definition: Option<(&'a Revision, FileDigest)>,
    expansion_digest: FileDigest,
    parent: SymbolId,
    symbol: SymbolId,
    only_type_witness: bool,
    needs_type_witness: bool,
}

fn collect_file<'a>(
    publication: &PackagePublication,
    file: &RustLspMacroFile<'_>,
    unit: &'a PackageSourceUnit,
    existing: &[&'a PackageSymbol],
    inventory: (&Objects<'_>, &Definitions<'a>, ExpansionLabels<'_>),
    limits: SyntaxLimits,
    output: &mut Observations<'a>,
) {
    let (objects, definitions, labels) = inventory;
    if labels.is_empty() {
        return;
    }
    let Some(original) = parse(file.path, &unit.source, limits) else {
        return;
    };
    let sites = sites(&original, true, None);
    let scopes = member_scopes(file, &unit.source, &sites, limits);
    let members = sites_in_members(&original, &scopes);
    let starts = line_starts(&unit.source);
    let mut work = limits.source_bytes_max();
    for observation in file.expansions.iter().take(limits.syntax_elements_max()) {
        if observation.names.len() > limits.syntax_elements_max() {
            continue;
        }
        let associated = members.contains_key(&observation.site.invocation);
        let Some(site) = sites
            .get(&observation.site.invocation)
            .or_else(|| members.get(&observation.site.invocation))
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
        let owner = if associated {
            associated_members::owner(&original, site.invocation, &scopes, existing, objects)
        } else {
            module_owner(&original, site.invocation, existing, objects)
        };
        let Some(owner) = owner else {
            continue;
        };
        let Ok(module) = SymbolIdentity::parse(owner.symbol.as_str()) else {
            continue;
        };
        if module.owner() != &publication.owner || module.language() != &unit.language {
            continue;
        }
        let scope = ObservedScope {
            unit,
            owner,
            dependency: definition.identity.owner() != module.owner(),
            definition,
            expansion_digest,
            starts: &starts,
            needs_type_witness: associated
                && objects
                    .get(&owner.symbol)
                    .is_some_and(|object| object.kind.0 == "impl_item"),
        };
        let Some((names, confirmed)) = expansion_candidates(
            file.path,
            &original,
            observation,
            &module,
            scope,
            (&labels, existing, objects),
            limits,
        ) else {
            continue;
        };
        let observation = RustLspMacroItems {
            names: match labels {
                ExpansionLabels::Definitions => observation.names,
                ExpansionLabels::Foreign(..) | ExpansionLabels::Constants(_) => &confirmed,
            },
            ..*observation
        };
        collect_names(output, names, &observation, &module, objects, scope);
    }
}

type ForeignLabels = BTreeMap<SourceRange, Option<foreign_declarations::Role>>;
type ExpansionCandidates = (BTreeMap<SourceRange, Candidate>, Vec<SourceRange>);

enum ExpansionLabels<'source> {
    Definitions,
    Foreign(
        ForeignLabels,
        &'source foreign_declarations::ImportedOwners<'source>,
    ),
    Constants(constants::Labels),
}

impl<'source> ExpansionLabels<'source> {
    fn new(
        tokens: ExpansionTokens<'_>,
        imports: &'source foreign_declarations::ImportedOwners<'source>,
        limits: SyntaxLimits,
    ) -> Self {
        match tokens {
            ExpansionTokens::Definitions => Self::Definitions,
            ExpansionTokens::Foreign(tokens) => {
                Self::Foreign(foreign_declarations::labels(tokens, limits), imports)
            }
            ExpansionTokens::Constants(tokens) => {
                Self::Constants(constants::labels(tokens, limits))
            }
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Definitions => false,
            Self::Foreign(labels, _) => labels.is_empty(),
            Self::Constants(labels) => labels.is_empty(),
        }
    }
}

fn expansion_candidates(
    path: &ProjectPath,
    original: &SyntaxDocument,
    observation: &RustLspMacroItems<'_>,
    module: &SymbolIdentity,
    scope: ObservedScope<'_, '_>,
    inventory: (&ExpansionLabels<'_>, &[&PackageSymbol], &Objects<'_>),
    limits: SyntaxLimits,
) -> Option<ExpansionCandidates> {
    let ObservedScope {
        unit,
        owner,
        dependency,
        definition,
        ..
    } = scope;
    let (labels, existing, objects) = inventory;
    let site = &observation.site;
    let associated = owner.syntax_element_kind.as_deref() == Some("impl_item");
    let local_scope = selected_in_scope(definition, unit, original, site, module);
    let mut confirmed = Vec::new();
    let names = match labels {
        ExpansionLabels::Foreign(labels, imports) => {
            if associated
                || dependency
                || (!local_scope
                    && (!imports.allows(definition, site, owner)
                        || local_macro_shadows(original, definition, site.invocation)))
            {
                return None;
            }
            foreign_declarations::confirm(
                foreign_declarations::candidates(
                    path,
                    &unit.source,
                    site.invocation,
                    observation.expansion,
                    limits,
                ),
                labels,
                &mut confirmed,
            )
        }
        ExpansionLabels::Constants(labels)
            if local_scope
                && !associated
                && !dependency
                && definition
                    .identity
                    .qualified_path()
                    .split_last()
                    .is_some_and(|(_, parent)| module.qualified_path().starts_with(parent)) =>
        {
            let owners = constants::Owners {
                module,
                existing,
                objects,
            };
            let names =
                constants::candidates(path, &unit.source, observation, labels, &owners, limits);
            confirmed.extend(names.keys().copied());
            names
        }
        ExpansionLabels::Definitions if local_scope && associated && dependency => {
            associated_members::candidates(
                path,
                &unit.source,
                site.invocation,
                observation.expansion,
                limits,
            )
        }
        ExpansionLabels::Definitions if local_scope && !associated => candidates(
            path,
            &unit.source,
            site.invocation,
            observation.expansion,
            limits,
            true,
        ),
        _ => return None,
    };
    Some((names, confirmed))
}

fn sites_in_members(
    original: &SyntaxDocument,
    scopes: &BTreeSet<ByteRange>,
) -> BTreeMap<SourceRange, RustLspMacroSite> {
    if scopes.is_empty() {
        return BTreeMap::new();
    }
    sites(original, true, Some(scopes))
}

fn member_scopes(
    file: &RustLspMacroFile<'_>,
    source: &str,
    module_items: &BTreeMap<SourceRange, RustLspMacroSite>,
    limits: SyntaxLimits,
) -> BTreeSet<ByteRange> {
    if file
        .expansions
        .iter()
        .any(|row| !module_items.contains_key(&row.site.invocation))
    {
        return associated_members::scopes(source, limits);
    }
    BTreeSet::new()
}

#[derive(Clone, Copy)]
struct ObservedScope<'a, 'line> {
    unit: &'a PackageSourceUnit,
    owner: &'a PackageSymbol,
    dependency: bool,
    definition: &'line Definition<'a>,
    expansion_digest: FileDigest,
    starts: &'line [usize],
    needs_type_witness: bool,
}

fn collect_names<'a>(
    output: &mut Observations<'a>,
    mut names: BTreeMap<SourceRange, Candidate>,
    observation: &RustLspMacroItems<'_>,
    module: &SymbolIdentity,
    objects: &Objects<'_>,
    scope: ObservedScope<'a, '_>,
) {
    let ObservedScope {
        unit,
        owner,
        dependency,
        definition,
        expansion_digest,
        starts,
        needs_type_witness,
    } = scope;
    let claimed = unique_claims(observation.names);
    for (&range, &unique) in &claimed {
        if range.start() <= observation.site.name.end() || !unique {
            continue;
        }
        let Some(name) = names.remove(&range) else {
            continue;
        };
        if name
            .variant_owner
            .as_ref()
            .is_some_and(|owner| !owner.confirmed(&claimed))
            || name
                .member_owner
                .is_some_and(|owner| claimed.get(&owner) != Some(&true))
        {
            continue;
        }
        let Some((symbol, parent)) = item_identity(module, &name.relative) else {
            continue;
        };
        let only_type_witness = if let Some(object) = objects.get(&symbol) {
            if !associated_members::nominal(name.kind.exact) || object.kind.0 != "impl_item" {
                continue;
            }
            true
        } else {
            false
        };
        output
            .entry((dependency, symbol.clone()))
            .and_modify(|entry| *entry = None)
            .or_insert(Some(Observed {
                unit,
                owner,
                name,
                range,
                line: line_of(starts, range.start()),
                invocation: observation.site.invocation,
                definition: observation.definition.range,
                definition_unit: definition.unit,
                dependency_definition: dependency
                    .then_some((definition.revision, definition.digest)),
                expansion_digest,
                parent,
                symbol,
                only_type_witness,
                needs_type_witness,
            }));
    }
}

fn local_macro_shadows(
    original: &SyntaxDocument,
    definition: &Definition<'_>,
    invocation: SourceRange,
) -> bool {
    let symbols = original.facts().symbols();
    symbols.iter().any(|macro_symbol| {
        macro_symbol.syntax_element_kind == Some("macro_definition")
            && macro_symbol.name == definition.name
            && macro_symbol.item_range.end <= invocation.start()
            && macro_symbol.container.as_deref().is_none_or(|container| {
                symbols.iter().any(|module| {
                    module.kind == "module"
                        && module.qualified_name == container
                        && module
                            .body_range
                            .is_some_and(|range| range.encloses(bytes(invocation)))
                })
            })
    })
}

fn selected_in_scope(
    definition: &Definition<'_>,
    invocation_unit: &PackageSourceUnit,
    original: &SyntaxDocument,
    site: &RustLspMacroSite,
    module: &SymbolIdentity,
) -> bool {
    let bare = site.name.start() == site.invocation.start();
    if definition.identity.owner() != module.owner() {
        return !bare || !local_macro_shadows(original, definition, site.invocation);
    }
    if !bare {
        return false;
    }
    if definition.unit.unit == invocation_unit.unit {
        return definition.item_end <= site.invocation.start();
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
    output: Observations<'_>,
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
    let admitted = ordered_records(output);
    let has_fields = admitted.iter().any(|row| row.name.variant_owner.is_some());
    let mut variant_anchors = retained_variant_anchors(publication, has_fields);
    let type_anchors = admitted
        .iter()
        .filter(|row| row.only_type_witness)
        .map(|row| (row.symbol.clone(), row.unit.unit.clone()))
        .collect::<BTreeSet<_>>();
    let mut member_anchors = BTreeSet::new();
    for row in admitted {
        if row.only_type_witness
            || !known.contains(&row.parent)
            || (row.needs_type_witness
                && row.name.kind.exact == "function"
                && !type_anchors.contains(&(row.parent.clone(), row.unit.unit.clone())))
            || row.name.variant_owner.as_ref().is_some_and(|owner| {
                !variant_anchors.contains(&(row.parent.clone(), row.unit.unit.clone(), owner.name))
            })
            || row.name.member_owner.is_some_and(|owner| {
                !member_anchors.contains(&(row.parent.clone(), row.unit.unit.clone(), owner))
            })
        {
            continue;
        }
        let length = row.range.end() - row.range.start();
        if remaining == 0 || length > record_max || length > bytes_left {
            report.dropped_at_limit += 1;
            continue;
        }
        if has_fields && row.name.kind.exact == "variant" {
            variant_anchors.insert((row.symbol.clone(), row.unit.unit.clone(), row.range));
        }
        if row.name.kind.exact == "function" {
            member_anchors.insert((row.symbol.clone(), row.unit.unit.clone(), row.range));
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

fn ordered_records(output: Observations<'_>) -> Vec<Observed<'_>> {
    let local_symbols = output
        .keys()
        .take_while(|(dependency, _)| !dependency)
        .map(|(_, symbol)| symbol.clone())
        .collect::<BTreeSet<_>>();
    let mut admitted = output
        .into_iter()
        .filter_map(|((dependency, symbol), row)| {
            (!dependency || !local_symbols.contains(&symbol)).then_some((dependency, row?))
        })
        .collect::<Vec<_>>();
    // Existing local and dependency items keep capacity before associated members.
    admitted.sort_by_key(|(dependency, row)| {
        (
            *dependency,
            row.name.kind.exact == "function" || row.name.member_owner.is_some(),
            row.name.variant_owner.is_some(),
            row.name.member_owner.is_some(),
        )
    });
    admitted.into_iter().map(|(_, row)| row).collect()
}

fn retained_variant_anchors(
    publication: &PackagePublication,
    has_fields: bool,
) -> BTreeSet<(SymbolId, PublishedUnitId, SourceRange)> {
    if !has_fields {
        return BTreeSet::new();
    }
    let variants = publication
        .objects
        .iter()
        .filter(|object| object.kinds.contains(&SymbolKind::Variant))
        .filter_map(|object| object.id.as_ref())
        .collect::<BTreeSet<_>>();
    publication
        .declarations
        .iter()
        .filter_map(|declaration| {
            if declaration.origin.source_kind != SourceKind::Authored
                || declaration.syntax_element_kind.as_deref() != Some("identifier")
                || !variants.contains(&declaration.symbol)
            {
                return None;
            }
            let range = SourceRange::new(declaration.range.start, declaration.range.end).ok()?;
            Some((declaration.symbol.clone(), declaration.unit.clone(), range))
        })
        .collect()
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
        syntax_element_kind: Some(
            if row.name.kind.exact == "parameter_binding" && row.name.name == "self" {
                "self"
            } else {
                "identifier"
            }
            .to_owned(),
        ),
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
    let mut provenance = serde_json::json!({
        "producer": producer,
        "invocation": [row.invocation.start(), row.invocation.end()],
        "definition": [row.definition.start(), row.definition.end()],
        "definition_unit": row.definition_unit.unit,
        "expansion_digest": row.expansion_digest.as_bytes(),
    });
    if let Some((revision, digest)) = row.dependency_definition {
        provenance["dependency_definition"] = serde_json::json!({
            "revision": revision,
            "source_digest": digest.as_bytes(),
        });
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
                data: provenance,
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

    const VARIANT_SOURCE: &str = r"
macro_rules! describe {
    (pub enum $name:ident<$l:lifetime> {
        $($variant:ident { $field:ident : $ty:ty } : [$inputs:expr] => [$outputs:expr]),*
    }) => {
        pub enum $name<$l> { $($variant { $field: $ty, generated: u8 }),* }
    };
}
describe! {
    pub enum Node<'a> {
        Leaf { value: &'a str } : [generated] => [1],
        Branch { value: u16 } : [0] => [1]
    }
}";
    const VARIANT_EXPANSION: &str = r"
pub enum Node<'a> {
    Leaf { value: &'a str, generated: u8 },
    Branch { value: u16, generated: u8 }
}";

    #[test]
    fn variant_field_proof_requires_original_group_and_confirmed_owner() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let limits = SyntaxLimits::DEFAULT;
        let source = VARIANT_SOURCE;
        let site = PackageAnalyzer::rust_lsp_macro_sites(&path, source, limits)[0];
        let fields = |confirmed| {
            PackageAnalyzer::rust_lsp_macro_variant_field_sites(
                &path,
                source,
                site.invocation,
                VARIANT_EXPANSION,
                limits,
                confirmed,
            )
        };
        let names = PackageAnalyzer::rust_lsp_macro_name_sites(
            &path,
            source,
            site.invocation,
            VARIANT_EXPANSION,
            limits,
        );
        assert_eq!(
            names
                .iter()
                .filter_map(|&name| text_at(source, name))
                .collect::<Vec<_>>(),
            ["Node", "Leaf", "Branch"]
        );
        assert!(fields(&names[..1]).is_empty());
        assert_eq!(fields(&names[..2]).len(), 1);
        assert_eq!(fields(&names).len(), 2);
        assert_eq!(fields(&[names[0], names[1], names[1], names[2]]).len(), 1);

        for malformed in ["value: u16, value: u16", "value: "] {
            let source = source.replace("value: u16", malformed);
            let site = PackageAnalyzer::rust_lsp_macro_sites(&path, &source, limits)[0];
            let confirmed = PackageAnalyzer::rust_lsp_macro_name_sites(
                &path,
                &source,
                site.invocation,
                VARIANT_EXPANSION,
                limits,
            );
            let remaining = PackageAnalyzer::rust_lsp_macro_variant_field_sites(
                &path,
                &source,
                site.invocation,
                VARIANT_EXPANSION,
                limits,
                &confirmed,
            );
            assert_eq!(remaining.len(), 1);
            assert_eq!(text_at(&source, remaining[0]), Some("value"));
            assert!(remaining[0].start() < confirmed[2].start());
        }
    }

    #[test]
    fn local_expansion_keeps_repeated_field_names_under_their_written_variants() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let limits = ExactPackageLimits::new(1024, 2 * 1024 * 1024);
        let site = PackageAnalyzer::rust_lsp_macro_sites(&path, VARIANT_SOURCE, limits.syntax())[0];
        let mut names = PackageAnalyzer::rust_lsp_macro_name_sites(
            &path,
            VARIANT_SOURCE,
            site.invocation,
            VARIANT_EXPANSION,
            limits.syntax(),
        );
        names.extend(PackageAnalyzer::rust_lsp_macro_variant_field_sites(
            &path,
            VARIANT_SOURCE,
            site.invocation,
            VARIANT_EXPANSION,
            limits.syntax(),
            &names,
        ));
        let definition = VARIANT_SOURCE.find("describe").expect("macro name");
        let definition = u64::try_from(definition).expect("source offset");
        let digest = FileDigest::of(VARIANT_SOURCE.as_bytes());
        let observation = RustLspMacroItems {
            site,
            definition: RustLspMacroDefinition {
                path: &path,
                digest,
                range: SourceRange::new(definition, definition + 8).expect("macro range"),
            },
            expansion: VARIANT_EXPANSION,
            names: &names,
        };
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("1").expect("revision"),
        };
        let base = authored_macro_fixture(vec![("src/lib.rs", VARIANT_SOURCE)]);
        let apply = |publication: &mut PackagePublication, observation, limits| {
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
            .expect("bounded scoped field admission")
        };

        let mut complete = base.clone();
        assert_eq!(apply(&mut complete, observation, limits).admitted, 5);
        assert_variant_field_publication(&complete);

        let mut capped = base.clone();
        let bound = limits
            .with_declarations(u32::try_from(base.declarations.len() + 3).expect("fixture count"))
            .expect("fixture bound");
        let report = apply(&mut capped, observation, bound);
        assert_eq!((report.admitted, report.dropped_at_limit), (3, 2));
        assert!(
            capped
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().ends_with("/Node/Leaf"))
        );
        assert!(
            capped
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().ends_with("/Node/Branch"))
        );
        assert_eq!(apply(&mut capped, observation, limits).admitted, 2);
        assert_eq!(capped, complete);

        let mut incomplete = base.clone();
        let subset = names
            .iter()
            .copied()
            .filter(|&name| text_at(VARIANT_SOURCE, name) != Some("Branch"))
            .collect::<Vec<_>>();
        assert_eq!(
            apply(
                &mut incomplete,
                RustLspMacroItems {
                    names: &subset,
                    ..observation
                },
                limits
            )
            .admitted,
            3
        );
        assert!(
            incomplete
                .declarations
                .iter()
                .all(|row| !row.symbol.as_str().contains("/Node/Branch"))
        );
    }

    fn assert_variant_field_publication(complete: &PackagePublication) {
        for (variant, written) in [("Leaf", "value: &'a str"), ("Branch", "value: u16")] {
            let start = u64::try_from(VARIANT_SOURCE.find(written).expect("written field"))
                .expect("source offset");
            let suffix = format!("/Node/{variant}/value");
            let field = complete
                .declarations
                .iter()
                .find(|row| row.symbol.as_str().ends_with(&suffix))
                .expect("variant field declaration");
            assert_eq!(field.source, "value");
            assert_eq!((field.range.start, field.range.end), (start, start + 5));
            assert!(field.signature_indices.is_empty());
        }
        assert!(
            complete
                .declarations
                .iter()
                .all(|row| !row.symbol.as_str().ends_with("/generated"))
        );
        complete.validate_facts().expect("validated publication");
    }

    const DEPENDENCY_SOURCE: &str = r"
#[macro_export]
macro_rules! define { ($name:ident) => { pub struct $name; }; }
";
    const CONSUMER_SOURCE: &str = r"
use support::define;
define!(Bare);
support::define!(Qualified);
pub mod shadow {
    macro_rules! define { ($name:ident) => { pub struct $name; }; }
    define!(Shadowed);
    support::define!(StillQualified);
}
";

    struct DependencyCase {
        base: PackagePublication,
        dependency: PackagePublication,
        unit: ProjectPath,
        locator: ProjectPath,
        limits: ExactPackageLimits,
    }

    impl DependencyCase {
        fn new() -> Self {
            let mut package = fixture::identity();
            package.name = "support".to_owned();
            let owner = package.owner().expect("fixture owner");
            let origin = fixture::origin(&package);
            let language = fixture::language(ShippedLanguage::Rust);
            let unit = ProjectPath::new("src/lib.rs").expect("fixture unit");
            let manifest = ProjectPath::new("Cargo.toml").expect("fixture manifest");
            let metadata = [crate::PackageSource::new(
                &manifest,
                "[package]\nname='support'\nversion='1.0.0'\nedition='2024'\n",
            )];
            let sources = [crate::PackageSource::new(&unit, DEPENDENCY_SOURCE)];
            let limits = ExactPackageLimits::new(8, 16_384);
            let input = crate::ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
                .expect("checked dependency input")
                .with_framework_context(&metadata, &[])
                .expect("checked dependency metadata");
            let dependency = PackageAnalyzer::analyze_with_syntax(input, 1, |_| None)
                .expect("authored dependency")
                .publication()
                .clone();
            Self {
                base: authored_macro_fixture(vec![("src/lib.rs", CONSUMER_SOURCE)]),
                dependency,
                unit,
                locator: ProjectPath::new("support/src/lib.rs").expect("engine locator"),
                limits,
            }
        }

        fn authority(&self) -> RustLspMacroDependency<'_> {
            RustLspMacroDependency {
                locator: &self.locator,
                unit: &self.unit,
                publication: &self.dependency,
            }
        }

        fn observe(
            &self,
            dependencies: &[RustLspMacroDependency<'_>],
            digest: FileDigest,
            limits: ExactPackageLimits,
            select_local: bool,
        ) -> PackagePublication {
            let sites = PackageAnalyzer::rust_lsp_dependency_macro_sites(
                &self.unit,
                CONSUMER_SOURCE,
                limits.syntax(),
            );
            assert_eq!(sites.len(), 4);
            assert_eq!(PackageAnalyzer::rust_lsp_macro_sites(
                &self.unit, CONSUMER_SOURCE, limits.syntax(),
            ).len(), 2);
            let generated = [
                "pub struct Bare;",
                "pub struct Qualified;",
                "pub struct Shadowed;",
                "pub struct StillQualified;",
            ];
            let names = sites
                .iter()
                .zip(generated)
                .map(|(site, expansion)| {
                    let names = PackageAnalyzer::rust_lsp_macro_name_sites(
                        &self.unit,
                        CONSUMER_SOURCE,
                        site.invocation,
                        expansion,
                        limits.syntax(),
                    );
                    assert_eq!(names.len(), 1);
                    names
                })
                .collect::<Vec<_>>();
            let start = DEPENDENCY_SOURCE.find("define").expect("export name") as u64;
            let mut observations = sites
                .iter()
                .copied()
                .zip(generated)
                .zip(&names)
                .map(|((site, expansion), names)| RustLspMacroItems {
                    site,
                    definition: RustLspMacroDefinition {
                        path: &self.locator,
                        digest,
                        range: SourceRange::new(start, start + 6).expect("export range"),
                    },
                    expansion,
                    names,
                })
                .collect::<Vec<_>>();
            if select_local {
                let local = CONSUMER_SOURCE
                    .find("macro_rules! define")
                    .expect("shadowing macro") as u64
                    + 13;
                observations.push(RustLspMacroItems {
                    definition: RustLspMacroDefinition {
                        path: &self.unit,
                        digest: FileDigest::of(CONSUMER_SOURCE.as_bytes()),
                        range: SourceRange::new(local, local + 6).expect("local macro range"),
                    },
                    ..observations[2]
                });
            }
            let producer = FactProducer {
                name: ProducerName::parse("rust-analyzer").expect("fixture producer"),
                revision: ProducerRevision::parse("1").expect("fixture revision"),
            };
            let mut result = self.base.clone();
            PackageAnalyzer::admit_rust_lsp_macro_items_with_dependencies(
                &mut result,
                &[RustLspMacroFile {
                    path: &self.unit,
                    digest: FileDigest::of(CONSUMER_SOURCE.as_bytes()),
                    expansions: &observations,
                }],
                dependencies,
                &producer,
                limits,
            )
            .expect("checked macro admissions");
            result
        }
    }

    #[test]
    fn dependency_macro_names_keep_consumer_identity_and_definition_provenance() {
        let fixture = DependencyCase::new();
        let digest = FileDigest::of(DEPENDENCY_SOURCE.as_bytes());
        let accepted = fixture.observe(&[fixture.authority()], digest, fixture.limits, true);
        for name in ["Bare", "Qualified", "StillQualified", "Shadowed"] {
            let declaration = accepted
                .declarations
                .iter()
                .find(|row| row.source == name)
                .expect("authored macro name");
            assert!(declaration.unit.as_str().contains("/beacon/src/lib.rs"));
            let object = accepted
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&declaration.symbol))
                .expect("consumer object");
            let provenance =
                &object.extensions.0[&ExtensionKey("org.lsp.rust-macro-name".to_owned())].data;
            let expected_owner = if name == "Shadowed" {
                "beacon"
            } else {
                "support"
            };
            assert!(
                provenance["definition_unit"]
                    .as_str()
                    .expect("definition provenance")
                    .ends_with(&format!("/{expected_owner}/src/lib.rs"))
            );
            if name == "Shadowed" {
                assert!(provenance.get("dependency_definition").is_none());
            } else {
                assert_eq!(
                    provenance["dependency_definition"],
                    serde_json::json!({
                        "revision": fixture.dependency.revision,
                        "source_digest": digest.as_bytes(),
                    })
                );
            }
        }
        assert_eq!(
            accepted.declarations.len(),
            fixture.base.declarations.len() + 4
        );
        assert_eq!(accepted.edges, fixture.base.edges);
        accepted
            .validate_facts()
            .expect("published source and owned identities");
        let dependency_only =
            fixture.observe(&[fixture.authority()], digest, fixture.limits, false);
        assert_eq!(
            dependency_only.declarations.len(),
            fixture.base.declarations.len() + 3
        );
        assert!(
            dependency_only
                .declarations
                .iter()
                .all(|row| row.source != "Shadowed")
        );
    }

    #[test]
    fn dependency_definition_refusals_and_caps_preserve_local_macro_names() {
        let fixture = DependencyCase::new();
        let digest = FileDigest::of(DEPENDENCY_SOURCE.as_bytes());
        let local = fixture.observe(&[], digest, fixture.limits, true);
        assert_eq!(
            local.declarations.len(),
            fixture.base.declarations.len() + 1
        );
        for (dependencies, asserted_digest) in [
            (vec![fixture.authority(), fixture.authority()], digest),
            (vec![fixture.authority()], FileDigest::of(b"changed")),
            (
                vec![RustLspMacroDependency {
                    locator: &fixture.unit,
                    ..fixture.authority()
                }],
                digest,
            ),
        ] {
            assert_eq!(
                fixture.observe(&dependencies, asserted_digest, fixture.limits, true),
                local
            );
        }
        let limits = fixture
            .limits
            .with_declarations(
                u32::try_from(fixture.base.declarations.len() + 1)
                    .expect("fixture declaration count"),
            )
            .expect("one supplemental record");
        assert_eq!(
            fixture.observe(&[fixture.authority()], digest, limits, true),
            local
        );
    }

    const MEMBERS_SOURCE: &str = r"
use support::define;
pub struct First;
pub struct Second;
impl First {
    define! {
        #[doc = stringify!(score)]
        fn score(&self, amount: u32) -> u32;
    }
}
impl First {
    support::define!(fn check(&self, delta: u32) -> u32;);
}
impl Second {
    support::define!(fn score(&self, ticket: u32) -> u32;);
}
trait Other {}
impl Other for First {
    support::define!(fn refused(&self););
}
mod shadow {
    pub struct Third;
    macro_rules! define { ($name:ident) => { pub struct $name; }; }
    impl Third { define!(fn refused(&self);); }
}
";

    const MEMBER_EXPANSIONS: [&str; 4] = [
        "pub fn score(&self, amount: u32) -> u32 { amount }",
        "pub fn check(&self, delta: u32) -> u32 { delta }",
        "pub fn score(&self, ticket: u32) -> u32 { ticket }",
        "pub fn refused(&self) {}",
    ];

    struct MemberFixture {
        dependency: DependencyCase,
        publication: PackagePublication,
        sites: Vec<RustLspMacroSite>,
        names: Vec<Vec<SourceRange>>,
        definition: SourceRange,
    }

    impl MemberFixture {
        fn new() -> Self {
            let dependency = DependencyCase::new();
            let sites = PackageAnalyzer::rust_lsp_dependency_macro_member_sites(
                &dependency.unit,
                MEMBERS_SOURCE,
                dependency.limits.syntax(),
            );
            assert_eq!(
                sites.len(),
                4,
                "trait implementations remain outside this route"
            );
            let names = sites
                .iter()
                .zip(MEMBER_EXPANSIONS)
                .map(|(site, expansion)| {
                    PackageAnalyzer::rust_lsp_macro_member_name_sites(
                        &dependency.unit,
                        MEMBERS_SOURCE,
                        site.invocation,
                        expansion,
                        dependency.limits.syntax(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(names.iter().map(Vec::len).collect::<Vec<_>>(), [3, 3, 3, 2]);
            let start = u64::try_from(DEPENDENCY_SOURCE.find("define").expect("export"))
                .expect("export byte position");
            Self {
                dependency,
                publication: authored_macro_fixture(vec![("src/lib.rs", MEMBERS_SOURCE)]),
                sites,
                names,
                definition: SourceRange::new(start, start + 6).expect("export name"),
            }
        }

        fn collect(
            &self,
            dependencies: &[RustLspMacroDependency<'_>],
            limits: ExactPackageLimits,
        ) -> PackagePublication {
            let observations = self
                .sites
                .iter()
                .copied()
                .zip(MEMBER_EXPANSIONS)
                .zip(&self.names)
                .map(|((site, expansion), names)| RustLspMacroItems {
                    site,
                    definition: RustLspMacroDefinition {
                        path: &self.dependency.locator,
                        digest: FileDigest::of(DEPENDENCY_SOURCE.as_bytes()),
                        range: self.definition,
                    },
                    expansion,
                    names,
                })
                .collect::<Vec<_>>();
            let mut result = self.publication.clone();
            PackageAnalyzer::admit_rust_lsp_macro_items_with_dependencies(
                &mut result,
                &[RustLspMacroFile {
                    path: &self.dependency.unit,
                    digest: FileDigest::of(MEMBERS_SOURCE.as_bytes()),
                    expansions: &observations,
                }],
                dependencies,
                &FactProducer {
                    name: ProducerName::parse("rust-analyzer").expect("producer"),
                    revision: ProducerRevision::parse("1").expect("revision"),
                },
                limits,
            )
            .expect("checked macro members");
            result
        }
    }

    #[test]
    fn dependency_impl_members_keep_written_type_scope_and_parameters() {
        let fixture = MemberFixture::new();
        let result = fixture.collect(&[fixture.dependency.authority()], fixture.dependency.limits);
        for (relative, argument) in [
            ("First/score", "amount"),
            ("First/check", "delta"),
            ("Second/score", "ticket"),
        ] {
            let method = result
                .declarations
                .iter()
                .find(|row| row.symbol.as_str().ends_with(relative))
                .expect("written method declaration");
            let names = [
                format!("{relative}/self%23parameter0"),
                format!("{relative}/{argument}%23parameter0"),
            ];
            for name in names {
                let object = result
                    .objects
                    .iter()
                    .find(|row| {
                        row.id
                            .as_ref()
                            .is_some_and(|symbol| symbol.as_str().ends_with(&name))
                    })
                    .expect("written parameter");
                assert_eq!(object.container.as_ref(), Some(&method.symbol));
            }
            let start = usize::try_from(method.range.start).expect("source offset");
            assert!(MEMBERS_SOURCE[..start].ends_with("fn "));
        }
        assert_eq!(
            result.declarations.len(),
            fixture.publication.declarations.len() + 9
        );
        assert_eq!(result.edges, fixture.publication.edges);
        result
            .validate_facts()
            .expect("checked owners and source witnesses");
    }

    #[test]
    fn dependency_impl_members_refuse_missing_authority_and_uncorroborated_parameters() {
        let mut fixture = MemberFixture::new();
        assert_eq!(
            fixture.collect(&[], fixture.dependency.limits),
            fixture.publication
        );
        assert_eq!(
            fixture.collect(
                &[
                    fixture.dependency.authority(),
                    fixture.dependency.authority()
                ],
                fixture.dependency.limits,
            ),
            fixture.publication
        );
        let count = u32::try_from(fixture.publication.declarations.len() + 1)
            .expect("fixture declaration count");
        let capped = fixture.collect(
            &[fixture.dependency.authority()],
            fixture
                .dependency
                .limits
                .with_declarations(count)
                .expect("one member"),
        );
        assert_eq!(
            capped.declarations.len(),
            fixture.publication.declarations.len() + 1
        );
        let unconfirmed = fixture.names[0].remove(0);
        assert_eq!(text_at(MEMBERS_SOURCE, unconfirmed), Some("score"));
        let reduced = fixture.collect(&[fixture.dependency.authority()], fixture.dependency.limits);
        assert_eq!(
            reduced.declarations.len(),
            fixture.publication.declarations.len() + 6
        );
        assert!(
            !reduced
                .declarations
                .iter()
                .any(|row| row.symbol.as_str().contains("/First/score"))
        );
    }

    #[test]
    fn dependency_impl_requires_a_concrete_nominal_type_or_an_independent_written_witness() {
        let source =
            "support::define!(Product);\nimpl Product { support::define!(fn show(&self);); }";
        let fixture = DependencyCase::new();
        let scope = fixture.limits.syntax();
        let root = PackageAnalyzer::rust_lsp_dependency_macro_sites(&fixture.unit, source, scope);
        let members =
            PackageAnalyzer::rust_lsp_dependency_macro_member_sites(&fixture.unit, source, scope);
        let written_type = "pub struct Product;";
        let written_method = "pub fn show(&self) {}";
        let type_names = PackageAnalyzer::rust_lsp_macro_name_sites(
            &fixture.unit,
            source,
            root[0].invocation,
            written_type,
            scope,
        );
        let member_names = PackageAnalyzer::rust_lsp_macro_member_name_sites(
            &fixture.unit,
            source,
            members[0].invocation,
            written_method,
            scope,
        );
        let start = u64::try_from(DEPENDENCY_SOURCE.find("define").expect("macro name"))
            .expect("export byte position");
        let definition = RustLspMacroDefinition {
            path: &fixture.locator,
            digest: FileDigest::of(DEPENDENCY_SOURCE.as_bytes()),
            range: SourceRange::new(start, start + 6).expect("exact export token"),
        };
        let root_row = RustLspMacroItems {
            site: root[0],
            definition,
            expansion: written_type,
            names: &type_names,
        };
        let member_row = RustLspMacroItems {
            site: members[0],
            definition,
            expansion: written_method,
            names: &member_names,
        };
        let base = authored_macro_fixture(vec![("src/lib.rs", source)]);
        let Some(placeholder) = base.objects.iter().find(|row| row.name == "Product") else {
            panic!("retained authored implementation");
        };
        assert_eq!(placeholder.kind.0, "impl_item");
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("1").expect("revision"),
        };
        for (observed, admitted) in [
            (vec![member_row], 0),
            (vec![root_row, member_row], 2),
            (vec![root_row, root_row, member_row], 0),
        ] {
            let mut graph = base.clone();
            let report = PackageAnalyzer::admit_rust_lsp_macro_items_with_dependencies(
                &mut graph,
                &[RustLspMacroFile {
                    path: &fixture.unit,
                    digest: FileDigest::of(source.as_bytes()),
                    expansions: &observed,
                }],
                &[fixture.authority()],
                &producer,
                fixture.limits,
            )
            .expect("checked owner scope");
            assert_eq!(report.admitted, admitted);
            assert_eq!(
                graph.objects.iter().find(|row| row.name == "Product"),
                Some(placeholder),
            );
        }
    }

    const CONSTANT_SOURCE: &str = r"
    pub struct Board;
    pub const STABLE: u8 = 0;
    macro_rules! publish_values { ($($tokens:tt)*) => { $($tokens)* }; }
    publish_values!(TOP, SLOT, SECOND);
    pub fn read() -> u8 { STABLE }
    ";
    const CONSTANT_EXPANSION: &str =
        "pub const TOP: u8 = 1; impl Board { pub const SLOT: u8 = 2; pub const SECOND: u8 = 3; }";

    #[test]
    fn macro_constants_require_retained_nominal_and_exact_native_roles() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture unit");
        let limits = ExactPackageLimits::new(16, 2 * 1024 * 1024);
        let digest = FileDigest::of(CONSTANT_SOURCE.as_bytes());
        let token = |name: &str| {
            let offset = CONSTANT_SOURCE.find(name).expect("written claim");
            SourceRange::new(offset as u64, (offset + name.len()) as u64).expect("token range")
        };
        let observation = RustLspMacroItems {
            site: PackageAnalyzer::rust_lsp_macro_sites(&path, CONSTANT_SOURCE, limits.syntax())[0],
            definition: RustLspMacroDefinition {
                path: &path,
                digest,
                range: token("publish_values"),
            },
            expansion: CONSTANT_EXPANSION,
            names: &[],
        };
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("1").expect("revision"),
        };
        let base = authored_macro_fixture(vec![(path.as_str(), CONSTANT_SOURCE)]);
        let labels = [
            (token("TOP"), false),
            (token("SLOT"), true),
            (token("SECOND"), true),
        ];
        let publish =
            |graph: &mut PackagePublication, observation, labels: &[_], digest, limits| {
                PackageAnalyzer::admit_rust_lsp_macro_constants(
                    graph,
                    &[RustLspMacroConstantFile {
                        macros: RustLspMacroFile {
                            path: &path,
                            digest,
                            expansions: &[observation],
                        },
                        declarations: labels,
                    }],
                    &producer,
                    limits,
                )
                .expect("canonical constant admission")
            };
        let mut graph = base.clone();
        let report = publish(&mut graph, observation, &labels, digest, limits);
        assert_eq!(report.admitted, 3);
        assert_exact_constant_facets(&graph, &base, &labels);
        let limited = limits
            .with_declarations(u32::try_from(base.declarations.len() + 1).expect("capacity"))
            .expect("one remaining declaration");
        assert_eq!(
            publish(&mut base.clone(), observation, &labels, digest, limited),
            RustLspMacroAdmission {
                admitted: 1,
                dropped_at_limit: 2
            }
        );
        assert_eq!(
            publish(
                &mut base.clone(),
                observation,
                &labels,
                FileDigest::of(b"stale"),
                limits
            )
            .admitted,
            0
        );
        let mismatched = [
            (token("TOP"), true),
            (token("SLOT"), false),
            (token("SECOND"), true),
        ];
        assert_eq!(
            publish(&mut base.clone(), observation, &mismatched, digest, limits).admitted,
            1
        );
        let duplicate = [labels[0], labels[1], labels[1], labels[2]];
        assert_eq!(
            publish(&mut base.clone(), observation, &duplicate, digest, limits).admitted,
            2
        );
    }

    fn assert_exact_constant_facets(
        graph: &PackagePublication,
        base: &PackagePublication,
        labels: &[(SourceRange, bool)],
    ) {
        let mut admitted = 0;
        for (name, suffix, (range, _)) in [
            ("TOP", "/TOP", labels[0]),
            ("SLOT", "/Board/SLOT", labels[1]),
            ("SECOND", "/Board/SECOND", labels[2]),
        ] {
            let row = graph
                .declarations
                .iter()
                .find(|row| {
                    row.symbol.as_str().ends_with(suffix)
                        && row.range.start == range.start()
                        && row.range.end == range.end()
                })
                .expect("unique complete authored facet");
            assert_eq!(row.source, name);
            assert!(row.public);
            assert!(row.signature_indices.is_empty());
            if let Some(object) = graph
                .objects
                .iter()
                .find(|object| object.id.as_ref() == Some(&row.symbol))
            {
                assert_eq!(object.kind.0, "constant");
                assert!(object.kinds.contains(&SymbolKind::Value));
                admitted += 1;
            }
        }
        assert_eq!(admitted, 3);
        assert!(
            base.declarations
                .iter()
                .all(|row| graph.declarations.contains(row))
        );
        assert_eq!(graph.edges, base.edges);
        graph.validate_facts().expect("valid constant publication");
    }

    #[test]
    fn foreign_declarations_keep_verified_same_source_and_imported_callable_owners() {
        const DEFINITION: &str =
            "macro_rules! declare_abi { ($($tokens:tt)*) => { $($tokens)* }; }";
        const WRITTEN: &str =
            "declare_abi! { pub fn first(item: u32) -> i32, pub fn second(item: u8) -> i32, }";
        const EMITTED: &str =
            "unsafe extern \"C\" { pub fn first(item:u32)->i32; pub fn second(item:u8)->i32; }";
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("1").expect("revision"),
        };
        let limits = ExactPackageLimits::new(16, 2 * 1024 * 1024);
        let local = format!("{DEFINITION}\n{WRITTEN}");
        let direct = "unsafe extern \"C\" { pub fn stable(item: u8) -> i32; }";
        for (root, source, consumer, definition, authorized) in [
            ("", local.as_str(), "src/lib.rs", "src/lib.rs", true),
            (
                "#[macro_use] mod definitions; pub mod api;",
                WRITTEN,
                "src/api.rs",
                "src/definitions.rs",
                true,
            ),
            (
                "mod definitions; pub mod api;",
                WRITTEN,
                "src/api.rs",
                "src/definitions.rs",
                false,
            ),
            (
                "pub mod api; #[macro_use] mod definitions;",
                WRITTEN,
                "src/api.rs",
                "src/definitions.rs",
                false,
            ),
        ] {
            let owned_root = format!("{root}\n{direct}");
            let fixture = if consumer == "src/lib.rs" {
                vec![("src/lib.rs", source)]
            } else {
                vec![
                    ("src/lib.rs", owned_root.as_str()),
                    ("src/definitions.rs", DEFINITION),
                    (consumer, source),
                ]
            };
            let base = authored_macro_fixture(fixture);
            let path = ProjectPath::new(consumer).expect("consumer");
            let defining = ProjectPath::new(definition).expect("definition");
            let defining_source = if definition == consumer {
                source
            } else {
                DEFINITION
            };
            let definition_offset = u64::try_from(
                defining_source
                    .find("declare_abi")
                    .expect("definition token"),
            )
            .expect("offset");
            let site = PackageAnalyzer::rust_lsp_macro_sites(&path, source, limits.syntax())[0];
            let expansion = RustLspMacroItems {
                site,
                definition: RustLspMacroDefinition {
                    path: &defining,
                    digest: FileDigest::of(defining_source.as_bytes()),
                    range: SourceRange::new(definition_offset, definition_offset + 11)
                        .expect("definition range"),
                },
                expansion: EMITTED,
                names: &[],
            };
            let tokens = foreign_declarations::candidates(
                &path,
                source,
                site.invocation,
                EMITTED,
                limits.syntax(),
            );
            assert_eq!(tokens.len(), 4);
            let exact = tokens
                .into_iter()
                .map(|(range, (role, _))| (range, role == foreign_declarations::Role::Function))
                .collect::<Vec<_>>();
            let file = RustLspForeignDeclarationFile {
                macros: RustLspMacroFile {
                    path: &path,
                    digest: FileDigest::of(source.as_bytes()),
                    expansions: &[expansion],
                },
                declarations: &exact,
            };
            assert_foreign_labels(&base, file, authorized, &producer, limits);
            if authorized {
                assert_foreign_capacity(&base, file, &producer, limits);
            }
        }
    }

    fn assert_foreign_labels(
        base: &PackagePublication,
        file: RustLspForeignDeclarationFile<'_>,
        authorized: bool,
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) {
        let mut missing = file.declarations.to_vec();
        missing.pop();
        let mut duplicate = file.declarations.to_vec();
        duplicate.push(file.declarations[1]);
        let mut wrong_role = file.declarations.to_vec();
        wrong_role[0].1 = false;
        for (labels, accepted) in [
            (file.declarations, 4),
            (&missing, 2),
            (&duplicate, 2),
            (&wrong_role, 2),
        ] {
            let mut graph = base.clone();
            let admission = PackageAnalyzer::admit_rust_lsp_foreign_declarations(
                &mut graph,
                &[RustLspForeignDeclarationFile {
                    declarations: labels,
                    ..file
                }],
                producer,
                limits,
            )
            .expect("valid foreign proof");
            assert_eq!(admission.admitted, if authorized { accepted } else { 0 });
            assert_eq!(graph.edges, base.edges);
            graph.validate_facts().expect("valid authored graph");
            if authorized && accepted == 4 {
                let names = graph
                    .declarations
                    .iter()
                    .filter(|row| row.source == "item")
                    .map(|row| row.symbol.as_str())
                    .collect::<Vec<_>>();
                assert!(
                    names
                        .iter()
                        .any(|name| name.ends_with("/first/item%23parameter0"))
                );
                assert!(
                    names
                        .iter()
                        .any(|name| name.ends_with("/second/item%23parameter0"))
                );
            }
        }
    }

    fn assert_foreign_capacity(
        base: &PackagePublication,
        file: RustLspForeignDeclarationFile<'_>,
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) {
        let limited = limits
            .with_declarations(
                u32::try_from(base.declarations.len() + 1).expect("fixture allowance"),
            )
            .expect("valid declaration allowance");
        let mut graph = base.clone();
        let refused = RustLspForeignDeclarationFile {
            macros: RustLspMacroFile {
                digest: FileDigest::of(b"changed"),
                ..file.macros
            },
            ..file
        };
        assert_eq!(
            PackageAnalyzer::admit_rust_lsp_foreign_declarations(
                &mut graph,
                &[refused],
                producer,
                limits,
            )
            .expect("wrong digest fallback")
            .admitted,
            0
        );
        let capped = PackageAnalyzer::admit_rust_lsp_foreign_declarations(
            &mut graph,
            &[file],
            producer,
            limited,
        )
        .expect("bounded publication");
        assert_eq!(capped.admitted, 1);
        assert!(capped.dropped_at_limit > 0);
        assert!(
            base.declarations
                .iter()
                .all(|row| graph.declarations.contains(row))
        );
        assert_eq!(graph.edges, base.edges);
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
