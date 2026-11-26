//! Expands checked names in an open Rust source through the existing request transport.

use lsp_types::TextDocumentPositionParams;
use lsp_types::request::Request;
use rift_core::{FileDigest, ProjectPath};
use rift_error::RiftError;

use super::EngineSession;

/// One Rust analyzer expansion response, separate from its authored source.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RustMacroExpansion {
    /// Selected macro name.
    pub name: String,
    /// Bounded engine response text used to inspect the emitted declaration shapes.
    pub expansion: String,
}

enum ExpandRustMacro {}

impl Request for ExpandRustMacro {
    type Params = TextDocumentPositionParams;
    type Result = Option<RustMacroExpansion>;
    const METHOD: &'static str = "rust-analyzer/expandMacro";
}

impl EngineSession {
    /// Inspects expansions at positions in the exact open Rust document.
    ///
    /// The caller establishes engine readiness and keeps the result separate from the
    /// original file. A missing or different opened document provides no evidence.
    /// # Errors
    /// Returns a typed error for invalid positions, batch bounds or exchange failures.
    /// # Cancel safety
    /// Responses to an interrupted batch are settled by the next exchange.
    pub async fn rust_macro_expansions(
        &mut self,
        path: &ProjectPath,
        digest: FileDigest,
        positions: &[lsp_types::Position],
    ) -> Result<Option<Vec<Option<RustMacroExpansion>>>, RiftError> {
        let uri = self.document_uri(path)?;
        if self.open_documents.get(&uri) != Some(&Some(digest)) {
            return Ok(None);
        }
        let params = positions
            .iter()
            .map(|position| self.position_params(path, *position))
            .collect::<Result<Vec<_>, RiftError>>()?;
        self.request_batch::<ExpandRustMacro>(params)
            .await
            .map(Some)
    }
}
