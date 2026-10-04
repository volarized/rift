//! Text of the `rift://map` resource: totals, the module tree, hubs, and packages.
//!
//! The header names the revision. Sections follow directly under it in a fixed order and are left
//! out when empty; the warnings section comes last. Pagination is not written: the map is always
//! one page.

use rift_protocol::dependencies::PackageContextEntry;
use rift_protocol::map::{MapHub, MapLanguage, MapModule, MapModuleRelationship, WorkspaceMap};

use super::facts::{FACT_SEPARATOR, Facts, cut_hash, package_text, wire_name};
use super::layout::{Sections, counted};
use crate::output::text::{TextError, TextWriter};

/// Writes the header, every section the map holds, and the warnings section.
pub(super) fn answer(out: &mut TextWriter, map: &WorkspaceMap) -> Result<(), TextError> {
    let WorkspaceMap {
        revision,
        languages,
        modules,
        hubs,
        entry_points,
        docs,
        module_relationships,
        packages,
        warnings,
        pagination: _,
    } = map;
    out.raw_line(0, &format!("map {}", cut_hash(&revision.0)))?;
    let mut sections = Sections::new(out);
    if !languages.is_empty() {
        sections.open("languages")?;
        for language in languages {
            sections.entry(0, &language_line(language))?;
        }
    }
    if !modules.is_empty() {
        sections.open("modules")?;
        module_lines(&mut sections, modules)?;
    }
    if !hubs.is_empty() {
        sections.open("hubs")?;
        for hub in hubs {
            sections.entry(0, &hub_line(hub))?;
        }
    }
    if !entry_points.is_empty() {
        sections.open("entry points")?;
        for symbol in entry_points {
            sections.entry(0, &symbol.0)?;
        }
    }
    if !docs.is_empty() {
        sections.open("docs")?;
        for path in docs {
            sections.entry(0, &path.0)?;
        }
    }
    if !module_relationships.is_empty() {
        sections.open("module relationships")?;
        for relationship in module_relationships {
            sections.entry(0, &relationship_line(relationship))?;
        }
    }
    if !packages.is_empty() {
        sections.open("packages")?;
        for package in packages {
            sections.entry(0, &package_line(package)?)?;
        }
    }
    sections.warnings(warnings)
}

/// `language · N files · M symbols`.
fn language_line(language: &MapLanguage) -> String {
    let MapLanguage {
        language,
        files,
        symbols,
    } = language;
    let mut facts = Facts::default();
    facts.push(&language.identity_segment());
    facts.push(&counts(*files, *symbols));
    facts.as_str().to_owned()
}

/// `N files · M symbols`, in the singular for one.
fn counts(files: u64, symbols: u64) -> String {
    format!(
        "{}{FACT_SEPARATOR}{}",
        counted(files, "file"),
        counted(symbols, "symbol")
    )
}

/// Writes the module tree in path order, a child one level deeper than its parent.
///
/// The walk keeps its own stack, so the depth of the tree costs no call depth.
fn module_lines(sections: &mut Sections<'_>, modules: &[MapModule]) -> Result<(), TextError> {
    let mut pending: Vec<(&MapModule, usize, &str)> =
        modules.iter().rev().map(|module| (module, 0, "")).collect();
    while let Some((module, depth, parent)) = pending.pop() {
        let MapModule {
            path,
            files,
            symbols,
            children,
        } = module;
        let mut facts = Facts::default();
        facts.push(relative_path(parent, &path.0));
        facts.push(&counts(*files, *symbols));
        sections.entry(depth, facts.as_str())?;
        pending.extend(
            children
                .iter()
                .rev()
                .map(|child| (child, depth.saturating_add(1), path.0.as_str())),
        );
    }
    Ok(())
}

/// `path` without its `parent/` prefix, or `path` itself when it does not extend `parent`.
fn relative_path<'a>(parent: &str, path: &'a str) -> &'a str {
    path.strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
}

/// `symbol · kind · N references`.
fn hub_line(hub: &MapHub) -> String {
    let MapHub {
        symbol,
        kind,
        references,
    } = hub;
    let mut facts = Facts::default();
    facts.push(&symbol.0);
    facts.push(&kind.0);
    facts.push(&references_fact(*references));
    facts.as_str().to_owned()
}

/// `from → to · N references`.
fn relationship_line(relationship: &MapModuleRelationship) -> String {
    let MapModuleRelationship {
        from,
        to,
        references,
    } = relationship;
    let mut facts = Facts::default();
    facts.push(&format!("{} → {}", from.0, to.0));
    facts.push(&references_fact(*references));
    facts.as_str().to_owned()
}

/// `N references`, in the singular for one.
fn references_fact(references: u64) -> String {
    counted(references, "reference")
}

/// One package entry, written like a package entry of a warning.
fn package_line(package: &PackageContextEntry) -> Result<String, TextError> {
    let PackageContextEntry {
        manager,
        name,
        version,
        requirement,
        availability,
    } = package;
    Ok(package_text(
        (manager, name),
        (
            version.as_deref().unwrap_or_default(),
            requirement.as_deref().unwrap_or_default(),
        ),
        &wire_name(availability)?,
    ))
}
