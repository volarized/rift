//! Observe ordinary Bun index builds on a disposable checkout.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion, bun_eligibility};
use rift_index::{
    PathChange, PathChanges, WorkspaceDigests, WorkspaceIndex, WorkspaceIndexLimits,
    capture_digests,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RIFT_SOURCE: &str = "da38628da2d2613f63fb65f3ffb3eb703ed1d7d9";
const RIFT_TREE: &str = "345747e13e159e484c63491e2d09871124635253";
const BUN_SOURCE: &str = "744846f844374847c902b5e7fd59b4342a51ef99";
const MARKER: &str = "bench/test/isolate/setup.ts";
const GATES: [&str; 6] = [
    "capture",
    "omitted_input",
    "framework_context",
    "alias_selector",
    "provider_claim",
    "admission_prefix",
];
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Fingerprint(Sha256);

impl fmt::Write for Fingerprint {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.0.update(value.as_bytes());
        Ok(())
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn binary_digest() -> Result<String> {
    let mut file = fs::File::open(std::env::current_exe()?)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            return Ok(hex(digest.finalize()));
        }
        digest.update(&buffer[..length]);
    }
}

fn graph_digest(index: &WorkspaceIndex, current_keys: bool) -> Result<String> {
    let graph = index.normalized_graph();
    let mut output = Fingerprint(Sha256::new());
    writeln!(
        &mut output,
        "{:?} {:?}",
        index.tree_revision(),
        index.static_targets_omitted()
    )?;
    if current_keys {
        writeln!(
            &mut output,
            "{:?} {:?} {:?}",
            graph.index_revision(),
            graph.source_revision(),
            graph.tree_revision()
        )?;
    }
    for record in graph.records() {
        writeln!(
            &mut output,
            "{:?} {:?}",
            record.identity(),
            record.resolution()
        )?;
        for key in record.contributions() {
            if current_keys {
                writeln!(&mut output, "key {key:?}")?;
            }
            let row = graph
                .contribution(key)
                .ok_or("record references unavailable contribution")?;
            writeln!(
                &mut output,
                "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
                key.reference(),
                row.source(),
                row.facts(),
                row.origin(),
                row.identity_anchor(),
                row.equivalence(),
                row.references(),
                row.edges(),
                row.namespaced()
            )?;
        }
    }
    for association in graph.candidates() {
        writeln!(
            &mut output,
            "{:?} {:?} {:?}",
            association.source(),
            association.target(),
            association.state()
        )?;
    }
    for reference in graph.references() {
        writeln!(
            &mut output,
            "{:?} {:?} {:?} {:?}",
            reference.source().reference(),
            reference.binding(),
            reference.role(),
            reference.targets()
        )?;
    }
    for edge in graph.edges() {
        writeln!(
            &mut output,
            "{:?} {:?} {:?} {:?} {:?}",
            edge.source().reference(),
            edge.kind(),
            edge.target(),
            edge.occurrence(),
            edge.derivation()
        )?;
    }
    let edges = index.edges();
    let mut targets = BTreeSet::new();
    writeln!(&mut output, "dropped {}", edges.dropped_edges())?;
    for edge in edges.edges() {
        writeln!(&mut output, "out {edge:?}")?;
        targets.insert(edge.to());
    }
    for target in targets {
        for edge in edges.incoming(target) {
            writeln!(&mut output, "in {edge:?}")?;
        }
    }
    for (unit, language, unavailable, unresolved) in index.bun_observed_coverage() {
        writeln!(
            &mut output,
            "coverage {unit:?} {language:?} {unavailable} {unresolved:?}"
        )?;
    }
    for warning in index.warnings() {
        writeln!(
            &mut output,
            "warning {:?} {:?} {}",
            warning.path(),
            warning.reason(),
            warning.holds_text()
        )?;
    }
    Ok(hex(output.0.finalize()))
}

fn build(root: &Path) -> Result<WorkspaceIndex> {
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?;
    if bun_eligibility::active() {
        let unresolved = index.left_out_file_count();
        bun_eligibility::note(
            "omitted_input",
            &[("held_warning_rows", unresolved)],
            0,
            0,
            None,
            None,
        );
    }
    Ok(index)
}

fn capture(root: &Path) -> Result<WorkspaceDigests> {
    Ok(capture_digests(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
    )?)
}

struct Restore {
    root: PathBuf,
    originals: BTreeMap<ProjectPath, Vec<u8>>,
    clean: bool,
}

impl Drop for Restore {
    fn drop(&mut self) {
        if !self.clean {
            for (path, bytes) in &self.originals {
                if let Err(error) = fs::write(self.root.join(path.as_str()), bytes) {
                    eprintln!("could not restore {}: {error}", path.as_str());
                }
            }
        }
    }
}

fn saved_phase(
    root: &Path,
    recipe: &Path,
    names: &[ProjectPath],
    from: &str,
    to: &str,
) -> Result<()> {
    for name in names {
        let path = name.as_str();
        let old = fs::read(recipe.join(from).join(path))?;
        let new = fs::read(recipe.join(to).join(path))?;
        let target = root.join(path);
        if old == new || fs::read(&target)? != old {
            return Err(format!("saved input does not match {path}").into());
        }
        fs::write(target, new)?;
    }
    Ok(())
}

fn marker(index: &WorkspaceIndex, expected: Option<&str>) -> Result<()> {
    let graph = index.normalized_graph();
    for name in ["lane_incr_probe_31", "lane_incr_probe_50"] {
        let identity =
            format!("rift://symbol/local/typescript/.%2Fbench%2Ftest%2Fisolate%2Fsetup.ts/{name}");
        let present = graph
            .records()
            .iter()
            .filter(|record| {
                record
                    .identity()
                    .is_some_and(|identity_now| identity_now.as_str() == identity)
            })
            .collect::<Vec<_>>();
        if expected != Some(name) {
            if !present.is_empty() {
                return Err(format!("unexpected current marker {name}").into());
            }
            continue;
        }
        if present.len() != 1 || present[0].contributions().len() != 1 {
            return Err(format!("unexpected marker membership for {name}").into());
        }
        let row = graph
            .contribution(&present[0].contributions()[0])
            .ok_or("expected marker row")?;
        let range = row.source().ok_or("expected marker source")?.range();
        if range.start() != 3062 || range.end() != 3106 {
            return Err(format!("unexpected marker binding {range:?}").into());
        }
    }
    Ok(())
}

fn emit_gate(
    phase: &str,
    family: &str,
    named: &BTreeMap<&'static str, bun_eligibility::Observation>,
    cold: &BTreeMap<&'static str, bun_eligibility::Observation>,
) {
    if family == "capture" {
        if let Some(named) = named.get(family) {
            println!(
                "{}",
                json!({"event":"gate","phase":phase,"family":family,
                "status":"closed","observed":named.observed,"unknown":0,"wall_ns":named.wall_ns,
                "first":null,"named_counters":named.counters})
            );
        }
        return;
    }
    let Some(source) = cold.get(family).or_else(|| named.get(family)) else {
        return;
    };
    let named_witness = named.get(family).and_then(|record| record.first.as_ref());
    let affected = named
        .get(family)
        .and_then(|record| record.first_affected.as_ref())
        .or(source.first_affected.as_ref());
    let first = affected.or(named_witness).or(source.first.as_ref());
    println!(
        "{}",
        json!({"event":"gate","phase":phase,"family":family,
            "status":if first.is_some() {"unproved"} else {"closed"},"observed":source.observed,
            "unknown":source.unknown.max(named.get(family).map_or(0, |entry| entry.unknown)),
            "wall_ns":named.get(family).map_or(0, |entry| entry.wall_ns),"first":first,
        "first_scope":if affected.is_some() { "named_path_dependency" } else { "complete_corpus" },
            "named_counters":named.get(family).map(|entry| &entry.counters),
            "independent_cold_counters":cold.get(family).map(|entry| &entry.counters),
            "independent_cold_observation_ns":cold.get(family).map(|entry| entry.wall_ns),
        })
    );
}

fn arguments() -> Result<(PathBuf, PathBuf)> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().ok_or("expected disposable Bun root")?).canonicalize()?;
    let recipe = PathBuf::from(args.next().ok_or("expected saved recipe root")?).canonicalize()?;
    let sentinel = root
        .parent()
        .ok_or("missing disposable runner directory")?
        .join("bun-observer-scratch");
    if args.next().is_some()
        || root.join(".git").exists()
        || fs::read_to_string(sentinel)?.trim() != BUN_SOURCE
        || root == recipe
    {
        return Err("expected verified disposable Bun root".into());
    }
    Ok((root, recipe))
}

fn main() -> Result<()> {
    if !bun_eligibility::compiled() {
        return Err("build observer with RIFT_BUN_ELIGIBILITY=1".into());
    }
    let (root, recipe) = arguments()?;
    let paths = fs::read_to_string(recipe.join("paths.txt"))?;
    let paths = paths
        .lines()
        .map(ProjectPath::new)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let one = [ProjectPath::new(MARKER)?];
    if paths.len() != 50
        || paths.iter().collect::<BTreeSet<_>>().len() != 50
        || !paths.contains(&one[0])
    {
        return Err("expected exact 50-path recipe".into());
    }
    let originals = paths
        .iter()
        .map(|path| {
            let candidate = root.join(path.as_str());
            if !candidate.canonicalize()?.starts_with(&root)
                || !candidate.symlink_metadata()?.is_file()
            {
                return Err(
                    format!("saved path escapes the scratch root: {}", path.as_str()).into(),
                );
            }
            let on_disk = fs::read(candidate)?;
            if on_disk != fs::read(recipe.join("original").join(path.as_str()))? {
                return Err(format!("original bytes differ at {}", path.as_str()).into());
            }
            Ok((path.clone(), on_disk))
        })
        .collect::<Result<_>>()?;
    let mut restore = Restore {
        root: root.clone(),
        originals,
        clean: false,
    };
    let binary = binary_digest()?;
    println!(
        "{}",
        json!({"event":"provenance","mode":"ordinary_observer",
        "rift_source":RIFT_SOURCE,"rift_tree":RIFT_TREE,"bun_source":BUN_SOURCE,
        "observer_binary_sha256":binary})
    );
    let mut current = build(&root)?;
    let original = capture(&root)?;
    if !original.iter().eq(current.digests().iter()) {
        return Err("ordinary original capture mismatch".into());
    }
    marker(&current, None)?;
    for (phase, names, from, to, expected) in [
        (
            "single",
            one.as_slice(),
            "original",
            "single",
            Some("lane_incr_probe_31"),
        ),
        ("single_restore", one.as_slice(), "single", "original", None),
        (
            "burst",
            paths.as_slice(),
            "original",
            "burst",
            Some("lane_incr_probe_50"),
        ),
        ("burst_restore", paths.as_slice(), "burst", "original", None),
    ] {
        let held_digest = graph_digest(&current, true)?;
        let held_sources = current.digests();
        let held_tree = current.tree_revision();
        saved_phase(&root, &recipe, names, from, to)?;
        let capture_started = Instant::now();
        let requested = capture(&root)?;
        let capture_ns = capture_started.elapsed().as_nanos();
        let changes = PathChanges::between(&held_sources, &requested);
        let expected_names = names.iter().collect::<BTreeSet<_>>();
        if changes.len() != names.len()
            || changes.iter().any(|(path, change)| {
                change != PathChange::Modified || !expected_names.contains(path)
            })
        {
            return Err(format!("actual change set differs from {phase}").into());
        }
        bun_eligibility::begin(names.iter().map(|path| path.as_str().to_owned()));
        let started = Instant::now();
        let next = current.rebuilt(&changes)?;
        let named_rebuild_ns = started.elapsed().as_nanos();
        let named = bun_eligibility::finish();
        let closed_capture = capture(&root)?;
        if !requested.iter().eq(closed_capture.iter())
            || !requested.iter().eq(next.digests().iter())
        {
            return Err(format!("closing capture differs from {phase}").into());
        }
        marker(&next, expected)?;
        let held_key_check =
            held_digest == graph_digest(&current, true)? && held_tree == current.tree_revision();
        drop(current);
        bun_eligibility::begin(names.iter().map(|path| path.as_str().to_owned()));
        let started = Instant::now();
        let independent = build(&root)?;
        let independent_cold_ns = started.elapsed().as_nanos();
        let cold = bun_eligibility::finish();
        marker(&independent, expected)?;
        let exact = graph_digest(&next, false)?;
        let agrees = exact == graph_digest(&independent, false)?
            && independent.digests().iter().eq(next.digests().iter());
        drop(independent);
        if to == "original" && !requested.iter().eq(original.iter()) {
            return Err(format!("restore changed original inventory in {phase}").into());
        }
        for family in GATES {
            emit_gate(phase, family, &named, &cold);
        }
        println!(
            "{}",
            json!({"event":"ordinary","phase":phase,"source_paths_changed":changes.len(),
            "capture_verified":true,"held_key_check":held_key_check,"complete_graph":true,
            "independent_cold_agrees":agrees,"ordered_records_and_edges_sha256":exact,
            "accepted_files":next.file_count(),"omitted_or_unparsed":next.left_out_file_count(),
            "current_revision":next.tree_revision(),"held_revision":held_tree,
            "capture_ns":capture_ns,"ordinary_named_ns":named_rebuild_ns,"ordinary_independent_cold_ns":independent_cold_ns})
        );
        if !agrees || !held_key_check {
            return Err(format!("ordinary graph or held oracle differs in {phase}").into());
        }
        current = next;
    }
    restore.clean = true;
    Ok(())
}
