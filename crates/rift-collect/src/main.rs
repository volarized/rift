//! Run a collector over a supplied source inventory.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_collect::{
    Category, CollectionCache, CollectionSource, Error, Inventory, Producer, SOURCE_BYTES_MAX,
    StdioCollector, contract_schema,
};
use rift_core::{FileDigest, ProviderId, SourceUnitId};
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

async fn run() -> Result<bool, Error> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next();
    if mode.as_deref() == Some("schema".as_ref()) && args.next().is_none() {
        serde_json::to_writer_pretty(io::stdout().lock(), &contract_schema())?;
        println!();
        return Ok(true);
    }
    let [Some(manifest), Some(cache), Some(separator), Some(program)] =
        std::array::from_fn(|_| args.next())
    else {
        return Err(usage());
    };
    if mode.as_deref() != Some("collect".as_ref()) || separator != "--" {
        return Err(usage());
    }
    let arguments: Vec<OsString> = args.collect();
    let path = PathBuf::from(manifest);
    let root = path.parent().unwrap_or_else(|| Path::new(""));
    let manifest: Manifest = serde_json::from_slice(&read_bounded(&path, MANIFEST_BYTES_MAX)?)?;
    let id = ProviderId::new(manifest.producer.clone())
        .map_err(|error| Error::Input(error.to_string()))?;
    let producer = Producer::new(
        id,
        parse_digest(&manifest.revision)?,
        parse_digest(&manifest.context)?,
    );
    let mut inventory = Inventory::default();
    let mut selection = Vec::new();
    for source in manifest.sources {
        let unit =
            SourceUnitId::parse(&source.unit).map_err(|error| Error::Input(error.to_string()))?;
        let text = String::from_utf8(read_bounded(
            &root.join(source.path),
            SOURCE_BYTES_MAX as u64,
        )?)
        .map_err(|error| Error::Input(error.to_string()))?;
        if source.selected {
            selection.push(unit.clone());
        }
        inventory.insert(CollectionSource::new(unit, source.filename, text.into())?)?;
    }
    let cache = CollectionCache::open(Path::new(&cache)).await?;
    let mut collector = StdioCollector::new(program, arguments, Duration::from_secs(30));
    let mut output = BufWriter::new(io::stdout().lock());
    let result = cache.collect(&producer, &mut collector, &inventory, &selection, manifest.category, |source, batch| {
        serde_json::to_writer(&mut output, &json!({
            "unit": source.unit().to_string(), "category": manifest.category, "batch": batch,
            "producer": manifest.producer, "revision": manifest.revision, "context": manifest.context,
            "source_sha256": source.sha256()
        }))?;
        output.write_all(b"\n")?;
        Ok(())
    }).await;
    let closed = collector.close().await;
    let stats = result?;
    closed?;
    output.flush()?;
    eprintln!(
        "reused={} collected={} partial={}",
        stats.reused, stats.collected, stats.partial
    );
    Ok(stats.partial == 0)
}

fn usage() -> Error {
    Error::Input(
        "usage: rift-collect collect MANIFEST CACHE -- PROGRAM [ARGS...] | rift-collect schema"
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
