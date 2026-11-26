//! The capability record negotiated with one engine at initialize.
//!
//! The session offers UTF-8 positions first with UTF-16 as the mandatory
//! fallback, and records which operations the engine advertised. Typed
//! operations consult this record before sending anything.

use lsp_types::{
    CallHierarchyClientCapabilities, CallHierarchyServerCapability, ClientCapabilities,
    DiagnosticClientCapabilities, DiagnosticServerCapabilities,
    DiagnosticWorkspaceClientCapabilities, DidChangeWatchedFilesClientCapabilities,
    GeneralClientCapabilities, InitializeResult, OneOf, PositionEncodingKind,
    ReferenceClientCapabilities, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensServerCapabilities, TextDocumentClientCapabilities, WindowClientCapabilities,
    WorkspaceClientCapabilities,
};
use rift_error::{RiftError, errors};

/// How one byte offset maps to an LSP `character` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionEncoding {
    /// Characters count UTF-8 bytes.
    Utf8,
    /// Characters count UTF-16 code units, the protocol default.
    Utf16,
}

/// Advertised engine operations that supply inheritance edges.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EdgeCapabilities {
    /// Whether the engine advertises implementation edges.
    pub implementation: bool,
    /// Whether the engine advertises type hierarchy edges.
    pub type_hierarchy: bool,
}

/// Advertised declaration and inferred type definition operations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DefinitionCapabilities {
    /// Whether the engine resolves source positions to declarations.
    pub definition: bool,
    /// Whether the engine resolves a symbol's inferred type to its declaration.
    pub type_definition: bool,
}

/// What one engine advertised at initialize.
#[derive(Clone, Debug, PartialEq)]
pub struct Capabilities {
    /// The negotiated position encoding.
    pub position_encoding: PositionEncoding,
    /// Whether the engine serves `textDocument/references`.
    pub references: bool,
    /// Whether the engine serves `textDocument/prepareCallHierarchy` and
    /// `callHierarchy/outgoingCalls`.
    pub call_hierarchy: bool,
    /// Advertised declaration and inferred type definition operations.
    pub definitions: DefinitionCapabilities,
    /// Advertised implementation and type hierarchy operations.
    pub edges: EdgeCapabilities,
    /// Whether the engine serves `textDocument/diagnostic`.
    pub pull_diagnostics: bool,
    /// Legend negotiated for complete semantic-token answers.
    pub semantic_tokens: Option<SemanticTokensLegend>,
    /// The identifier the engine registered its diagnostics under.
    pub diagnostic_identifier: Option<String>,
}

impl Default for Capabilities {
    /// The protocol defaults before negotiation: UTF-16, nothing served.
    fn default() -> Self {
        Self {
            position_encoding: PositionEncoding::Utf16,
            references: false,
            call_hierarchy: false,
            definitions: DefinitionCapabilities::default(),
            edges: EdgeCapabilities::default(),
            pull_diagnostics: false,
            semantic_tokens: None,
            diagnostic_identifier: None,
        }
    }
}

impl Capabilities {
    /// Retains the type hierarchy capability missing from the dependency's initialize model.
    pub(crate) fn negotiated_edge_answer(answer: serde_json::Value) -> Result<Self, RiftError> {
        let edge_type_hierarchy = answer
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("typeHierarchyProvider"))
            .is_some_and(|value| value.as_bool() == Some(true) || value.is_object());
        let answer = serde_json::from_value(answer).map_err(|source| {
            errors::lsp::engine_result_invalid()
                .method("initialize")
                .source(source)
                .error()
        })?;
        let mut capabilities = Self::negotiated(&answer)?;
        capabilities.edges.type_hierarchy = edge_type_hierarchy;
        Ok(capabilities)
    }
    /// Builds the record from an engine's initialize answer.
    ///
    /// An absent position encoding is UTF-16, the protocol default.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the engine picked an encoding the
    /// session never offered.
    pub fn negotiated(answer: &InitializeResult) -> Result<Self, RiftError> {
        let advertised = &answer.capabilities;
        let position_encoding = match advertised.position_encoding.as_ref() {
            None => PositionEncoding::Utf16,
            Some(kind) if *kind == PositionEncodingKind::UTF8 => PositionEncoding::Utf8,
            Some(kind) if *kind == PositionEncodingKind::UTF16 => PositionEncoding::Utf16,
            Some(kind) => {
                return errors::lsp::capabilities_position_encoding_unsupported()
                    .encoding(kind.as_str())
                    .fail();
            }
        };
        let references = match advertised.references_provider.as_ref() {
            Some(OneOf::Left(served)) => *served,
            Some(OneOf::Right(_options)) => true,
            None => false,
        };
        let call_hierarchy = match advertised.call_hierarchy_provider {
            Some(CallHierarchyServerCapability::Simple(served)) => served,
            Some(CallHierarchyServerCapability::Options(_options)) => true,
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
        let semantic_tokens = advertised
            .semantic_tokens_provider
            .as_ref()
            .and_then(|provider| {
                let options = match provider {
                    SemanticTokensServerCapabilities::SemanticTokensOptions(options) => options,
                    SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                        registration,
                    ) => &registration.semantic_tokens_options,
                };
                match options.full {
                    Some(
                        SemanticTokensFullOptions::Bool(true)
                        | SemanticTokensFullOptions::Delta { .. },
                    ) => Some(options.legend.clone()),
                    Some(SemanticTokensFullOptions::Bool(false)) | None => None,
                }
            });
        Ok(Self {
            position_encoding,
            references,
            call_hierarchy,
            definitions: DefinitionCapabilities {
                definition: match advertised.definition_provider {
                    Some(OneOf::Left(served)) => served,
                    Some(OneOf::Right(_)) => true,
                    None => false,
                },
                type_definition: match advertised.type_definition_provider {
                    Some(lsp_types::TypeDefinitionProviderCapability::Simple(served)) => served,
                    Some(lsp_types::TypeDefinitionProviderCapability::Options(_)) => true,
                    None => false,
                },
            },
            edges: EdgeCapabilities {
                implementation: match advertised.implementation_provider {
                    Some(lsp_types::ImplementationProviderCapability::Simple(served)) => served,
                    Some(lsp_types::ImplementationProviderCapability::Options(_)) => true,
                    None => false,
                },
                type_hierarchy: false,
            },
            pull_diagnostics,
            semantic_tokens,
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
/// call hierarchy, and document diagnostic pulls.
///
/// `textDocument.callHierarchy` is what makes typescript-language-server
/// advertise `callHierarchyProvider` at all: 6.0.0 declares it only when the
/// client offers it.
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
            implementation: Some(lsp_types::GotoCapability::default()),
            type_definition: Some(lsp_types::GotoCapability::default()),
            definition: Some(lsp_types::GotoCapability::default()),
            type_hierarchy: Some(lsp_types::TypeHierarchyClientCapabilities::default()),
            call_hierarchy: Some(CallHierarchyClientCapabilities::default()),
            semantic_tokens: Some(crate::rust_declarations::offered()),
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
    fn test_edge_capability_forms_are_gated_by_the_advertised_flags() {
        for (value, expected) in [
            (serde_json::Value::Null, false),
            (serde_json::json!(false), false),
            (serde_json::json!(true), true),
            (serde_json::json!({}), true),
        ] {
            let answer = serde_json::json!({"capabilities":{"implementationProvider":value,"typeHierarchyProvider":value}});
            let negotiated =
                Capabilities::negotiated_edge_answer(answer).expect("edge capabilities");
            assert_eq!(negotiated.edges.implementation, expected);
            assert_eq!(negotiated.edges.type_hierarchy, expected);
        }
        let offered = offered().text_document.expect("text document capabilities");
        assert!(offered.implementation.is_some());
        assert!(offered.type_hierarchy.is_some());
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
    fn call_hierarchy_forms_map_to_the_call_hierarchy_flag() {
        let negotiated = |provider| {
            Capabilities::negotiated(&answer(ServerCapabilities {
                call_hierarchy_provider: provider,
                ..ServerCapabilities::default()
            }))
            .expect("record")
            .call_hierarchy
        };
        assert!(!negotiated(None));
        assert!(!negotiated(Some(CallHierarchyServerCapability::Simple(
            false
        ))));
        assert!(negotiated(Some(CallHierarchyServerCapability::Simple(
            true
        ))));
        assert!(negotiated(Some(CallHierarchyServerCapability::Options(
            lsp_types::CallHierarchyOptions::default()
        ))));
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
            error.slug(),
            errors::lsp::capabilities_position_encoding_unsupported::SLUG
        );
        assert_eq!(
            error
                .context()
                .find(|(key, _)| *key == "encoding")
                .expect("unsupported encoding evidence is present")
                .1,
            "utf-32"
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
        assert!(
            text_document.call_hierarchy.is_some(),
            "typescript-language-server advertises call hierarchy only when offered it"
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
