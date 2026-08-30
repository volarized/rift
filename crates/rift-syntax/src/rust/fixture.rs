//! Test fixtures shared by the Rust binding and layout suites.

use rift_binding::{BindingGraph, ResolutionSet};
use rift_core::{
    ContributionOrigin, ProjectPath, SourceKind, SourceLocation, SourceUnitId, encode_path,
};

use crate::{RustSyntaxProvider, SyntaxDocument, SyntaxProvider, SyntaxSource};

pub(super) fn analyze(path: &str, text: &str) -> SyntaxDocument {
    let path = ProjectPath::new(path).expect("fixture path");
    RustSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text })
        .expect("fixture parses")
}

pub(super) fn source_unit(path: &str) -> SourceUnitId {
    SourceUnitId::parse(&format!("rift://source/project/{}", encode_path(path)))
        .expect("fixture unit identity")
}

pub(super) fn origin() -> ContributionOrigin {
    let location = SourceLocation::Project { package: None };
    ContributionOrigin::new(Some(location), SourceKind::Authored).expect("authored origin")
}

pub(super) fn offset(text: &str, needle: &str) -> u64 {
    u64::try_from(text.find(needle).expect("needle in fixture")).expect("offset fits")
}

pub(super) fn last_offset(text: &str, needle: &str) -> u64 {
    u64::try_from(text.rfind(needle).expect("needle in fixture")).expect("offset fits")
}

/// Targets of the reference starting at `at` in `unit_path`: `(name, unit key, start)`.
pub(super) fn targets_at(
    graph: &BindingGraph,
    set: &ResolutionSet,
    unit_path: &str,
    at: u64,
) -> Vec<(String, String, u64)> {
    let reference = graph
        .reference_ids()
        .find(|id| {
            let reference = graph.reference(*id);
            let unit = graph.unit(graph.scope(reference.scope()).unit());
            unit.source().key().as_str() == unit_path && reference.range().start() == at
        })
        .expect("reference at offset");
    set.resolution(reference)
        .targets()
        .iter()
        .map(|id| {
            let definition = graph.definition(*id);
            let unit = graph.unit(graph.scope(definition.scope()).unit());
            (
                definition.name().as_str().to_owned(),
                unit.source().key().as_str().to_owned(),
                definition.range().start(),
            )
        })
        .collect()
}
