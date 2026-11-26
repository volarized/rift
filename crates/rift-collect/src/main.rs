//! Run a collector over a supplied source inventory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_collect::{
    Category, CollectionCache, CollectionSource, CollectionStats, Collector, Error, Inventory,
    JvmSyntaxCollector, LspCollector, Producer, SOURCE_BYTES_MAX, StdioCollector, contract_schema,
};
use rift_core::{FileDigest, ProjectPath, ProviderId, SourceUnitId};
use rift_lsp::{EngineLaunch, JvmLanguageServer};
use serde::Deserialize;
use serde_json::json;

const MANIFEST_BYTES_MAX: u64 = 8 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    producer: String,
    revision: String,
    context: String,
    category: Category,
    sources: Vec<Source>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    unit: String,
    path: PathBuf,
    filename: String,
    #[serde(default = "selected_by_default")]
    selected: bool,
}

const fn selected_by_default() -> bool {
    true
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Input(format!(
            "{} exceeds its byte bound",
            path.display()
        )));
    }
    Ok(bytes)
}

fn parse_digest(value: &str) -> Result<FileDigest, Error> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Input("expected a lowercase SHA-256 digest".into()));
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|error| Error::Input(error.to_string()))?;
    }
    Ok(FileDigest::from_bytes(bytes))
}

enum Mode {
    Stdio {
        program: OsString,
        arguments: Vec<OsString>,
    },
    Static,
    Lsp {
        server: JvmLanguageServer,
        command: Option<(String, Vec<String>)>,
    },
}

fn command(
    arguments: &mut impl Iterator<Item = OsString>,
) -> Result<(OsString, Vec<OsString>), Error> {
    match (arguments.next(), arguments.next()) {
        (Some(separator), Some(program)) if separator == "--" => Ok((program, arguments.collect())),
        _ => Err(usage()),
    }
}

fn lsp_path(root: &Path, absolute: &Path) -> Result<ProjectPath, Error> {
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| Error::Input("LSP source falls outside its workspace".into()))?;
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::Input("LSP source path must be UTF-8".into()))?;
    Ok(ProjectPath::new(parts.join("/"))?)
}

fn lsp_context(
    context: FileDigest,
    server: JvmLanguageServer,
    launch: &EngineLaunch,
    root: &Path,
    inventory: &Inventory,
    paths: &BTreeMap<SourceUnitId, ProjectPath>,
) -> Result<FileDigest, Error> {
    // The server may read undeclared dependencies between admitted units. Bind the entire
    // inventory to retained batches, including sources without incoming target locations.
    let mut input = serde_json::to_vec(&(
        context.as_bytes(),
        server.language_id(),
        &launch.program,
        &launch.arguments,
        root,
    ))?;
    for (unit, path) in paths {
        let source = inventory
            .get(unit)
            .ok_or_else(|| Error::Input(format!("missing LSP source: {unit}")))?;
        serde_json::to_writer(
            &mut input,
            &(
                unit.to_string(),
                path.as_str(),
                source.revision().as_bytes(),
            ),
        )?;
    }
    Ok(FileDigest::of(&input))
}

struct LspWorkspace {
    root: PathBuf,
    paths: BTreeMap<SourceUnitId, ProjectPath>,
}

struct CollectionRun {
    manifest: Manifest,
    id: ProviderId,
    revision: FileDigest,
    context: FileDigest,
    inventory: Inventory,
    selection: Vec<SourceUnitId>,
    cache: CollectionCache,
}

impl CollectionRun {
    async fn load(
        manifest_path: &Path,
        cache_path: &Path,
        lsp: Option<&mut LspWorkspace>,
    ) -> Result<Self, Error> {
        let root = manifest_root(manifest_path);
        let mut lsp = lsp;
        let mut manifest: Manifest =
            serde_json::from_slice(&read_bounded(manifest_path, MANIFEST_BYTES_MAX)?)?;
        let id = ProviderId::new(manifest.producer.clone())
            .map_err(|error| Error::Input(error.to_string()))?;
        let revision = parse_digest(&manifest.revision)?;
        let context = parse_digest(&manifest.context)?;
        let mut inventory = Inventory::default();
        let mut selection = Vec::new();
        for source in std::mem::take(&mut manifest.sources) {
            let unit = SourceUnitId::parse(&source.unit)
                .map_err(|error| Error::Input(error.to_string()))?;
            let mut path = root.join(source.path);
            if let Some(lsp) = &mut lsp {
                path = path.canonicalize()?;
                if !path.is_file() {
                    return Err(Error::Input("LSP source must be a regular file".into()));
                }
                lsp.paths.insert(unit.clone(), lsp_path(&lsp.root, &path)?);
            }
            let text = String::from_utf8(read_bounded(&path, SOURCE_BYTES_MAX as u64)?)
                .map_err(|error| Error::Input(error.to_string()))?;
            if source.selected {
                selection.push(unit.clone());
            }
            inventory.insert(CollectionSource::new(unit, source.filename, text.into())?)?;
        }
        Ok(Self {
            manifest,
            id,
            revision,
            context,
            inventory,
            selection,
            cache: CollectionCache::open(cache_path).await?,
        })
    }

    async fn write<C: Collector>(
        &self,
        context: FileDigest,
        collector: &mut C,
        output: &mut impl Write,
    ) -> Result<CollectionStats, Error>
    where
        Error: From<C::Error>,
    {
        let producer = Producer::new(self.id.clone(), self.revision, context);
        let mut context_text = String::with_capacity(64);
        for byte in context.as_bytes() {
            let _ = write!(context_text, "{byte:02x}");
        }
        self.cache
            .collect(
                &producer,
                collector,
                &self.inventory,
                &self.selection,
                self.manifest.category,
                |source, batch| {
                    serde_json::to_writer(
                        &mut *output,
                        &json!({
                            "unit": source.unit().to_string(),
                            "category": self.manifest.category,
                            "batch": batch,
                            "producer": self.manifest.producer,
                            "revision": self.manifest.revision,
                            "context": context_text,
                            "source_sha256": source.sha256()
                        }),
                    )?;
                    output.write_all(b"\n")?;
                    Ok(())
                },
            )
            .await
    }

    async fn write_lsp(
        &self,
        server: JvmLanguageServer,
        workspace: LspWorkspace,
        command: Option<(String, Vec<String>)>,
        output: &mut impl Write,
    ) -> Result<CollectionStats, Error> {
        let (program, arguments) = match command {
            Some(command) => command,
            None => (
                server.program().to_owned(),
                server
                    .arguments(&workspace.root)
                    .ok_or_else(|| Error::Input("LSP workspace path must be UTF-8".into()))?,
            ),
        };
        let launch = EngineLaunch {
            program,
            arguments,
            environment: BTreeMap::new(),
            initialization_options: None,
            startup_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(15),
            settle_delay: Duration::from_millis(500),
            stderr_capture_bytes: 8 << 10,
        };
        let context = lsp_context(
            self.context,
            server,
            &launch,
            &workspace.root,
            &self.inventory,
            &workspace.paths,
        )?;
        let mut collector = LspCollector::new(
            server,
            workspace.root,
            workspace.paths,
            launch,
            Duration::from_secs(60),
        )?;
        let result = self.write(context, &mut collector, output).await;
        let stderr = collector.close().await;
        if result.is_err()
            && let Some(stderr) = stderr
        {
            eprint!("{}", stderr.text);
        }
        result
    }
}

fn manifest_root(path: &Path) -> &Path {
    path.parent()
        .filter(|root| !root.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn parse_arguments(
    mode: Option<&std::ffi::OsStr>,
    mut args: impl Iterator<Item = OsString>,
) -> Result<(Mode, PathBuf, PathBuf), Error> {
    let server = if mode == Some("lsp".as_ref()) {
        Some(
            args.next()
                .as_deref()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(JvmLanguageServer::for_language)
                .ok_or_else(usage)?,
        )
    } else {
        None
    };
    let [Some(manifest), Some(cache)] = std::array::from_fn(|_| args.next()) else {
        return Err(usage());
    };
    let mode = match mode.and_then(std::ffi::OsStr::to_str) {
        Some("collect") => {
            let (program, arguments) = command(&mut args)?;
            Mode::Stdio { program, arguments }
        }
        Some("static") if args.next().is_none() => Mode::Static,
        Some("lsp") => {
            let mut args = args.peekable();
            let command = if args.peek().is_some() {
                let (program, arguments) = command(&mut args)?;
                let text = |value: OsString| {
                    value
                        .into_string()
                        .map_err(|_| Error::Input("LSP command must be UTF-8".into()))
                };
                Some((
                    text(program)?,
                    arguments.into_iter().map(text).collect::<Result<_, _>>()?,
                ))
            } else {
                None
            };
            Mode::Lsp {
                server: server.ok_or_else(usage)?,
                command,
            }
        }
        _ => return Err(usage()),
    };
    Ok((mode, manifest.into(), cache.into()))
}

async fn run() -> Result<bool, Error> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    if mode.as_deref() == Some("schema".as_ref()) && args.next().is_none() {
        serde_json::to_writer_pretty(io::stdout().lock(), &contract_schema())?;
        println!();
        return Ok(true);
    }
    let (mode, manifest, cache) = parse_arguments(mode.as_deref(), args)?;
    let mut lsp = if matches!(mode, Mode::Lsp { .. }) {
        Some(LspWorkspace {
            root: manifest_root(&manifest).canonicalize()?,
            paths: BTreeMap::new(),
        })
    } else {
        None
    };
    let run = CollectionRun::load(&manifest, &cache, lsp.as_mut()).await?;
    let mut output = BufWriter::new(io::stdout().lock());
    let stats = match mode {
        Mode::Stdio { program, arguments } => {
            let mut collector = StdioCollector::new(program, arguments, Duration::from_secs(30));
            let result = run.write(run.context, &mut collector, &mut output).await;
            let closed = collector.close().await;
            let stats = result?;
            closed?;
            stats
        }
        Mode::Static => {
            let context = FileDigest::of(&[run.context.as_bytes().as_slice(), b"static"].concat());
            run.write(context, &mut JvmSyntaxCollector, &mut output)
                .await?
        }
        Mode::Lsp { server, command } => {
            run.write_lsp(server, lsp.ok_or_else(usage)?, command, &mut output)
                .await?
        }
    };
    output.flush()?;
    eprintln!(
        "reused={} collected={} partial={}",
        stats.reused, stats.collected, stats.partial
    );
    Ok(stats.partial == 0)
}

fn usage() -> Error {
    Error::Input(
        "usage: rift-collect collect MANIFEST CACHE -- PROGRAM [ARGS...] | rift-collect static MANIFEST CACHE | rift-collect lsp LANGUAGE MANIFEST CACHE [-- PROGRAM [ARGS...]] | rift-collect schema"
            .into(),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(2),
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
