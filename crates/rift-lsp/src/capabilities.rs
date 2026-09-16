//! The capability record negotiated with one engine at initialize.
//!
//! The session offers UTF-8 positions first with UTF-16 as the mandatory
//! fallback, and records which operations the engine advertised. Typed
//! operations consult this record before sending anything.

use lsp_types::{
    ClientCapabilities, DiagnosticClientCapabilities, DiagnosticServerCapabilities,
    DiagnosticWorkspaceClientCapabilities, DidChangeWatchedFilesClientCapabilities,
    GeneralClientCapabilities, InitializeResult, OneOf, PositionEncodingKind,
    ReferenceClientCapabilities, TextDocumentClientCapabilities, WindowClientCapabilities,
    WorkspaceClientCapabilities,
};
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

/// How one byte offset maps to an LSP `character` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionEncoding {
    /// Characters count UTF-8 bytes.
    Utf8,
    /// Characters count UTF-16 code units, the protocol default.
    Utf16,
}

/// An initialize answer outside what the session offered.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitiesFault {
    /// The engine picked a position encoding the session never offered.
    PositionEncodingUnsupported {
        /// The encoding as answered.
        encoding: String,
    },
}

impl Fault for CapabilitiesFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::CapabilityUnavailable)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let Self::PositionEncodingUnsupported { encoding } = self;
        vec![
            ErrorContext::new("fault", fault_label(self)),
            ErrorContext::new("encoding", encoding.clone()),
        ]
    }
}

/// An engine answer the capability record refuses.
pub type CapabilitiesError = Error<CapabilitiesFault>;

/// What one engine advertised at initialize.
#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    /// The negotiated position encoding.
    pub position_encoding: PositionEncoding,
    /// Whether the engine serves `textDocument/references`.
    pub references: bool,
    /// Whether the engine serves `textDocument/diagnostic`.
    pub pull_diagnostics: bool,
    /// The identifier the engine registered its diagnostics under.
    pub diagnostic_identifier: Option<String>,
}

impl Default for Capabilities {
    /// The protocol defaults before negotiation: UTF-16, nothing served.
    fn default() -> Self {
        Self {
            position_encoding: PositionEncoding::Utf16,
            references: false,
            pull_diagnostics: false,
            diagnostic_identifier: None,
        }
    }
}

impl Capabilities {
    /// Builds the record from an engine's initialize answer.
    ///
    /// An absent position encoding is UTF-16, the protocol default.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilitiesError`] when the engine picked an encoding the
    /// session never offered.
    pub fn negotiated(answer: &InitializeResult) -> Result<Self, CapabilitiesError> {
        let advertised = &answer.capabilities;
        let position_encoding = match advertised.position_encoding.as_ref() {
            None => PositionEncoding::Utf16,
            Some(kind) if *kind == PositionEncodingKind::UTF8 => PositionEncoding::Utf8,
            Some(kind) if *kind == PositionEncodingKind::UTF16 => PositionEncoding::Utf16,
            Some(kind) => {
                return Err(Error::new(CapabilitiesFault::PositionEncodingUnsupported {
                    encoding: kind.as_str().to_owned(),
                }));
            }
        };
        let references = match advertised.references_provider.as_ref() {
            Some(OneOf::Left(served)) => *served,
            Some(OneOf::Right(_options)) => true,
            None => false,
        };
        let (pull_diagnostics, diagnostic_identifier) =
            match advertised.diagnostic_provider.as_ref() {
                Some(DiagnosticServerCapabilities::Options(options)) => {
                    (true, options.identifier.clone())
                }
                Some(DiagnosticServerCapabilities::RegistrationOptions(registration)) => {
                    (true, registration.diagnostic_options.identifier.clone())
                }
                None => (false, None),
            };
        Ok(Self {
            position_encoding,
            references,
            pull_diagnostics,
            diagnostic_identifier,
        })
    }
}

/// Whether one LSP glob matches a slash-separated relative path.
///
/// `*` and `?` stay inside one path segment and `**` crosses segments, the
/// LSP pattern grammar. A glob that does not compile matches nothing.
/// Shared with `session`'s watched-file matching, so the pattern grammar
/// has one compiled representation.
pub(crate) fn glob_matches(glob: &str, ignore_case: bool, path: &str) -> bool {
    globset::GlobBuilder::new(glob)
        .literal_separator(true)
        .case_insensitive(ignore_case)
        .build()
        .is_ok_and(|compiled| compiled.compile_matcher().is_match(path))
}

/// What the session offers every engine.
///
/// UTF-8 positions preferred with the mandatory UTF-16 fallback, references,
/// and document diagnostic pulls.
///
/// `window.workDoneProgress` is what makes an engine report the work it is
/// doing: the protocol forbids server-initiated progress unless the client
/// declares it, so without this entry an engine loading a project reports
/// nothing and every answer it gives while loading reads as settled.
///
/// `textDocument.diagnostic.dynamicRegistration` is what makes an engine
/// advertise `diagnostic_provider` at all when its own pull-diagnostics
/// support is conditional on that flag: tombi, the TOML engine, sets
/// `diagnostic_provider` only when the client declares dynamic
/// registration, so without this entry its pull-diagnostics chain never
/// closes.
///
/// `workspace.didChangeWatchedFiles.dynamicRegistration` is what lets an
/// engine ask, through `client/registerCapability`, which paths it wants
/// told about after a change lands outside any document it has open;
/// without this entry an engine has no channel to register that interest
/// on, and `EngineSession::notify_changed_paths` has nothing to match
/// against.
#[must_use]
pub fn offered() -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16,
            ]),
            ..GeneralClientCapabilities::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            diagnostic: Some(DiagnosticWorkspaceClientCapabilities {
                refresh_support: Some(true),
            }),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            ..WorkspaceClientCapabilities::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            references: Some(ReferenceClientCapabilities::default()),
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(true),
                ..DiagnosticClientCapabilities::default()
            }),
            ..TextDocumentClientCapabilities::default()
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..WindowClientCapabilities::default()
        }),
        ..ClientCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{DiagnosticOptions, ServerCapabilities};

    fn answer(capabilities: ServerCapabilities) -> InitializeResult {
        InitializeResult {
            capabilities,
            ..InitializeResult::default()
        }
    }

    #[test]
    fn absent_answers_negotiate_the_protocol_defaults() {
        let record =
            Capabilities::negotiated(&answer(ServerCapabilities::default())).expect("record");
        assert_eq!(record.position_encoding, PositionEncoding::Utf16);
        assert!(!record.references);
        assert!(!record.pull_diagnostics);
        assert_eq!(record.diagnostic_identifier, None);
    }

    #[test]
    fn references_forms_map_to_the_references_flag() {
        let absent = answer(ServerCapabilities::default());
        assert!(
            !Capabilities::negotiated(&absent)
                .expect("record")
                .references
        );
        let plain = answer(ServerCapabilities {
            references_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        assert!(Capabilities::negotiated(&plain).expect("record").references);
        let refused = answer(ServerCapabilities {
            references_provider: Some(OneOf::Left(false)),
            ..ServerCapabilities::default()
        });
        assert!(
            !Capabilities::negotiated(&refused)
                .expect("record")
                .references
        );
        let options = answer(ServerCapabilities {
            references_provider: Some(OneOf::Right(lsp_types::ReferencesOptions {
                work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
            })),
            ..ServerCapabilities::default()
        });
        assert!(
            Capabilities::negotiated(&options)
                .expect("record")
                .references
        );
    }

    #[test]
    fn utf8_is_accepted_and_an_unoffered_encoding_is_refused() {
        let utf8 = answer(ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF8),
            ..ServerCapabilities::default()
        });
        assert_eq!(
            Capabilities::negotiated(&utf8)
                .expect("record")
                .position_encoding,
            PositionEncoding::Utf8
        );
        let utf32 = answer(ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF32),
            ..ServerCapabilities::default()
        });
        let error = Capabilities::negotiated(&utf32).expect_err("utf-32 was never offered");
        assert_eq!(
            *error.fault(),
            CapabilitiesFault::PositionEncodingUnsupported {
                encoding: "utf-32".to_owned()
            }
        );
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
        assert!(error.to_string().contains("encoding utf-32"));
    }

    #[test]
    fn registered_diagnostics_are_recorded_with_their_identifier() {
        let advertised = answer(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::RegistrationOptions(
                lsp_types::DiagnosticRegistrationOptions {
                    text_document_registration_options:
                        lsp_types::TextDocumentRegistrationOptions::default(),
                    diagnostic_options: DiagnosticOptions {
                        identifier: Some("registered".to_owned()),
                        ..DiagnosticOptions::default()
                    },
                    static_registration_options: lsp_types::StaticRegistrationOptions::default(),
                },
            )),
            ..ServerCapabilities::default()
        });
        let record = Capabilities::negotiated(&advertised).expect("record");
        assert!(record.pull_diagnostics);
        assert_eq!(record.diagnostic_identifier.as_deref(), Some("registered"));
    }

    #[test]
    fn diagnostic_options_are_recorded() {
        let advertised = answer(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
                identifier: Some("probe".to_owned()),
                ..DiagnosticOptions::default()
            })),
            ..ServerCapabilities::default()
        });
        let record = Capabilities::negotiated(&advertised).expect("record");
        assert!(record.pull_diagnostics);
        assert_eq!(record.diagnostic_identifier.as_deref(), Some("probe"));
    }

    #[test]
    fn offered_capabilities_state_the_encoding_preference_in_order() {
        let offered = offered();
        let encodings = offered
            .general
            .expect("general capabilities are offered")
            .position_encodings
            .expect("encodings are offered");
        assert_eq!(
            encodings,
            [PositionEncodingKind::UTF8, PositionEncodingKind::UTF16]
        );
        let workspace = offered
            .workspace
            .expect("workspace capabilities are offered");
        assert_eq!(
            workspace
                .diagnostic
                .expect("workspace diagnostic capabilities are offered")
                .refresh_support,
            Some(true),
            "diagnostic refresh requests can invalidate an earlier pull"
        );
        let text_document = offered
            .text_document
            .expect("text document capabilities are offered");
        assert!(
            text_document.references.is_some(),
            "reference requests require textDocument/references"
        );
        let window = offered.window.expect("window capabilities are offered");
        assert_eq!(
            window.work_done_progress,
            Some(true),
            "an engine only reports its work when the client declares this"
        );
        let diagnostic = text_document
            .diagnostic
            .expect("diagnostic capabilities are offered");
        assert_eq!(
            diagnostic.dynamic_registration,
            Some(true),
            "an engine that gates diagnostic_provider on dynamic \
             registration only advertises it when the client declares this"
        );
    }
}
