//! Installed language server selection by package manager.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Installation layout used to find an installed language server.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LspPackageManager {
    /// Rust toolchain selected by the Cargo project.
    Cargo,
    /// JavaScript and TypeScript packages installed by Bun.
    Bun,
    /// JavaScript and TypeScript packages installed by npm.
    Npm,
    /// Python packages installed in the uv project environment.
    Uv,
}

impl LspPackageManager {
    /// Configuration spelling of this package manager.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Bun => "bun",
            Self::Npm => "npm",
            Self::Uv => "uv",
        }
    }

    pub(super) fn language_evidence(self, language: &str) -> Vec<(&'static str, String)> {
        vec![
            ("field", "languages.lsp.package_manager".to_owned()),
            ("language", language.to_owned()),
            ("package_manager", self.as_str().to_owned()),
            (
                "detail",
                "package manager does not serve this language".to_owned(),
            ),
        ]
    }

    /// Whether this installation layout supplies the exact language's default server.
    #[must_use]
    pub fn supports(self, language: &str) -> bool {
        match self {
            Self::Cargo => language == "rust",
            Self::Bun | Self::Npm => {
                matches!(language, "javascript" | "typescript" | "typescript:tsx")
            }
            Self::Uv => language == "python",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{ConfigurationViolation, WorkspaceConfiguration};
    use serde_json::json;

    #[test]
    fn incompatible_manager_refusals_keep_language_and_manager()
    -> Result<(), Box<dyn std::error::Error>> {
        for (manager, language) in [
            ("cargo", "python"),
            ("bun", "rust"),
            ("npm", "python"),
            ("uv", "typescript"),
        ] {
            let configuration: WorkspaceConfiguration = serde_json::from_value(
                json!({"languages": {language: {"lsp": {"package_manager": manager}}}}),
            )?;
            let violation = configuration
                .validate()
                .expect_err("manager cannot serve this language");
            let error = crate::configuration::configuration_violation_error(&violation);
            let context: std::collections::BTreeMap<_, _> = error.context().collect();
            assert_eq!(
                context.get("field").map(String::as_str),
                Some("languages.lsp.package_manager")
            );
            assert_eq!(context.get("language").map(String::as_str), Some(language));
            assert_eq!(
                context.get("package_manager").map(String::as_str),
                Some(manager)
            );
        }
        Ok(())
    }

    #[test]
    fn package_managers_select_installed_servers_without_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        for (manager, language) in [
            (LspPackageManager::Cargo, "rust"),
            (LspPackageManager::Bun, "typescript"),
            (LspPackageManager::Npm, "typescript:tsx"),
            (LspPackageManager::Uv, "python"),
        ] {
            let value = json!({"languages": {language: {"lsp": {"package_manager": manager}}}});
            let configuration: WorkspaceConfiguration = serde_json::from_value(value)?;
            configuration.validate().map_err(|violation| {
                crate::configuration::configuration_violation_error(&violation)
            })?;
            let resolved = configuration
                .resolve_language_lsp(language)
                .ok_or("missing process")?;
            assert_eq!(resolved.configuration.package_manager, Some(manager));
            assert!(resolved.configuration.command.is_none());
        }
        Ok(())
    }

    #[test]
    fn conflicting_selectors_and_incompatible_languages_refuse()
    -> Result<(), Box<dyn std::error::Error>> {
        for selectors in [
            json!({"package_manager":"cargo","command":"rust-analyzer"}),
            json!({"package_manager":"cargo","embedded":"ty"}),
        ] {
            let configuration: WorkspaceConfiguration =
                serde_json::from_value(json!({"lsp":{"rust":selectors}}))?;
            assert!(matches!(
                configuration.validate(),
                Err(ConfigurationViolation::LspEngineSelectionConflict { .. })
            ));
        }
        for value in [
            json!({"languages":{"python":{"lsp":{"package_manager":"cargo"}}}}),
            json!({"languages":{"python":{"lsp":"rust"}},"lsp":{"rust":{"package_manager":"cargo"}}}),
        ] {
            let configuration: WorkspaceConfiguration = serde_json::from_value(value)?;
            let violation = configuration
                .validate()
                .expect_err("Cargo does not serve Python");
            assert!(matches!(
                violation,
                ConfigurationViolation::LspPackageManagerLanguageInvalid { .. }
            ));
            let error = crate::configuration::configuration_violation_error(&violation);
            assert_eq!(error.slug().as_str(), "rift.core.configuration_invalid");
        }
        Ok(())
    }
}
