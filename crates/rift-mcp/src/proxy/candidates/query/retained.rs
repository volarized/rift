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
pub(crate) struct Provider {
    pub(crate) backend: String,
    pub(crate) root: String,
    pub(crate) owner: SymbolOwner,
    pub(crate) view: CapturedView,
    pub(crate) selection: rift_search::query::QueryProviderSelection,
    pub(crate) symbols: Vec<(SymbolId, SymbolId)>,
    pub(crate) upstream: Vec<crate::server::candidates::QueryGlobalSelection>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub(crate) provider: usize,
    pub(crate) original: SourceUnitId,
    pub(crate) public: SourceUnitId,
    pub(crate) digest: Option<SourceDigest>,
    pub(crate) bytes: Option<std::ops::Range<usize>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub(crate) answer: SavedQuery,
    pub(crate) providers: Vec<Provider>,
    pub(crate) sources: Vec<Source>,
}

pub(crate) struct Input {
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
    pub(crate) fn prepare(
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
                upstream: std::mem::take(&mut selected.upstream),
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
        normalize_sources(&mut sources, &providers, &source_bytes, budget)?;
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

    pub(crate) fn include_view(&mut self, view: &CapturedView) -> Result<(), RiftError> {
        self.expires = self.expires.min(crate::proxy::captured_expiry(view)?);
        Ok(())
    }

    pub(crate) fn include_source(
        &mut self,
        request: &rift_protocol::source_read::GetSourceParams,
        result: &rift_protocol::source_read::GetSourceResult,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<(), RiftError> {
        let rift_protocol::source_read::GetSourceResult::Found {
            source,
            source_complete: true,
            ..
        } = result
        else {
            return Ok(());
        };
        if request.range.is_some() {
            return Ok(());
        }
        if !result.is_valid_for(request)
            || source.span.range.start != 0
            || usize::try_from(source.span.range.end).ok() != Some(source.text.len())
        {
            return Err(unavailable());
        }
        let index = self
            .sources
            .binary_search_by(|source| source.public.as_str().cmp(request.unit.as_str()))
            .map_err(|_| unavailable())?;
        let selected = &mut self.sources[index];
        // The hex encoder and checked SourceDigest each own 64 SHA-256 bytes.
        budget.charge_material(2 * 64)?;
        let digest = rift_search::query::source_text_digest(&source.text)?;
        let previous = selected
            .bytes
            .as_ref()
            .map(|range| self.source_bytes.get(range.clone()).ok_or_else(unavailable))
            .transpose()?;
        let previous = previous
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| unavailable())?;
        rift_search::query::require_source_agreement(
            selected.digest.as_ref(),
            previous,
            Some(&digest),
            Some(&source.text),
        )?;
        if selected.bytes.is_none() {
            budget.charge_bytes(source.text.as_bytes())?;
            let start = self.source_bytes.len();
            let end = start
                .checked_add(source.text.len())
                .filter(|end| *end <= self.maximum)
                .ok_or_else(unavailable)?;
            self.source_bytes
                .try_reserve_exact(source.text.len())
                .map_err(|_| unavailable())?;
            self.source_bytes.extend_from_slice(source.text.as_bytes());
            selected.bytes = Some(start..end);
        }
        selected.digest = Some(digest);
        Ok(())
    }

    pub(crate) fn original_context(&self) -> Result<(&str, &SymbolOwner), RiftError> {
        let provider = self.providers.first().ok_or_else(unavailable)?;
        Ok((&provider.root, &provider.owner))
    }

    pub(crate) fn capture(
        self,
        views: &rift_server::LocalSymbolViews,
        root: &Path,
        owner: &SymbolOwner,
        answer: &QueryAnswer,
        warnings: Vec<ReadWarning>,
        budget: &mut rift_server::QueryFactBudget<'_>,
    ) -> Result<QueryReadResult, RiftError> {
        self.capture_with(views, (root, owner), answer, warnings, budget, |_| Ok(true))
    }

    pub(crate) fn capture_with(
        self,
        views: &rift_server::LocalSymbolViews,
        (root, owner): (&Path, &SymbolOwner),
        answer: &QueryAnswer,
        warnings: Vec<ReadWarning>,
        budget: &mut rift_server::QueryFactBudget<'_>,
        admit: impl FnOnce(&CapturedView) -> Result<bool, RiftError>,
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
            if !admit(view)? {
                return Ok(false);
            }
            response = Some(first);
            Ok(true)
        })?;
        if admitted.is_none() {
            return Err(unavailable());
        }
        response.ok_or_else(unavailable)
    }
}

pub(crate) fn provider_keys(
    providers: &[Provider],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Vec<SourceDigest>, RiftError> {
    budget.charge_material(
        providers
            .len()
            .checked_mul(size_of::<SourceDigest>())
            .ok_or_else(unavailable)?,
    )?;
    let mut keys = Vec::new();
    keys.try_reserve_exact(providers.len())
        .map_err(|_| unavailable())?;
    for provider in providers {
        budget.check()?;
        let value = (
            &provider.backend,
            &provider.root,
            &provider.owner,
            &provider.selection,
            &provider.upstream,
        );
        budget.charge(&value)?;
        let encoded = crate::proxy::encode_bounded(&value, super::super::CANDIDATE_BYTES_MAX)
            .map_err(encoding_failure)?;
        let digest = rift_core::FileDigest::of(&encoded);
        budget.charge_material(
            digest
                .as_bytes()
                .len()
                .checked_mul(4)
                .ok_or_else(unavailable)?,
        )?;
        keys.push(SourceDigest::parse(
            &data_encoding::HEXLOWER.encode(digest.as_bytes()),
        )?);
    }
    Ok(keys)
}

fn normalize_sources(
    sources: &mut Vec<Source>,
    providers: &[Provider],
    bytes: &[u8],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let keys = provider_keys(providers, budget)?;
    budget.check()?;
    sources.sort_unstable_by(|left, right| left.public.as_str().cmp(right.public.as_str()));
    budget.check()?;
    let mut refused = None;
    sources.dedup_by(|added, original| {
        if refused.is_some() || added.public != original.public {
            return false;
        }
        let validate = |budget: &mut rift_server::QueryFactBudget<'_>| -> Result<(), RiftError> {
            budget.check()?;
            if added.original != original.original
                || keys
                    .get(added.provider)
                    .zip(keys.get(original.provider))
                    .is_none_or(|(left, right)| left != right)
            {
                return Err(unavailable());
            }
            let text = |range: &Option<std::ops::Range<usize>>| -> Result<Option<&str>, RiftError> {
                range
                    .as_ref()
                    .map(|range| {
                        bytes
                            .get(range.clone())
                            .ok_or_else(unavailable)
                            .and_then(|bytes| std::str::from_utf8(bytes).map_err(|_| unavailable()))
                    })
                    .transpose()
            };
            rift_search::query::require_source_agreement(
                original.digest.as_ref(),
                text(&original.bytes)?,
                added.digest.as_ref(),
                text(&added.bytes)?,
            )
        };
        if let Err(error) = validate(budget) {
            refused = Some(error);
            return false;
        }
        if original.digest.is_none() {
            original.digest = added.digest.take();
        }
        if original.bytes.is_none() {
            original.bytes = added.bytes.take();
        }
        true
    });
    if let Some(error) = refused {
        return Err(error);
    }
    Ok(())
}

pub(crate) fn metadata(captured: &rift_server::CapturedQueryView) -> Result<Metadata, RiftError> {
    let mut metadata: Metadata =
        serde_json::from_slice(captured.material().metadata()).map_err(encoding_failure)?;
    metadata.answer = metadata.answer.at_view(captured.view().clone());
    Ok(metadata)
}

pub(crate) fn unavailable() -> RiftError {
    errors::server::read_unavailable()
        .operation("query retention")
        .detail("retained query material is unavailable; repeat the original query")
        .error()
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
