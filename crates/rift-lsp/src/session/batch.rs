//! Bounded groups of LSP requests sharing one session read loop.

use std::collections::BTreeMap;

use lsp_types::request::{CallHierarchyOutgoingCalls, CallHierarchyPrepare, References, Request};
use lsp_types::{
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, Location, PartialResultParams, Position, ReferenceContext,
    ReferenceParams, WorkDoneProgressParams,
};
use rift_core::ProjectPath;
use rift_error::{RiftError, errors};
use serde_json::Value;
use tokio::time::Instant;

use super::{EngineSession, correlation, ends_session, require};

impl EngineSession {
    /// Resolves source positions for a bounded group of declaration positions.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn definition_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<Location>>, RiftError> {
        use lsp_types::GotoDefinitionParams;
        use lsp_types::request::GotoDefinition;
        require(
            self.capabilities.definitions.definition,
            GotoDefinition::METHOD,
        )?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(GotoDefinitionParams {
                    text_document_position_params: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<GotoDefinition>(params).await?;
        Ok(answers
            .into_iter()
            .map(super::edges::definition_locations)
            .collect())
    }

    /// Resolves inferred types for a bounded group of declaration positions.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn type_definition_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<Location>>, RiftError> {
        use lsp_types::request::{GotoTypeDefinition, GotoTypeDefinitionParams};
        require(
            self.capabilities.definitions.type_definition,
            GotoTypeDefinition::METHOD,
        )?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(GotoTypeDefinitionParams {
                    text_document_position_params: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<GotoTypeDefinition>(params).await?;
        Ok(answers
            .into_iter()
            .map(super::edges::definition_locations)
            .collect())
    }

    /// Reads references for every position, preserving request order across reversed responses.
    ///
    /// # Errors
    /// Refuses an absent capability, a group above the pending bound, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn references_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<Location>>, RiftError> {
        require(self.capabilities.references, References::METHOD)?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(ReferenceParams {
                    text_document_position: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<References>(params).await?;
        Ok(answers.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Prepares call hierarchy items for a bounded group of declaration positions.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn prepare_call_hierarchy_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<CallHierarchyItem>>, RiftError> {
        require(
            self.capabilities.call_hierarchy,
            CallHierarchyPrepare::METHOD,
        )?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(CallHierarchyPrepareParams {
                    text_document_position_params: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<CallHierarchyPrepare>(params).await?;
        Ok(answers.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Reads outgoing calls for a bounded group of prepared hierarchy items.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn outgoing_calls_batch(
        &mut self,
        items: Vec<CallHierarchyItem>,
    ) -> Result<Vec<Vec<CallHierarchyOutgoingCall>>, RiftError> {
        require(
            self.capabilities.call_hierarchy,
            CallHierarchyOutgoingCalls::METHOD,
        )?;
        let params = items
            .into_iter()
            .map(|item| CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .collect();
        let answers = self
            .request_batch::<CallHierarchyOutgoingCalls>(params)
            .await?;
        Ok(answers.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Prepares type hierarchy items for a bounded group of declaration positions.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn prepare_type_hierarchy_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<lsp_types::TypeHierarchyItem>>, RiftError> {
        use lsp_types::request::TypeHierarchyPrepare;
        require(
            self.capabilities.edges.type_hierarchy,
            TypeHierarchyPrepare::METHOD,
        )?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(lsp_types::TypeHierarchyPrepareParams {
                    text_document_position_params: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<TypeHierarchyPrepare>(params).await?;
        Ok(answers.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Reads supertypes for a bounded group of prepared hierarchy items.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn supertype_edges_batch(
        &mut self,
        items: Vec<lsp_types::TypeHierarchyItem>,
    ) -> Result<Vec<Vec<lsp_types::TypeHierarchyItem>>, RiftError> {
        use lsp_types::request::TypeHierarchySupertypes;
        require(
            self.capabilities.edges.type_hierarchy,
            TypeHierarchySupertypes::METHOD,
        )?;
        let params = items
            .into_iter()
            .map(|item| lsp_types::TypeHierarchySupertypesParams {
                item,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .collect();
        let answers = self
            .request_batch::<TypeHierarchySupertypes>(params)
            .await?;
        Ok(answers.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Reads implementations for a bounded group of declaration positions.
    ///
    /// # Errors
    /// Refuses an absent capability, an oversized group, or an exchange failure.
    ///
    /// # Cancel safety
    /// Dropping the future leaves pending responses for the next exchange to discard.
    pub async fn implementation_batch(
        &mut self,
        positions: &[(ProjectPath, Position)],
    ) -> Result<Vec<Vec<Location>>, RiftError> {
        use lsp_types::request::{GotoImplementation, GotoImplementationParams};
        require(
            self.capabilities.edges.implementation,
            GotoImplementation::METHOD,
        )?;
        let params = positions
            .iter()
            .map(|(path, position)| {
                Ok(GotoImplementationParams {
                    text_document_position_params: self.position_params(path, *position)?,
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let answers = self.request_batch::<GotoImplementation>(params).await?;
        Ok(answers
            .into_iter()
            .map(super::edges::definition_locations)
            .collect())
    }

    pub(super) async fn request_batch<R: Request>(
        &mut self,
        params: Vec<R::Params>,
    ) -> Result<Vec<R::Result>, RiftError> {
        self.refuse_ended()?;
        let maximum = correlation::PENDING_REQUESTS_MAX;
        if params.len() > maximum {
            return errors::lsp::correlation_pending_requests_exceeded()
                .field("correlation.pending_requests_max")
                .limit(maximum)
                .required(params.len())
                .fail();
        }
        self.empty_answers.forget();
        if self.deadline_passed() {
            return errors::lsp::engine_analyzing().attempts(1_u64).fail();
        }
        let timeout = self.request_timeout;
        let (end, deadline_cut) = self.wait_end(Instant::now(), timeout);
        let answer = match tokio::time::timeout_at(end, self.exchange_batch::<R>(params)).await {
            Ok(Err(error)) if error.slug() == errors::lsp::engine_analyzing::SLUG => {
                // The deadline passed between two frames: nothing waits for the frames
                // already written.
                Err(self.unanswered(R::METHOD, timeout, true).await)
            }
            Ok(answer) => answer,
            Err(_elapsed) => Err(self.unanswered(R::METHOD, timeout, deadline_cut).await),
        };
        if answer.as_ref().is_err_and(ends_session) {
            self.end().await;
        }
        answer
    }

    /// Discards cancelled responses only when the next exchange needs their slots.
    pub(super) async fn make_room(
        &mut self,
        required: usize,
        method: &'static str,
    ) -> Result<(), RiftError> {
        while self.correlation.pending_count().saturating_add(required)
            > correlation::PENDING_REQUESTS_MAX
        {
            let payload = self.next_payload(method).await?;
            let incoming = correlation::classify(&payload)
                .ok_or_else(|| errors::lsp::engine_message_unreadable().error())?;
            if let Some(request) = incoming.method {
                self.route_unrequested(&request, incoming.id, incoming.params, method)
                    .await?;
            } else {
                self.correlation
                    .conclude(incoming.id.as_ref().unwrap_or(&Value::Null))?;
            }
        }
        Ok(())
    }

    async fn exchange_batch<R: Request>(
        &mut self,
        params: Vec<R::Params>,
    ) -> Result<Vec<R::Result>, RiftError> {
        self.make_room(params.len(), R::METHOD).await?;
        let mut pending = BTreeMap::new();
        for (position, params) in params.into_iter().enumerate() {
            if position > 0 && self.deadline_passed() {
                return errors::lsp::engine_analyzing().attempts(1_u64).fail();
            }
            let value = serde_json::to_value(params).map_err(|source| {
                errors::lsp::engine_result_invalid()
                    .method(R::METHOD)
                    .source(source)
                    .error()
            })?;
            let id = self.correlation.begin(R::METHOD)?;
            pending.insert(id.value(), position);
            self.write_payload(correlation::request(id, R::METHOD, &value), R::METHOD)
                .await?;
        }
        let mut answers = BTreeMap::new();
        let mut failure = None;
        while !pending.is_empty() {
            let payload = self.next_payload(R::METHOD).await?;
            let incoming = correlation::classify(&payload)
                .ok_or_else(|| errors::lsp::engine_message_unreadable().error())?;
            if let Some(method) = incoming.method {
                self.route_unrequested(&method, incoming.id, incoming.params, R::METHOD)
                    .await?;
                continue;
            }
            let id = incoming.id.as_ref().unwrap_or(&Value::Null);
            let method = self.correlation.conclude(id)?;
            let Some(position) = id.as_u64().and_then(|id| pending.remove(&id)) else {
                continue;
            };
            match super::response_result::<R>(method, incoming) {
                Ok(answer) => {
                    answers.insert(position, answer);
                }
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(answers.into_values().collect()),
        }
    }
}
