//! Text of the `rift://workspace` resource: language configuration and one page of source units.
//!
//! The header names the source page when there are several. The catalog and language
//! configuration follow, with the URI to read when another page exists.

use std::borrow::Cow;

use rift_protocol::workspace::{
    WorkspaceLanguageSummary, WorkspaceLspSummary, WorkspaceResourcePage, WorkspaceSourceUnit,
};

use super::facts::{Facts, quoted_value, wire_name};
use super::layout::{self, Sections};
use crate::output::text::{TextError, TextWriter};
use crate::resource::workspace_page_uri;

/// Levels that put `include` and `exclude` lines under their language.
const PATTERN_DEPTH: usize = 1;

/// Writes the header, language configuration, and source units.
pub(super) fn answer(out: &mut TextWriter, page: &WorkspaceResourcePage) -> Result<(), TextError> {
    let WorkspaceResourcePage {
        languages,
        source,
        pagination,
        revision,
        ..
    } = page;
    let mut header = Facts::default();
    header.push("workspace");
    if let Some(position) = layout::page_fact(pagination) {
        header.push(&position);
    }
    out.raw_line(0, header.as_str())?;
    let mut sections = Sections::new(out);
    if !languages.is_empty() {
        sections.open("languages")?;
        for language in languages {
            language_lines(&mut sections, language)?;
        }
    }
    if !source.is_empty() {
        sections.open("source")?;
        for unit in source {
            sections.entry(0, &unit_line(unit))?;
        }
    } else if pagination.page > pagination.pages {
        out.raw_line(0, "No source files on this page.")?;
    }
    if let Some(next) = pagination
        .page
        .checked_add(1)
        .filter(|next| *next <= pagination.pages)
    {
        out.raw_line(
            0,
            &format!(
                "next | {}",
                workspace_page_uri(next, pagination.max_results, revision)
            ),
        )?;
    }
    Ok(())
}

/// The language line, then its `include` and `exclude` lines.
fn language_lines(
    sections: &mut Sections<'_>,
    summary: &WorkspaceLanguageSummary,
) -> Result<(), TextError> {
    let WorkspaceLanguageSummary {
        language,
        enabled,
        include,
        exclude,
        execution,
        syntax,
        lsp,
    } = summary;
    let mut facts = Facts::default();
    facts.push(&language.identity_segment());
    if !enabled {
        facts.push("disabled");
    }
    if *syntax {
        facts.push("syntax");
    }
    if *execution {
        facts.push("execution");
    }
    if let Some(WorkspaceLspSummary { process, state }) = lsp {
        facts.push(&format!("lsp {process} {}", wire_name(state)?));
    }
    sections.entry(0, facts.as_str())?;
    patterns_line(
        sections,
        "include",
        include.iter().map(|pattern| quoted_value(&pattern.0)),
    )?;
    patterns_line(
        sections,
        "exclude",
        exclude.iter().map(|pattern| quoted_value(&pattern.0)),
    )
}

/// `label p1, p2` under the language, or nothing without patterns.
fn patterns_line<'a>(
    sections: &mut Sections<'_>,
    label: &str,
    patterns: impl Iterator<Item = Cow<'a, str>>,
) -> Result<(), TextError> {
    let patterns: Vec<Cow<'a, str>> = patterns.collect();
    if patterns.is_empty() {
        return Ok(());
    }
    sections.entry(PATTERN_DEPTH, &format!("{label} {}", patterns.join(", ")))
}

/// `path | language`, with only the path when no language matched.
fn unit_line(unit: &WorkspaceSourceUnit) -> String {
    let WorkspaceSourceUnit { path, language, .. } = unit;
    let mut facts = Facts::default();
    facts.push_value(&path.0);
    if let Some(language) = language {
        facts.push(&language.identity_segment());
    }
    facts.as_str().to_owned()
}
