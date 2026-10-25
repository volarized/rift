//! Recorded symbol timelines from one original captured workspace and store snapshot.

use rift_error::{RiftError, errors};
use rift_protocol::read::SymbolId;
use rift_search::query::{
    FactRequirement, FactRequirements, QueryFactFamily, QueryFacts, QueryLimits,
};

use crate::QueryFactBudget;
use crate::history::SymbolTimelines;
use crate::read::{LocalSymbolViewInput, ReadService};

impl ReadService {
    /// Adds recorded timelines for the selected canonical symbol catalog.
    ///
    /// Missing or incomplete store facts keep the history family unavailable. The read
    /// uses one recorded snapshot and never requests a fill or analyzes source.
    ///
    /// # Errors
    /// Refuses another captured workspace or revision, invalid recorded facts,
    /// cancellation, and exhausted request admission.
    pub fn query_histories(
        &self,
        selected: &LocalSymbolViewInput,
        requirements: &FactRequirements,
        facts: &mut QueryFacts,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
    ) -> Result<(), RiftError> {
        require_foreign_histories(selected.owner(), requirements, facts);
        if !requirements.requires(FactRequirement::History)
            || !facts
                .unavailable_families
                .contains(&QueryFactFamily::History)
            || (!requirements.requires(FactRequirement::AllHistories)
                && requirements.history_symbols.is_empty())
        {
            return Ok(());
        }
        budget.check()?;
        let original = self.index().symbol_view()?;
        if self.index().root() != selected.root()
            || self.index().owner() != selected.owner()
            || original.fingerprint() != selected.index().fingerprint()
            || self.revision() != selected.captured_revision()?.as_ref()
        {
            return errors::server::read_unavailable()
                .operation("query history")
                .detail(
                    "captured history selection differs from its original workspace or revision",
                )
                .fail();
        }
        let Some(store) = self.stored_history() else {
            return Ok(());
        };
        if !self.history_configuration().enabled {
            return Ok(());
        }
        let timelines = SymbolTimelines::open_recorded(
            self.index().root(),
            self.revision(),
            self.history_configuration(),
            self.workspace_limits().syntax(),
            store,
        )?;
        let all = requirements.requires(FactRequirement::AllHistories);
        let mut complete = requirements.history_symbols.iter().all(|identity| {
            rift_protocol::identity::SymbolIdentity::parse(identity.as_str())
                .is_ok_and(|identity| identity.owner() == selected.owner())
        });
        if all {
            complete &= facts.symbols.iter().all(|record| {
                record.symbol.id.as_ref().is_none_or(|identity| {
                    rift_protocol::identity::SymbolIdentity::parse(identity.as_str())
                        .is_ok_and(|identity| identity.owner() == selected.owner())
                        || facts
                            .histories
                            .iter()
                            .any(|history| history.symbol == *identity && history.complete)
                })
            });
        }
        for (identity, object) in selected.index().objects() {
            budget.check()?;
            if !all
                && !requirements
                    .history_symbols
                    .iter()
                    .any(|wanted| wanted.as_str() == identity.as_str())
            {
                continue;
            }
            object?;
            if facts
                .histories
                .iter()
                .any(|history| history.symbol.as_str() == identity.as_str() && history.complete)
            {
                continue;
            }
            let (bindings, _) = selected.index().symbols_by_identity_page(identity, 0, 1)?;
            let Some(matched) = bindings.first().copied() else {
                continue;
            };
            if facts.histories.len() >= limits.quads_max {
                return errors::server::read_unavailable()
                    .operation("query history")
                    .detail("query exceeds search.query.statements")
                    .fail();
            }
            budget.charge(&identity.as_str())?;
            let history = timelines.recorded_timeline(
                matched,
                SymbolId(identity.as_str().to_owned()),
                budget,
            )?;
            complete &= history.complete;
            facts.histories.retain(|held| held.symbol != history.symbol);
            facts.histories.push(history);
        }
        if complete {
            facts
                .unavailable_families
                .retain(|family| *family != QueryFactFamily::History);
        }
        Ok(())
    }
}

pub(super) fn require_foreign_histories(
    owner: &rift_protocol::identity::SymbolOwner,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
) {
    if !requirements.requires(FactRequirement::History) {
        return;
    }
    let missing = |id: &SymbolId| {
        rift_protocol::identity::SymbolIdentity::parse(id.as_str())
            .is_ok_and(|identity| identity.owner() != owner)
            && !facts
                .histories
                .iter()
                .any(|history| history.symbol == *id && history.complete)
    };
    let unavailable = if requirements.requires(FactRequirement::AllHistories) {
        facts
            .symbols
            .iter()
            .filter_map(|record| record.symbol.id.as_ref())
            .any(missing)
    } else {
        requirements.history_symbols.iter().any(missing)
    };
    if unavailable
        && !facts
            .unavailable_families
            .contains(&QueryFactFamily::History)
    {
        facts.unavailable_families.push(QueryFactFamily::History);
    }
}
