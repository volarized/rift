//! Text of the `rift://workspace` resource: language configuration and one page of source units.
//!
//! The header names the configuration revision, and the page when there are several. The sections
//! follow directly under it, and the warnings section comes last. A language writes only what
//! departs from the normal state of an enabled language.

use rift_protocol::workspace::{
    WorkspaceLanguageSummary, WorkspaceLspSummary, WorkspaceResourcePage, WorkspaceSourceUnit,
};

use super::facts::{Facts, cut_hash, wire_name};
use super::layout::{self, Sections};
use crate::output::text::{TextError, TextWriter};

/// Levels that put `include` and `exclude` lines under their language.
const PATTERN_DEPTH: usize = 1;

/// Writes the header, the languages, the source units, and the warnings section.
pub(super) fn answer(out: &mut TextWriter, page: &WorkspaceResourcePage) -> Result<(), TextError> {
    let WorkspaceResourcePage {
        configuration_revision,
        languages,
        source,
        warnings,
        pagination,
    } = page;
    let mut header = Facts::default();
    header.push(&format!(
        "workspace {}",
        cut_hash(&configuration_revision.0)
    ));
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
    }
    sections.warnings(warnings)
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
        include.iter().map(|pattern| pattern.0.as_str()),
    )?;
    patterns_line(
        sections,
        "exclude",
        exclude.iter().map(|pattern| pattern.0.as_str()),
    )
}

/// `label p1, p2` under the language, or nothing without patterns.
fn patterns_line<'a>(
    sections: &mut Sections<'_>,
    label: &str,
    patterns: impl Iterator<Item = &'a str>,
) -> Result<(), TextError> {
    let patterns: Vec<&str> = patterns.collect();
    if patterns.is_empty() {
        return Ok(());
    }
    sections.entry(PATTERN_DEPTH, &format!("{label} {}", patterns.join(", ")))
}

/// `path · language · digest`, without the language when no language matched.
fn unit_line(unit: &WorkspaceSourceUnit) -> String {
    let WorkspaceSourceUnit {
        path,
        digest,
        language,
    } = unit;
    let mut facts = Facts::default();
    facts.push(&path.0);
    if let Some(language) = language {
        facts.push(&language.identity_segment());
    }
    facts.push(cut_hash(&digest.0));
    facts.as_str().to_owned()
}
