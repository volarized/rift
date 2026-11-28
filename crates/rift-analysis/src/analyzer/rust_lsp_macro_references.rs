//! Checks uses of original Rust macro names after admitting their declarations.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{FileDigest, ProjectPath, SourceRange};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::index::{PackagePublication, PackageSourceUnit, PackageSymbol, StoredEdge};
use rift_protocol::producer::FactProducer;
use rift_protocol::read::{
    EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, ExtensionKey, Extensions, SourceKind,
    SourceUnitId, SourceUnitSpan, Symbol, SymbolId, TextRange,
};
use rift_syntax::{ByteRange, RustSyntaxProvider, SyntaxLimits, SyntaxProvider, SyntaxSource};

use super::{PackageAnalyzer, file_digest, rust_lsp_operators::checked_units};
use crate::ExactPackageLimits;

/// A written occurrence whose unique Rust language-server definition is an original macro name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustLspMacroReference<'source> {
    /// Package-relative path of the original occurrence.
    pub source: &'source ProjectPath,
    /// Digest of the source the language server used for the occurrence.
    pub source_digest: FileDigest,
    /// Exact range of the original written occurrence.
    pub occurrence: SourceRange,
    /// Package-relative path of the independently selected definition.
    pub target: &'source ProjectPath,
    /// Digest of the source the language server used for the definition.
    pub target_digest: FileDigest,
    /// Exact range of the original macro name declaration.
    pub target_name: SourceRange,
}

/// Supplemental checked edges and the number left out at a bound.
#[derive(Debug, Default)]
pub struct RustLspMacroReferenceEdges {
    /// Authored uses with one existing source owner and one admitted macro target.
    pub edges: Vec<StoredEdge>,
    /// Observations or edges that did not fit the requested bound.
    pub dropped_at_limit: usize,
}

impl PackageAnalyzer {
    /// Builds written Rust macro-use edges after the original targets have been admitted.
    ///
    /// The language server must independently identify the exact original name in a retained
    /// unit. Source syntax distinguishes a call, a field write, a field read, and a reference.
    /// Established edges consume capacity first.
    #[must_use]
    pub fn rust_lsp_macro_reference_edges(
        publication: &PackagePublication,
        observed: &[RustLspMacroReference<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> RustLspMacroReferenceEdges {
        let mut report = RustLspMacroReferenceEdges::default();
        let maximum = limits.edges_max();
        if observed.is_empty() || observed.len() > maximum {
            report.dropped_at_limit = observed.len();
            return report;
        }
        let (source_paths, target_paths) = observed.iter().fold(
            (BTreeSet::new(), BTreeSet::new()),
            |(mut sources, mut targets), row| {
                sources.insert(row.source.as_str());
                targets.insert(row.target.as_str());
                (sources, targets)
            },
        );
        let sources = checked_units(publication, &source_paths);
        let targets = macro_targets(publication, &target_paths);
        let mut shapes = BTreeMap::new();
        let incumbent = incumbent_edges(publication);
        let room = maximum.saturating_sub(publication.edges.len());
        for row in unique_uses(observed).into_values().flatten() {
            let Some(source) = sources
                .get(row.source.as_str())
                .filter(|unit| unit.digest == row.source_digest)
            else {
                continue;
            };
            let Some(target) = targets
                .get(&(row.target.as_str(), row.target_name))
                .copied()
                .flatten()
                .filter(|target| target.digest == row.target_digest)
            else {
                continue;
            };
            if row.source == row.target && row.occurrence == row.target_name {
                continue;
            }
            let Some(from) = source.callable(row.occurrence) else {
                continue;
            };
            let Some(shape) = shapes
                .entry(row.source.as_str())
                .or_insert_with(|| SourceShapes::capture(row.source, source.unit, limits.syntax()))
            else {
                continue;
            };
            let Some(kind) = shape.kind(source.unit, row.occurrence, target.object) else {
                continue;
            };
            let span = row.occurrence;
            let key = (
                &from.declaration.symbol,
                &target.declaration.symbol,
                &source.unit.unit,
                span.start(),
                span.end(),
                kind,
            );
            if incumbent.contains(&key) {
                continue;
            }
            if report.edges.len() >= room {
                report.dropped_at_limit += 1;
                continue;
            }
            report.edges.push(edge(
                from.declaration,
                target.declaration,
                source.unit,
                span,
                kind,
                producer,
            ));
        }
        report
    }
}

type Uses<'a> = BTreeMap<(&'a ProjectPath, SourceRange), Option<&'a RustLspMacroReference<'a>>>;

fn unique_uses<'a>(observed: &'a [RustLspMacroReference<'a>]) -> Uses<'a> {
    let mut uses = Uses::new();
    for row in observed {
        uses.entry((row.source, row.occurrence))
            .and_modify(|old| {
                if *old != Some(row) {
                    *old = None;
                }
            })
            .or_insert(Some(row));
    }
    uses
}

#[derive(Clone, Copy)]
struct MacroTarget<'a> {
    declaration: &'a PackageSymbol,
    object: &'a Symbol,
    digest: FileDigest,
}

type MacroTargets<'a> = BTreeMap<(&'a str, SourceRange), Option<MacroTarget<'a>>>;

fn macro_targets<'a>(
    publication: &'a PackagePublication,
    paths: &BTreeSet<&str>,
) -> MacroTargets<'a> {
    let proof = ExtensionKey("org.lsp.rust-macro-name".to_owned());
    let objects = publication
        .objects
        .iter()
        .filter_map(|object| {
            let identity = object.id.as_ref()?;
            let owner = SymbolIdentity::parse(identity.as_str()).ok()?;
            (owner.owner() == &publication.owner
                && object.language.name == "rust"
                && matches!(
                    object.kind.0.as_str(),
                    "enum" | "variant" | "field_declaration"
                )
                && object
                    .extensions
                    .0
                    .get(&proof)
                    .is_some_and(|extension| extension.version == 1))
            .then_some((identity, object))
        })
        .collect::<BTreeMap<_, _>>();
    let units = publication
        .units
        .iter()
        .filter_map(|unit| {
            if !paths.contains(unit.path.0.as_str())
                || !unit.source_complete
                || unit.language.name != "rust"
            {
                return None;
            }
            let digest = FileDigest::of(unit.source.as_bytes());
            (file_digest(digest) == unit.content_digest).then_some((&unit.unit, (unit, digest)))
        })
        .collect::<BTreeMap<_, _>>();
    let mut targets = MacroTargets::new();
    for declaration in &publication.declarations {
        let Some(&object) = objects.get(&declaration.symbol) else {
            continue;
        };
        let Some(&(unit, digest)) = units.get(&declaration.unit) else {
            continue;
        };
        let Ok(range) = SourceRange::new(declaration.range.start, declaration.range.end) else {
            continue;
        };
        if declaration.origin.source_kind != SourceKind::Authored
            || declaration.syntax_element_kind.as_deref() != Some("identifier")
            || !matches_written(unit, range, &object.name)
        {
            continue;
        }
        targets
            .entry((unit.path.0.as_str(), range))
            .and_modify(|earlier| {
                if earlier.is_some_and(|old| old.declaration.symbol != declaration.symbol) {
                    *earlier = None;
                }
            })
            .or_insert(Some(MacroTarget {
                declaration,
                object,
                digest,
            }));
    }
    targets
}

struct SourceShapes {
    names: BTreeSet<ByteRange>,
    field_roles: BTreeMap<ByteRange, EdgeKind>,
    callees: BTreeSet<u64>,
}

impl SourceShapes {
    fn capture(path: &ProjectPath, unit: &PackageSourceUnit, limits: SyntaxLimits) -> Option<Self> {
        let document = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path,
                    text: &unit.source,
                },
                limits,
            )
            .ok()?;
        let facts = document.facts().static_facts()?;
        let mut names = BTreeSet::new();
        let mut real_calls = BTreeSet::new();
        let mut unavailable = Vec::new();
        for element in document.syntax_elements() {
            let blocked = matches!(
                element.kind,
                "ERROR" | "macro_definition" | "macro_invocation"
            ) || element
                .parent
                .and_then(|index| unavailable.get(index))
                .copied()
                == Some(true);
            unavailable.push(blocked);
            if blocked || element.has_error {
                continue;
            }
            if matches!(
                element.kind,
                "identifier"
                    | "type_identifier"
                    | "field_identifier"
                    | "shorthand_field_identifier"
            ) {
                names.insert(element.range);
            } else if element.kind == "call_expression" {
                real_calls.insert(element.range);
            }
        }
        let field_roles = facts
            .rust_field_sites()
            .iter()
            .map(|site| {
                (
                    site.field,
                    if site.literal.is_some() {
                        EdgeKind::Writes
                    } else {
                        EdgeKind::Reads
                    },
                )
            })
            .collect();
        let callees = facts
            .calls()
            .iter()
            .filter_map(|call| {
                real_calls
                    .contains(&call.range)
                    .then_some(call.callee_range.end)
            })
            .collect();
        Some(Self {
            names,
            field_roles,
            callees,
        })
    }

    fn kind(
        &self,
        unit: &PackageSourceUnit,
        range: SourceRange,
        target: &Symbol,
    ) -> Option<EdgeKind> {
        let bytes = ByteRange {
            start: range.start(),
            end: range.end(),
        };
        if !self.names.contains(&bytes) || !matches_written(unit, range, &target.name) {
            return None;
        }
        if target.kind.0 == "field_declaration" {
            Some(
                self.field_roles
                    .get(&bytes)
                    .copied()
                    .unwrap_or(EdgeKind::References),
            )
        } else if target.kind.0 == "variant" && self.callees.contains(&range.end()) {
            Some(EdgeKind::Calls)
        } else {
            Some(EdgeKind::References)
        }
    }
}

fn matches_written(unit: &PackageSourceUnit, range: SourceRange, name: &str) -> bool {
    let (Ok(start), Ok(end)) = (usize::try_from(range.start()), usize::try_from(range.end()))
    else {
        return false;
    };
    unit.source
        .get(start..end)
        .is_some_and(|written| written == name || written.strip_prefix("r#") == Some(name))
}

type EdgeKey<'a> = (
    &'a SymbolId,
    &'a SymbolId,
    &'a SourceUnitId,
    u64,
    u64,
    EdgeKind,
);

fn incumbent_edges(publication: &PackagePublication) -> BTreeSet<EdgeKey<'_>> {
    publication
        .edges
        .iter()
        .filter_map(|edge| {
            let span = edge.occurrence.as_ref()?;
            Some(edge.kinds.iter().map(move |kind| {
                (
                    &edge.from,
                    &edge.to,
                    &span.unit,
                    span.range.start,
                    span.range.end,
                    *kind,
                )
            }))
        })
        .flatten()
        .collect()
}

fn edge(
    source: &PackageSymbol,
    target: &PackageSymbol,
    unit: &PackageSourceUnit,
    range: SourceRange,
    kind: EdgeKind,
    producer: &FactProducer,
) -> StoredEdge {
    let word = match kind {
        EdgeKind::Writes => "write",
        EdgeKind::Reads => "read",
        EdgeKind::Calls => "call",
        _ => "reference",
    };
    StoredEdge {
        from: source.symbol.clone(),
        to: target.symbol.clone(),
        kind: ExactKind(word.to_owned()),
        kinds: vec![kind],
        occurrence: Some(SourceUnitSpan {
            unit: unit.unit.clone(),
            range: TextRange {
                start: range.start(),
                end: range.end(),
            },
        }),
        provider: EdgeProvider::Lsp,
        derivation: EdgeDerivation::Resolution,
        confidence: None,
        producer: Some(producer.clone()),
        extensions: Extensions::default(),
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::producer::{FactCoverage, FactKind, ProducerName, ProducerRevision};
    use rift_syntax::ShippedLanguage;

    use super::*;
    use crate::analyzer::{RustLspMacroDefinition, RustLspMacroFile, RustLspMacroItems, fixture};

    const MANIFEST: &str = "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n";
    const SOURCE: &str = r"
macro_rules! define {
    ($name:ident { $variant:ident { $field:ident : $ty:ty }, $count:ident }) => {
        pub enum $name { $variant { $field: $ty }, $count(u8) }
    };
}
define!(Node { Leaf { value: u8 }, Count });

pub enum Ordinary { Plain { value: u8 } }

pub fn use_nodes(node: Node) -> u8 {
    let _leaf = Node::Leaf { value: 7 };
    let _count = Node::Count(1);
    let _plain = Ordinary::Plain { value: 3 };
    match node {
        Node::Leaf { value: written } => written,
        Node::Count(value) => value,
    }
}";
    const EXPANSION: &str = "pub enum Node { Leaf { value: u8 }, Count(u8) }";

    fn range(context: &str, name: &str) -> SourceRange {
        let start = SOURCE.find(context).expect("written context")
            + context.find(name).expect("written name");
        SourceRange::new(start as u64, (start + name.len()) as u64).expect("written range")
    }

    fn producer() -> FactProducer {
        FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer name"),
            revision: ProducerRevision::parse("1").expect("producer revision"),
        }
    }

    fn checked_package(path: &ProjectPath, limits: ExactPackageLimits) -> PackagePublication {
        let mut publication = fixture::package_result_with_metadata(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", SOURCE)],
            None,
            Some(("Cargo.toml", MANIFEST)),
        )
        .expect("authored fixture")
        .publication()
        .clone();
        let site = PackageAnalyzer::rust_lsp_macro_sites(path, SOURCE, limits.syntax())[0];
        let mut names = PackageAnalyzer::rust_lsp_macro_name_sites(
            path,
            SOURCE,
            site.invocation,
            EXPANSION,
            limits.syntax(),
        );
        names.extend(PackageAnalyzer::rust_lsp_macro_variant_field_sites(
            path,
            SOURCE,
            site.invocation,
            EXPANSION,
            limits.syntax(),
            &names,
        ));
        let digest = FileDigest::of(SOURCE.as_bytes());
        let expansion = RustLspMacroItems {
            site,
            definition: RustLspMacroDefinition {
                path,
                digest,
                range: range("macro_rules! define", "define"),
            },
            expansion: EXPANSION,
            names: &names,
        };
        let admission = PackageAnalyzer::admit_rust_lsp_macro_items(
            &mut publication,
            &[RustLspMacroFile {
                path,
                digest,
                expansions: &[expansion],
            }],
            &producer(),
            limits,
        )
        .expect("macro declarations checked");
        assert_eq!(admission.admitted, 4);
        publication
    }

    fn observation<'source>(
        path: &'source ProjectPath,
        source_context: &str,
        target_context: &str,
        name: &str,
    ) -> RustLspMacroReference<'source> {
        let digest = FileDigest::of(SOURCE.as_bytes());
        RustLspMacroReference {
            source: path,
            source_digest: digest,
            occurrence: range(source_context, name),
            target: path,
            target_digest: digest,
            target_name: range(target_context, name),
        }
    }

    #[test]
    fn confirmed_macro_uses_retain_call_write_read_and_reference_roles() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let limits = ExactPackageLimits::new(1024, 2 * 1024 * 1024);
        let mut publication = checked_package(&path, limits);
        let written = [
            (
                "Node::Leaf { value: 7",
                "define!(Node",
                "Node",
                "/Node",
                EdgeKind::References,
            ),
            (
                "Node::Leaf { value: 7",
                "define!(Node { Leaf",
                "Leaf",
                "/Node/Leaf",
                EdgeKind::References,
            ),
            (
                "Node::Leaf { value: 7",
                "Leaf { value: u8",
                "value",
                "/Node/Leaf/value",
                EdgeKind::Writes,
            ),
            (
                "Node::Count(1)",
                "value: u8 }, Count",
                "Count",
                "/Node/Count",
                EdgeKind::Calls,
            ),
            (
                "Node::Leaf { value: written",
                "Leaf { value: u8",
                "value",
                "/Node/Leaf/value",
                EdgeKind::Reads,
            ),
            (
                "Node::Count(value)",
                "value: u8 }, Count",
                "Count",
                "/Node/Count",
                EdgeKind::References,
            ),
        ];
        let observed =
            written.map(|(source, target, name, _, _)| observation(&path, source, target, name));
        let checked = PackageAnalyzer::rust_lsp_macro_reference_edges(
            &publication,
            &observed,
            &producer(),
            limits,
        );
        assert_eq!(checked.edges.len(), written.len());
        for ((_, _, _, target, role), (edge, row)) in
            written.into_iter().zip(checked.edges.iter().zip(observed))
        {
            assert!(edge.from.as_str().ends_with("/use_nodes"));
            assert!(edge.to.as_str().ends_with(target));
            assert_eq!(edge.kinds, [role]);
            let occurrence = edge.occurrence.as_ref().expect("exact occurrence");
            assert_eq!(
                (occurrence.range.start, occurrence.range.end),
                (row.occurrence.start(), row.occurrence.end())
            );
        }
        let capacity = limits
            .with_edges(publication.edges.len() + 1)
            .expect("edge capacity");
        let capped = PackageAnalyzer::rust_lsp_macro_reference_edges(
            &publication,
            &observed,
            &producer(),
            capacity,
        );
        assert_eq!(capped.edges, checked.edges[..1]);
        assert_eq!(capped.dropped_at_limit, written.len() - 1);
        assert!(publication.edges.iter().any(|edge| {
            edge.to.as_str().ends_with("/Ordinary/Plain/value")
                && edge.kinds.contains(&EdgeKind::Writes)
        }));
        publication
            .coverage
            .iter_mut()
            .find(|coverage| coverage.language.name == "rust")
            .expect("Rust package coverage")
            .facts
            .push(FactCoverage {
                kind: FactKind::Edges,
                producer: Some(producer()),
            });
        publication.edges.extend(checked.edges);
        publication.validate_facts().expect("valid authored edges");
    }

    #[test]
    fn macro_uses_refuse_changed_ambiguous_and_non_macro_authority() {
        let path = ProjectPath::new("src/lib.rs").expect("fixture path");
        let limits = ExactPackageLimits::new(1024, 2 * 1024 * 1024);
        let publication = checked_package(&path, limits);
        let written = observation(&path, "Node::Leaf { value: 7", "Leaf { value: u8", "value");
        let rejected = [
            RustLspMacroReference {
                target_digest: FileDigest::of(b"changed target"),
                ..written
            },
            RustLspMacroReference {
                source_digest: FileDigest::of(b"changed source"),
                ..written
            },
            RustLspMacroReference {
                occurrence: written.target_name,
                ..written
            },
            RustLspMacroReference {
                target_name: range("Plain { value: u8", "value"),
                ..written
            },
            RustLspMacroReference {
                occurrence: range("Node::Leaf { value: 7", "Leaf"),
                ..written
            },
        ];
        for row in rejected {
            assert!(
                PackageAnalyzer::rust_lsp_macro_reference_edges(
                    &publication,
                    &[row],
                    &producer(),
                    limits
                )
                .edges
                .is_empty()
            );
        }
        assert!(
            PackageAnalyzer::rust_lsp_macro_reference_edges(
                &publication,
                &[written, rejected[3]],
                &producer(),
                limits,
            )
            .edges
            .is_empty()
        );
        assert_eq!(
            PackageAnalyzer::rust_lsp_macro_reference_edges(
                &publication,
                &[written, written],
                &producer(),
                limits,
            )
            .edges
            .len(),
            1
        );
    }
}
