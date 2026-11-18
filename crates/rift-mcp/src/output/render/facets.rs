//! Text of a facets answer, with declaration source and syntax structure.

use super::facts::wire_name;
use super::layout;
use crate::output::text::{TextError, TextWriter, quoted, visible};
use rift_protocol::facets_read::FacetDeclaration;
use rift_protocol::read::{
    FacetRegion, FacetsResult, NodeFacet, SourceUnitId, SyntaxElement, TextRange,
};
const SEPARATOR: &str = " | ";

pub(super) fn answer(out: &mut TextWriter, result: &FacetsResult) -> Result<(), TextError> {
    out.raw_line(0, &visible(result.symbol.as_str()))?;
    for declaration in &result.declarations {
        declaration_text(out, declaration)?;
    }
    if result.declarations.is_empty() {
        out.raw_line(0, "No declaration source available.")?;
    }
    if let Some(cursor) = &result.facet_cursor {
        out.raw_line(0, &format!("next declarations | facet_cursor {cursor}"))?;
    }
    layout::selected_revision(out, Some(&result.revision))
}

fn declaration_text(out: &mut TextWriter, declaration: &FacetDeclaration) -> Result<(), TextError> {
    location_text(
        out,
        &declaration.source.span.unit,
        &declaration.range,
        &format!(
            "Declaration {} | {}",
            declaration.declaration,
            visible(&declaration.kind.0)
        ),
    )?;
    out.raw_lines(1, &declaration.source.text)?;
    for element in &declaration.syntax {
        entry(out, element)?;
    }
    Ok(())
}

fn location_text(
    out: &mut TextWriter,
    unit: &SourceUnitId,
    range: &TextRange,
    heading: &str,
) -> Result<(), TextError> {
    let path = unit.project_path();
    let path = path.as_deref().unwrap_or(unit.as_str());
    let path = visible(path);
    out.raw_line(
        0,
        &format!(
            "{heading} | {} | bytes {}..{}",
            quoted(&path, &[SEPARATOR]),
            range.start,
            range.end
        ),
    )
}

/// Writes a syntax element's kind, byte range, address, and other facts on one line.
fn entry(out: &mut TextWriter, syntax_element: &SyntaxElement) -> Result<(), TextError> {
    let SyntaxElement {
        declaration,
        index,
        unit: _,
        language: _,
        kind,
        facets,
        range,
        source,
        regions,
        parent,
        extensions: _,
    } = syntax_element;
    let kind = visible(&kind.0);
    let mut facts = vec![
        quoted(&kind, &[SEPARATOR]).into_owned(),
        format!("bytes {}..{}", range.start, range.end),
    ];
    if let Some(declaration) = declaration {
        facts.push(format!("declaration {declaration}"));
    }
    facts.push(format!("index {index}"));
    if let Some(parent) = parent {
        facts.push(format!("parent {parent}"));
    }
    let count = source.lines().count();
    if count > 1 {
        facts.push(format!("{count} lines"));
    }
    if facets.contains(&NodeFacet::Generated) {
        facts.push("generated".to_owned());
    }
    if facets.contains(&NodeFacet::Test) {
        facts.push("test".to_owned());
    }
    facts.extend(region_facts(regions)?);
    out.raw_line(1, &facts.join(SEPARATOR))
}

/// The regions as `role START..END`, one fact per region.
fn region_facts(regions: &[FacetRegion]) -> Result<Vec<String>, TextError> {
    regions
        .iter()
        .map(|FacetRegion { role, range }| {
            Ok(format!(
                "{} {}..{}",
                wire_name(role)?,
                range.start,
                range.end
            ))
        })
        .collect()
}
