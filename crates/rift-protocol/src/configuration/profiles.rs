//! Profiles declared in the repository's root configuration file.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ConfigurationViolation, ExecutionConfiguration, LanguageConfiguration, LspConfiguration,
    WorkspaceConfiguration,
};
use crate::dependencies::DependenciesConfiguration;
use crate::identity::valid_registration_name;
use crate::source::SourceConfiguration;

/// Profiles one configuration file may declare, at most.
pub const PROFILE_COUNT_MAX: usize = 64;
/// Folder components a profile root may hold, at most.
pub const PROFILE_ROOT_DEPTH_MAX: usize = 63;

/// One project folder served as `local:name` from the root `rift.toml`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = declare_profile_contract)]
pub struct ProfileConfiguration {
    /// Folder below the repository root, with at most 63 components and no `.` or `..` segments.
    #[schemars(length(min = 1, max = 262_144))]
    pub root: String,
    /// Source selection relative to this profile's folder. Root source selection is not inherited.
    #[serde(default)]
    pub source: SourceConfiguration,
    /// Dependency settings for this folder. Absence inherits the root settings.
    #[serde(default)]
    pub dependencies: Option<DependenciesConfiguration>,
    /// Execution bounds for this folder. Absence inherits the root settings.
    #[serde(default)]
    pub execution: Option<ExecutionConfiguration>,
    /// Language entries for this folder, replacing the root language entries.
    #[serde(default)]
    pub languages: BTreeMap<String, LanguageConfiguration>,
    /// Named LSP selections for this folder. An entry replaces the root selection of that name.
    #[serde(default)]
    pub lsp: BTreeMap<String, LspConfiguration>,
}

fn declare_profile_contract(schema: &mut schemars::Schema) {
    crate::schema::declare_workspace_contract(schema);
    if let Some(root) = schema
        .ensure_object()
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("root"))
        .and_then(serde_json::Value::as_object_mut)
    {
        root.insert("pattern".to_owned(), serde_json::json!(format!(r"^[^/\\:\u0000-\u001f\u007f-\u009f]+(?:/[^/\\:\u0000-\u001f\u007f-\u009f]+){{0,{}}}$", PROFILE_ROOT_DEPTH_MAX - 1)));
        root.insert(
            "not".to_owned(),
            serde_json::json!({"pattern": r"(^|/)(\.|\.\.|~)(/|$)"}),
        );
    }
}

impl WorkspaceConfiguration {
    /// Excludes profile folders from the primary project's source selection.
    #[must_use]
    pub fn with_profile_exclusions(self) -> Self {
        let roots: Vec<String> = self
            .profiles
            .values()
            .map(|profile| profile.root.clone())
            .collect();
        self.with_folder_exclusions(&roots)
    }

    /// Excludes folders another registration serves from the primary project's source selection.
    ///
    /// Each folder is relative to the primary root with `/` separators.
    #[must_use]
    pub fn with_folder_exclusions(mut self, folders: &[String]) -> Self {
        for folder in folders {
            let pattern = subtree_pattern(folder);
            if !self
                .source
                .exclude
                .iter()
                .any(|excluded| excluded.0 == pattern)
            {
                self.source.exclude.push(crate::read::PathPattern(pattern));
            }
        }
        self
    }

    /// Resolves a root-defined profile while retaining shared server and provider settings.
    #[must_use]
    pub fn for_profile(&self, name: &str) -> Option<Self> {
        let profile = self.profiles.get(name)?;
        let mut selected = self.clone();
        selected.profiles.clear();
        selected.mcp.extra.clear();
        selected.server.port = None;
        selected.server.port_range = None;
        selected.source = profile.source.clone();
        selected.languages.clone_from(&profile.languages);
        selected.lsp.extend(profile.lsp.clone());
        if let Some(dependencies) = &profile.dependencies {
            selected.dependencies.clone_from(dependencies);
        }
        if let Some(execution) = &profile.execution {
            selected.execution.clone_from(execution);
        }
        Some(selected)
    }
}

/// Quotes ignore metacharacters before selecting a profile folder's descendants.
#[must_use]
pub fn subtree_pattern(root: &str) -> String {
    use std::fmt::Write as _;
    let mut pattern = String::with_capacity(root.len());
    for character in root.chars() {
        match character {
            '!' | '#' if pattern.is_empty() => {
                let _ = write!(pattern, "{{{character},{character}}}");
            }
            '*' => pattern.push_str("[*]"),
            '?' => pattern.push_str("[?]"),
            '[' => pattern.push_str("[[]"),
            ']' => pattern.push_str("[]]"),
            '{' => pattern.push_str("[{]"),
            '}' => pattern.push_str("[}]"),
            _ => pattern.push(character),
        }
    }
    pattern.push_str("/**");
    pattern
}

pub(super) fn violation(configuration: &WorkspaceConfiguration) -> Option<ConfigurationViolation> {
    if configuration.profiles.len() > PROFILE_COUNT_MAX {
        return super::out_of_range(
            "profiles",
            configuration.profiles.len() as u64,
            0,
            PROFILE_COUNT_MAX as u64,
        );
    }
    let mut roots = Vec::<&str>::new();
    for (name, profile) in &configuration.profiles {
        let detail = if !valid_registration_name(name)
            || name.len() > crate::identity::SYMBOL_ID_BYTES_MAX - "local:".len()
        {
            Some("profile name is not canonical")
        } else if !valid_root(&profile.root) {
            Some("profile root must name a folder below this project using forward slashes")
        } else if roots.iter().any(|root| overlaps(root, &profile.root)) {
            Some("profile roots overlap; select separate project folders")
        } else {
            None
        };
        if let Some(detail) = detail {
            return Some(ConfigurationViolation::McpRegistrationInvalid {
                field: "profiles",
                name: name.clone(),
                detail,
            });
        }
        roots.push(&profile.root);
        let mut lsp = configuration.lsp.clone();
        lsp.extend(profile.lsp.clone());
        let selected = profile
            .source
            .violation()
            .or_else(|| {
                profile
                    .execution
                    .as_ref()
                    .and_then(ExecutionConfiguration::violation)
            })
            .or_else(|| {
                profile
                    .dependencies
                    .as_ref()
                    .and_then(DependenciesConfiguration::violation)
            })
            .or_else(|| super::languages_violation(&profile.languages, &lsp))
            .or_else(|| super::lsp_configurations_violation(&lsp));
        if selected.is_some() {
            return selected;
        }
    }
    None
}

fn valid_root(root: &str) -> bool {
    !root.is_empty()
        && u64::try_from(root.len()).is_ok_and(|bytes| bytes <= super::CONFIGURATION_FILE_BYTES_MAX)
        && !root.contains(['\\', ':'])
        && !root.chars().any(char::is_control)
        && root.split('/').take(PROFILE_ROOT_DEPTH_MAX + 1).count() <= PROFILE_ROOT_DEPTH_MAX
        && root
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".." | "~"))
}

fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|tail| tail.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|tail| tail.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_enforce_entry_bound_in_model_and_schema() -> Result<(), Box<dyn std::error::Error>>
    {
        let schema = serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration))?;
        let validator = jsonschema::validator_for(&schema)?;
        let profiles: serde_json::Map<_, _> = (0..PROFILE_COUNT_MAX)
            .map(|index| {
                let name = format!("project-{index}");
                (name.clone(), serde_json::json!({"root": name}))
            })
            .collect();
        let mut input = serde_json::json!({"profiles": profiles});
        let configuration: WorkspaceConfiguration = serde_json::from_value(input.clone())?;
        assert!(configuration.validate().is_ok());
        assert!(validator.is_valid(&input));
        input["profiles"]
            .as_object_mut()
            .expect("profile declarations")
            .insert("extra".to_owned(), serde_json::json!({"root": "extra"}));
        let configuration: WorkspaceConfiguration = serde_json::from_value(input.clone())?;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "profiles",
                value: 65,
                max: 64,
                ..
            })
        ));
        assert!(!validator.is_valid(&input));
        Ok(())
    }

    #[test]
    fn profile_overrides_dependencies_and_merges_named_processes()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut configuration: WorkspaceConfiguration = serde_json::from_value(
            serde_json::json!({
                "dependencies": {"resolution":"static"},
                "lsp": {"shared":{"command":"root-server"}, "retained":{"command":"retained-server"}},
                "profiles": {"docs": {"root":"docs", "dependencies":{"resolution":"auto", "command_timeout":"3s"},
                    "languages":{"typescript":{"lsp":"shared"}}, "lsp":{"shared":{"command":"profile-server"}}}}
            }),
        )?;
        assert!(configuration.validate().is_ok());
        let selected = configuration
            .for_profile("docs")
            .ok_or("missing docs profile")?;
        assert_eq!(selected.dependencies.command_timeout.milliseconds(), 3_000);
        assert_ne!(
            selected.dependencies.resolution,
            configuration.dependencies.resolution
        );
        assert_eq!(
            selected
                .resolve_language_lsp("typescript")
                .ok_or("missing shared process")?
                .configuration
                .command
                .as_ref()
                .map(super::super::CommandInput::program),
            Some("profile-server")
        );
        assert_eq!(
            selected.lsp["retained"]
                .command
                .as_ref()
                .map(super::super::CommandInput::program),
            Some("retained-server")
        );
        configuration
            .profiles
            .get_mut("docs")
            .ok_or("missing docs profile")?
            .dependencies
            .as_mut()
            .ok_or("missing profile dependencies")?
            .command_timeout = super::super::Duration::from_millis(0);
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "dependencies.command_timeout",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn profile_execution_preserves_child_configuration_and_inherits_when_absent()
    -> Result<(), Box<dyn std::error::Error>> {
        let expected = serde_json::json!({"max_code":"16kb", "max_timeout":"30s", "max_output":"8kb", "max_concurrent":2});
        let mut configuration: WorkspaceConfiguration = serde_json::from_value(
            serde_json::json!({
                "execution": {"max_code":"32kb", "max_timeout":"1m", "max_output":"16kb", "max_concurrent":4},
                "profiles": {"docs": {"root":"docs", "execution": expected}}
            }),
        )?;
        configuration
            .validate()
            .map_err(|violation| super::super::configuration_violation_error(&violation))?;
        let selected = configuration
            .for_profile("docs")
            .ok_or("missing docs profile")?;
        assert_eq!(serde_json::to_value(selected.execution)?, expected);
        configuration
            .profiles
            .get_mut("docs")
            .ok_or("missing docs profile")?
            .execution = None;
        assert_eq!(
            configuration
                .for_profile("docs")
                .ok_or("missing docs profile")?
                .execution,
            configuration.execution
        );
        configuration
            .profiles
            .get_mut("docs")
            .ok_or("missing docs profile")?
            .execution = Some(ExecutionConfiguration {
            max_concurrent: 0,
            ..ExecutionConfiguration::default()
        });
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "execution.max_concurrent",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn profile_schema_requires_root_and_matches_relative_folder_rules() {
        let schema = serde_json::to_value(schemars::schema_for!(ProfileConfiguration))
            .expect("profile schema");
        let validator = jsonschema::validator_for(&schema).expect("profile schema compiles");
        assert!(!validator.is_valid(&serde_json::json!({})));
        assert!(serde_json::from_value::<ProfileConfiguration>(serde_json::json!({})).is_err());
        for (depth, accepted) in [
            (PROFILE_ROOT_DEPTH_MAX, true),
            (PROFILE_ROOT_DEPTH_MAX + 1, false),
        ] {
            let root = std::iter::repeat_n("docs", depth)
                .collect::<Vec<_>>()
                .join("/");
            assert_eq!(valid_root(&root), accepted);
            assert_eq!(
                validator.is_valid(&serde_json::json!({"root": root})),
                accepted
            );
        }
        for root in ["docs", "docs/api", "docs space", "docs[1]", "~docs"] {
            assert!(
                validator.is_valid(&serde_json::json!({"root": root})),
                "{root}"
            );
            assert!(valid_root(root), "{root}");
        }
        for root in [
            "",
            ".",
            "..",
            "docs/../api",
            "/docs",
            "docs/",
            "docs//api",
            "docs\\api",
            "C:docs",
            "~",
            "docs/~",
            "docs\n",
        ] {
            assert!(
                !validator.is_valid(&serde_json::json!({"root": root})),
                "{root}"
            );
            assert!(!valid_root(root), "{root}");
        }
    }

    #[test]
    fn profiles_reject_invalid_names_roots_and_overlapping_folders() {
        for (name, root) in [
            ("Docs", "docs"),
            ("all", "docs"),
            ("docs", "."),
            ("docs", "../docs"),
            ("docs", "/docs"),
            ("docs", "docs\\api"),
            ("docs", "docs//api"),
        ] {
            let mut configuration = WorkspaceConfiguration::default();
            configuration.profiles.insert(
                name.to_owned(),
                ProfileConfiguration {
                    root: root.to_owned(),
                    ..ProfileConfiguration::default()
                },
            );
            assert!(configuration.validate().is_err(), "{name}: {root}");
        }
        let mut configuration = WorkspaceConfiguration::default();
        for (name, root) in [("docs", "docs"), ("api", "docs/api")] {
            configuration.profiles.insert(
                name.to_owned(),
                ProfileConfiguration {
                    root: root.to_owned(),
                    ..ProfileConfiguration::default()
                },
            );
        }
        assert!(configuration.validate().is_err());
    }

    #[test]
    fn profile_replaces_source_and_languages_and_retains_shared_settings() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration
            .source
            .exclude
            .push(crate::read::PathPattern("docs/**".to_owned()));
        configuration
            .languages
            .insert("rust".to_owned(), LanguageConfiguration::default());
        configuration.telemetry.disabled = true;
        configuration.server.port = Some(39123);
        configuration
            .mcp
            .extra
            .insert("tools".to_owned(), "../tools".to_owned());
        let mut profile = ProfileConfiguration {
            root: "docs".to_owned(),
            ..ProfileConfiguration::default()
        };
        profile
            .languages
            .insert("typescript".to_owned(), LanguageConfiguration::default());
        configuration.profiles.insert("docs".to_owned(), profile);
        let selected = configuration.for_profile("docs").expect("declared profile");
        assert!(selected.source.exclude.is_empty());
        assert!(!selected.languages.contains_key("rust"));
        assert!(selected.languages.contains_key("typescript"));
        assert!(selected.profiles.is_empty());
        assert!(selected.mcp.extra.is_empty());
        assert!(selected.telemetry.disabled);
        assert_eq!(selected.server.port, None);
        assert_eq!(selected.server.port_range, None);
        assert!(configuration.for_profile("missing").is_none());
    }
}
