//! Java and Kotlin fact collection through a language server.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lsp_types::Range;
use rift_core::{CapturedStream, FileDigest, ProjectPath, SourceUnitId};
use rift_error::errors;
use rift_lsp::correlation::PENDING_REQUESTS_MAX;
use rift_lsp::{EngineLaunch, EngineSession, JvmLanguageServer, LineIndex, PositionEncoding};
use rift_syntax::JvmLanguage;
use tokio::time::Instant;

use crate::{
    Batch, ByteRange, Category, CollectionInput, CollectionSource, Collector, Coverage, Error,
    Evidence, JvmSyntaxCollector, OBSERVATIONS_MAX, Observation, SOURCE_BYTES_MAX,
    SOURCE_COUNT_MAX, SourceAnchor,
};

/// Resolves authored Java or Kotlin occurrences with the project's language server.
///
/// Paths map the admitted sources to their workspace files. The producer context must cover
/// the whole workspace snapshot the server reads, its build configuration, and its toolchain.
/// A server's URI for a binary or an unadmitted source cannot establish a physical target.
pub struct LspCollector {
    server: JvmLanguageServer,
    root: PathBuf,
    paths: BTreeMap<SourceUnitId, ProjectPath>,
    units: BTreeMap<ProjectPath, SourceUnitId>,
    launch: EngineLaunch,
    deadline: Duration,
    session: Option<EngineSession>,
    stderr: Option<CapturedStream>,
}

impl LspCollector {
    /// Prepares a session. It starts on the first source with a fact request.
    ///
    /// `root` must be the absolute workspace path under which all `paths` are expressed.
    /// The session's startup deadline and the collection deadline are separate.
    ///
    /// # Errors
    ///
    /// Returns an input error for conflicting or oversized path mappings.
    pub fn new(
        server: JvmLanguageServer,
        root: PathBuf,
        paths: BTreeMap<SourceUnitId, ProjectPath>,
        launch: EngineLaunch,
        deadline: Duration,
    ) -> Result<Self, Error> {
        if !root.is_absolute() || paths.len() > SOURCE_COUNT_MAX {
            return Err(Error::Input(
                "invalid LSP source paths or workspace root".into(),
            ));
        }
        let mut units = BTreeMap::new();
        for (unit, path) in &paths {
            if path.as_str().is_empty() || units.insert(path.clone(), unit.clone()).is_some() {
                return Err(Error::Input(format!(
                    "empty or repeated LSP source path: {path}"
                )));
            }
        }
        Ok(Self {
            server,
            root,
            paths,
            units,
            launch,
            deadline,
            session: None,
            stderr: None,
        })
    }

    /// Stops the started server and returns its captured standard error.
    pub async fn close(&mut self) -> Option<CapturedStream> {
        match self.session.take() {
            Some(session) => Some(session.shutdown().await),
            None => self.stderr.take(),
        }
    }
}

impl Collector for LspCollector {
    type Error = Error;

    async fn collect(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> Result<Batch, Error> {
        let source = input.selected();
        if !matches!(
            (self.server, JvmLanguage::from_filename(source.filename())),
            (JvmLanguageServer::Java, Some(JvmLanguage::Java))
                | (JvmLanguageServer::Kotlin, Some(JvmLanguage::Kotlin))
        ) {
            return Ok(unsupported());
        }
        let mut syntax = JvmSyntaxCollector;
        let batch = syntax.collect(input, category).await?;
        if batch.observations.is_empty() {
            return Ok(batch);
        }
        let path = self
            .paths
            .get(source.unit())
            .ok_or_else(|| Error::Input(format!("source has no LSP path: {}", source.unit())))?;
        let mut session = match self.session.take() {
            Some(session) => session,
            None => EngineSession::start(self.launch.clone(), &self.root).await?,
        };
        let deadline = Instant::now()
            .checked_add(self.deadline)
            .ok_or(Error::Deadline)?;
        session.set_deadline(Some(deadline));
        let result = tokio::time::timeout_at(
            deadline,
            collect_with_session(
                &mut session,
                self.server,
                path,
                &self.units,
                input,
                batch,
                deadline,
            ),
        )
        .await;
        session.set_deadline(None);
        let result = match result {
            Ok(Err(Error::Engine(error)))
                if error.slug() == errors::lsp::engine_analyzing::SLUG
                    && Instant::now() >= deadline =>
            {
                Err(Error::Deadline)
            }
            result => result.unwrap_or(Err(Error::Deadline)),
        };
        if result.is_ok() && session.is_intact() {
            self.session = Some(session);
        } else {
            self.stderr = Some(session.shutdown().await);
        }
        result
    }
}

fn unsupported() -> Batch {
    Batch {
        coverage: Coverage::Unsupported,
        observations: Vec::new(),
    }
}

async fn collect_with_session(
    session: &mut EngineSession,
    server: JvmLanguageServer,
    path: &ProjectPath,
    units: &BTreeMap<ProjectPath, SourceUnitId>,
    input: &mut CollectionInput<'_>,
    batch: Batch,
    deadline: Instant,
) -> Result<Batch, Error> {
    if !session.capabilities().definitions.definition {
        return Ok(unsupported());
    }
    input.validate(&batch)?;
    let source = input.selected();
    if !source_is_current(session.root().filesystem_path(), path, source) {
        return Ok(Batch {
            coverage: Coverage::Partial,
            observations: Vec::new(),
        });
    }
    session
        .open(path, server.language_id(), source.text().to_owned())
        .await?;
    let collected = async {
        session
            .read_output(deadline, |session| {
                !session.is_analyzing() && session.walk_is_quiet()
            })
            .await?;
        definitions(session, path, units, input, batch, deadline).await
    }
    .await;
    session.close(path).await?;
    collected
}

async fn definitions(
    session: &mut EngineSession,
    path: &ProjectPath,
    units: &BTreeMap<ProjectPath, SourceUnitId>,
    input: &mut CollectionInput<'_>,
    batch: Batch,
    deadline: Instant,
) -> Result<Batch, Error> {
    let line_index = LineIndex::new(input.selected().text());
    let encoding = session.capabilities().position_encoding;
    let mut targets = DefinitionTargets::default();
    let mut output = Batch {
        coverage: batch.coverage,
        observations: Vec::with_capacity(batch.observations.len()),
    };
    for (chunk, sites) in batch.observations.chunks(PENDING_REQUESTS_MAX).enumerate() {
        let positions = sites
            .iter()
            .map(|site| {
                let byte = usize::try_from(site.occurrence.start)
                    .map_err(|_| Error::Input("LSP occurrence exceeds its byte bound".into()))?;
                Ok((path.clone(), line_index.position(encoding, byte)?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let mut answers = session.definition_batch(&positions).await?;
        if session.is_analyzing() {
            session
                .read_output(deadline, |session| !session.is_analyzing())
                .await?;
            if session.is_analyzing() {
                output.coverage = Coverage::Partial;
                break;
            }
            answers = session.definition_batch(&positions).await?;
            if session.is_analyzing() {
                output.coverage = Coverage::Partial;
                break;
            }
        }
        for (index, (site, locations)) in sites.iter().zip(answers).enumerate() {
            let site_index = chunk * PENDING_REQUESTS_MAX + index;
            let remaining = OBSERVATIONS_MAX - output.observations.len();
            if locations.len().max(1) > remaining {
                output.coverage = Coverage::Partial;
                return Ok(output);
            }
            let single_answer = locations.len() == 1;
            let mut anchors = BTreeMap::new();
            for location in locations {
                let Ok(path) = session.root().project_path(&location.uri) else {
                    continue;
                };
                if let Some(anchor) = targets.anchor(
                    units,
                    input,
                    session.root().filesystem_path(),
                    path,
                    encoding,
                    location.range,
                )? {
                    anchors.insert(
                        (anchor.unit.clone(), anchor.range.start, anchor.range.end),
                        anchor,
                    );
                }
            }
            if anchors.is_empty() {
                output.observations.push(observation(
                    site_index,
                    0,
                    site,
                    None,
                    Evidence::Unresolved,
                ));
            } else {
                let evidence = if single_answer {
                    Evidence::Resolution
                } else {
                    Evidence::Candidate
                };
                output
                    .observations
                    .extend(
                        anchors
                            .into_values()
                            .enumerate()
                            .map(|(target_index, anchor)| {
                                observation(site_index, target_index, site, Some(anchor), evidence)
                            }),
                    );
            }
        }
    }
    if targets.changed {
        output.coverage = Coverage::Partial;
    }
    Ok(output)
}

fn observation(
    site_index: usize,
    target_index: usize,
    site: &Observation,
    target: Option<SourceAnchor>,
    evidence: Evidence,
) -> Observation {
    Observation {
        id: format!("{site_index}.{target_index}"),
        occurrence: site.occurrence,
        enclosing: site.enclosing,
        target,
        evidence,
    }
}

#[derive(Default)]
struct DefinitionTargets<'a> {
    // Each admitted target registers a supporting source through CollectionInput once.
    indexed: BTreeMap<ProjectPath, Option<(&'a CollectionSource, LineIndex<'a>)>>,
    changed: bool,
}

impl<'a> DefinitionTargets<'a> {
    fn anchor(
        &mut self,
        units: &BTreeMap<ProjectPath, SourceUnitId>,
        input: &mut CollectionInput<'a>,
        root: &Path,
        path: ProjectPath,
        encoding: PositionEncoding,
        range: Range,
    ) -> Result<Option<SourceAnchor>, Error> {
        let Some(unit) = units.get(&path) else {
            return Ok(None);
        };
        let target = match self.indexed.entry(path) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let Some(source) = input.read(unit)? else {
                    return Ok(None);
                };
                let current = source_is_current(root, entry.key(), source);
                self.changed |= !current;
                entry.insert(current.then(|| (source, LineIndex::new(source.text()))))
            }
        };
        let Some((source, index)) = target else {
            return Ok(None);
        };
        let (Ok(start), Ok(end)) = (
            index.byte_offset(encoding, range.start),
            index.byte_offset(encoding, range.end),
        ) else {
            return Ok(None);
        };
        if start >= end {
            return Ok(None);
        }
        let range = ByteRange {
            start: start as u64,
            end: end as u64,
        };
        Ok(Some(SourceAnchor {
            unit: unit.to_string(),
            source_sha256: source.sha256().to_owned(),
            range,
        }))
    }
}

/// The server can consult a supporting file without a didOpen overlay. Check its current
/// bytes before attaching a location to the admitted snapshot, once per file per request.
fn source_is_current(root: &Path, path: &ProjectPath, source: &CollectionSource) -> bool {
    let path = root.join(path.as_str());
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    let source_bytes = source.text().len() as u64;
    if !metadata.is_file() || metadata.len() != source_bytes {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::with_capacity(source.text().len());
    file.take(SOURCE_BYTES_MAX as u64 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && FileDigest::of(&bytes) == source.digest()
}

#[cfg(test)]
mod tests;
