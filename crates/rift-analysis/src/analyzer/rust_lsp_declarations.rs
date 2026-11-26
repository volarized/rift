//! Admits authored Rust binding locations confirmed by a language server.

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
use rift_syntax::{ByteRange, SyntaxElement, SyntaxSource, registry::provider_for_language};

use super::{PackageAnalyzer, bound, digest_of, file_digest, line_of};
use crate::{EnclosingDefinitions, ExactPackageLimits};

/// One source-local declaration confirmed by a Rust language server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustLspDeclaration {
    /// Physical byte range in the exact source.
    pub range: SourceRange,
    /// Whether the binding is a formal parameter.
    pub parameter: bool,
}

/// A complete token answer tied to the bytes opened with the language server.
#[derive(Debug, Clone)]
pub struct RustLspDeclarationFile<'source> {
    /// Package-relative path of the opened source.
    pub path: &'source ProjectPath,
    /// Digest of the opened bytes.
    pub digest: FileDigest,
    /// Source-local observations with macro tokens excluded by the adapter.
    pub declarations: &'source [RustLspDeclaration],
}

/// Outcome of supplementing an existing package publication with Rust declarations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RustLspDeclarationAdmission {
    /// Physical declarations and logical objects added together.
    pub admitted: usize,
    /// Otherwise accepted declarations left out by existing publication bounds.
    pub dropped_at_limit: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingProof {
    SemanticToken,
    ShorthandDefinition,
}

impl PackageAnalyzer {
    /// Adds physically authored Rust bindings under their retained callable owners.
    ///
    /// Complete source and its full digest must match a retained unit of this package.
    /// The token must match an authored syntax element under an admitted callable.
    /// Existing declarations keep their IDs and consume capacity first.
    ///
    /// # Errors
    /// Returns an error when a new declaration's canonical record cannot be rendered.
    pub fn admit_rust_lsp_declarations(
        publication: &mut PackagePublication,
        observed: &[RustLspDeclarationFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspDeclarationAdmission, RiftError> {
        Self::admit_rust_lsp_bindings(
            publication,
            observed,
            producer,
            limits,
            BindingProof::SemanticToken,
        )
    }

    /// Adds authored Rust shorthand bindings selected by a definition response.
    ///
    /// Each observation must be the only local definition at the same physical token on the
    /// opened source. A response may also name a field; its location confers no binding authority.
    /// Declaration tokens and retained syntax bindings consume capacity before these observations.
    /// # Errors
    /// Returns an error when a new declaration's canonical record cannot be rendered.
    pub fn admit_rust_lsp_shorthand_declarations(
        publication: &mut PackagePublication,
        observed: &[RustLspDeclarationFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> Result<RustLspDeclarationAdmission, RiftError> {
        Self::admit_rust_lsp_bindings(
            publication,
            observed,
            producer,
            limits,
            BindingProof::ShorthandDefinition,
        )
    }

    fn admit_rust_lsp_bindings(
        publication: &mut PackagePublication,
        observed: &[RustLspDeclarationFile<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
        proof: BindingProof,
    ) -> Result<RustLspDeclarationAdmission, RiftError> {
        if observed.is_empty() {
            return Ok(RustLspDeclarationAdmission::default());
        }

        let mut output = Supplement::new(publication, producer, limits, proof);
        let units = publication
            .units
            .iter()
            .filter_map(|unit| {
                (unit.source_complete && unit.language.name == "rust")
                    .then_some((unit.path.0.as_str(), unit))
            })
            .collect::<BTreeMap<_, _>>();
        let mut files = BTreeMap::new();
        for file in observed {
            files
                .entry(file.path.as_str())
                .and_modify(|retained| *retained = None)
                .or_insert(Some(file));
        }
        let mut declarations = BTreeMap::<&str, Vec<&PackageSymbol>>::new();
        for declaration in &publication.declarations {
            declarations
                .entry(declaration.unit.as_str())
                .or_default()
                .push(declaration);
        }
        let mut known = publication
            .objects
            .iter()
            .filter_map(|object| object.id.as_ref())
            .cloned()
            .collect::<BTreeSet<_>>();
        for (path, observation) in files {
            let Some((file, unit)) = observation.zip(units.get(path)) else {
                continue;
            };
            let Some(existing) = declarations.get(unit.unit.as_str()) else {
                continue;
            };
            if !matches_source(publication, file, unit) {
                continue;
            }
            output.read_file(publication, file, unit, existing, &mut known)?;
        }

        let report = RustLspDeclarationAdmission {
            admitted: output.declarations.len(),
            dropped_at_limit: output.dropped,
        };
        if report.admitted != 0 {
            publication.objects.extend(output.objects);
            publication
                .objects
                .sort_by(|left, right| left.id.cmp(&right.id));
            publication.declarations.extend(output.declarations);
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

fn matches_source(
    publication: &PackagePublication,
    file: &RustLspDeclarationFile<'_>,
    unit: &PackageSourceUnit,
) -> bool {
    let Ok(identity) = SourceUnitId::parse(unit.unit.as_str()) else {
        return false;
    };
    identity.source_owner() == Some(&publication.owner)
        && identity.key().as_str() == file.path.as_str()
        && file.digest == FileDigest::of(unit.source.as_bytes())
        && file_digest(file.digest) == unit.content_digest
}

struct Supplement<'a> {
    objects: Vec<Symbol>,
    declarations: Vec<PackageSymbol>,
    producer: &'a FactProducer,
    limits: ExactPackageLimits,
    slots: usize,
    bytes: u64,
    record_bytes: u64,
    dropped: usize,
    proof: BindingProof,
}

impl<'a> Supplement<'a> {
    fn new(
        publication: &PackagePublication,
        producer: &'a FactProducer,
        limits: ExactPackageLimits,
        proof: BindingProof,
    ) -> Self {
        let symbols = bound(limits.publication().symbols);
        let slots = symbols.saturating_sub(publication.objects.len()).min(
            symbols
                .min(limits.declarations_max())
                .saturating_sub(publication.declarations.len()),
        );
        let (bytes, record_bytes) = limits.retained_source().map_or(
            (
                u64::MAX,
                u64::from(rift_protocol::index::PACKAGE_SOURCE_BYTES_MAX),
            ),
            |retained| {
                (
                    retained
                        .total_bytes_max
                        .saturating_sub(retained_source_bytes(publication)),
                    u64::from(retained.record_bytes_max),
                )
            },
        );
        Self {
            objects: Vec::new(),
            declarations: Vec::new(),
            producer,
            limits,
            slots,
            bytes,
            record_bytes,
            dropped: 0,
            proof,
        }
    }

    fn read_file(
        &mut self,
        publication: &PackagePublication,
        file: &RustLspDeclarationFile<'_>,
        unit: &PackageSourceUnit,
        existing: &[&PackageSymbol],
        known: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let Some(provider) = provider_for_language(&unit.language) else {
            return Ok(());
        };
        let Ok(syntax) = provider.analyze(
            SyntaxSource {
                path: file.path,
                text: &unit.source,
            },
            self.limits.syntax(),
        ) else {
            return Ok(());
        };
        let syntax = BindingSyntax::new(syntax.syntax_elements());
        let owners = callable_owners(existing);
        let ranges = existing
            .iter()
            .map(|row| (row.range.start, row.range.end))
            .collect::<BTreeSet<_>>();
        let mut observations = BTreeMap::new();
        for observation in file.declarations {
            observations
                .entry(observation.range)
                .and_modify(|row| *row = None)
                .or_insert(Some(observation));
        }
        let starts = super::line_starts(&unit.source);
        let mut ordinals = BTreeMap::new();
        for (range, observation) in observations {
            let Some(observation) = observation else {
                continue;
            };
            let Some((kind, scope)) = syntax.binding(range, self.proof) else {
                continue;
            };
            let Some(owner) = owners
                .resolve(scope.start, scope.end)
                .copied()
                .flatten()
                .filter(|owner| owner.range.end == scope.end)
            else {
                continue;
            };
            let Ok(callable) = SymbolIdentity::parse(owner.symbol.as_str()) else {
                continue;
            };
            if callable.owner() != &publication.owner || callable.language() != &unit.language {
                continue;
            }
            let start = usize::try_from(range.start()).unwrap_or(usize::MAX);
            let end = usize::try_from(range.end()).unwrap_or(usize::MAX);
            let Some(name) = unit.source.get(start..end) else {
                continue;
            };
            let ordinal = ordinals.entry((&owner.symbol, name)).or_insert(0_usize);
            let category = match self.proof {
                BindingProof::SemanticToken => "lsp",
                BindingProof::ShorthandDefinition => "lsp_shorthand",
            };
            let component = format!("{name}#{category}{ordinal}");
            *ordinal += 1;
            if ranges.contains(&(range.start(), range.end())) {
                continue;
            }
            let mut qualified = callable.qualified_path().to_vec();
            qualified.push(component);
            let Ok(identity) =
                SymbolIdentity::new(publication.owner.clone(), unit.language.clone(), qualified)
            else {
                continue;
            };
            let symbol = SymbolId::from_identity(&identity);
            if !known.insert(symbol.clone()) {
                continue;
            }
            let length = range.end() - range.start();
            if self.slots == 0 || length > self.bytes || length > self.record_bytes {
                self.dropped += 1;
                continue;
            }
            self.add(
                publication,
                Binding {
                    unit,
                    owner,
                    line: line_of(&starts, range.start()),
                    name,
                    kind,
                    observation,
                    symbol,
                },
            )?;
            self.slots -= 1;
            self.bytes -= length;
        }
        Ok(())
    }

    fn add(
        &mut self,
        publication: &PackagePublication,
        binding: Binding<'_>,
    ) -> Result<(), RiftError> {
        let Binding {
            unit,
            owner,
            line,
            name,
            kind,
            observation,
            symbol,
        } = binding;
        let range = observation.range;
        let mut declaration = PackageSymbol {
            symbol: symbol.clone(),
            origin: owner.origin.clone(),
            unit: unit.unit.clone(),
            range: TextRange {
                start: range.start(),
                end: range.end(),
            },
            syntax_element_kind: Some(kind.to_owned()),
            line,
            signature_indices: Vec::new(),
            type_indices: Vec::new(),
            documentation_indices: Vec::new(),
            source: name.to_owned(),
            source_complete: true,
            public: false,
            digest: Digest(String::new()),
        };
        declaration.digest = digest_of(&declaration, &publication.owner)?;
        let (kind, portable) = match (self.proof, observation.parameter) {
            (BindingProof::ShorthandDefinition, _) => ("shorthand_binding", SymbolKind::Value),
            (_, true) => ("parameter_binding", SymbolKind::Parameter),
            (_, false) => ("local_binding", SymbolKind::Value),
        };
        let mut extensions = BTreeMap::new();
        extensions.insert(
            ExtensionKey("org.lsp.rust-declaration".to_owned()),
            ExtensionValue {
                version: 1,
                data: serde_json::json!(self.producer),
            },
        );
        self.objects.push(Symbol {
            id: Some(symbol),
            language: unit.language.clone(),
            name: name.to_owned(),
            kind: ExactKind(kind.to_owned()),
            kinds: vec![portable],
            origin: owner.origin.clone(),
            container: Some(owner.symbol.clone()),
            modifiers: Vec::new(),
            visibility: None,
            types: Vec::new(),
            signatures: Vec::new(),
            documentation: Vec::new(),
            extensions: Extensions(extensions),
            document_local: true,
        });
        self.declarations.push(declaration);
        Ok(())
    }
}

fn callable(kind: &str) -> bool {
    matches!(kind, "function_item" | "function_signature_item")
}

fn callable_owners<'a>(
    declarations: &[&'a PackageSymbol],
) -> EnclosingDefinitions<Option<&'a PackageSymbol>> {
    let mut owners = BTreeMap::new();
    for &declaration in declarations {
        if declaration.origin.source_kind != SourceKind::Authored
            || !declaration
                .syntax_element_kind
                .as_deref()
                .is_some_and(callable)
        {
            continue;
        }
        owners
            .entry((declaration.range.start, declaration.range.end))
            .and_modify(|existing: &mut Option<&PackageSymbol>| {
                if existing.is_some_and(|owner| owner.symbol != declaration.symbol) {
                    *existing = None;
                }
            })
            .or_insert(Some(declaration));
    }
    EnclosingDefinitions::new(
        owners
            .into_iter()
            .map(|((start, end), owner)| (start, end, owner)),
    )
}

struct BindingSyntax<'tree> {
    tree: &'tree [SyntaxElement],
    elements: BTreeMap<ByteRange, usize>,
    first_child: Vec<Option<usize>>,
}

impl<'tree> BindingSyntax<'tree> {
    fn new(tree: &'tree [SyntaxElement]) -> Self {
        let elements = tree
            .iter()
            .enumerate()
            .filter_map(|(index, element)| {
                (!element.has_error
                    && matches!(element.kind, "identifier" | "shorthand_field_identifier"))
                .then_some((element.range, index))
            })
            .collect::<BTreeMap<_, _>>();
        let mut first_child = vec![None; tree.len()];
        for (index, element) in tree.iter().enumerate() {
            if let Some(parent) = element.parent
                && !matches!(
                    element.kind,
                    "mutable_specifier"
                        | "label"
                        | "line_comment"
                        | "block_comment"
                        | "attribute_item"
                        | "inner_attribute_item"
                )
            {
                first_child[parent].get_or_insert(index);
            }
        }

        Self {
            tree,
            elements,
            first_child,
        }
    }

    fn binding(
        &self,
        range: SourceRange,
        proof: BindingProof,
    ) -> Option<(&'static str, ByteRange)> {
        let &index = self.elements.get(&ByteRange {
            start: range.start(),
            end: range.end(),
        })?;
        let kind = self.tree[index].kind;
        if proof == BindingProof::ShorthandDefinition && kind != "shorthand_field_identifier" {
            return None;
        }
        let scope = syntax_callable(self.tree, &self.first_child, index, proof)?;
        Some((kind, scope))
    }
}

struct Binding<'a> {
    unit: &'a PackageSourceUnit,
    owner: &'a PackageSymbol,
    line: u64,
    name: &'a str,
    kind: &'static str,
    observation: &'a RustLspDeclaration,
    symbol: SymbolId,
}

fn syntax_callable(
    tree: &[SyntaxElement],
    first: &[Option<usize>],
    mut index: usize,
    proof: BindingProof,
) -> Option<ByteRange> {
    let mut current = tree.get(index)?;
    let mut in_binding = false;
    while let Some(parent) = current.parent {
        current = tree.get(parent)?;
        if in_binding {
            match current.kind {
                "token_tree"
                | "macro_invocation"
                | "attribute_item"
                | "inner_attribute_item"
                | "macro_definition" => return None,
                "closure_expression" if proof == BindingProof::ShorthandDefinition => return None,
                kind if callable(kind) => return Some(current.range),
                _ => {}
            }
        } else {
            match current.kind {
                "or_pattern" if proof == BindingProof::ShorthandDefinition => return None,
                "tuple_pattern"
                | "slice_pattern"
                | "reference_pattern"
                | "or_pattern"
                | "tuple_struct_pattern"
                | "struct_pattern"
                | "field_pattern"
                | "mut_pattern"
                | "ref_pattern"
                | "captured_pattern" => {}
                "let_declaration" | "let_condition" | "parameter" | "for_expression"
                | "match_pattern"
                    if first.get(parent).copied().flatten() == Some(index) =>
                {
                    in_binding = true;
                }
                "closure_parameters" => in_binding = true,
                _ => return None,
            }
        }
        index = parent;
    }
    None
}

pub(super) fn retained_source_bytes(publication: &PackagePublication) -> u64 {
    let lengths = publication
        .units
        .iter()
        .map(|unit| unit.source.len())
        .chain(publication.declarations.iter().map(|row| row.source.len()))
        .chain(publication.documents.iter().flat_map(|doc| {
            [doc.file_content.as_ref(), doc.declaration_source.as_ref()]
                .into_iter()
                .flatten()
                .map(String::len)
        }));
    lengths.fold(0_u64, |sum, length| {
        sum.saturating_add(u64::try_from(length).unwrap_or(u64::MAX))
    })
}
