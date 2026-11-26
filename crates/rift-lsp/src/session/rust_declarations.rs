//! Requests authored local binding tokens from the exact open Rust source.

use lsp_types::request::SemanticTokensFullRequest;
use lsp_types::{
    PartialResultParams, SemanticTokensParams, SemanticTokensResult, TextDocumentIdentifier,
    WorkDoneProgressParams,
};
use rift_core::{FileDigest, ProjectPath};
use rift_error::RiftError;

use super::EngineSession;
use crate::rust_declarations::{RustDeclarationTokens, RustTokenLegend};

impl EngineSession {
    /// Requests local declaration observations for the exact open Rust source.
    ///
    /// Returns no evidence when full tokens or the required legend are unavailable,
    /// the opened text is different, or the answer does not describe one valid
    /// non-overlapping token stream. The caller establishes readiness before
    /// consuming observations, as with other engine requests.
    ///
    /// # Errors
    /// Returns an error when resolving the document URI or exchanging the request fails.
    ///
    /// # Cancel safety
    /// A dropped request leaves its stale answer pending for the next exchange.
    pub async fn rust_declarations(
        &mut self,
        path: &ProjectPath,
        source: &str,
    ) -> Result<Option<RustDeclarationTokens>, RiftError> {
        let uri = self.document_uri(path)?;
        let Some(&Some(digest)) = self.open_documents.get(&uri) else {
            return Ok(None);
        };
        if digest != FileDigest::of(source.as_bytes()) {
            return Ok(None);
        }
        let Some(legend) = self.capabilities.semantic_tokens.clone() else {
            return Ok(None);
        };
        let Some(legend) = RustTokenLegend::new(&legend) else {
            return Ok(None);
        };
        let response = self
            .request::<SemanticTokensFullRequest>(SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await?;
        let Some(SemanticTokensResult::Tokens(tokens)) = response else {
            self.empty_answers.record(true);
            return Ok(None);
        };
        let converted = legend.convert(
            path.clone(),
            digest,
            source,
            self.capabilities.position_encoding,
            &tokens.data,
        );
        self.empty_answers.record(
            converted
                .as_ref()
                .is_none_or(|answer| answer.tokens().is_empty()),
        );
        Ok(converted)
    }
}
