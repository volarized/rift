use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::{PackagePublication, PackageSourceUnit, PackageSymbol};
use rift_protocol::read::SymbolId;
use serde::Deserialize;
use std::collections::BTreeMap;

use super::{NamespaceInput, PackageSource};

/// An admitted dependency publication and its original selected sources.
///
/// The caller supplies a publication accepted from the selected immutable release.
/// Namespace input validates its manifest selector. Requested declarations retain
/// their defining owner; foreign objects are never copied into the source publication.
#[derive(Debug, Clone, Copy)]
pub struct PackageDependency<'input> {
    name: &'input str,
    manifest: PackageSource<'input>,
    publication: &'input PackagePublication,
    sources: &'input [PackageSource<'input>],
}

impl<'input> PackageDependency<'input> {
    /// Borrows one dependency witness; `with_dependencies` validates its selector.
    #[must_use]
    pub const fn new(
        name: &'input str,
        manifest: PackageSource<'input>,
        publication: &'input PackagePublication,
        sources: &'input [PackageSource<'input>],
    ) -> Self {
        Self {
            name,
            manifest,
            publication,
            sources,
        }
    }

    /// Authored dependency name from the source package's Cargo manifest.
    #[must_use]
    pub const fn name(self) -> &'input str {
        self.name
    }

    /// Original selected dependency source bytes.
    #[must_use]
    pub const fn sources(self) -> &'input [PackageSource<'input>] {
        self.sources
    }

    /// Admitted defining publication, borrowed without retaining its graph.
    #[must_use]
    pub const fn publication(self) -> &'input PackagePublication {
        self.publication
    }

    /// Selects supported exact Cargo dependencies from the original package manifest.
    ///
    /// The result identifies requested releases, not admitted declarations. Non-exact,
    /// alternate-registry, path, Git, and workspace selectors remain unresolved.
    ///
    /// # Errors
    /// Returns the existing input refusal for changed owner metadata or exceeded bounds.
    pub fn selectors(
        owner: &SymbolOwner,
        manifest: PackageSource<'_>,
        limits: super::ExactPackageLimits,
    ) -> Result<Vec<(String, SymbolOwner)>, RiftError> {
        if manifest.path().as_str().rsplit('/').next() != Some("Cargo.toml")
            || manifest.text().len() > limits.syntax().source_bytes_max()
            || u64::try_from(manifest.text().len()).map_err(|_| invalid())? > limits.bytes_max()
        {
            return Err(invalid());
        }
        let metadata: CargoDependencies = toml::from_str(manifest.text()).map_err(|_| invalid())?;
        let SymbolOwner::Package {
            manager,
            name,
            version,
            ..
        } = owner
        else {
            return Err(invalid());
        };
        if manager != "cargo"
            || metadata.package.name != *name
            || metadata.package.version != *version
            || metadata.dependencies.len()
                > usize::try_from(limits.files_max()).map_err(|_| invalid())?
        {
            return Err(invalid());
        }
        Ok(metadata
            .dependencies
            .into_iter()
            .filter_map(|(name, selector)| selector.owner(&name).map(|owner| (name, owner)))
            .collect())
    }

    pub(super) fn validate(self, input: &NamespaceInput<'_>) -> Result<(), RiftError> {
        let admitted = input
            .files()
            .iter()
            .chain(input.context_sources())
            .find(|source| source.path() == self.manifest.path())
            .ok_or_else(invalid)?;
        if self.manifest.path().as_str().rsplit('/').next() != Some("Cargo.toml")
            || self.manifest.text().len() > input.syntax().source_bytes_max()
            || self.manifest.text() != admitted.text()
            || self.manifest.source_unit() != admitted.source_unit()
            || self.manifest.origin() != admitted.origin()
            || input.language().name != "rust"
            || input.language().dialect.is_some()
            || self.publication.format_revision
                != rift_protocol::index::PACKAGE_PUBLICATION_FORMAT_REVISION
            || self.publication.identity_format
                != rift_protocol::identity::SYMBOL_IDENTITY_FORMAT_REVISION
            || self.publication.analyzer_revision != crate::analyzer_revision()
        {
            return Err(invalid());
        }
        let metadata: CargoDependencies =
            toml::from_str(self.manifest.text()).map_err(|_| invalid())?;
        let SymbolOwner::Package {
            manager,
            name,
            version,
            ..
        } = input.owner()
        else {
            return Err(invalid());
        };
        if manager != "cargo"
            || metadata.package.name != *name
            || metadata.package.version != *version
        {
            return Err(invalid());
        }
        let selector = metadata.dependencies.get(self.name).ok_or_else(invalid)?;
        if !selector.matches(self.name, &self.publication.owner) {
            return Err(invalid());
        }
        Ok(())
    }

    pub(crate) fn target(
        self,
        path: &[String],
        language: &rift_protocol::read::Language,
        work: &mut usize,
        work_max: usize,
    ) -> Option<(
        rift_core::SymbolId,
        &'input PackageSymbol,
        &'input PackageSourceUnit,
    )> {
        if path.first()?.as_str() != self.name {
            return None;
        }
        let mut found = None;
        for object in &self.publication.objects {
            charge(work, 1, work_max)?;
            let Some(identity) = object.id.as_ref() else {
                continue;
            };
            charge(work, identity.as_str().len(), work_max)?;
            let parsed = SymbolIdentity::parse(identity.as_str()).ok()?;
            if parsed.owner() == &self.publication.owner
                && parsed.language() == language
                && parsed.qualified_path().get(1..) == path.get(1..)
                && found.replace(identity).is_some()
            {
                return None;
            }
        }
        let wire = found?;
        let (declaration, unit) = self.declaration(wire, work, work_max)?;
        Some((
            rift_core::SymbolId::new(wire.as_str()).ok()?,
            declaration,
            unit,
        ))
    }

    /// Checks one requested target against admitted identity and original source.
    ///
    /// Every visited row and compared source byte spends the caller's work bound.
    /// Missing, duplicate, private, truncated, or changed witnesses return `None`.
    pub(crate) fn declaration(
        self,
        identity: &SymbolId,
        work: &mut usize,
        work_max: usize,
    ) -> Option<(&'input PackageSymbol, &'input PackageSourceUnit)> {
        let parsed = SymbolIdentity::parse(identity.as_str()).ok()?;
        if parsed.owner() != &self.publication.owner
            || parsed.language().name != "rust"
            || parsed.language().dialect.is_some()
        {
            return None;
        }
        let object = unique(self.publication.objects.iter(), work, work_max, |object| {
            object.id.as_ref() == Some(identity)
        })?;
        if object.language != *parsed.language()
            || parsed.qualified_path().last() != Some(&object.name)
        {
            return None;
        }
        let declaration = unique(
            self.publication.declarations.iter(),
            work,
            work_max,
            |row| &row.symbol == identity,
        )?;
        if !declaration.public || !declaration.source_complete {
            return None;
        }
        let unit = unique(self.publication.units.iter(), work, work_max, |unit| {
            unit.unit == declaration.unit
        })?;
        if !unit.source_complete || unit.language != *parsed.language() {
            return None;
        }
        let source = unique(self.sources.iter(), work, work_max, |source| {
            source.path().as_str() == unit.path.0.as_str()
        })?;
        let physical = source.source_unit()?;
        if physical.source_owner() != Some(&self.publication.owner)
            || physical.to_string() != unit.unit.as_str()
        {
            return None;
        }
        charge(work, unit.source.len(), work_max)?;
        if unit.source != source.text() {
            return None;
        }
        let start = usize::try_from(declaration.range.start).ok()?;
        let end = usize::try_from(declaration.range.end).ok()?;
        let original = source.text().get(start..end)?;
        charge(work, original.len(), work_max)?;
        if original != declaration.source {
            return None;
        }
        Some((declaration, unit))
    }
}

fn invalid() -> RiftError {
    errors::analysis::package_input_identity_invalid().error()
}

fn charge(work: &mut usize, amount: usize, maximum: usize) -> Option<()> {
    *work = work.checked_add(amount)?;
    (*work <= maximum).then_some(())
}

fn unique<'input, T>(
    rows: impl Iterator<Item = &'input T>,
    work: &mut usize,
    maximum: usize,
    matches: impl Fn(&T) -> bool,
) -> Option<&'input T> {
    let mut selected = None;
    for row in rows {
        charge(work, 1, maximum)?;
        if matches(row) && selected.replace(row).is_some() {
            return None;
        }
    }
    selected
}

#[derive(Deserialize)]
struct CargoDependencies {
    package: CargoPackage,
    #[serde(default)]
    dependencies: BTreeMap<String, CargoDependency>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CargoDependency {
    Version(String),
    Selected(CargoSelection),
}

#[derive(Deserialize)]
struct CargoSelection {
    version: Option<String>,
    package: Option<String>,
    registry: Option<String>,
    path: Option<String>,
    git: Option<String>,
    workspace: Option<bool>,
}

impl CargoDependency {
    fn owner(&self, binding: &str) -> Option<SymbolOwner> {
        let (name, version) = match self {
            Self::Version(version) => (binding, version.as_str()),
            Self::Selected(selected) => {
                if selected.registry.is_some()
                    || selected.path.is_some()
                    || selected.git.is_some()
                    || selected.workspace.is_some()
                {
                    return None;
                }
                (
                    selected.package.as_deref().unwrap_or(binding),
                    selected.version.as_deref()?,
                )
            }
        };
        let version = version.strip_prefix('=')?;
        let owner = rift_protocol::read::PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
        .owner()
        .ok()?;
        self.matches(binding, &owner).then_some(owner)
    }

    fn matches(&self, binding: &str, owner: &SymbolOwner) -> bool {
        let SymbolOwner::Package {
            manager,
            registry,
            name,
            version,
        } = owner
        else {
            return false;
        };
        if manager != "cargo" || registry != "crates.io" {
            return false;
        }
        let expected = format!("={version}");
        match self {
            Self::Version(selected) => binding == name && selected == &expected,
            Self::Selected(selected) => {
                selected.package.as_deref().unwrap_or(binding) == name
                    && selected.version.as_deref() == Some(expected.as_str())
                    && selected.registry.is_none()
                    && selected.path.is_none()
                    && selected.git.is_none()
                    && selected.workspace.is_none()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer};
    use rift_core::{ContributionOrigin, SourceKind, SourceLocation, SourceUnitId};
    use rift_protocol::read::{Language, PackageIdentity};

    const TARGET: &str = "pub trait Serialize {}";

    fn fixture(check: impl FnOnce(PackageDependency<'_>, NamespaceInput<'_>)) {
        let package = PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "serde_core".to_owned(),
            version: "1.0.228".to_owned(),
        };
        let owner = package.owner().expect("target owner");
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("target origin");
        let language = Language::from_identity_segment("rust").expect("Rust");
        let root = rift_core::ProjectPath::new("src/lib.rs").expect("root");
        let manifest_path = rift_core::ProjectPath::new("Cargo.toml").expect("manifest");
        let unit = SourceUnitId::for_owner(owner.clone(), root.as_str()).expect("unit");
        let sources = [PackageSource::new(&root, TARGET)
            .with_source_unit(&unit, &origin)
            .expect("source")];
        let metadata = [PackageSource::new(
            &manifest_path,
            "[package]\nname='serde_core'\nversion='1.0.228'\n[lib]\npath='src/lib.rs'\n",
        )];
        let limits = ExactPackageLimits::new(4, 1_048_576);
        let target = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
            .expect("target input")
            .with_framework_context(&metadata, &[])
            .expect("metadata")
            .with_library(&owner, metadata[0], sources[0])
            .expect("library");
        let analysis = PackageAnalyzer::analyze(target, 1).expect("target analysis");
        let source_package = PackageIdentity {
            name: "serde".to_owned(),
            ..package
        };
        let source_owner = source_package.owner().expect("source owner");
        let source_origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: source_package,
            }),
            SourceKind::Authored,
        )
        .expect("source origin");
        let source_files = [PackageSource::new(&root, "pub use serde_core::Serialize;")];
        let source_metadata = [PackageSource::new(
            &manifest_path,
            "[package]\nname='serde'\nversion='1.0.228'\n[dependencies.serde_core]\nversion='=1.0.228'\n",
        )];
        let source = ExactPackageInput::new(
            &source_owner,
            &language,
            &source_origin,
            &source_files,
            limits,
        )
        .expect("source input")
        .with_framework_context(&source_metadata, &[])
        .expect("source metadata");
        check(
            PackageDependency::new(
                "serde_core",
                source_metadata[0],
                analysis.publication(),
                &sources,
            ),
            source.namespace_input(),
        );
    }

    fn identity(dependency: PackageDependency<'_>) -> &SymbolId {
        &dependency
            .publication
            .declarations
            .iter()
            .find(|row| row.symbol.as_str().ends_with("/Serialize"))
            .expect("observed trait")
            .symbol
    }

    #[test]
    fn dependency_witness_keeps_defining_owner_and_original_declaration() {
        fixture(|dependency, input| {
            dependency.validate(&input).expect("exact selector");
            let mut work = 0;
            let (declaration, unit) = dependency
                .declaration(identity(dependency), &mut work, 1_048_576)
                .expect("admitted target");
            assert!(std::ptr::eq(
                declaration,
                &raw const dependency.publication.declarations[0]
            ));
            assert_eq!(declaration.source, TARGET);
            assert_eq!(unit.source, TARGET);
            assert_eq!(
                SymbolIdentity::parse(declaration.symbol.as_str())
                    .expect("identity")
                    .owner(),
                &dependency.publication.owner
            );
            assert!(work > 0);
        });
    }

    #[test]
    fn dependency_witness_refuses_wrong_selector_owner_and_manifest() {
        fixture(|dependency, input| {
            for version in ["1.0.227", "1.0.229"] {
                let mut publication = dependency.publication.clone();
                if let SymbolOwner::Package {
                    version: selected, ..
                } = &mut publication.owner
                {
                    *selected = version.to_owned();
                }
                assert!(
                    PackageDependency {
                        publication: &publication,
                        ..dependency
                    }
                    .validate(&input)
                    .is_err()
                );
            }
            assert!(
                PackageDependency {
                    name: "other",
                    ..dependency
                }
                .validate(&input)
                .is_err()
            );
            let path = rift_core::ProjectPath::new("other/Cargo.toml").expect("other manifest");
            let manifest = PackageSource::new(&path, dependency.manifest.text());
            assert!(
                PackageDependency {
                    manifest,
                    ..dependency
                }
                .validate(&input)
                .is_err()
            );
        });
    }

    #[test]
    fn dependency_witness_refuses_missing_and_ambiguous_targets() {
        fixture(|dependency, _| {
            let target = identity(dependency).clone();
            for duplicate in [false, true] {
                let mut publication = dependency.publication.clone();
                if duplicate {
                    publication
                        .declarations
                        .push(publication.declarations[0].clone());
                } else {
                    publication.declarations.clear();
                }
                let changed = PackageDependency {
                    publication: &publication,
                    ..dependency
                };
                assert!(changed.declaration(&target, &mut 0, 1_048_576).is_none());
            }
            let missing = PackageDependency {
                sources: &[],
                ..dependency
            };
            assert!(missing.declaration(&target, &mut 0, 1_048_576).is_none());
        });
    }

    #[test]
    fn dependency_witness_refuses_changed_range_bytes_and_truncated_source() {
        fixture(|dependency, _| {
            let target = identity(dependency).clone();
            for change in 0..4 {
                let mut publication = dependency.publication.clone();
                match change {
                    0 => publication.declarations[0].range.end += 1,
                    1 => publication.declarations[0].source.push(' '),
                    2 => publication.units[0].source_complete = false,
                    _ => publication.units[0].source.push(' '),
                }
                let changed = PackageDependency {
                    publication: &publication,
                    ..dependency
                };
                assert!(changed.declaration(&target, &mut 0, 1_048_576).is_none());
            }
        });
    }

    #[test]
    fn dependency_witness_refuses_foreign_units_private_and_duplicate_objects() {
        fixture(|dependency, _| {
            let target = identity(dependency).clone();
            for change in 0..3 {
                let mut publication = dependency.publication.clone();
                match change {
                    0 => publication.declarations[0].public = false,
                    1 => publication.objects.push(publication.objects[0].clone()),
                    _ => publication.units.push(publication.units[0].clone()),
                }
                let changed = PackageDependency {
                    publication: &publication,
                    ..dependency
                };
                assert!(changed.declaration(&target, &mut 0, 1_048_576).is_none());
            }
            let foreign = SourceUnitId::for_owner(
                SymbolOwner::Package {
                    manager: "cargo".to_owned(),
                    registry: "crates.io".to_owned(),
                    name: "other".to_owned(),
                    version: "1.0.228".to_owned(),
                },
                "src/lib.rs",
            )
            .expect("foreign unit");
            let package = rift_protocol::read::PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "other".to_owned(),
                version: "1.0.228".to_owned(),
            };
            let origin = ContributionOrigin::new(
                Some(SourceLocation::Dependency { package }),
                SourceKind::Authored,
            )
            .expect("foreign origin");
            let sources = [PackageSource::new(dependency.sources[0].path(), TARGET)
                .with_source_unit(&foreign, &origin)
                .expect("foreign source")];
            let changed = PackageDependency {
                sources: &sources,
                ..dependency
            };
            assert!(changed.declaration(&target, &mut 0, 1_048_576).is_none());
        });
    }

    #[test]
    fn dependency_witness_charges_requested_lookup_before_returning_target() {
        fixture(|dependency, _| {
            let target = identity(dependency);
            let mut work = 0;
            assert!(
                dependency
                    .declaration(target, &mut work, 1_048_576)
                    .is_some()
            );
            let required = work;
            assert!(dependency.declaration(target, &mut 0, required).is_some());
            assert!(
                dependency
                    .declaration(target, &mut 0, required - 1)
                    .is_none()
            );
            assert!(
                dependency
                    .declaration(target, &mut work, required)
                    .is_none()
            );
        });
    }
    #[test]
    fn dependency_selectors_keep_exact_renamed_owners_and_skip_unknown_versions() {
        fixture(|dependency, input| {
            let text = "[package]\nname='serde'\nversion='1.0.228'\n[dependencies]\nrenamed={package='serde_core',version='=1.0.228'}\nunknown='1.0'\nlocal={path='../local'}\nworkspace={workspace=true}\nforeign={version='=1.0.228',registry='other'}\n";
            let manifest = PackageSource::new(dependency.manifest.path(), text);
            let selected = PackageDependency::selectors(
                input.owner(),
                manifest,
                ExactPackageLimits::new(8, 1_048_576),
            )
            .expect("bounded original selectors");
            assert_eq!(
                selected,
                vec![("renamed".to_owned(), dependency.publication.owner.clone())]
            );
        });
    }

    #[test]
    fn dependency_selectors_refuse_changed_owner_and_count_or_byte_bounds() {
        fixture(|dependency, input| {
            let text = "[package]\nname='serde'\nversion='1.0.228'\n[dependencies]\nserde_core='=1.0.228'\nsecond='=1.0.228'\n";
            let manifest = PackageSource::new(dependency.manifest.path(), text);
            assert!(
                PackageDependency::selectors(
                    input.owner(),
                    manifest,
                    ExactPackageLimits::new(1, 1_048_576)
                )
                .is_err()
            );
            assert!(
                PackageDependency::selectors(
                    input.owner(),
                    manifest,
                    ExactPackageLimits::new(8, 1)
                )
                .is_err()
            );
            assert!(
                PackageDependency::selectors(
                    &dependency.publication.owner,
                    manifest,
                    ExactPackageLimits::new(8, 1_048_576)
                )
                .is_err()
            );
        });
    }

    #[test]
    fn dependency_witness_refuses_other_analyzer_revision() {
        fixture(|dependency, input| {
            let mut publication = dependency.publication.clone();
            publication.analyzer_revision = rift_protocol::read::Digest("other".to_owned());
            let changed = PackageDependency::new(
                dependency.name,
                dependency.manifest,
                &publication,
                dependency.sources,
            );
            assert!(changed.validate(&input).is_err());
        });
    }
}
