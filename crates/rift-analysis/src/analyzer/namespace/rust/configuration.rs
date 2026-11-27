//! Evaluates Cargo default features and target predicates for conditional Rust items.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ProjectPath;
use rift_syntax::ByteRange;
use serde::Deserialize;

use super::macros::{Condition, configured_with_flags, tokens};
use crate::{NamespaceInput, RustTarget};

pub(crate) fn valid_flags(flags: &[&str]) -> bool {
    flags.iter().all(|flag| {
        let Some(tokens) = tokens(flag) else {
            return false;
        };
        if tokens.concat() != *flag {
            return false;
        }
        match tokens.as_slice() {
            [name] => super::crate_name_is_valid(name),
            [name, "=", value] => {
                super::crate_name_is_valid(name)
                    && value.starts_with('"')
                    && value.ends_with('"')
                    && !value.contains('\\')
            }
            _ => false,
        }
    })
}

pub(crate) struct Configurations<'input> {
    manifests: BTreeMap<&'input str, Option<ManifestConfiguration>>,
    captured: Option<(&'input str, BTreeSet<&'input str>)>,
    target: Option<RustTarget>,
    depth: usize,
}

pub(crate) struct Configuration<'input> {
    features: Option<&'input Features>,
    registry_cfg_if: bool,
    flags: Option<&'input BTreeSet<&'input str>>,
    target: Option<RustTarget>,
    depth: usize,
}

#[derive(Default, Deserialize)]
struct Manifest {
    package: Option<toml::Table>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    dependencies: toml::Table,
    #[serde(default)]
    target: toml::Table,
    patch: Option<toml::Table>,
    replace: Option<toml::Table>,
}

struct ManifestConfiguration {
    features: Features,
    registry_cfg_if: bool,
}

impl Manifest {
    /// The direct, unmodified crates.io cfg-if 1.x dependency under the extern prelude.
    fn registry_cfg_if(&self) -> bool {
        if !matches!(
            self.package
                .as_ref()
                .and_then(|package| package.get("edition"))
                .and_then(toml::Value::as_str),
            Some("2018" | "2021" | "2024")
        ) || self.patch.is_some()
            || self.replace.is_some()
            || self.target.values().any(|target| {
                target
                    .get("dependencies")
                    .and_then(|dependencies| dependencies.get("cfg-if"))
                    .is_some()
            })
        {
            return false;
        }
        let version = match self.dependencies.get("cfg-if") {
            Some(toml::Value::String(version)) => version,
            Some(toml::Value::Table(dependency)) if dependency.len() == 1 => {
                let Some(version) = dependency.get("version").and_then(toml::Value::as_str) else {
                    return false;
                };
                version
            }
            _ => return false,
        };
        semver::VersionReq::parse(version).is_ok_and(|requirement| {
            matches!(requirement.comparators.as_slice(), [comparator]
            if comparator.major == 1 && comparator.pre.is_empty()
                && matches!(
                    comparator.op,
                    semver::Op::Exact | semver::Op::Caret | semver::Op::Tilde | semver::Op::Wildcard
                ))
        })
    }

    /// Declared features plus the implicit feature of each optional dependency that no
    /// feature names with `dep:`, as Cargo defines them.
    fn declared_features(self) -> BTreeMap<String, Vec<String>> {
        let mut features = self.features;
        let targets = self
            .target
            .values()
            .filter_map(|target| target.get("dependencies")?.as_table());
        let optional = std::iter::once(&self.dependencies)
            .chain(targets)
            .flat_map(|dependencies| dependencies.iter())
            .filter(|(_, dependency)| {
                dependency
                    .get("optional")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        for name in optional {
            let explicit = format!("dep:{name}");
            if features.values().flatten().any(|value| *value == explicit) {
                continue;
            }
            features.entry(name).or_insert_with(|| vec![explicit]);
        }
        features
    }
}

struct Features {
    declared: BTreeMap<String, Vec<String>>,
    enabled: BTreeSet<String>,
}

impl Features {
    fn new(declared: BTreeMap<String, Vec<String>>) -> Self {
        let mut enabled = BTreeSet::new();
        let mut pending = vec!["default".to_owned()];
        // Each declared feature is visited once; manifest bytes bound the graph.
        while let Some(name) = pending.pop() {
            let Some(dependencies) = declared.get(&name) else {
                continue;
            };
            if enabled.insert(name) {
                pending.extend(
                    dependencies
                        .iter()
                        .filter(|name| declared.contains_key(*name))
                        .cloned(),
                );
            }
        }
        Self { declared, enabled }
    }

    fn contains(&self, literal: &str) -> Option<bool> {
        let name = literal.strip_prefix('"')?.strip_suffix('"')?;
        if name.contains('\\') || !self.declared.contains_key(name) {
            return None;
        }
        Some(self.enabled.contains(name))
    }
}

impl<'input> Configurations<'input> {
    pub(crate) fn new(input: &NamespaceInput<'input>) -> Self {
        let mut manifests = BTreeMap::new();
        for file in input.files().iter().chain(input.context_sources()) {
            let manifest = file.path().as_str();
            let parent = if manifest == "Cargo.toml" {
                ""
            } else if let Some(parent) = manifest.strip_suffix("/Cargo.toml") {
                parent
            } else {
                continue;
            };
            let metadata = toml::from_str::<Manifest>(file.text()).ok();
            if metadata
                .as_ref()
                .is_some_and(|metadata| metadata.package.is_none())
            {
                continue;
            }
            let configuration = metadata.map(|metadata| ManifestConfiguration {
                registry_cfg_if: matches!(
                    input.owner(),
                    rift_protocol::identity::SymbolOwner::Package { manager, registry, .. }
                        if manager == "cargo" && registry == "crates.io"
                ) && metadata.registry_cfg_if(),
                features: Features::new(metadata.declared_features()),
            });
            manifests
                .entry(parent)
                .and_modify(|configuration| *configuration = None)
                .or_insert(configuration);
        }
        let captured = input.rust_configuration().map(|(manifest, flags)| {
            let parent = manifest
                .as_str()
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
            (parent, flags.iter().copied().collect())
        });
        Self {
            manifests,
            captured,
            target: input.rust_target(),
            depth: input.syntax().syntax_depth_max(),
        }
    }

    pub(crate) fn file(&self, path: &str) -> Configuration<'_> {
        let mut directory = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        // Each lookup removes one path segment, under the source path bound.
        loop {
            if let Some(configuration) = self.manifests.get(directory) {
                let configuration = configuration.as_ref();
                return Configuration {
                    features: configuration.map(|configuration| &configuration.features),
                    registry_cfg_if: configuration
                        .is_some_and(|configuration| configuration.registry_cfg_if),
                    flags: self
                        .captured
                        .as_ref()
                        .filter(|(parent, _)| *parent == directory)
                        .map(|(_, flags)| flags),
                    target: self.target,
                    depth: self.depth,
                };
            }
            if directory.is_empty() {
                return Configuration {
                    features: None,
                    registry_cfg_if: false,
                    flags: None,
                    target: self.target,
                    depth: self.depth,
                };
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

pub(super) enum SelectedModulePath {
    Default,
    Literal(ProjectPath),
}

impl Configuration<'_> {
    pub(super) const fn captured_registry_cfg_if(&self) -> bool {
        self.registry_cfg_if && self.flags.is_some()
    }

    pub(super) fn module_path(
        &self,
        source: &str,
        attributes: &[ByteRange],
    ) -> Option<SelectedModulePath> {
        let mut path = None;
        for &range in attributes {
            let source = super::macros::text_range(source, range)?;
            let tokens = tokens(source)?;
            let ["#", "[", attribute @ .., "]"] = tokens.as_slice() else {
                return None;
            };
            self.path_attribute(attribute, self.depth, &mut path)?;
        }
        Some(path.map_or(SelectedModulePath::Default, SelectedModulePath::Literal))
    }

    fn path_attribute(
        &self,
        input: &[&str],
        depth: usize,
        path: &mut Option<ProjectPath>,
    ) -> Option<()> {
        let depth = depth.checked_sub(1)?;
        match input {
            ["path", "=", literal] if path.is_none() => {
                let content = literal.strip_prefix('"')?.strip_suffix('"')?;
                if content.contains('\\')
                    || content.len() > rift_core::constants::PROJECT_PATH_BYTES_MAX
                {
                    return None;
                }
                *path = Some(ProjectPath::new(content).ok()?);
            }
            ["cfg_attr", "(", body @ .., ")"] => {
                let parts = split(body)?;
                let (predicate, nested) = parts.split_first()?;
                if nested.is_empty() {
                    return None;
                }
                if self.predicate(predicate, depth)? {
                    for attribute in nested {
                        self.path_attribute(attribute, depth, path)?;
                    }
                }
            }
            [
                "cfg" | "allow" | "expect" | "warn" | "deny" | "forbid" | "doc" | "deprecated",
                ..,
            ] => {}
            _ => return None,
        }
        Some(())
    }

    pub(crate) fn admitted(
        &self,
        facts: &rift_syntax::SyntaxFacts,
        source: &str,
        range: rift_syntax::ByteRange,
    ) -> Option<bool> {
        let Some(facts) = facts.static_facts() else {
            return Some(true);
        };
        let mut admitted = Some(true);
        for item in facts.conditional_items() {
            if item.range.start < range.end && range.start < item.range.end {
                let attribute = usize::try_from(item.attribute.start)
                    .ok()
                    .zip(usize::try_from(item.attribute.end).ok())
                    .and_then(|(start, end)| source.get(start..end));
                admitted = and(
                    admitted,
                    attribute.and_then(|attribute| self.attribute(attribute)),
                );
            }
        }
        admitted
    }

    pub(crate) fn attribute(&self, source: &str) -> Option<bool> {
        let tokens = tokens(source)?;
        let (["#", "[", value @ .., "]"] | ["#", "!", "[", value @ .., "]"]) = tokens.as_slice()
        else {
            return None;
        };
        self.value(value, self.depth)
    }

    /// The value of one `cfg` predicate under this configuration, unknown when unsure.
    pub(crate) fn condition(&self, predicate: &[&str]) -> Option<bool> {
        self.predicate(predicate, self.depth)
    }

    fn predicate(&self, input: &[&str], depth: usize) -> Option<bool> {
        // Static collection uses the normal Cargo build, with test code disabled.
        // Complete captured flags decide only the build they were supplied for.
        // Otherwise the selected target decides its own predicates and custom flags
        // remain unknown.
        match configured_with_flags(
            input,
            true,
            depth,
            &|literal| self.features.as_ref()?.contains(literal),
            &|atom| {
                if atom.iter().any(|token| token.contains('\\')) {
                    return None;
                }
                match self.flags {
                    Some(flags) => Some(flags.contains(atom.concat().as_str())),
                    None => self.target?.predicate(atom),
                }
            },
        )? {
            Condition::Known(value) => Some(value),
            Condition::Unknown => None,
        }
    }

    fn value(&self, input: &[&str], depth: usize) -> Option<bool> {
        if depth == 0 {
            return None;
        }
        match input {
            ["cfg", "(", predicate @ .., ")"] => self.predicate(predicate, depth - 1),
            ["cfg_attr", "(", body @ .., ")"] => {
                let parts = split(body)?;
                let (predicate, attributes) = parts.split_first()?;
                if attributes.is_empty() {
                    return None;
                }
                let condition = self.predicate(predicate, depth - 1);
                if condition == Some(false) {
                    return Some(true);
                }
                let mut presence = Some(true);
                for attribute in attributes {
                    presence = and(presence, self.value(attribute, depth - 1));
                }
                match (condition, presence) {
                    (_, Some(true)) => Some(true),
                    (Some(true), presence) => presence,
                    _ => None,
                }
            }
            _ => Some(true),
        }
    }
}

fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(super) fn split<'tokens, 'source>(
    input: &'tokens [&'source str],
) -> Option<Vec<&'tokens [&'source str]>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0_usize;
    for (index, token) in input.iter().enumerate() {
        match *token {
            "(" => depth = depth.checked_add(1)?,
            ")" => depth = depth.checked_sub(1)?,
            "," if depth == 0 => {
                if start == index {
                    return None;
                }
                parts.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    if start < input.len() {
        parts.push(&input[start..]);
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::{Configuration, Features};
    use std::collections::BTreeMap;

    #[test]
    fn test_rust_captured_configuration_stays_with_its_manifest() {
        let manifest = rift_core::ProjectPath::new("Cargo.toml").expect("manifest");
        let nested = rift_core::ProjectPath::new("nested/Cargo.toml").expect("nested manifest");
        let source = rift_core::ProjectPath::new("src/lib.rs").expect("source");
        let files = [crate::PackageSource::new(&source, "pub struct Item;")];
        let context = [
            crate::PackageSource::new(
                &manifest,
                "[package]\nname='fixture'\nversion='0.1.0'\n[features]\nstd=[]\n",
            ),
            crate::PackageSource::new(&nested, "[package]\nname='nested'\nversion='0.1.0'\n"),
        ];
        let language = rift_syntax::ShippedLanguage::Rust.language();
        let flags = ["custom", "feature=\"std\""];
        let input = crate::NamespaceInput::project(
            &rift_protocol::identity::SymbolOwner::Local,
            &language,
            &files,
            &context,
            &[],
            crate::ExactPackageLimits::new(10, 65_536),
        )
        .expect("input")
        .with_rust_configuration(&manifest, &flags)
        .expect("flags");
        let configurations = super::Configurations::new(&input);
        for (path, attribute, expected) in [
            ("src/lib.rs", "#[cfg(custom)]", Some(true)),
            ("src/lib.rs", "#[cfg(feature=\"std\")]", Some(true)),
            ("src/lib.rs", "#[cfg(absent)]", Some(false)),
            ("nested/src/lib.rs", "#[cfg(custom)]", None),
            ("nested/src/lib.rs", "#[cfg(feature=\"std\")]", None),
            ("src/lib.rs", "#[cfg(key=\"a\\nb\")]", None),
            ("src/lib.rs", "#[cfg(not(custom = invalid))]", None),
        ] {
            assert_eq!(
                configurations.file(path).attribute(attribute),
                expected,
                "{path}: {attribute}"
            );
        }
    }

    #[test]
    fn test_rust_configuration_combines_known_defaults_and_unknown_flags() {
        let declared = BTreeMap::from([
            ("default".to_owned(), vec!["std".to_owned()]),
            ("std".to_owned(), vec![]),
            ("off".to_owned(), vec![]),
        ]);
        let features = Features::new(declared);
        let configuration = Configuration {
            features: Some(&features),
            registry_cfg_if: false,
            flags: None,
            target: Some(crate::RustTarget::PACKAGE),
            depth: 32,
        };
        for (attribute, expected) in [
            ("#[cfg(feature=\"std\")]", Some(true)),
            ("#[cfg(feature=\"off\")]", Some(false)),
            ("#[cfg(unknown)]", None),
            ("#[cfg(all(unknown, feature=\"off\"))]", Some(false)),
            ("#[cfg(not(all(unknown, test)))]", Some(true)),
            ("#[cfg_attr(feature=\"std\", cfg(unknown))]", None),
            ("#[cfg_attr(feature=\"off\", cfg(unknown))]", Some(true)),
            ("#[cfg_attr(docsrs, doc(cfg(feature=\"std\")))]", Some(true)),
            ("#[cfg(any())]", Some(false)),
            ("#[cfg(all())]", Some(true)),
            ("#[cfg(unix)]", Some(true)),
            ("#[cfg(windows)]", Some(false)),
            ("#[cfg(any(windows, target_os = \"redox\"))]", Some(false)),
            (
                "#[cfg(all(feature=\"std\", target_family=\"unix\"))]",
                Some(true),
            ),
            ("#[cfg(not(target_os = \"emscripten\"))]", Some(true)),
            ("#[cfg(all(unix, unknown))]", None),
            ("#[cfg(any(unix, unknown))]", Some(true)),
            ("#[cfg(target_feature=\"sse2\")]", None),
            ("#[cfg(target_os = \"a\\nb\")]", None),
        ] {
            assert_eq!(configuration.attribute(attribute), expected, "{attribute}");
        }
    }
}
