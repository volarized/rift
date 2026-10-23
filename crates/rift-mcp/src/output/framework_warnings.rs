//! Framework warnings already shown by one MCP proxy connection.

use std::collections::BTreeSet;
use std::sync::Mutex;

use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{ReadWarning, SyntaxFramework};
use rift_protocol::symbol_read::GetSymbolResult;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize as _, Serialize};
use sha2::{Digest as _, Sha256};

use super::render::text_of;

/// Owner/framework observations held by one connection, at most.
const FRAMEWORK_WARNINGS_MAX: usize = 256;

/// Clones of a proxy share this state; upstream reconnects keep it.
#[derive(Debug)]
pub(crate) struct FrameworkWarnings {
    shown: Mutex<Vec<ShownFramework>>,
}

#[derive(Debug)]
struct ShownFramework {
    owner: [u8; 32],
    framework: SyntaxFramework,
    facts: [u8; 32],
}

impl ShownFramework {
    fn same_framework(&self, other: &Self) -> bool {
        self.owner == other.owner && self.framework == other.framework
    }
}

impl Default for FrameworkWarnings {
    fn default() -> Self {
        Self {
            shown: Mutex::new(Vec::with_capacity(FRAMEWORK_WARNINGS_MAX)),
        }
    }
}

impl FrameworkWarnings {
    /// Renders a copy of a canonical symbol answer, leaving its captured JSON unchanged.
    /// Other tool results and request-specific warnings pass through.
    pub(crate) fn select_result(
        &self,
        mut result: CallToolResult,
        project: &str,
    ) -> CallToolResult {
        let Some(value) = result.structured_content.as_ref() else {
            return result;
        };
        let Ok(mut answer) = GetSymbolResult::deserialize(value) else {
            return result;
        };
        let (id, warnings) = match &mut answer {
            GetSymbolResult::Found {
                symbol, warnings, ..
            } => (symbol.id.as_ref(), warnings),
            GetSymbolResult::Missing {
                symbol_not_found,
                warnings,
                ..
            } => (Some(&symbol_not_found.id), warnings),
            GetSymbolResult::Unavailable { id, warnings, .. } => (Some(&*id), warnings),
        };
        let Some(identity) = id.and_then(|id| SymbolIdentity::parse(id.as_str()).ok()) else {
            return result;
        };
        let Ok(owner) = digest(&(project, identity.owner())) else {
            return result;
        };
        let Ok(observations) = observations(owner, warnings) else {
            return result;
        };
        let mut shown = self
            .shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hidden: Vec<_> = observations
            .iter()
            .filter(|observation| {
                shown.iter().any(|previous| {
                    previous.same_framework(observation) && previous.facts == observation.facts
                })
            })
            .map(|observation| observation.framework)
            .collect();
        if !hidden.is_empty() {
            warnings.retain(|warning| {
                !matches!(warning, ReadWarning::FrameworkContextUnresolved { framework, .. }
                    if hidden.contains(framework))
            });
            let Ok(text) = text_of(&answer) else {
                return result;
            };
            result.content = vec![ContentBlock::text(text)];
        }
        shown.retain(|previous| {
            previous.owner != owner
                || observations
                    .iter()
                    .any(|observation| previous.same_framework(observation))
        });
        for observation in observations {
            if let Some(previous) = shown
                .iter_mut()
                .find(|previous| previous.same_framework(&observation))
            {
                *previous = observation;
            } else {
                if shown.len() == FRAMEWORK_WARNINGS_MAX {
                    shown.remove(0);
                }
                shown.push(observation);
            }
        }
        result
    }
}

fn observations(
    owner: [u8; 32],
    warnings: &[ReadWarning],
) -> Result<Vec<ShownFramework>, serde_json::Error> {
    let mut groups: Vec<(SyntaxFramework, BTreeSet<(String, String)>)> = Vec::new();
    for warning in warnings {
        if let ReadWarning::FrameworkContextUnresolved {
            unit,
            framework,
            detail,
        } = warning
        {
            let index = groups
                .iter()
                .position(|(selected, _)| selected == framework)
                .unwrap_or_else(|| {
                    groups.push((*framework, BTreeSet::new()));
                    groups.len() - 1
                });
            groups[index].1.insert((detail.clone(), unit.0.clone()));
        }
    }
    groups
        .into_iter()
        .map(|(framework, facts)| {
            Ok(ShownFramework {
                owner,
                framework,
                facts: digest(&facts)?,
            })
        })
        .collect()
}

fn digest(value: &impl Serialize) -> Result<[u8; 32], serde_json::Error> {
    serde_json::to_vec(value).map(|bytes| Sha256::digest(bytes).into())
}

#[cfg(test)]
#[path = "framework_warnings_tests.rs"]
mod tests;
