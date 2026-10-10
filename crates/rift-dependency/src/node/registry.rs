//! Registry endpoints established by bounded project configuration.

use std::collections::BTreeMap;
use std::path::Path;

use rift_protocol::identity::canonical_registry_endpoint;
use rift_protocol::read::ProjectPath;
use serde::Deserialize;

use crate::context::ContextAnswer;
use crate::manifest::{file_beside, read_static_file};
use crate::resolver::StaticInputs;

/// One registry degradation per manifest; every dependency remains in the context.
#[derive(Default)]
pub(crate) struct RegistryDegradations {
    unresolved: BTreeMap<String, (usize, String)>,
}

impl RegistryDegradations {
    pub(crate) fn unresolved(&mut self, manifest: &ProjectPath, name: &str) {
        let (count, _) = self
            .unresolved
            .entry(manifest.0.clone())
            .or_insert_with(|| (0, name.to_owned()));
        // Each observation comes from a bounded parsed manifest or lockfile.
        *count += 1;
    }

    pub(crate) fn report(self, answer: &mut ContextAnswer) {
        for (manifest, (count, name)) in self.unresolved {
            let packages = if count == 1 {
                name
            } else {
                format!("{count} packages; first package {name}")
            };
            answer.degradations.push(format!(
                "{manifest}: registry unresolved for {packages}; no package owner reported; \
                 user configuration, environment variables, and command-line flags were not read"
            ));
        }
    }
}

const NPMRC: &str = ".npmrc";
const BUNFIG: &str = "bunfig.toml";
const PUBLIC_REGISTRY: &str = "npmjs.org";

/// Observed project settings only. User configuration, environment variables, and
/// command-line flags are not inputs to static resolution.
#[derive(Default)]
pub(crate) struct RegistryConfig {
    default: Option<String>,
    scopes: BTreeMap<String, Option<String>>,
    failed: bool,
}

impl RegistryConfig {
    pub(crate) fn read(
        directory: &Path,
        manifest: &ProjectPath,
        bun: bool,
        inputs: &mut dyn StaticInputs,
        answer: &mut ContextAnswer,
    ) -> Self {
        let mut config = Self::default();
        for file in [Some(NPMRC), bun.then_some(BUNFIG)].into_iter().flatten() {
            let bytes = match read_static_file(directory, file, inputs) {
                Ok(bytes) => bytes,
                Err(error) if error.is_absent() => continue,
                Err(_) => {
                    config.failed = true;
                    answer.inputs.push(file_beside(manifest, file));
                    answer.degradations.push(format!(
                        "{}: {file} could not be read within its bound; registry unresolved",
                        manifest.0
                    ));
                    continue;
                }
            };
            answer.inputs.push(file_beside(manifest, file));
            let accepted = if file == NPMRC {
                config.read_npmrc(&bytes)
            } else {
                config.read_bunfig(&bytes)
            };
            if !accepted {
                config.failed = true;
                answer.degradations.push(format!(
                    "{}: {file} registry configuration could not be parsed; registry unresolved",
                    manifest.0
                ));
            }
        }
        config
    }

    pub(crate) fn registry(&self, name: &str) -> Option<&str> {
        if self.failed {
            return None;
        }
        if let Some((scope, _)) = name.strip_prefix('@').and_then(|name| name.split_once('/'))
            && let Some(endpoint) = self.scopes.get(scope)
        {
            return endpoint.as_deref();
        }
        self.default.as_deref()
    }

    fn read_npmrc(&mut self, bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        let mut section = false;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with([';', '#']) {
                continue;
            }
            if line.starts_with('[') {
                section = true;
                continue;
            }
            if section {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                if line == "registry" || line.ends_with(":registry") {
                    return false;
                }
                continue;
            };
            let key = key.trim();
            if key == "registry" {
                self.default = ini_value(value).and_then(|value| npm_registry(&value));
            } else if let Some(scope) = key
                .strip_prefix('@')
                .and_then(|key| key.strip_suffix(":registry"))
            {
                self.scopes.insert(
                    scope.to_owned(),
                    ini_value(value).and_then(|value| npm_registry(&value)),
                );
            } else if key == "registry[]" || key.ends_with(":registry[]") {
                return false;
            }
        }
        true
    }

    fn read_bunfig(&mut self, bytes: &[u8]) -> bool {
        let Ok(config) = toml::from_slice::<BunConfig>(bytes) else {
            return false;
        };
        if let Some(install) = config.install {
            if let Some(registry) = install.registry {
                self.default = registry.endpoint();
            }
            for (scope, registry) in install.scopes {
                self.scopes.insert(
                    scope.trim_start_matches('@').to_owned(),
                    registry.endpoint(),
                );
            }
        }
        true
    }
}

/// Accepted registry URL, with the explicitly recognized official alias normalized.
pub(crate) fn npm_registry(value: &str) -> Option<String> {
    if value.contains('$') {
        return None;
    }
    let endpoint = canonical_registry_endpoint(value).ok()?;
    Some(
        if endpoint == "registry.npmjs.org" || endpoint == PUBLIC_REGISTRY {
            PUBLIC_REGISTRY.to_owned()
        } else {
            endpoint
        },
    )
}

// npm/ini trims values, decodes quoted JSON, and stops unquoted values at an
// unescaped comment. Unsupported escapes remain invalid endpoint input.
fn ini_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value).ok();
    }
    if value.starts_with('\'') && value.ends_with('\'') {
        return Some(value[1..value.len() - 1].to_owned());
    }
    let mut result = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            if !matches!(character, '\\' | ';' | '#') {
                result.push('\\');
            }
            result.push(character);
            escaped = false;
        } else if matches!(character, ';' | '#') {
            break;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    Some(result.trim().to_owned())
}

#[derive(Deserialize)]
struct BunConfig {
    install: Option<BunInstall>,
}

#[derive(Deserialize)]
struct BunInstall {
    registry: Option<BunRegistry>,
    #[serde(default)]
    scopes: BTreeMap<String, BunRegistry>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BunRegistry {
    Url(String),
    Table { url: String },
}

impl BunRegistry {
    fn endpoint(self) -> Option<String> {
        let url = match self {
            Self::Url(url) | Self::Table { url } => url,
        };
        npm_registry(&url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_scope_and_bun_override_preserve_registry_path() {
        let mut config = RegistryConfig::default();
        assert_eq!(config.registry("demo"), None);
        assert!(config.read_npmrc(b"registry=https://registry.npmjs.org/\n@org:registry='https://registry.example/releases'\n//registry.example/:_authToken=secret\n"));
        assert_eq!(config.registry("demo"), Some("npmjs.org"));
        assert_eq!(
            config.registry("@org/demo"),
            Some("registry.example/releases")
        );
        assert!(config.read_bunfig(b"[install]\nregistry={url='https://bun.example/npm',token='secret'}\n[install.scopes]\norg='https://bun.example/scoped'\n"));
        assert_eq!(config.registry("demo"), Some("bun.example/npm"));
        assert_eq!(config.registry("@org/demo"), Some("bun.example/scoped"));
    }

    #[test]
    fn invalid_scope_refuses_default_and_credentials() {
        let mut config = RegistryConfig::default();
        assert!(config.read_npmrc(
            b"registry=https://registry.npmjs.org/ # default\n@org:registry=${REGISTRY}\n"
        ));
        assert_eq!(config.registry("@org/demo"), None);
        for value in [
            "https://user:secret@registry.example/",
            "https://registry.example/?token=secret",
            "https://registry.example/#secret",
            "http://registry.example/",
            "${REGISTRY}",
        ] {
            assert_eq!(npm_registry(value), None);
        }
        assert!(!config.read_npmrc(b"registry[]=https://registry.example/"));
        assert!(!config.read_bunfig(b"[install]\nregistry ="));
    }
}
