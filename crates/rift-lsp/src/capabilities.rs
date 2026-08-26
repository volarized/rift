//! The capability record negotiated with one engine at initialize.
//!
//! The session offers UTF-8 positions first with UTF-16 as the mandatory
//! fallback, and records which operations the engine advertised. Typed
//! operations consult this record before sending anything.

use lsp_types::{
    ClientCapabilities, DiagnosticClientCapabilities, DiagnosticServerCapabilities,
    FileOperationFilter, GeneralClientCapabilities, InitializeResult, OneOf, PositionEncodingKind,
    ReferenceClientCapabilities, RenameClientCapabilities, TextDocumentClientCapabilities,
    WindowClientCapabilities, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
    WorkspaceFileOperationsClientCapabilities,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independently negotiated LSP capability, mirroring the \
              protocol's own capability grid, not a boolean mode parameter"
)]
#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    /// The negotiated position encoding.
    pub position_encoding: PositionEncoding,
    /// Whether the engine serves `textDocument/rename`.
    pub rename: bool,
    /// Whether the engine serves `textDocument/prepareRename`.
    pub prepare_rename: bool,
    /// Whether the engine serves `textDocument/references`.
    pub references: bool,
    /// The `workspace/willRenameFiles` filters; absent when unserved.
    pub will_rename_filters: Option<Vec<FileOperationFilter>>,
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
            rename: false,
            prepare_rename: false,
            references: false,
            will_rename_filters: None,
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
        let (rename, prepare_rename) = match advertised.rename_provider.as_ref() {
            Some(OneOf::Left(served)) => (*served, false),
            Some(OneOf::Right(options)) => (true, options.prepare_provider == Some(true)),
            None => (false, false),
        };
        let references = match advertised.references_provider.as_ref() {
            Some(OneOf::Left(served)) => *served,
            Some(OneOf::Right(_options)) => true,
            None => false,
        };
        let will_rename_filters = advertised
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.file_operations.as_ref())
            .and_then(|operations| operations.will_rename.as_ref())
            .map(|registration| registration.filters.clone());
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
            rename,
            prepare_rename,
            references,
            will_rename_filters,
            pull_diagnostics,
            diagnostic_identifier,
        })
    }

    /// Whether the engine serves `workspace/willRenameFiles`.
    #[must_use]
    pub fn will_rename_files(&self) -> bool {
        self.will_rename_filters.is_some()
    }

    /// Whether one project-relative file path matches the engine's
    /// will-rename filters.
    ///
    /// At most [`WILL_RENAME_FILTERS_MAX`] filters are consulted. A filter
    /// naming a non-`file` scheme or the folder pattern kind does not apply
    /// to a file move, and a glob the matcher cannot compile matches
    /// nothing. Absent filters match nothing: the capability gate, not this
    /// check, owns that refusal.
    #[must_use]
    pub fn will_rename_matches(&self, path: &str) -> bool {
        let Some(filters) = self.will_rename_filters.as_deref() else {
            return false;
        };
        filters
            .iter()
            .take(WILL_RENAME_FILTERS_MAX)
            .any(|filter| filter_matches(filter, path))
    }
}

/// Most will-rename filters one match consults.
pub const WILL_RENAME_FILTERS_MAX: usize = 64;

/// Whether one file-operation filter applies to and matches a file path.
fn filter_matches(filter: &FileOperationFilter, path: &str) -> bool {
    if filter
        .scheme
        .as_deref()
        .is_some_and(|scheme| scheme != "file")
    {
        return false;
    }
    if filter.pattern.matches == Some(lsp_types::FileOperationPatternKind::Folder) {
        return false;
    }
    let ignore_case = filter
        .pattern
        .options
        .as_ref()
        .is_some_and(|options| options.ignore_case == Some(true));
    glob_matches(&filter.pattern.glob, ignore_case, path)
}

/// Whether one LSP glob matches a slash-separated relative path.
///
/// `*` and `?` stay inside one path segment and `**` crosses segments, the
/// LSP pattern grammar. A glob that does not compile matches nothing.
fn glob_matches(glob: &str, ignore_case: bool, path: &str) -> bool {
    globset::GlobBuilder::new(glob)
        .literal_separator(true)
        .case_insensitive(ignore_case)
        .build()
        .ok()
        .is_some_and(|compiled| compiled.compile_matcher().is_match(path))
}

/// What the session offers every engine.
///
/// UTF-8 positions preferred with the mandatory UTF-16 fallback, prepared
/// renames, will-rename requests, and document diagnostic pulls.
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
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                ..WorkspaceEditClientCapabilities::default()
            }),
            file_operations: Some(WorkspaceFileOperationsClientCapabilities {
                will_rename: Some(true),
                ..WorkspaceFileOperationsClientCapabilities::default()
            }),
            ..WorkspaceClientCapabilities::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            rename: Some(RenameClientCapabilities {
                prepare_support: Some(true),
                ..RenameClientCapabilities::default()
            }),
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
    use lsp_types::{
        DiagnosticOptions, FileOperationRegistrationOptions, RenameOptions, ServerCapabilities,
        WorkspaceFileOperationsServerCapabilities, WorkspaceServerCapabilities,
    };

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
        assert!(!record.rename && !record.prepare_rename);
        assert!(!record.references);
        assert!(!record.will_rename_files());
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
    fn rename_forms_map_to_the_rename_and_prepare_flags() {
        let plain = answer(ServerCapabilities {
            rename_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        let record = Capabilities::negotiated(&plain).expect("record");
        assert!(record.rename && !record.prepare_rename);
        let refused = answer(ServerCapabilities {
            rename_provider: Some(OneOf::Left(false)),
            ..ServerCapabilities::default()
        });
        assert!(!Capabilities::negotiated(&refused).expect("record").rename);
        let prepared = answer(ServerCapabilities {
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: lsp_types::WorkDoneProgressOptions::default(),
            })),
            ..ServerCapabilities::default()
        });
        let record = Capabilities::negotiated(&prepared).expect("record");
        assert!(record.rename && record.prepare_rename);
    }

    #[test]
    fn will_rename_filters_and_diagnostic_options_are_recorded() {
        let advertised = answer(ServerCapabilities {
            workspace: Some(WorkspaceServerCapabilities {
                workspace_folders: None,
                file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                    will_rename: Some(FileOperationRegistrationOptions { filters: vec![] }),
                    ..WorkspaceFileOperationsServerCapabilities::default()
                }),
            }),
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
                identifier: Some("probe".to_owned()),
                ..DiagnosticOptions::default()
            })),
            ..ServerCapabilities::default()
        });
        let record = Capabilities::negotiated(&advertised).expect("record");
        assert!(record.will_rename_files());
        assert!(record.pull_diagnostics);
        assert_eq!(record.diagnostic_identifier.as_deref(), Some("probe"));
    }

    /// A record advertising exactly these will-rename filters.
    fn filtered(filters: Vec<lsp_types::FileOperationFilter>) -> Capabilities {
        Capabilities {
            will_rename_filters: Some(filters),
            ..Capabilities::default()
        }
    }

    fn filter(glob: &str) -> lsp_types::FileOperationFilter {
        lsp_types::FileOperationFilter {
            scheme: None,
            pattern: lsp_types::FileOperationPattern {
                glob: glob.to_owned(),
                matches: None,
                options: None,
            },
        }
    }

    #[test]
    fn will_rename_matches_follows_the_lsp_glob_grammar() {
        let record = filtered(vec![filter("**/*.{ts,tsx}")]);
        assert!(record.will_rename_matches("src/App.tsx"));
        assert!(record.will_rename_matches("index.ts"));
        assert!(!record.will_rename_matches("src/lib.rs"));
        let single_segment = filtered(vec![filter("*.rs")]);
        assert!(single_segment.will_rename_matches("lib.rs"));
        assert!(
            !single_segment.will_rename_matches("src/lib.rs"),
            "`*` must not cross a path segment"
        );
        assert!(filtered(vec![filter("**/*")]).will_rename_matches("a/b/c.py"));
        assert!(
            !filtered(vec![filter("**/*.[")]).will_rename_matches("lib.rs"),
            "a glob that does not compile matches nothing"
        );
        assert!(
            !Capabilities::default().will_rename_matches("lib.rs"),
            "absent filters match nothing"
        );
    }

    #[test]
    fn will_rename_filters_apply_by_scheme_kind_and_case_option() {
        let mut untitled = filter("**/*.rs");
        untitled.scheme = Some("untitled".to_owned());
        assert!(!filtered(vec![untitled]).will_rename_matches("lib.rs"));
        let mut file_scheme = filter("**/*.rs");
        file_scheme.scheme = Some("file".to_owned());
        assert!(filtered(vec![file_scheme]).will_rename_matches("lib.rs"));
        let mut folders = filter("**/*.rs");
        folders.pattern.matches = Some(lsp_types::FileOperationPatternKind::Folder);
        assert!(
            !filtered(vec![folders]).will_rename_matches("lib.rs"),
            "a folder filter does not apply to a file move"
        );
        let mut cased = filter("**/*.RS");
        assert!(!filtered(vec![cased.clone()]).will_rename_matches("lib.rs"));
        cased.pattern.options = Some(lsp_types::FileOperationPatternOptions {
            ignore_case: Some(true),
        });
        assert!(filtered(vec![cased]).will_rename_matches("lib.rs"));
    }

    #[test]
    fn will_rename_matches_consults_filters_only_up_to_the_bound() {
        let mut filters = vec![filter("**/*.py"); WILL_RENAME_FILTERS_MAX];
        filters.push(filter("**/*.rs"));
        assert!(
            !filtered(filters).will_rename_matches("lib.rs"),
            "the matching filter past the bound is never consulted"
        );
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
        let operations = workspace
            .file_operations
            .expect("file operations are offered");
        assert_eq!(operations.will_rename, Some(true));
        let text_document = offered
            .text_document
            .expect("text document capabilities are offered");
        assert!(
            text_document.references.is_some(),
            "the remove tools stand on textDocument/references being offered"
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
