//! Shared naming for package and runtime targets of every engine edge kind.

use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{EdgeKind, ReadWarning, SymbolId, SymbolKind};
use rift_protocol::revision::Revision;
use rift_search::sparql::RevisionIri;
use rift_server::{CalleePackage, PackageCallee};

use super::{
    AdjacencyWalk, InstalledTarget, claim_request, coverage_failure, exact_request,
    selected_release,
};

impl AdjacencyWalk<'_> {
    pub(super) async fn name_package_calls(
        &mut self,
        calls: &mut rift_server::GraphCalls,
        lookup_details: bool,
    ) -> Result<(), RiftError> {
        let packages = std::mem::take(&mut calls.packages);
        let callees = packages
            .iter()
            .map(|(callee, _)| callee.clone())
            .collect::<Vec<_>>();
        let named = self
            .name_package_targets(&callees, EdgeKind::Calls, lookup_details)
            .await?;
        for ((callee, occurrences), identity) in packages.into_iter().zip(named) {
            if let Some(identity) = identity {
                calls.name_package_call(&callee, identity, occurrences);
            } else {
                calls.complete = false;
            }
        }
        Ok(())
    }

    pub(super) async fn name_package_targets(
        &mut self,
        callees: &[PackageCallee],
        kind: EdgeKind,
        lookup_details: bool,
    ) -> Result<Vec<Option<SymbolId>>, RiftError> {
        if callees.is_empty() {
            return Ok(Vec::new());
        }
        let installed = if let Some(installed) = self.use_installed {
            installed
        } else {
            let installed = if self.configuration.enabled {
                if self.remaining.requests_max == 0 {
                    self.report_missing_targets(kind, callees.len());
                    return Ok(vec![None; callees.len()]);
                }
                claim_request(&mut self.remaining)?;
                let client = self.server.global.client(&self.configuration).await?;
                let capabilities = client.get_capabilities().await?;
                !capabilities
                    .supported_features
                    .iter()
                    .any(|feature| feature == "declarations")
            } else {
                true
            };
            self.use_installed = Some(installed);
            installed
        };
        let named = if installed {
            self.capture_installed_targets(callees, kind).await?
        } else {
            self.name_published_targets(callees, kind, lookup_details)
                .await?
        };
        let dropped = named.iter().filter(|identity| identity.is_none()).count();
        self.report_missing_targets(kind, dropped);
        Ok(named)
    }

    async fn name_published_targets(
        &mut self,
        callees: &[PackageCallee],
        kind: EdgeKind,
        lookup_details: bool,
    ) -> Result<Vec<Option<SymbolId>>, RiftError> {
        let client = self.server.global.client(&self.configuration).await?;
        let declarations = crate::global::callee_declarations_bounded(
            &client,
            callees,
            &mut self.remaining.requests_max,
        )
        .await?;
        let dependencies = self.resolved.published.reads.dependency_context();
        let mut named = Vec::with_capacity(callees.len());
        for callee in callees {
            let Some(declaration) = declarations.declaration(callee) else {
                named.push(None);
                continue;
            };
            if !release_matches(callee, &declaration.id, &declaration.revision, dependencies)? {
                named.push(None);
                continue;
            }
            if self
                .acquired
                .named_package_symbols
                .get(&declaration.id)
                .is_some_and(|(_, held)| held.as_str() != declaration.revision.to_string())
            {
                named.push(None);
                continue;
            }
            if (lookup_details || type_target(kind))
                && !self
                    .acquired
                    .named_package_symbols
                    .contains_key(&declaration.id)
            {
                if self.remaining.requests_max == 0 {
                    named.push(None);
                    continue;
                }
                claim_request(&mut self.remaining)?;
                let answer = client
                    .get_symbol(&exact_request(&declaration.id, &declaration.revision))
                    .await?;
                let rift_protocol::symbol_read::GetSymbolResult::Found {
                    symbol, revision, ..
                } = answer
                else {
                    named.push(None);
                    continue;
                };
                if revision != declaration.revision {
                    named.push(None);
                    continue;
                }
                self.acquired.named_package_symbols.insert(
                    declaration.id.clone(),
                    (*symbol, RevisionIri::parse(&revision.to_string())?),
                );
            }
            if type_target(kind)
                && self
                    .acquired
                    .named_package_symbols
                    .get(&declaration.id)
                    .is_none_or(|(symbol, _)| !symbol.kinds.contains(&SymbolKind::Type))
            {
                named.push(None);
                continue;
            }
            named.push(Some(declaration.id.clone()));
        }
        Ok(named)
    }

    async fn capture_installed_targets(
        &mut self,
        callees: &[PackageCallee],
        kind: EdgeKind,
    ) -> Result<Vec<Option<SymbolId>>, RiftError> {
        let (_, roots) = self.local.as_ref().ok_or_else(coverage_failure)?;
        let roots = roots.clone();
        let tree = self.server.root.clone();
        let configuration = self
            .resolved
            .published
            .configuration
            .accepted_for_tool(super::super::super::super::wire::ErrorPhase::Read)
            .map_err(|_| coverage_failure())?;
        let callees = callees.to_vec();
        let maximum = usize::try_from(self.remaining.bytes_max).unwrap_or(usize::MAX);
        let end = self.deadline.at();
        let targets = self
            .server
            .blocking
            .run("capture installed engine targets", move || {
                let mut retained = 0usize;
                let mut targets = Vec::with_capacity(callees.len());
                for callee in callees {
                    if tokio::time::Instant::now() >= end || retained == maximum {
                        targets.push(None);
                        continue;
                    }
                    let target =
                        InstalledTarget::capture(callee, &roots, &tree, &configuration).ok();
                    let target = target.filter(|target| {
                        !type_target(kind) || target.symbol.kinds.contains(&SymbolKind::Type)
                    });
                    if let Some(target) = target {
                        if let Some(bytes) = retained
                            .checked_add(target.retained_bytes())
                            .filter(|bytes| *bytes <= maximum)
                        {
                            retained = bytes;
                            targets.push(Some(target));
                        } else {
                            targets.push(None);
                        }
                    } else {
                        targets.push(None);
                    }
                }
                Ok(targets)
            })
            .await?;
        let mut named = Vec::with_capacity(targets.len());
        for target in targets {
            let Some(target) = target else {
                named.push(None);
                continue;
            };
            let identity = target.id()?.clone();
            if !release_matches(
                &target.callee,
                &identity,
                &target.revision,
                self.resolved.published.reads.dependency_context(),
            )? {
                named.push(None);
                continue;
            }
            if let Some(held) = self.acquired.installed.get(&identity)
                && !held.same_capture(&target)
            {
                named.push(None);
                continue;
            }
            self.remaining.bytes_max = self
                .remaining
                .bytes_max
                .checked_sub(
                    u64::try_from(target.retained_bytes()).map_err(|_| coverage_failure())?,
                )
                .ok_or_else(coverage_failure)?;
            self.acquired
                .installed
                .insert(identity.clone(), Arc::new(target));
            named.push(Some(identity));
        }
        Ok(named)
    }

    fn report_missing_targets(&mut self, kind: EdgeKind, dropped: usize) {
        if dropped == 0 {
            return;
        }
        self.acquired.partial = true;
        if kind == EdgeKind::Calls {
            self.acquired.warnings.push(ReadWarning::CalleesDropped {
                callees: u64::try_from(dropped).unwrap_or(u64::MAX),
                detail: "the engine named package or runtime positions whose admitted sources or exact publications established no declaration, so this answer omits those calls".to_owned(),
            });
        }
        self.acquired.warnings.push(ReadWarning::EdgeCoverageMissing {
            kinds: vec![kind],
            detail: format!("{dropped} engine targets for {} could not be named within the accepted source, publication and read bounds, so this answer keeps the independently established edges", kind.as_ref()),
        });
    }
}

fn type_target(kind: EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::HasType | EdgeKind::Extends | EdgeKind::Implements
    )
}

fn release_matches(
    callee: &PackageCallee,
    identity: &SymbolId,
    revision: &Revision,
    context: &rift_dependency::DependencyContext,
) -> Result<bool, RiftError> {
    let identity = SymbolIdentity::parse(identity.as_str())?;
    let observed = match callee.package() {
        CalleePackage::Installed(package) if &package.owner()? == identity.owner() => {
            Some(package.release()?)
        }
        CalleePackage::Runtime(runtime) if &runtime.owner()? == identity.owner() => {
            Some(Revision::release(identity.owner(), &runtime.version)?)
        }
        _ => selected_release(context, identity.owner()),
    };
    Ok(observed.as_ref() == Some(revision))
}
