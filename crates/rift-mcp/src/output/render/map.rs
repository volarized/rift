//! Text of the workspace orientation snapshot.

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry};
use rift_protocol::map::WorkspaceMap;

use super::layout::{Sections, counted};
use crate::output::text::{TextError, TextWriter};
use crate::resource::WORKSPACE_URI;

/// Most entries shown in each section of the text map.
const ENTRIES_MAX: usize = 8;

/// Languages, directories, and entry points that help locate a starting point.
pub(super) fn answer(out: &mut TextWriter, map: &WorkspaceMap) -> Result<(), TextError> {
    out.raw_line(0, "workspace map")?;
    let mut sections = Sections::new(out);
    if !map.languages.is_empty() {
        sections.open("languages")?;
        for language in map.languages.iter().take(ENTRIES_MAX) {
            sections.entry(
                0,
                &format!(
                    "{} | {} | {}",
                    language.language.identity_segment(),
                    counted(language.files, "file"),
                    counted(language.symbols, "declaration")
                ),
            )?;
        }
    }
    if !map.modules.is_empty() {
        sections.open("directories")?;
        for module in map.modules.iter().take(ENTRIES_MAX) {
            sections.entry(
                0,
                &format!("{} | {}", module.path.0, counted(module.files, "file")),
            )?;
        }
    }
    if !map.entry_points.is_empty() {
        sections.open("entry points")?;
        for symbol in map.entry_points.iter().take(ENTRIES_MAX) {
            sections.entry(0, symbol.as_str())?;
        }
    }
    let mut hubs = map.hubs.iter().filter(|hub| hub.references > 0).peekable();
    if hubs.peek().is_some() {
        sections.open("most referenced")?;
        for hub in hubs.take(ENTRIES_MAX) {
            sections.entry(
                0,
                &format!(
                    "{} | {}",
                    hub.symbol.as_str(),
                    counted(hub.references, "reference")
                ),
            )?;
        }
    }
    if !map.docs.is_empty() {
        sections.open("documentation")?;
        for path in map.docs.iter().take(ENTRIES_MAX) {
            sections.entry(0, &path.0)?;
        }
    }
    if !map.packages.is_empty() {
        sections.open("packages")?;
        for package in map.packages.iter().take(ENTRIES_MAX) {
            sections.entry(0, &package_line(package))?;
        }
    }
    if !map.recommendations.is_empty() {
        sections.open("language server recommendations")?;
        for recommendation in map.recommendations.iter().take(ENTRIES_MAX) {
            sections.entry(
                0,
                &format!(
                    "{} | {}",
                    recommendation.language.identity_segment(),
                    recommendation.root,
                ),
            )?;
            sections.entry(1, &recommendation.command)?;
        }
    }
    if map.languages.is_empty() && map.modules.is_empty() && map.docs.is_empty() {
        out.raw_line(0, "No indexed files.")?;
    }
    out.raw_line(0, &format!("source catalog | {WORKSPACE_URI}"))
}

/// One dependency context entry: `name@version` for a pinned release, `name requirement` for a
/// declared range, then its package manager, and its availability when no public registry
/// serves it.
fn package_line(package: &PackageContextEntry) -> String {
    let selected = match (&package.version, &package.requirement) {
        (Some(version), _) => format!("{}@{version}", package.name),
        (None, Some(requirement)) => format!("{} {requirement}", package.name),
        (None, None) => package.name.clone(),
    };
    if package.availability == PackageAvailability::Canonical {
        format!("{selected} ({})", package.manager)
    } else {
        let availability = serde_json::to_value(package.availability)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        format!("{selected} ({} | {availability})", package.manager)
    }
}
