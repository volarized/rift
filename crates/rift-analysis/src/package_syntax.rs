//! Per-file syntax inputs and facts for package assembly.

use std::cell::Cell;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use rift_core::{FileDigest, ProjectPath};
use rift_error::{ErrorContext, ErrorValue, RiftError, errors};
use rift_protocol::read::Language;
use rift_syntax::{SyntaxDocument, SyntaxFacts, SyntaxLimits, SyntaxProvider, SyntaxSource};

use crate::{PackageSource, analyzer_digest};
use rift_protocol::read::PackageIdentity;

/// The inputs that decide one shipped provider's syntax facts.
///
/// Paths and package versions are assigned during package assembly. The analyzer digest
/// covers the checked source manifest. A caller retaining facts across builds must also
/// establish agreement with the parser dependencies its build resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSyntaxIdentity {
    /// Digest of the complete source bytes.
    pub source_digest: FileDigest,
    /// Actual provider language, including its dialect.
    pub language: Language,
    /// Effective bounds used by the provider.
    pub limits: SyntaxLimits,
    /// Full digest returned by [`analyzer_digest`].
    pub analyzer_digest: String,
}

/// Shared per-file facts and the inputs their producer used.
///
/// The caller owns retention and any stored representation. Construct restored facts
/// through the checked syntax APIs before assigning their recorded identity here.
#[derive(Debug, Clone)]
pub struct PackageSyntax {
    identity: PackageSyntaxIdentity,
    facts: Arc<SyntaxFacts>,
}

impl PackageSyntax {
    /// Associates immutable facts with their recorded producer inputs.
    ///
    /// Package analysis compares both the identity and the facts' source witness before
    /// accepting them. A mismatch runs the current provider.
    #[must_use]
    pub fn new(identity: PackageSyntaxIdentity, facts: Arc<SyntaxFacts>) -> Self {
        Self { identity, facts }
    }

    /// Returns the producer inputs, including the full analyzer digest.
    #[must_use]
    pub const fn identity(&self) -> &PackageSyntaxIdentity {
        &self.identity
    }

    /// Returns shared path-independent facts.
    #[must_use]
    pub fn facts(&self) -> &Arc<SyntaxFacts> {
        &self.facts
    }
}

/// One selected package source before its syntax provider runs.
///
/// The analyzer supplies this value to its optional composition hook. The hook can
/// retain a successful [`Self::parse`] result before a later file or assembly fails.
#[derive(Debug)]
pub struct PackageSyntaxSource<'source> {
    file: PackageSource<'source>,
    package: &'source PackageIdentity,
    identity: PackageSyntaxIdentity,
    provider: Option<&'static dyn SyntaxProvider>,
    documentation: bool,
    provider_calls: Cell<u64>,
}

impl<'source> PackageSyntaxSource<'source> {
    pub(crate) fn new(
        file: PackageSource<'source>,
        package: &'source PackageIdentity,
        package_language: &'source Language,
        limits: SyntaxLimits,
    ) -> Self {
        let extension = Path::new(file.path().as_str())
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        let documentation = matches!(extension, "rst" | "txt" | "ipynb");
        let provider = (!documentation)
            .then(|| rift_syntax::registry::provider_for_extension(extension))
            .flatten();
        let language = provider.map_or_else(
            || crate::analyzer::source_language(extension, package_language),
            |provider| provider.language().clone(),
        );
        Self {
            file,
            package,
            identity: PackageSyntaxIdentity {
                source_digest: FileDigest::of(file.text().as_bytes()),
                language,
                limits,
                analyzer_digest: analyzer_digest(),
            },
            provider,
            documentation,
            provider_calls: Cell::new(0),
        }
    }

    /// Returns the selected source path.
    #[must_use]
    pub fn path(&self) -> &ProjectPath {
        self.file.path()
    }

    /// Returns the complete selected UTF-8 source.
    #[must_use]
    pub fn text(&self) -> &str {
        self.file.text()
    }

    /// Returns source, provider, and bound identities before parsing.
    #[must_use]
    pub const fn identity(&self) -> &PackageSyntaxIdentity {
        &self.identity
    }

    /// Reports whether a shipped syntax provider serves this file.
    ///
    /// RST, plain text, and notebooks use documentation extraction during assembly;
    /// those files do not supply reusable package syntax.
    #[must_use]
    pub const fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// Parses this source through the analyzer's existing provider selection.
    ///
    /// # Errors
    ///
    /// Returns the same provider failures and package context as ordinary analysis.
    ///
    /// # Panics
    ///
    /// Panics if this source invokes its provider more than `u64::MAX` times.
    pub fn parse(&self) -> Result<PackageSyntax, RiftError> {
        let facts = match self.provider {
            Some(provider) => {
                self.provider_calls.set(
                    self.provider_calls
                        .get()
                        .checked_add(1)
                        .expect("syntax provider call count must fit u64"),
                );
                provider
                    .analyze(
                        SyntaxSource {
                            path: self.file.path(),
                            text: self.file.text(),
                        },
                        self.identity.limits,
                    )
                    .map_err(|error| {
                        error
                            .with(ErrorContext::new(
                                "package",
                                ErrorValue::formatted(crate::analyzer::package_label(self.package)),
                            ))
                            .with(ErrorContext::new(
                                "path",
                                ErrorValue::path(self.file.path().as_str()),
                            ))
                    })?
                    .into_facts()
            }
            None if self.documentation => {
                SyntaxDocument::empty(self.identity.language.clone(), self.file.path().clone())
                    .into_facts()
            }
            None => {
                return errors::analysis::package_syntax_unavailable()
                    .package(crate::analyzer::package_label(self.package))
                    .path(self.file.path().as_str())
                    .fail();
            }
        };
        Ok(PackageSyntax::new(self.identity.clone(), facts))
    }

    pub(crate) fn accepts(&self, syntax: &PackageSyntax) -> bool {
        self.provider.is_some()
            && self.identity == syntax.identity
            && syntax.facts.language() == &self.identity.language
            && syntax.facts.syntax_limits() == Some(self.identity.limits)
            && syntax.facts.source_digest() == Some(&self.identity.source_digest)
            && self.file.text().len() <= self.identity.limits.source_bytes_max()
    }

    pub(crate) fn provider_calls(&self) -> u64 {
        self.provider_calls.get()
    }
}

/// Actual package syntax work during one analysis call.
///
/// Documentation extraction has separate parsers and is excluded from these counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PackageSyntaxWork {
    /// Calls to shipped syntax providers, including calls made by the composition hook.
    pub provider_calls: u64,
    /// Files accepting supplied facts without invoking their syntax provider.
    pub reused_files: u64,
}
