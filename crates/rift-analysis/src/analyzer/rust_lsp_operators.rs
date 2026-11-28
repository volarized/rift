//! Checked Rust operator sites and the authored calls a language server resolves.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rift_core::{FileDigest, ProjectPath, SourceRange};
use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::index::{PackagePublication, PackageSourceUnit, PackageSymbol, StoredEdge};
use rift_protocol::producer::FactProducer;
use rift_protocol::read::{
    EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, Extensions, SourceKind, SourceUnitId,
    SourceUnitSpan, Symbol, SymbolId, SymbolKind, TextRange,
};
use rift_syntax::RustQuery;

use super::{PackageAnalyzer, file_digest};
use crate::{EnclosingDefinitions, ExactPackageLimits};

const OPERATORS: &str = r#"
    (binary_expression operator: ["+" "-" "*" "/" "%" "&" "|" "^" "<<" ">>"
        "==" "!=" "<" "<=" ">" ">="] @binary)
    (unary_expression ["-" "!" "*"] @unary)
    (compound_assignment_expr operator: ["+=" "-=" "*=" "/=" "%=" "&=" "|=" "^=" "<<=" ">>="] @assign)
"#;
const IMPLEMENTATION_METHODS: &str =
    "(impl_item trait: (_) body: (declaration_list (function_item name: (identifier) @name)))";

/// A written operator token established by the Rust grammar on exact source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RustLspOperatorSite {
    range: SourceRange,
    digest: FileDigest,
    method: &'static str,
}

impl RustLspOperatorSite {
    /// Byte range of the operator token in the file the caller supplied.
    #[must_use]
    pub const fn range(self) -> SourceRange {
        self.range
    }
}

/// One definition selected by the language server from a checked Rust operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RustLspOperatorDefinition<'source> {
    /// Package-relative path of the file containing the operator.
    pub source: &'source ProjectPath,
    /// The checked original operator token.
    pub site: RustLspOperatorSite,
    /// Package-relative path of the implementation the server selected.
    pub target: &'source ProjectPath,
    /// Digest of the implementation source the server used.
    pub target_digest: FileDigest,
    /// Exact range of the selected implementation's written name.
    pub target_name: SourceRange,
}

/// A bounded extension to the incumbent package edge publication.
#[derive(Debug, Default)]
pub struct RustLspOperatorEdges {
    /// New call occurrences established by the language server.
    pub edges: Vec<StoredEdge>,
    /// Observations or accepted calls that did not fit the work or publication bound.
    pub dropped_at_limit: usize,
}

impl PackageAnalyzer {
    /// Finds overloadable operator tokens at authored Rust expression sites.
    ///
    /// # Errors
    /// Returns a typed refusal when the source or number of captures exceeds its bound.
    pub fn rust_lsp_operator_sites(
        source: &str,
        sites_max: usize,
    ) -> Result<Vec<RustLspOperatorSite>, RiftError> {
        static QUERY: OnceLock<Result<RustQuery, RiftError>> = OnceLock::new();
        let query = QUERY
            .get_or_init(|| RustQuery::new(OPERATORS))
            .as_ref()
            .map_err(Clone::clone)?;
        let digest = FileDigest::of(source.as_bytes());
        Ok(query
            .captures(source, sites_max)?
            .into_iter()
            .filter_map(|capture| {
                let range = SourceRange::new(capture.range.start, capture.range.end).ok()?;
                let method = operator_method(capture.name.as_ref(), range_text(source, range)?)?;
                Some(RustLspOperatorSite {
                    range,
                    digest,
                    method,
                })
            })
            .collect())
    }

    /// Makes calls from a checked operator to an authored implementation selected by Rust LSP.
    ///
    /// The source and target must match complete retained bytes. The source callable and the
    /// method declaration must each have one package identity. Earlier edges keep their capacity.
    #[must_use]
    pub fn rust_lsp_operator_edges(
        publication: &PackagePublication,
        observed: &[RustLspOperatorDefinition<'_>],
        producer: &FactProducer,
        limits: ExactPackageLimits,
    ) -> RustLspOperatorEdges {
        if observed.is_empty() {
            return RustLspOperatorEdges::default();
        }
        let mut report = RustLspOperatorEdges::default();
        let edge_max = limits.edges_max();
        if observed.len() > edge_max {
            report.dropped_at_limit = observed.len();
            return report;
        }
        let unique = unique_sites(observed.iter());
        let paths = unique
            .values()
            .flatten()
            .flat_map(|row| [row.source.as_str(), row.target.as_str()])
            .collect::<BTreeSet<_>>();
        let units = checked_units(publication, &paths);
        let mut methods = BTreeMap::new();
        let incumbent = publication
            .edges
            .iter()
            .filter_map(|edge| (edge.kinds.contains(&EdgeKind::Calls)).then_some(edge_key(edge)?))
            .collect::<BTreeSet<_>>();
        let room = edge_max.saturating_sub(publication.edges.len());
        for observation in unique.into_values().flatten() {
            let Some(source) = units
                .get(observation.source.as_str())
                .filter(|file| file.digest == observation.site.digest && file.unit.source_complete)
            else {
                continue;
            };
            let Some(target) = units.get(observation.target.as_str()).filter(|file| {
                file.digest == observation.target_digest && file.unit.source_complete
            }) else {
                continue;
            };
            let Some(from) = source.callable(observation.site.range) else {
                continue;
            };
            let Some(to) = target.callable(observation.target_name) else {
                continue;
            };
            if to.object.name != observation.site.method
                || range_text(&target.unit.source, observation.target_name)
                    != Some(observation.site.method)
            {
                continue;
            }
            let names = methods
                .entry(observation.target.as_str())
                .or_insert_with(|| {
                    method_names(&target.unit.source, limits.syntax().syntax_elements_max()).ok()
                });
            let Some(names) = names else {
                report.dropped_at_limit += 1;
                continue;
            };
            if !names.contains(&observation.target_name) {
                continue;
            }
            let range = observation.site.range;
            let key = (
                &from.declaration.symbol,
                &to.declaration.symbol,
                &source.unit.unit,
                range.start(),
                range.end(),
            );
            if incumbent.contains(&key) {
                continue;
            }
            if report.edges.len() >= room {
                report.dropped_at_limit += 1;
                continue;
            }
            report.edges.push(StoredEdge {
                from: from.declaration.symbol.clone(),
                kind: ExactKind("call".to_owned()),
                kinds: vec![EdgeKind::Calls],
                to: to.declaration.symbol.clone(),
                occurrence: Some(SourceUnitSpan {
                    unit: source.unit.unit.clone(),
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
            });
        }
        report
    }
}

type EdgeKey<'a> = (&'a SymbolId, &'a SymbolId, &'a SourceUnitId, u64, u64);

fn edge_key(edge: &StoredEdge) -> Option<EdgeKey<'_>> {
    let span = edge.occurrence.as_ref()?;
    Some((
        &edge.from,
        &edge.to,
        &span.unit,
        span.range.start,
        span.range.end,
    ))
}

fn unique_sites<'a>(
    observations: impl Iterator<Item = &'a RustLspOperatorDefinition<'a>>,
) -> BTreeMap<(&'a ProjectPath, SourceRange), Option<&'a RustLspOperatorDefinition<'a>>> {
    let mut sites = BTreeMap::new();
    for observation in observations {
        sites
            .entry((observation.source, observation.site.range))
            .and_modify(|old| {
                if *old != Some(observation) {
                    *old = None;
                }
            })
            .or_insert(Some(observation));
    }
    sites
}

fn operator_method(kind: &str, written: &str) -> Option<&'static str> {
    Some(match (kind, written) {
        ("binary", "+") => "add",
        ("binary", "-") => "sub",
        ("binary", "*") => "mul",
        ("binary", "/") => "div",
        ("binary", "%") => "rem",
        ("binary", "&") => "bitand",
        ("binary", "|") => "bitor",
        ("binary", "^") => "bitxor",
        ("binary", "<<") => "shl",
        ("binary", ">>") => "shr",
        ("binary", "==") => "eq",
        ("binary", "!=") => "ne",
        ("binary", "<") => "lt",
        ("binary", "<=") => "le",
        ("binary", ">") => "gt",
        ("binary", ">=") => "ge",
        ("unary", "-") => "neg",
        ("unary", "!") => "not",
        ("unary", "*") => "deref",
        ("assign", "+=") => "add_assign",
        ("assign", "-=") => "sub_assign",
        ("assign", "*=") => "mul_assign",
        ("assign", "/=") => "div_assign",
        ("assign", "%=") => "rem_assign",
        ("assign", "&=") => "bitand_assign",
        ("assign", "|=") => "bitor_assign",
        ("assign", "^=") => "bitxor_assign",
        ("assign", "<<=") => "shl_assign",
        ("assign", ">>=") => "shr_assign",
        _ => return None,
    })
}

fn range_text(source: &str, range: SourceRange) -> Option<&str> {
    source.get(usize::try_from(range.start()).ok()?..usize::try_from(range.end()).ok()?)
}

fn method_names(source: &str, limit: usize) -> Result<BTreeSet<SourceRange>, RiftError> {
    static QUERY: OnceLock<RustQuery> = OnceLock::new();
    let query =
        QUERY.get_or_init(|| RustQuery::new(IMPLEMENTATION_METHODS).expect("Rust method grammar"));
    Ok(query
        .captures(source, limit)?
        .into_iter()
        .filter_map(|capture| SourceRange::new(capture.range.start, capture.range.end).ok())
        .collect())
}

pub(super) struct Callable<'a> {
    object: &'a Symbol,
    pub(super) declaration: &'a PackageSymbol,
}

pub(super) struct CheckedUnit<'a> {
    pub(super) unit: &'a PackageSourceUnit,
    pub(super) digest: FileDigest,
    callables: EnclosingDefinitions<Option<Callable<'a>>>,
}

impl CheckedUnit<'_> {
    pub(super) fn callable(&self, range: SourceRange) -> Option<&Callable<'_>> {
        self.callables.resolve(range.start(), range.end())?.as_ref()
    }
}

pub(super) fn checked_units<'a>(
    publication: &'a PackagePublication,
    paths: &BTreeSet<&str>,
) -> BTreeMap<&'a str, CheckedUnit<'a>> {
    let objects = publication
        .objects
        .iter()
        .filter_map(|object| {
            let id = object.id.as_ref()?;
            let identity = SymbolIdentity::parse(id.as_str()).ok()?;
            (object.language.name == "rust"
                && object.kinds.contains(&SymbolKind::Callable)
                && identity.owner() == &publication.owner)
                .then_some((id, object))
        })
        .collect::<BTreeMap<_, _>>();
    let mut declarations = BTreeMap::new();
    for declaration in &publication.declarations {
        let Some(&object) = objects.get(&declaration.symbol) else {
            continue;
        };
        if declaration.origin.source_kind != SourceKind::Authored {
            continue;
        }
        declarations
            .entry(&declaration.unit)
            .or_insert_with(BTreeMap::new)
            .entry((declaration.range.start, declaration.range.end))
            .and_modify(|previous: &mut Option<Callable<'_>>| {
                if previous
                    .as_ref()
                    .is_some_and(|row| row.declaration.symbol != declaration.symbol)
                {
                    *previous = None;
                }
            })
            .or_insert(Some(Callable {
                object,
                declaration,
            }));
    }
    publication
        .units
        .iter()
        .filter_map(|unit| {
            if unit.language.name != "rust"
                || !unit.source_complete
                || !paths.contains(unit.path.0.as_str())
            {
                return None;
            }
            let digest = FileDigest::of(unit.source.as_bytes());
            if file_digest(digest) != unit.content_digest {
                return None;
            }
            let callables = declarations.remove(&unit.unit)?;
            Some((
                unit.path.0.as_str(),
                CheckedUnit {
                    unit,
                    digest,
                    callables: EnclosingDefinitions::new(
                        callables
                            .into_iter()
                            .map(|((start, end), value)| (start, end, value)),
                    ),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::producer::{ProducerName, ProducerRevision};
    use rift_syntax::ShippedLanguage;

    const SOURCE: &str = r#"
pub struct Counter(pub u8);
impl core::ops::Add for Counter {
    type Output = Self;
    fn add(self, other: Self) -> Self { Self(self.0 + other.0) }
}
pub fn add(left: Counter, _: Counter) -> Counter { left }
pub fn combine(left: Counter, right: Counter) -> Counter {
    let op = "not a + token";
    left + right
}
"#;

    fn range_of(text: &str) -> SourceRange {
        let start = SOURCE.find(text).expect("authored fixture spelling");
        SourceRange::new(
            u64::try_from(start).expect("start"),
            u64::try_from(start + text.len()).expect("end"),
        )
        .expect("fixture range")
    }

    #[test]
    fn rust_lsp_calls_require_a_checked_operator_and_a_unique_implementation() {
        let analysis = super::super::fixture::package_result(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", SOURCE)],
            None,
        )
        .expect("authored Rust package");
        let publication = analysis.publication();
        let producer = FactProducer {
            name: ProducerName::parse("rust-analyzer").expect("producer"),
            revision: ProducerRevision::parse("0.0.1").expect("producer revision"),
        };
        let path = ProjectPath::new("src/lib.rs").expect("physical path");
        let sites = PackageAnalyzer::rust_lsp_operator_sites(SOURCE, 16).expect("checked sites");
        assert_eq!(sites.len(), 2);
        let operator = range_of("left + right");
        let site = sites[1];
        assert_eq!(site.range().start(), operator.start() + 5);
        assert_eq!(site.range().end(), operator.start() + 6);
        let method = range_of("fn add(");
        let target_name = SourceRange::new(method.start() + 3, method.start() + 6).expect("name");
        let digest = FileDigest::of(SOURCE.as_bytes());
        let valid = RustLspOperatorDefinition {
            source: &path,
            site,
            target: &path,
            target_digest: digest,
            target_name,
        };
        let limits = ExactPackageLimits::new(1, 1 << 20);
        let check = |defined: &[RustLspOperatorDefinition<'_>], held| {
            PackageAnalyzer::rust_lsp_operator_edges(publication, defined, &producer, held)
        };
        let selected = check(&[valid, valid], limits);
        let [edge] = selected.edges.as_slice() else {
            panic!("one checked operator call: {selected:?}");
        };
        assert!(edge.from.as_str().ends_with("/combine"));
        assert!(edge.to.as_str().ends_with("/Counter/add"));
        assert_eq!(edge.provider, EdgeProvider::Lsp);
        assert_eq!(edge.kinds, [EdgeKind::Calls]);
        assert_eq!(
            edge.occurrence.as_ref().expect("authored occurrence").range,
            TextRange {
                start: site.range().start(),
                end: site.range().end()
            }
        );

        let fallback = range_of("pub fn add(");
        let wrong_method = RustLspOperatorDefinition {
            target_name: SourceRange::new(fallback.start() + 7, fallback.start() + 10)
                .expect("other authored name"),
            ..valid
        };
        let stale = RustLspOperatorDefinition {
            target_digest: FileDigest::of(b"unrelated bytes"),
            ..valid
        };
        let wrong_name = RustLspOperatorDefinition {
            target_name: range_of("other.0"),
            ..valid
        };
        for rejected in [
            &[][..],
            &[wrong_method][..],
            &[stale][..],
            &[wrong_name][..],
            &[valid, wrong_method][..],
        ] {
            assert!(check(rejected, limits).edges.is_empty());
        }
        let incumbent = publication.edges.len();
        let bounded = check(
            &[valid],
            limits
                .with_edges(incumbent)
                .expect("existing edge capacity"),
        );
        assert!(bounded.edges.is_empty());
        assert_eq!(bounded.dropped_at_limit, 1);
    }
}
