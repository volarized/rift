//! Package and component context for source-bound framework syntax.

use std::collections::BTreeMap;
use std::path::Path;

use rift_core::ProjectPath;
use rift_error::{RiftError, errors};
use rift_protocol::configuration::{SyntaxFrameworkConfiguration, TailwindVersion};
use rift_protocol::read::{FileId, ReadWarning, SyntaxFramework};
use rift_syntax::{AngularTemplate, ByteRange, SyntaxDocument, SyntaxLimits, SyntaxSource};

use crate::{PackageSource, PathMatcher};

/// Framework interpretation for one source under its own package or component.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedFramework {
    /// Whether this source is an Angular external template.
    pub angular: bool,
    /// Original ranges of Angular templates owned by TypeScript components.
    pub templates: Vec<ByteRange>,
    /// Tailwind major version established for this source.
    pub tailwind: Option<TailwindVersion>,
}

/// Framework context resolved from bounded captured sources and explicit selections.
/// Raw syntax facts remain independent of this value.
#[derive(Debug, Default)]
pub struct FrameworkContext {
    files: BTreeMap<ProjectPath, ResolvedFramework>,
    digests: BTreeMap<ProjectPath, rift_core::FileDigest>,
    warnings: Vec<ReadWarning>,
    provider_calls: u64,
}

impl FrameworkContext {
    /// Resolves nearest package context, imported Angular component ownership, and
    /// Tailwind version plus configuration or stylesheet evidence.
    ///
    /// # Errors
    ///
    /// Returns invalid path selection, source-bound parser failure, or cancellation.
    pub fn resolve(
        sources: &[PackageSource<'_>],
        explicit: &[SyntaxFrameworkConfiguration],
        limits: SyntaxLimits,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let configuration = rift_protocol::configuration::SyntaxConfiguration {
            frameworks: explicit.to_vec(),
            ..rift_protocol::configuration::SyntaxConfiguration::default()
        };
        if let Some(violation) = configuration.violation() {
            return Err(rift_core::configuration_violation_error(&violation));
        }
        let (inputs, provider_calls) = FrameworkInputs::capture(sources, limits, cancelled)?;
        let explicit = explicit
            .iter()
            .map(|selection| {
                let include = selection
                    .include
                    .iter()
                    .map(|pattern| pattern.0.clone())
                    .collect::<Vec<_>>();
                PathMatcher::build(Path::new("."), &include, &[])
                    .map(|matcher| (selection, matcher))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut answer = Self {
            provider_calls,
            ..Self::default()
        };
        let mut blocked = std::collections::BTreeSet::new();
        for (path, text) in &inputs.sources {
            if !framework_source(path) {
                continue;
            }
            if cancelled() {
                return errors::index::workspace_cancelled().fail();
            }
            let selections = explicit
                .iter()
                .filter(|(selection, matcher)| {
                    !selection.include.is_empty() && matcher.includes(Path::new(path.as_str()))
                })
                .collect::<Vec<_>>();
            if selections.len() > 1 {
                blocked.insert((*path).clone());
                answer.warn(
                    path,
                    SyntaxFramework::Angular,
                    "More than one explicit framework context matches this source.",
                );
                answer.warn(
                    path,
                    SyntaxFramework::Tailwind,
                    "More than one explicit framework context matches this source.",
                );
                continue;
            }
            let selected = selections.first().map(|(selection, _)| *selection);
            let mut context = ResolvedFramework {
                angular: answer
                    .files
                    .get(*path)
                    .is_some_and(|context| context.angular)
                    || selected.is_some_and(|selection| selection.angular)
                        && ["html", "htm"]
                            .iter()
                            .any(|claimed| claimed.eq_ignore_ascii_case(extension(path))),
                tailwind: selected.and_then(|selection| selection.tailwind),
                ..ResolvedFramework::default()
            };
            answer.resolve_tailwind(path, &inputs, &mut context);
            answer.resolve_components(path, text, &inputs, limits, &mut context)?;
            if context.angular || !context.templates.is_empty() || context.tailwind.is_some() {
                answer.files.insert((*path).clone(), context);
            }
        }
        answer.files.retain(|path, _| !blocked.contains(path));
        answer.digests = answer
            .files
            .keys()
            .filter_map(|path| {
                inputs
                    .sources
                    .get(path)
                    .map(|text| (path.clone(), rift_core::FileDigest::of(text.as_bytes())))
            })
            .collect();
        Ok(answer)
    }

    fn resolve_tailwind(
        &mut self,
        path: &ProjectPath,
        inputs: &FrameworkInputs<'_>,
        context: &mut ResolvedFramework,
    ) {
        if context.tailwind.is_some() {
            return;
        }
        let Some((owner, Some(dependencies))) = nearest(&inputs.manifests, path) else {
            if inputs.intent.contains(path) {
                self.warn(path, SyntaxFramework::Tailwind, "Tailwind stylesheet has no valid package context; supply explicit framework context.");
            }
            return;
        };
        let Some(declared) = dependencies.get("tailwindcss") else {
            if inputs.intent.contains(path) {
                self.warn(path, SyntaxFramework::Tailwind, "Tailwind stylesheet has no declared package version; supply explicit framework context.");
            }
            return;
        };
        let version = match locked_version(&inputs.lockfiles, owner) {
            LockedVersion::Missing => exact_tailwind(declared),
            LockedVersion::Resolved(version) => Some(version),
            LockedVersion::Unresolved => None,
        };
        match version {
            Some(version)
                if inputs
                    .evidence
                    .get(owner)
                    .is_some_and(|(v3, v4)| match version {
                        TailwindVersion::V3 => *v3,
                        TailwindVersion::V4 => *v4,
                    }) =>
            {
                context.tailwind = Some(version);
            }
            Some(_) => self.warn(
                path,
                SyntaxFramework::Tailwind,
                "Tailwind package has no applicable configuration or stylesheet import.",
            ),
            None => self.warn(
                path,
                SyntaxFramework::Tailwind,
                "Tailwind package version is unresolved; supply explicit framework context.",
            ),
        }
    }

    fn resolve_components(
        &mut self,
        path: &ProjectPath,
        text: &str,
        inputs: &FrameworkInputs<'_>,
        limits: SyntaxLimits,
        context: &mut ResolvedFramework,
    ) -> Result<(), RiftError> {
        if ![
            rift_syntax::ShippedLanguage::TypeScript,
            rift_syntax::ShippedLanguage::TypeScriptTsx,
        ]
        .iter()
        .any(|language| language.definition().matches_extension(extension(path)))
            || !text.contains("@angular/core")
        {
            return Ok(());
        }
        let Some(provider) = rift_syntax::registry::provider_for_extension(extension(path)) else {
            return Ok(());
        };
        let source = SyntaxSource { path, text };
        self.provider_calls += 1;
        let document = match provider.analyze(source, limits) {
            Ok(document) => document,
            Err(error)
                if matches!(
                    error.slug(),
                    errors::syntax::source_too_large::SLUG
                        | errors::syntax::too_many_nodes::SLUG
                        | errors::syntax::too_deep::SLUG
                ) =>
            {
                self.warn(path, SyntaxFramework::Angular, "Angular component syntax exceeded its configured bounds; original source is retained.");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        for component in rift_syntax::angular_components(source, &document) {
            for template in component.templates {
                match template {
                    AngularTemplate::Inline { range } => context.templates.push(range),
                    AngularTemplate::External { path: authored } => {
                        let target = relative_source(path, &authored);
                        match target.as_ref().filter(|target| inputs.sources.contains_key(target)) {
                            Some(target) if ["html", "htm"].iter().any(|claimed| claimed.eq_ignore_ascii_case(extension(target))) && nearest(&inputs.manifests, target).map(|(owner, _)| owner)
                                == nearest(&inputs.manifests, path).map(|(owner, _)| owner) => {
                                self.files.entry(target.clone()).or_default().angular = true;
                            }
                            _ => self.warn(path, SyntaxFramework::Angular, "Angular template source is missing or outside its component package."),
                        }
                    }
                    AngularTemplate::Unresolved { .. } => self.warn(path, SyntaxFramework::Angular, "Angular template expression is dynamic or escaped; original source is retained."),
                }
            }
        }
        context.templates.sort_by_key(|range| range.start);
        Ok(())
    }

    /// Context for one captured path, absent when ordinary syntax alone applies.
    #[must_use]
    pub fn for_path(&self, path: &ProjectPath) -> Option<&ResolvedFramework> {
        self.files.get(path)
    }

    /// Typed conditions that left framework context unresolved.
    #[must_use]
    pub fn warnings(&self) -> &[ReadWarning] {
        &self.warnings
    }

    /// Parser calls used to resolve component ownership and stylesheet evidence.
    #[must_use]
    pub const fn provider_calls(&self) -> u64 {
        self.provider_calls
    }

    /// Applies resolved interpretation to original source bytes after ordinary syntax.
    ///
    /// # Errors
    ///
    /// Returns source witness, range, parser, or aggregate syntax-bound failures.
    pub fn apply(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
        mut document: SyntaxDocument,
    ) -> Result<(SyntaxDocument, Vec<ReadWarning>, u64), RiftError> {
        let Some(context) = self.for_path(source.path) else {
            return Ok((document, Vec::new(), 0));
        };
        if self.digests.get(source.path) != Some(&rift_core::FileDigest::of(source.text.as_bytes()))
        {
            return Ok((
                document,
                vec![warning(
                    source.path,
                    if context.angular || !context.templates.is_empty() {
                        SyntaxFramework::Angular
                    } else {
                        SyntaxFramework::Tailwind
                    },
                    "Framework context belongs to different source bytes; original source is retained.",
                )],
                0,
            ));
        }
        let mut calls = 0;
        if context.angular
            && document.language() != &rift_syntax::ShippedLanguage::HtmlAngular.language()
        {
            document = rift_syntax::ShippedLanguage::HtmlAngular
                .definition()
                .syntax_provider()
                .analyze(source, limits)?;
            calls += 1;
        }
        if !context.templates.is_empty() {
            document = rift_syntax::append_angular_templates(
                source,
                limits,
                &document,
                &context.templates,
            )?;
            calls += u64::try_from(
                context
                    .templates
                    .iter()
                    .filter(|range| range.start < range.end)
                    .count(),
            )
            .unwrap_or(u64::MAX);
        }
        let mut warnings = Vec::new();
        if let Some(version) = context.tailwind {
            let major = match version {
                TailwindVersion::V3 => 3,
                TailwindVersion::V4 => 4,
            };
            let facts = rift_syntax::tailwind_symbols(source, &document, major, limits)?;
            if !facts.unresolved.is_empty() {
                warnings.push(warning(
                    source.path,
                    SyntaxFramework::Tailwind,
                    "Dynamic class expressions are retained without Tailwind interpretation.",
                ));
            }
            document =
                rift_syntax::append_framework_symbols(source, limits, &document, facts.symbols)?;
        }
        Ok((document, warnings, calls))
    }

    fn warn(&mut self, path: &ProjectPath, framework: SyntaxFramework, detail: &str) {
        if framework_source(path) {
            self.warnings.push(warning(path, framework, detail));
        }
    }
}

type PackageDependencies<'source> = BTreeMap<&'source str, Option<BTreeMap<String, String>>>;

struct FrameworkInputs<'source> {
    sources: BTreeMap<&'source ProjectPath, &'source str>,
    manifests: PackageDependencies<'source>,
    lockfiles: PackageDependencies<'source>,
    evidence: BTreeMap<&'source str, (bool, bool)>,
    intent: std::collections::BTreeSet<&'source ProjectPath>,
}

impl<'source> FrameworkInputs<'source> {
    fn capture(
        sources: &[PackageSource<'source>],
        limits: SyntaxLimits,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(Self, u64), RiftError> {
        let sources: BTreeMap<_, _> = sources
            .iter()
            .map(|source| (source.path(), source.text()))
            .collect();
        let manifests = sources
            .iter()
            .filter(|(path, _)| file_name(path) == "package.json")
            .map(|(path, text)| {
                (
                    directory(path),
                    rift_dependency::node_package_dependencies(text.as_bytes()).ok(),
                )
            })
            .collect();
        let lockfiles = sources
            .iter()
            .filter(|(path, _)| file_name(path) == "package-lock.json")
            .map(|(path, text)| {
                (
                    directory(path),
                    rift_dependency::npm_package_versions(text.as_bytes()).ok(),
                )
            })
            .collect();
        let mut styles = BTreeMap::new();
        let mut calls = 0;
        let provider = rift_syntax::ShippedLanguage::Css
            .definition()
            .syntax_provider();
        for (path, text) in &sources {
            if extension(path).eq_ignore_ascii_case("css")
                && text.len() <= limits.source_bytes_max()
                && text.contains("tailwind")
            {
                if cancelled() {
                    return errors::index::workspace_cancelled().fail();
                }
                match provider.analyze(SyntaxSource { path, text }, limits) {
                    Ok(document) => {
                        styles.insert(*path, document);
                    }
                    Err(error)
                        if matches!(
                            error.slug(),
                            errors::syntax::source_too_large::SLUG
                                | errors::syntax::too_many_nodes::SLUG
                                | errors::syntax::too_deep::SLUG
                        ) => {}
                    Err(error) => return Err(error),
                }
                calls += 1;
            }
        }
        let mut evidence = BTreeMap::<&str, (bool, bool)>::new();
        let mut intent = std::collections::BTreeSet::new();
        for (path, text) in &sources {
            if cancelled() {
                return errors::index::workspace_cancelled().fail();
            }
            let (v3, v4) = stylesheet_evidence(path, text, styles.get(path));
            if extension(path).eq_ignore_ascii_case("css") && (v3 || v4) {
                intent.insert(*path);
            }
            if let Some((owner, _)) = nearest(&manifests, path) {
                let held = evidence.entry(owner).or_default();
                held.0 |= v3;
                held.1 |= v4;
            }
        }
        Ok((
            Self {
                sources,
                manifests,
                lockfiles,
                evidence,
                intent,
            },
            calls,
        ))
    }
}

fn warning(path: &ProjectPath, framework: SyntaxFramework, detail: &str) -> ReadWarning {
    ReadWarning::FrameworkContextUnresolved {
        unit: FileId(format!(
            "rift://file/{}",
            rift_core::encode_path(path.as_str())
        )),
        framework,
        detail: detail.to_owned(),
    }
}

fn directory(path: &ProjectPath) -> &str {
    path.as_str()
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent)
}

fn file_name(path: &ProjectPath) -> &str {
    path.as_str().rsplit('/').next().unwrap_or_default()
}

fn extension(path: &ProjectPath) -> &str {
    Path::new(path.as_str())
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
}

fn framework_source(path: &ProjectPath) -> bool {
    [
        rift_syntax::ShippedLanguage::Html,
        rift_syntax::ShippedLanguage::Css,
        rift_syntax::ShippedLanguage::JavaScript,
        rift_syntax::ShippedLanguage::TypeScript,
        rift_syntax::ShippedLanguage::TypeScriptTsx,
        rift_syntax::ShippedLanguage::Vue,
        rift_syntax::ShippedLanguage::Svelte,
    ]
    .iter()
    .any(|language| language.definition().matches_extension(extension(path)))
}

fn nearest<'owners, 'source, T>(
    owners: &'owners BTreeMap<&'source str, T>,
    path: &ProjectPath,
) -> Option<(&'source str, &'owners T)> {
    let mut parent = directory(path);
    loop {
        if let Some((owner, value)) = owners.get_key_value(parent) {
            return Some((*owner, value));
        }
        if parent.is_empty() {
            return None;
        }
        parent = parent.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

fn exact_tailwind(text: &str) -> Option<TailwindVersion> {
    match semver::Version::parse(text).ok()?.major {
        3 => Some(TailwindVersion::V3),
        4 => Some(TailwindVersion::V4),
        _ => None,
    }
}

enum LockedVersion {
    Missing,
    Resolved(TailwindVersion),
    Unresolved,
}

fn locked_version(
    lockfiles: &BTreeMap<&str, Option<BTreeMap<String, String>>>,
    owner: &str,
) -> LockedVersion {
    let Some((root, versions)) = lockfiles
        .iter()
        .filter(|(root, _)| {
            root.is_empty()
                || owner == **root
                || owner
                    .strip_prefix(**root)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|(root, _)| root.len())
    else {
        return LockedVersion::Missing;
    };
    let Some(versions) = versions.as_ref() else {
        return LockedVersion::Unresolved;
    };
    let relative = owner
        .strip_prefix(root)
        .unwrap_or(owner)
        .trim_start_matches('/');
    let key = if relative.is_empty() {
        "node_modules/tailwindcss".to_owned()
    } else {
        format!("{relative}/node_modules/tailwindcss")
    };
    versions
        .get(&key)
        .or_else(|| versions.get("node_modules/tailwindcss"))
        .map_or(LockedVersion::Missing, |version| {
            exact_tailwind(version).map_or(LockedVersion::Unresolved, LockedVersion::Resolved)
        })
}

fn stylesheet_evidence(
    path: &ProjectPath,
    text: &str,
    document: Option<&SyntaxDocument>,
) -> (bool, bool) {
    let mut v3 = matches!(
        file_name(path),
        "tailwind.config.js" | "tailwind.config.cjs" | "tailwind.config.mjs" | "tailwind.config.ts"
    );
    let mut v4 = false;
    if let Some(document) = document {
        for node in document.nodes() {
            v3 |= node.kind == "at_keyword" && source_range(text, node.range) == Some("@tailwind");
            v4 |= node.kind == "import_statement"
                && source_range(text, node.range).is_some_and(|import| {
                    import.split(['"', '\'']).nth(1).is_some_and(|target| {
                        target == "tailwindcss" || target.starts_with("tailwindcss/")
                    })
                });
        }
    }
    (v3, v4)
}

fn relative_source(source: &ProjectPath, authored: &str) -> Option<ProjectPath> {
    if authored.is_empty()
        || authored.contains('\\')
        || authored.contains(['?', '#'])
        || authored.chars().any(char::is_control)
    {
        return None;
    }
    let mut base = url::Url::parse("https://rift.invalid/root/").ok()?;
    base.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(source.as_str().split('/'));
    let target = base.join(authored).ok()?;
    if target.origin() != base.origin() {
        return None;
    }
    let path = target.path().strip_prefix("/root/")?;
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    ProjectPath::new(decoded.as_ref()).ok()
}

fn source_range(text: &str, range: ByteRange) -> Option<&str> {
    text.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
