//! Query answers and original read contexts use one captured-material owner.

use std::path::Path;
use std::time::SystemTime;

use rift_error::{RiftError, errors};
use rift_protocol::identity::{SourceDigest, SymbolOwner};
use rift_protocol::query::{QueryAnswer, QueryConfiguration, QueryReadResult};
use rift_protocol::read::{ReadWarning, SourceUnitId, SymbolId};
use rift_protocol::symbol_read::CapturedView;
use rift_search::sparql::QueryIdentities;
use serde::{Deserialize, Serialize};

use crate::server::QueryFactSelection;
use crate::server::candidates::{SavedQuery, SavedSymbol};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Provider {
    pub(super) backend: String,
    pub(super) root: String,
    pub(super) owner: SymbolOwner,
    pub(super) view: CapturedView,
    pub(super) selection: rift_search::query::QueryProviderSelection,
    pub(super) symbols: Vec<(SymbolId, SymbolId)>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    pub(super) provider: usize,
    pub(super) original: SourceUnitId,
    pub(super) public: SourceUnitId,
    pub(super) digest: Option<SourceDigest>,
    pub(super) bytes: Option<std::ops::Range<usize>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub(super) answer: SavedQuery,
    pub(super) providers: Vec<Provider>,
    pub(super) sources: Vec<Source>,
}

pub(super) struct Input {
    providers: Vec<Provider>,
    sources: Vec<Source>,
    facts: Vec<u8>,
    source_bytes: Vec<u8>,
    expires: SystemTime,
    original_view: CapturedView,
    configuration: QueryConfiguration,
    maximum: usize,
}

impl Input {
    pub(super) fn prepare(
        mut selections: Vec<(String, QueryFactSelection, QueryIdentities)>,
        configuration: QueryConfiguration,
        maximum: usize,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<Self, RiftError> {
        let original_view = selections
            .first()
            .map(|(_, selection, _)| selection.view.clone())
            .ok_or_else(unavailable)?;
        let mut expires = crate::proxy::captured_expiry(&original_view)?;
        let mut providers = Vec::new();
        let mut sources = Vec::new();
        let mut source_bytes = Vec::new();
        for (backend, selected, identities) in &mut selections {
            budget.check()?;
            expires = expires.min(crate::proxy::captured_expiry(&selected.view)?);
            let provider = providers.len();
            providers.push(Provider {
                backend: backend.clone(),
                root: selected.root.clone(),
                owner: selected.owner.clone(),
                view: selected.view.clone(),
                selection: selected.selection.clone(),
                symbols: std::mem::take(&mut identities.symbols),
            });
            for source in &mut selected.facts.sources {
                budget.check()?;
                let digest = rift_search::query::source_digest(source)?;
                let bytes = if let Some(text) = source.text.take() {
                    budget.charge_bytes(text.as_bytes())?;
                    let start = source_bytes.len();
                    let end = start
                        .checked_add(text.len())
                        .filter(|end| *end <= maximum)
                        .ok_or_else(unavailable)?;
                    source_bytes.extend_from_slice(text.as_bytes());
                    Some(start..end)
                } else {
                    None
                };
                let public = identities
                    .sources
                    .iter()
                    .find(|(original, _)| original == &source.unit)
                    .map_or_else(|| source.unit.clone(), |(_, public)| public.clone());
                sources.push(Source {
                    provider,
                    original: source.unit.clone(),
                    public,
                    digest,
                    bytes,
                });
            }
            for (original, public) in &identities.sources {
                if !sources
                    .iter()
                    .any(|source| source.provider == provider && source.original == *original)
                {
                    sources.push(Source {
                        provider,
                        original: original.clone(),
                        public: public.clone(),
                        digest: None,
                        bytes: None,
                    });
                }
            }
        }
        budget.charge(&providers)?;
        budget.charge(&sources)?;
        budget.charge(&selections)?;
        let facts = crate::proxy::encode_bounded(&selections, maximum).map_err(encoding_failure)?;
        Ok(Self {
            providers,
            sources,
            facts,
            source_bytes,
            expires,
            original_view,
            configuration,
            maximum,
        })
    }

    pub(super) fn capture(
        self,
        views: &rift_server::LocalSymbolViews,
        root: &Path,
        owner: &SymbolOwner,
        answer: &QueryAnswer,
        warnings: Vec<ReadWarning>,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<QueryReadResult, RiftError> {
        budget.check()?;
        let context = SavedSymbol {
            root: root.to_str().ok_or_else(unavailable)?.to_owned(),
            owner: owner.clone(),
            view: self.original_view,
        };
        let (saved, records, _) =
            SavedQuery::encoded(context, answer, &self.configuration, warnings, Vec::new())?;
        let metadata = Metadata {
            answer: saved,
            providers: self.providers,
            sources: self.sources,
        };
        budget.charge(&metadata)?;
        let metadata_bytes =
            crate::proxy::encode_bounded(&metadata, super::super::CANDIDATE_BYTES_MAX)
                .map_err(encoding_failure)?;
        budget.charge_bytes(&records)?;
        let page_records = records.clone();
        let mut saved = metadata.answer;
        let material =
            rift_server::QueryViewInput::new(root.to_path_buf(), owner.clone(), self.expires)?
                .with_records(records)
                .with_metadata(metadata_bytes)
                .with_facts(self.facts)
                .with_sources(self.source_bytes);
        let mut response = None;
        let admitted = views.capture_query_with(material, |view| {
            saved.context.view = view.clone();
            let page_maximum = usize::try_from(self.configuration.page_limit.bytes())
                .map_err(|_| unavailable())?;
            saved.validate_pages(&page_records, page_maximum.min(self.maximum), || {
                budget.check()
            })?;
            let first = saved.response(saved.decode_bytes(&page_records, Some(0))?, 0)?;
            budget.charge(&first)?;
            response = Some(first);
            Ok(true)
        })?;
        if admitted.is_none() {
            return Err(unavailable());
        }
        response.ok_or_else(unavailable)
    }
}

pub(super) fn metadata(captured: &rift_server::CapturedQueryView) -> Result<Metadata, RiftError> {
    let mut metadata: Metadata =
        serde_json::from_slice(captured.material().metadata()).map_err(encoding_failure)?;
    metadata.answer = metadata.answer.at_view(captured.view().clone());
    Ok(metadata)
}

pub(super) fn unavailable() -> RiftError {
    errors::server::read_unavailable()
        .operation("query retention")
        .detail("retained query material is unavailable; repeat the original query")
        .error()
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
