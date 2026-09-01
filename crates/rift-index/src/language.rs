//! Effective language and plain-text path selection.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rift_core::{LanguageFileSelection, LanguageFileSelections, TextFileInclusion};
use rift_syntax::{SyntaxProvider, registry};

use crate::PathMatcher;
use crate::workspace::{WorkspaceIndexError, WorkspaceIndexViolation, index_error_caused_by};

/// One accepted language entry with expanded path patterns.
#[derive(Debug)]
pub struct EffectiveLanguage {
    identity: String,
    enabled: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    provider: Option<&'static dyn SyntaxProvider>,
    matcher: Option<PathMatcher>,
}

impl EffectiveLanguage {
    /// Exact language identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Whether matched paths receive language service.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Effective include patterns after shipped defaults expand.
    #[must_use]
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// Effective exclude patterns.
    #[must_use]
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// Whether this build ships syntax analysis for the language.
    #[must_use]
    pub const fn has_syntax(&self) -> bool {
        self.provider.is_some()
    }

    /// Shipped syntax provider, when this build serves one.
    #[must_use]
    pub fn syntax_provider(&self) -> Option<&'static dyn SyntaxProvider> {
        self.provider
    }

    fn matches(&self, path: &Path) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|matcher| matcher.includes(path))
    }
}

/// Effective language and plain-text path selection.
#[derive(Debug)]
pub struct WorkspaceLanguagePolicy {
    root: PathBuf,
    languages: Vec<EffectiveLanguage>,
    text: Option<PathMatcher>,
}

impl WorkspaceLanguagePolicy {
    /// Expands shipped defaults and compiles every configured pattern.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` for invalid patterns or an unshipped
    /// language without a nonempty include list.
    pub fn build(
        root: &Path,
        selections: &LanguageFileSelections,
        text: &TextFileInclusion,
    ) -> Result<Self, WorkspaceIndexError> {
        let mut languages = Vec::new();
        let mut shipped = BTreeSet::new();
        for (definition, provider) in registry::shipped_languages() {
            let identity = definition.shipped().language().identity_segment();
            shipped.insert(identity.clone());
            let configured = selections
                .entries()
                .iter()
                .find(|selection| selection.identity() == identity);
            let enabled = configured.is_none_or(LanguageFileSelection::enabled);
            let include = configured
                .and_then(LanguageFileSelection::include)
                .map_or_else(
                    || {
                        definition
                            .extensions()
                            .iter()
                            .map(|extension| format!("**/*.{extension}"))
                            .collect()
                    },
                    <[String]>::to_vec,
                );
            let exclude = configured
                .map(LanguageFileSelection::exclude)
                .map_or_else(Vec::new, <[String]>::to_vec);
            languages.push(Self::entry(
                root,
                identity,
                enabled,
                include,
                exclude,
                Some(provider),
            )?);
        }
        for selection in selections.entries() {
            if shipped.contains(selection.identity()) {
                continue;
            }
            let include = selection
                .include()
                .filter(|include| !include.is_empty())
                .ok_or_else(|| {
                    index_error_caused_by(
                        WorkspaceIndexViolation::LanguageIncludeRequired,
                        None,
                        LanguagePolicyError::IncludeRequired {
                            language: selection.identity().to_owned(),
                        },
                    )
                })?
                .to_vec();
            languages.push(Self::entry(
                root,
                selection.identity().to_owned(),
                selection.enabled(),
                include,
                selection.exclude().to_vec(),
                None,
            )?);
        }
        languages.sort_by(|left, right| left.identity.cmp(&right.identity));
        let text = (!text.include().is_empty())
            .then(|| PathMatcher::build(root, text.include(), &[]))
            .transpose()?;
        Ok(Self {
            root: root.to_path_buf(),
            languages,
            text,
        })
    }

    fn entry(
        root: &Path,
        identity: String,
        enabled: bool,
        include: Vec<String>,
        exclude: Vec<String>,
        provider: Option<&'static dyn SyntaxProvider>,
    ) -> Result<EffectiveLanguage, WorkspaceIndexError> {
        let matcher = (!include.is_empty())
            .then(|| PathMatcher::build(root, &include, &exclude))
            .transpose()?;
        Ok(EffectiveLanguage {
            identity,
            enabled,
            include,
            exclude,
            provider,
            matcher,
        })
    }

    /// Effective entries in exact identity order.
    #[must_use]
    pub fn languages(&self) -> &[EffectiveLanguage] {
        &self.languages
    }

    /// Selects one exact language for a path.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` when two language entries match.
    pub fn language_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<&EffectiveLanguage>, WorkspaceIndexError> {
        let path = self.absolute(path);
        let mut matched = self
            .languages
            .iter()
            .filter(|language| language.matches(&path));
        let first = matched.next();
        if let (Some(first), Some(second)) = (first, matched.next()) {
            return Err(index_error_caused_by(
                WorkspaceIndexViolation::LanguageMatchConflict,
                Some(&path),
                LanguagePolicyError::MatchConflict {
                    first: first.identity.clone(),
                    second: second.identity.clone(),
                },
            ));
        }
        Ok(first)
    }

    /// Which lane one visible path joins.
    ///
    /// An enabled entry with a shipped provider makes the path source. Every
    /// other path falls through to `[search.text]`, so a path an entry claims
    /// while disabled, or one whose language ships no grammar, still reaches
    /// lexical search as plain text.
    ///
    /// # Errors
    ///
    /// Returns `WorkspaceIndexError` when two language entries match `path`.
    pub(crate) fn classifies(
        &self,
        path: &Path,
    ) -> Result<Option<ClassifiedPath>, WorkspaceIndexError> {
        let path = self.absolute(path);
        if let Some(provider) = self
            .language_for_path(&path)?
            .filter(|language| language.enabled)
            .and_then(EffectiveLanguage::syntax_provider)
        {
            return Ok(Some(ClassifiedPath::Source(provider)));
        }
        Ok(self
            .text
            .as_ref()
            .is_some_and(|matcher| matcher.includes(&path))
            .then_some(ClassifiedPath::Text))
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ClassifiedPath {
    Source(&'static dyn SyntaxProvider),
    Text,
}

#[derive(Debug)]
pub(crate) enum LanguagePolicyError {
    IncludeRequired { language: String },
    MatchConflict { first: String, second: String },
}

impl LanguagePolicyError {
    pub(crate) fn evidence(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::IncludeRequired { language } => vec![("language", language.clone())],
            Self::MatchConflict { first, second } => {
                vec![("first", first.clone()), ("second", second.clone())]
            }
        }
    }
}

impl std::fmt::Display for LanguagePolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncludeRequired { language } => write!(
                formatter,
                "unshipped language {language:?} requires a nonempty include list"
            ),
            Self::MatchConflict { first, second } => write!(
                formatter,
                "one path matches language entries {first:?} and {second:?}"
            ),
        }
    }
}

impl std::error::Error for LanguagePolicyError {}

#[cfg(test)]
mod tests {
    use rift_core::LanguageFileSelections;
    use rift_protocol::configuration::{LanguageConfiguration, WorkspaceConfiguration};

    use super::*;

    fn policy(configuration: &WorkspaceConfiguration) -> WorkspaceLanguagePolicy {
        WorkspaceLanguagePolicy::build(
            Path::new("/workspace"),
            &LanguageFileSelections::from(configuration),
            &TextFileInclusion::from(&configuration.search),
        )
        .expect("language policy")
    }

    #[test]
    fn test_shipped_defaults_select_exact_typescript_dialects() {
        let policy = policy(&WorkspaceConfiguration::default());
        let typescript = policy
            .language_for_path(Path::new("src/main.ts"))
            .expect("selection")
            .expect("typescript");
        let tsx = policy
            .language_for_path(Path::new("src/main.tsx"))
            .expect("selection")
            .expect("tsx");
        assert_eq!(typescript.identity(), "typescript");
        assert_eq!(tsx.identity(), "typescript:tsx");
    }

    #[test]
    fn test_explicit_empty_include_removes_shipped_claim() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            rift_protocol::configuration::LanguageConfiguration {
                include: Some(Vec::new()),
                ..Default::default()
            },
        );
        let policy = policy(&configuration);
        assert!(
            policy
                .language_for_path(Path::new("src/lib.rs"))
                .expect("selection")
                .is_none()
        );
    }

    #[test]
    fn test_language_claim_wins_over_text_pattern() {
        let policy = policy(&WorkspaceConfiguration::default());
        assert!(matches!(
            policy
                .classifies(Path::new("README.md"))
                .expect("selection"),
            Some(ClassifiedPath::Source(_))
        ));
    }

    #[test]
    fn test_unshipped_language_requires_nonempty_include() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration
            .languages
            .insert("python".to_owned(), LanguageConfiguration::default());
        let error = WorkspaceLanguagePolicy::build(
            Path::new("/workspace"),
            &LanguageFileSelections::from(&configuration),
            &TextFileInclusion::from(&configuration.search),
        )
        .expect_err("missing include");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::LanguageIncludeRequired
        );
    }

    /// A misspelled shipped name is a language no build ships, so it meets the
    /// same rule: the entry stands only as an explicit selection with its own
    /// patterns, and the refusal names the key to fix.
    #[test]
    fn test_a_misspelled_shipped_language_name_is_refused_without_patterns() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration
            .languages
            .insert("rustt".to_owned(), LanguageConfiguration::default());
        let error = WorkspaceLanguagePolicy::build(
            Path::new("/workspace"),
            &LanguageFileSelections::from(&configuration),
            &TextFileInclusion::from(&configuration.search),
        )
        .expect_err("a misspelled shipped name carries no shipped patterns");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::LanguageIncludeRequired
        );
        assert!(
            error.to_string().contains("rustt"),
            "the refusal names the key: {error}"
        );

        configuration.languages.insert(
            "rustt".to_owned(),
            LanguageConfiguration {
                include: Some(vec![rift_protocol::read::PathPattern(
                    "**/*.rustt".to_owned(),
                )]),
                ..LanguageConfiguration::default()
            },
        );
        let policy = policy(&configuration);
        let rust = policy
            .language_for_path(Path::new("src/lib.rs"))
            .expect("lookup")
            .expect("the shipped Rust entry keeps its own patterns");
        assert_eq!(
            rust.identity(),
            "rust",
            "the misspelled entry claims only what its own patterns name"
        );
        let claimed = policy
            .language_for_path(Path::new("src/main.rustt"))
            .expect("lookup")
            .expect("its own pattern selects it");
        assert_eq!(claimed.identity(), "rustt");
        assert!(!claimed.has_syntax(), "no build ships that grammar");
    }

    #[test]
    fn test_overlapping_language_patterns_report_both_entries() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            rift_protocol::configuration::LanguageConfiguration {
                include: Some(vec![rift_protocol::read::PathPattern("src/**".to_owned())]),
                ..Default::default()
            },
        );
        configuration.languages.insert(
            "python".to_owned(),
            rift_protocol::configuration::LanguageConfiguration {
                include: Some(vec![rift_protocol::read::PathPattern(
                    "src/lib.rs".to_owned(),
                )]),
                ..Default::default()
            },
        );
        let policy = policy(&configuration);
        let error = policy
            .language_for_path(Path::new("src/lib.rs"))
            .expect_err("conflict");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::LanguageMatchConflict
        );
        let message = error.to_string();
        assert!(message.contains("rust") && message.contains("python"));
    }

    /// A pattern the glob engine refuses refuses the whole policy, whether the
    /// entry names a language this build ships or one it does not.
    #[test]
    fn test_an_invalid_include_pattern_refuses_for_a_shipped_and_an_unshipped_language() {
        for identity in ["rust", "python"] {
            let mut configuration = WorkspaceConfiguration::default();
            configuration.languages.insert(
                identity.to_owned(),
                LanguageConfiguration {
                    include: Some(vec![rift_protocol::read::PathPattern("[".to_owned())]),
                    ..LanguageConfiguration::default()
                },
            );
            let error = WorkspaceLanguagePolicy::build(
                Path::new("/workspace"),
                &LanguageFileSelections::from(&configuration),
                &TextFileInclusion::from(&configuration.search),
            )
            .expect_err("an unclosed character class must refuse");
            assert_eq!(
                error.fault().violation(),
                WorkspaceIndexViolation::SourcePatternInvalid,
                "the {identity} entry names the pattern rule it broke"
            );
        }
    }

    /// Both refusals name the exact keys an operator has to reconcile: the
    /// language whose entry carries no patterns, and the two entries one path
    /// matched.
    #[test]
    fn test_language_policy_error_display_names_the_keys_to_reconcile() {
        let include_required = LanguagePolicyError::IncludeRequired {
            language: "python".to_owned(),
        };
        assert_eq!(
            include_required.to_string(),
            "unshipped language \"python\" requires a nonempty include list"
        );
        let conflict = LanguagePolicyError::MatchConflict {
            first: "rust".to_owned(),
            second: "python".to_owned(),
        };
        assert_eq!(
            conflict.to_string(),
            "one path matches language entries \"rust\" and \"python\""
        );
    }

    /// A `rift://workspace` page reports one entry per shipped provider plus
    /// one per configured entry the shipped set does not name, so the page's
    /// advertised bound has to cover both.
    #[test]
    fn test_every_shipped_provider_fits_the_workspace_page_language_bound() {
        use rift_protocol::configuration::LANGUAGES_MAX;
        use rift_protocol::workspace::WORKSPACE_LANGUAGE_SUMMARIES_MAX;

        let shipped = registry::providers().count();
        assert!(
            shipped + LANGUAGES_MAX <= WORKSPACE_LANGUAGE_SUMMARIES_MAX,
            "the workspace page must carry every effective entry: shipped={shipped}, \
             languages_max={LANGUAGES_MAX}, summaries_max={WORKSPACE_LANGUAGE_SUMMARIES_MAX}"
        );
    }

    /// Under the shipped defaults, a provider extension classifies as source
    /// and every other visible path falls through to the text lane, an
    /// extensionless one included.
    #[test]
    fn test_default_policy_classifies_provider_extensions_as_source_and_the_rest_as_text() {
        let policy = policy(&WorkspaceConfiguration::default());
        for path in ["lib.rs", "readme.md", "config.json", "deploy.yaml"] {
            assert!(
                matches!(
                    policy.classifies(Path::new(path)),
                    Ok(Some(ClassifiedPath::Source(_)))
                ),
                "a shipped provider must claim {path}"
            );
        }
        for path in ["notes.unknown", "justfile", "Dockerfile", "readme.txt"] {
            assert!(
                matches!(
                    policy.classifies(Path::new(path)),
                    Ok(Some(ClassifiedPath::Text))
                ),
                "an unclaimed path must join the text lane: {path}"
            );
        }
    }

    /// `enabled = false` drops syntax facts for the paths an entry claims and
    /// leaves them reachable as plain text, so lexical search still holds them.
    #[test]
    fn test_a_disabled_language_keeps_its_paths_in_the_text_lane() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            LanguageConfiguration {
                enabled: false,
                ..LanguageConfiguration::default()
            },
        );
        let policy = policy(&configuration);
        assert!(
            matches!(
                policy.classifies(Path::new("src/lib.rs")),
                Ok(Some(ClassifiedPath::Text))
            ),
            "a disabled entry drops syntax facts, not the file"
        );
    }

    /// An unshipped language selects a process without contributing syntax, so
    /// its paths reach lexical search as plain text.
    #[test]
    fn test_an_unshipped_language_keeps_its_paths_in_the_text_lane() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "python".to_owned(),
            LanguageConfiguration {
                include: Some(vec![rift_protocol::read::PathPattern("**/*.py".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        let policy = policy(&configuration);
        assert!(
            matches!(
                policy.classifies(Path::new("src/main.py")),
                Ok(Some(ClassifiedPath::Text))
            ),
            "a language with no shipped grammar still reaches lexical search"
        );
    }

    /// A bare language and one of its dialects are independent entries: a
    /// pattern set on one leaves the other's shipped patterns standing.
    #[test]
    fn test_a_dialect_inherits_no_matching_from_its_bare_language() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "typescript".to_owned(),
            LanguageConfiguration {
                include: Some(vec![rift_protocol::read::PathPattern(
                    "**/*.mts".to_owned(),
                )]),
                ..LanguageConfiguration::default()
            },
        );
        let policy = policy(&configuration);

        let typescript = policy
            .language_for_path(Path::new("src/main.mts"))
            .expect("lookup")
            .expect("the configured pattern selects typescript");
        assert_eq!(typescript.identity(), "typescript");
        assert!(
            policy
                .language_for_path(Path::new("src/main.ts"))
                .expect("lookup")
                .is_none(),
            "a present include replaces the shipped patterns"
        );
        let tsx = policy
            .language_for_path(Path::new("src/view.tsx"))
            .expect("lookup")
            .expect("the dialect keeps its own shipped patterns");
        assert_eq!(tsx.identity(), "typescript:tsx");
    }

    /// An empty `[languages]` table leaves every shipped provider serving the
    /// extensions it declares.
    #[test]
    fn test_absent_language_table_keeps_every_shipped_provider_and_pattern() {
        let policy = policy(&WorkspaceConfiguration::default());
        for (definition, _provider) in registry::shipped_languages() {
            let identity = definition.shipped().language().identity_segment();
            let entry = policy
                .languages()
                .iter()
                .find(|language| language.identity() == identity)
                .unwrap_or_else(|| panic!("shipped provider {identity} must have an entry"));
            assert!(entry.enabled(), "{identity} must stay enabled");
            assert!(entry.has_syntax(), "{identity} must keep its provider");
            let expected: Vec<String> = definition
                .extensions()
                .iter()
                .map(|extension| format!("**/*.{extension}"))
                .collect();
            assert_eq!(entry.include(), expected, "{identity} keeps its patterns");
            assert!(entry.exclude().is_empty(), "{identity} excludes nothing");
        }
    }
}
