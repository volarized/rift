//! Evaluates captured Cargo default features without assuming a target platform.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::macros::{Condition, configured, tokens};
use crate::NamespaceInput;

pub(crate) struct Configurations<'input> {
    manifests: BTreeMap<&'input str, Option<Features>>,
    depth: usize,
}

pub(crate) struct Configuration<'input> {
    features: Option<&'input Features>,
    depth: usize,
}

#[derive(Default, Deserialize)]
struct Manifest {
    package: Option<toml::Table>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
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
            let features = metadata.map(|metadata| Features::new(metadata.features));
            manifests
                .entry(parent)
                .and_modify(|features| *features = None)
                .or_insert(features);
        }
        Self {
            manifests,
            depth: input.syntax().syntax_depth_max(),
        }
    }

    pub(crate) fn file(&self, path: &str) -> Configuration<'_> {
        let mut directory = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        // Each lookup removes one path segment, under the source path bound.
        loop {
            if let Some(features) = self.manifests.get(directory) {
                return Configuration {
                    features: features.as_ref(),
                    depth: self.depth,
                };
            }
            if directory.is_empty() {
                return Configuration {
                    features: None,
                    depth: self.depth,
                };
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

impl Configuration<'_> {
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

    fn predicate(&self, input: &[&str], depth: usize) -> Option<bool> {
        // Static collection uses the normal Cargo build, with test code disabled.
        // Platform and custom flags remain unknown; only declared defaults are known.
        match configured(input, true, depth, &|literal| {
            self.features.as_ref()?.contains(literal)
        })? {
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

fn split<'tokens, 'source>(input: &'tokens [&'source str]) -> Option<Vec<&'tokens [&'source str]>> {
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
    fn test_rust_configuration_combines_known_defaults_and_unknown_flags() {
        let declared = BTreeMap::from([
            ("default".to_owned(), vec!["std".to_owned()]),
            ("std".to_owned(), vec![]),
            ("off".to_owned(), vec![]),
        ]);
        let features = Features::new(declared);
        let configuration = Configuration {
            features: Some(&features),
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
        ] {
            assert_eq!(configuration.attribute(attribute), expected, "{attribute}");
        }
    }
}
