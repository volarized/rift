//! Request-scoped LSP Contribution conversion.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Range, SymbolInformation, SymbolKind};
use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ContributionReference, DeclarationBinding,
    EquivalenceEvidence, ExactKind, ExtensionKey, ExtensionValue, Extensions, Language,
    PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability,
    SourceKind, SourceLocation, SourceRange, SourceRevision, SourceUnitId, SymbolFacet,
    TreeRevision,
};
use rift_provider::{
    AdapterPublication, ProviderInputMode, ProviderPublication, PublicationCoverage,
    PublicationLimits,
};
use serde_json::{Value, json};

use crate::capabilities::PositionEncoding;
use crate::position::LineIndex;

/// Stable address of one item inside one LSP operation answer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LspResultAddress(Vec<usize>);

impl LspResultAddress {
    /// Creates one result address.
    #[must_use]
    pub const fn new(parts: Vec<usize>) -> Self {
        Self(parts)
    }

    /// Returns path indexes from operation root.
    #[must_use]
    pub fn parts(&self) -> &[usize] {
        &self.0
    }

    fn display(&self) -> String {
        self.0
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// LSP request conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspContributionViolation {
    /// LSP name or generated provider identity is invalid.
    InvalidSymbol,
    /// LSP range does not address captured source.
    InvalidRange,
    /// Contribution validation failed.
    InvalidContribution,
    /// Request publication validation failed.
    InvalidPublication,
    /// Request coverage validation failed.
    InvalidCoverage,
}

/// Error returned by LSP Contribution conversion.
#[derive(Debug)]
pub struct LspContributionError {
    violation: LspContributionViolation,
    detail: String,
}

impl LspContributionError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> LspContributionViolation {
        self.violation
    }

    /// Returns failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for LspContributionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "LSP Contribution adapter rejected {:?}: {}",
            self.violation, self.detail
        )
    }
}

impl Error for LspContributionError {}

/// Captured inputs for one LSP document-symbol answer.
#[derive(Debug, Clone)]
pub struct LspContributionInput {
    /// Provider publishing result.
    pub provider: ProviderId,
    /// Provider publication revision.
    pub publication_revision: ProviderRevision,
    /// Source snapshot revision.
    pub source_revision: SourceRevision,
    /// Syntax tree revision.
    pub tree_revision: TreeRevision,
    /// Source unit queried by operation.
    pub unit: SourceUnitId,
    /// Source language.
    pub language: Language,
    /// Source text used for position conversion.
    pub source: String,
    /// Negotiated LSP position encoding.
    pub encoding: PositionEncoding,
    /// Publication bounds.
    pub limits: PublicationLimits,
}

/// Converts one bounded LSP document-symbol answer.
#[derive(Debug, Clone)]
pub struct LspContributionAdapter {
    provider: ProviderId,
    publication_revision: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    unit: SourceUnitId,
    language: Language,
    source: String,
    encoding: PositionEncoding,
    limits: PublicationLimits,
}

impl LspContributionAdapter {
    /// Creates one request adapter from captured request inputs.
    #[must_use]
    pub fn new(input: LspContributionInput) -> Self {
        Self {
            provider: input.provider,
            publication_revision: input.publication_revision,
            source_revision: input.source_revision,
            tree_revision: input.tree_revision,
            unit: input.unit,
            language: input.language,
            source: input.source,
            encoding: input.encoding,
            limits: input.limits,
        }
    }

    /// Converts one complete or partial document-symbol answer.
    ///
    /// Syntax matches are exact result-address associations. They supply explicit
    /// equivalence evidence but never an identity anchor.
    ///
    /// # Errors
    ///
    /// Returns [`LspContributionError`] when a result name, range, Contribution,
    /// publication, or request coverage is invalid.
    pub fn convert(
        &self,
        response: &DocumentSymbolResponse,
        syntax_matches: &BTreeMap<LspResultAddress, ContributionReference>,
    ) -> Result<AdapterPublication, LspContributionError> {
        let pending = match response {
            DocumentSymbolResponse::Nested(symbols) => nested_symbols(symbols),
            DocumentSymbolResponse::Flat(symbols) => flat_symbols(symbols),
        };
        let line_index = LineIndex::new(&self.source);
        let mut contributions = Vec::with_capacity(pending.len());
        let mut identities = BTreeSet::new();
        for symbol in pending {
            let provider_symbol = self.provider_symbol(&symbol.address)?;
            if !identities.insert(provider_symbol.clone()) {
                return Err(lsp_error(
                    LspContributionViolation::InvalidSymbol,
                    symbol.address.display(),
                ));
            }
            let binding = self.binding(&line_index, symbol.selection_range)?;
            let mut facts = PortableSymbolFacts::new(
                self.language.clone(),
                symbol.name,
                symbol.qualified_name,
                ExactKind(format!("lsp.{}", kind_name(symbol.kind))),
            )
            .facets(kind_facets(symbol.kind));
            if let Some(parent) = symbol.parent {
                facts = facts.container(ContributionReference::new(
                    self.provider.clone(),
                    self.provider_symbol(&parent)?,
                ));
            }
            let mut extensions = BTreeMap::new();
            extensions.insert(
                ExtensionKey("org.lsp.symbol".to_owned()),
                ExtensionValue {
                    version: 1,
                    data: symbol.data,
                },
            );
            let mut builder = Contribution::builder(
                ContributionKey::new(
                    self.provider.clone(),
                    self.publication_revision,
                    provider_symbol,
                ),
                SourceApplicability::Exact {
                    source_revision: self.source_revision,
                    tree_revision: self.tree_revision,
                },
                facts,
                ContributionOrigin::new(
                    Some(SourceLocation::Project { package: None }),
                    SourceKind::Authored,
                )
                .map_err(|error| {
                    lsp_error(
                        LspContributionViolation::InvalidContribution,
                        error.to_string(),
                    )
                })?,
            )
            .source(binding)
            .namespaced(Extensions(extensions));
            if let Some(target) = syntax_matches.get(&symbol.address) {
                builder = builder.equivalence(vec![EquivalenceEvidence::Explicit(target.clone())]);
            }
            contributions.push(builder.build().map_err(|error| {
                lsp_error(
                    LspContributionViolation::InvalidContribution,
                    error.to_string(),
                )
            })?);
        }
        let publication = ProviderPublication::new(
            self.provider.clone(),
            self.publication_revision,
            contributions,
            self.limits,
        )
        .map_err(|error| {
            lsp_error(
                LspContributionViolation::InvalidPublication,
                error.to_string(),
            )
        })?;
        AdapterPublication::new(
            ProviderInputMode::Operation,
            PublicationCoverage::Request,
            publication,
        )
        .map_err(|error| lsp_error(LspContributionViolation::InvalidCoverage, error.to_string()))
    }

    fn provider_symbol(
        &self,
        address: &LspResultAddress,
    ) -> Result<ProviderSymbolId, LspContributionError> {
        ProviderSymbolId::new(format!(
            "request:{}:{}",
            self.publication_revision.get(),
            address.display()
        ))
        .map_err(|error| lsp_error(LspContributionViolation::InvalidSymbol, error.to_string()))
    }

    fn binding(
        &self,
        index: &LineIndex<'_>,
        range: Range,
    ) -> Result<DeclarationBinding, LspContributionError> {
        let start = index
            .byte_offset(self.encoding, range.start)
            .map_err(|error| {
                lsp_error(LspContributionViolation::InvalidRange, error.to_string())
            })?;
        let end = index
            .byte_offset(self.encoding, range.end)
            .map_err(|error| {
                lsp_error(LspContributionViolation::InvalidRange, error.to_string())
            })?;
        let range = SourceRange::new(
            u64::try_from(start).map_err(|error| {
                lsp_error(LspContributionViolation::InvalidRange, error.to_string())
            })?,
            u64::try_from(end).map_err(|error| {
                lsp_error(LspContributionViolation::InvalidRange, error.to_string())
            })?,
        )
        .map_err(|error| lsp_error(LspContributionViolation::InvalidRange, error.to_string()))?;
        Ok(DeclarationBinding::new(self.unit.clone(), range, None))
    }
}

struct PendingSymbol {
    address: LspResultAddress,
    parent: Option<LspResultAddress>,
    name: String,
    qualified_name: String,
    kind: SymbolKind,
    selection_range: Range,
    data: Value,
}

fn nested_symbols(symbols: &[DocumentSymbol]) -> Vec<PendingSymbol> {
    let mut output = Vec::new();
    collect_nested(symbols, &[], None, None, &mut output);
    output
}

#[allow(deprecated)]
fn collect_nested(
    symbols: &[DocumentSymbol],
    prefix: &[usize],
    parent: Option<&LspResultAddress>,
    parent_name: Option<&str>,
    output: &mut Vec<PendingSymbol>,
) {
    for (index, symbol) in symbols.iter().enumerate() {
        let mut parts = prefix.to_vec();
        parts.push(index);
        let address = LspResultAddress::new(parts.clone());
        let qualified_name = parent_name.map_or_else(
            || symbol.name.clone(),
            |parent_name| format!("{parent_name}.{}", symbol.name),
        );
        output.push(PendingSymbol {
            address: address.clone(),
            parent: parent.cloned(),
            name: symbol.name.clone(),
            qualified_name: qualified_name.clone(),
            kind: symbol.kind,
            selection_range: symbol.selection_range,
            data: json!({
                "detail": symbol.detail,
                "kind": symbol.kind,
                "tags": symbol.tags,
                "deprecated": symbol.deprecated,
                "range": symbol.range,
                "selection_range": symbol.selection_range,
            }),
        });
        if let Some(children) = &symbol.children {
            collect_nested(
                children,
                &parts,
                Some(&address),
                Some(&qualified_name),
                output,
            );
        }
    }
}

#[allow(deprecated)]
fn flat_symbols(symbols: &[SymbolInformation]) -> Vec<PendingSymbol> {
    symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| PendingSymbol {
            address: LspResultAddress::new(vec![index]),
            parent: None,
            name: symbol.name.clone(),
            qualified_name: symbol.container_name.as_ref().map_or_else(
                || symbol.name.clone(),
                |container| format!("{container}.{}", symbol.name),
            ),
            kind: symbol.kind,
            selection_range: symbol.location.range,
            data: json!({
                "kind": symbol.kind,
                "tags": symbol.tags,
                "deprecated": symbol.deprecated,
                "location": symbol.location,
                "container_name": symbol.container_name,
            }),
        })
        .collect()
}

fn kind_name(kind: SymbolKind) -> &'static str {
    if kind == SymbolKind::FILE {
        "file"
    } else if kind == SymbolKind::MODULE {
        "module"
    } else if kind == SymbolKind::NAMESPACE {
        "namespace"
    } else if kind == SymbolKind::PACKAGE {
        "package"
    } else if kind == SymbolKind::CLASS {
        "class"
    } else if kind == SymbolKind::METHOD {
        "method"
    } else if kind == SymbolKind::PROPERTY {
        "property"
    } else if kind == SymbolKind::FIELD {
        "field"
    } else if kind == SymbolKind::CONSTRUCTOR {
        "constructor"
    } else if kind == SymbolKind::ENUM {
        "enum"
    } else if kind == SymbolKind::INTERFACE {
        "interface"
    } else if kind == SymbolKind::FUNCTION {
        "function"
    } else if kind == SymbolKind::VARIABLE {
        "variable"
    } else if kind == SymbolKind::CONSTANT {
        "constant"
    } else if kind == SymbolKind::STRUCT {
        "struct"
    } else if kind == SymbolKind::TYPE_PARAMETER {
        "type_parameter"
    } else {
        "symbol"
    }
}

fn kind_facets(kind: SymbolKind) -> Vec<SymbolFacet> {
    if [
        SymbolKind::CLASS,
        SymbolKind::ENUM,
        SymbolKind::INTERFACE,
        SymbolKind::STRUCT,
        SymbolKind::TYPE_PARAMETER,
    ]
    .contains(&kind)
    {
        vec![SymbolFacet::Type]
    } else if [
        SymbolKind::METHOD,
        SymbolKind::CONSTRUCTOR,
        SymbolKind::FUNCTION,
    ]
    .contains(&kind)
    {
        vec![SymbolFacet::Callable, SymbolFacet::Value]
    } else if [
        SymbolKind::MODULE,
        SymbolKind::NAMESPACE,
        SymbolKind::PACKAGE,
    ]
    .contains(&kind)
    {
        vec![SymbolFacet::Namespace]
    } else {
        vec![SymbolFacet::Value]
    }
}

fn lsp_error(
    violation: LspContributionViolation,
    detail: impl Into<String>,
) -> LspContributionError {
    LspContributionError {
        violation,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use lsp_types::{
        DocumentSymbol, DocumentSymbolResponse, Position, Range, SymbolKind, SymbolTag,
    };
    use rift_core::{
        Language, ProviderId, ProviderRevision, SourcePath, SourceResolverId, SourceRevision,
        SourceUnitId, TreeRevision,
    };
    use rift_provider::{ProviderInputMode, PublicationLimits};

    use super::{LspContributionAdapter, LspContributionInput, LspResultAddress};
    use crate::capabilities::PositionEncoding;

    fn adapter() -> LspContributionAdapter {
        LspContributionAdapter::new(LspContributionInput {
            provider: ProviderId::new("rust-analyzer").expect("provider"),
            publication_revision: ProviderRevision::new(7).expect("publication"),
            source_revision: SourceRevision::new(2).expect("source"),
            tree_revision: TreeRevision::new(3).expect("tree"),
            unit: SourceUnitId::new(
                SourceResolverId::new("project").expect("resolver"),
                SourcePath::new("src/lib.rs").expect("path"),
            )
            .expect("unit"),
            language: Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            source: "struct Beacon;\n".to_owned(),
            encoding: PositionEncoding::Utf16,
            limits: PublicationLimits::default(),
        })
    }

    #[allow(deprecated)]
    fn symbol() -> DocumentSymbol {
        DocumentSymbol {
            name: "Beacon".to_owned(),
            detail: Some("struct Beacon".to_owned()),
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range: Range::new(Position::new(0, 0), Position::new(0, 14)),
            selection_range: Range::new(Position::new(0, 7), Position::new(0, 13)),
            children: Some(vec![DocumentSymbol {
                name: "new".to_owned(),
                detail: Some("fn new() -> Beacon".to_owned()),
                kind: SymbolKind::FUNCTION,
                tags: Some(vec![SymbolTag::DEPRECATED]),
                deprecated: None,
                range: Range::new(Position::new(0, 7), Position::new(0, 13)),
                selection_range: Range::new(Position::new(0, 7), Position::new(0, 13)),
                children: None,
            }]),
        }
    }

    #[test]
    fn nested_answer_is_request_scoped_and_keeps_transient_equivalence() {
        let response = DocumentSymbolResponse::Nested(vec![symbol()]);
        let syntax = rift_core::ContributionReference::new(
            ProviderId::new("syntax").expect("provider"),
            rift_core::ProviderSymbolId::new("Beacon").expect("symbol"),
        );
        let matches = BTreeMap::from([(LspResultAddress::new(vec![0]), syntax)]);
        let output = adapter().convert(&response, &matches).expect("LSP answer");
        assert_eq!(output.mode(), ProviderInputMode::Operation);
        assert_eq!(output.publication().contributions().len(), 2);
        let beacon = &output.publication().contributions()[0];
        assert!(beacon.identity_anchor().is_none());
        assert_eq!(beacon.equivalence().len(), 1);
        assert_eq!(
            output.publication().contributions()[1]
                .facts()
                .expect("portable facts")
                .container_reference()
                .expect("parent")
                .symbol()
                .as_str(),
            "request:7:0"
        );
    }

    #[test]
    fn empty_partial_answer_remains_valid() {
        let response = DocumentSymbolResponse::Nested(Vec::new());
        let output = adapter()
            .convert(&response, &BTreeMap::new())
            .expect("empty answer");
        assert!(output.publication().contributions().is_empty());
    }

    #[test]
    fn every_supported_symbol_kind_maps_to_its_stable_name_and_facets() {
        let names = [
            (SymbolKind::FILE, "file"),
            (SymbolKind::MODULE, "module"),
            (SymbolKind::NAMESPACE, "namespace"),
            (SymbolKind::PACKAGE, "package"),
            (SymbolKind::CLASS, "class"),
            (SymbolKind::METHOD, "method"),
            (SymbolKind::PROPERTY, "property"),
            (SymbolKind::FIELD, "field"),
            (SymbolKind::CONSTRUCTOR, "constructor"),
            (SymbolKind::ENUM, "enum"),
            (SymbolKind::INTERFACE, "interface"),
            (SymbolKind::FUNCTION, "function"),
            (SymbolKind::VARIABLE, "variable"),
            (SymbolKind::CONSTANT, "constant"),
            (SymbolKind::STRUCT, "struct"),
            (SymbolKind::TYPE_PARAMETER, "type_parameter"),
            (SymbolKind::STRING, "symbol"),
        ];
        for (kind, expected) in names {
            assert_eq!(super::kind_name(kind), expected);
        }
        assert_eq!(
            super::kind_facets(SymbolKind::CLASS),
            vec![rift_core::SymbolFacet::Type]
        );
        assert_eq!(
            super::kind_facets(SymbolKind::FUNCTION),
            vec![
                rift_core::SymbolFacet::Callable,
                rift_core::SymbolFacet::Value
            ]
        );
        assert_eq!(
            super::kind_facets(SymbolKind::MODULE),
            vec![rift_core::SymbolFacet::Namespace]
        );
        assert_eq!(
            super::kind_facets(SymbolKind::PROPERTY),
            vec![rift_core::SymbolFacet::Value]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn flat_answer_keeps_container_name_and_location_data() {
        let response = DocumentSymbolResponse::Flat(vec![lsp_types::SymbolInformation {
            name: "new".to_owned(),
            kind: SymbolKind::METHOD,
            tags: Some(vec![SymbolTag::DEPRECATED]),
            deprecated: Some(true),
            location: lsp_types::Location {
                uri: crate::uri::parse_uri("file:///src/lib.rs").expect("URI"),
                range: Range::new(Position::new(0, 7), Position::new(0, 13)),
            },
            container_name: Some("Beacon".to_owned()),
        }]);
        let output = adapter()
            .convert(&response, &BTreeMap::new())
            .expect("flat answer");
        let contribution = &output.publication().contributions()[0];
        assert_eq!(
            contribution
                .facts()
                .expect("portable facts")
                .qualified_name(),
            "Beacon.new"
        );
        assert_eq!(
            contribution
                .namespaced()
                .0
                .get(&rift_core::ExtensionKey("org.lsp.symbol".to_owned()))
                .expect("LSP data")
                .data["container_name"],
            "Beacon"
        );
    }
}
