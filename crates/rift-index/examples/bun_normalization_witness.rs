//! Compares the named rebuild and cold graph outside server measurements.

use std::error::Error;
use std::fmt::{self, Debug, Write as _};
use std::fs;
use std::path::{Path, PathBuf};

use get_size2::GetSize;
use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion};
use rift_index::{FileRecord, PathChanges, WorkspaceIndex, WorkspaceIndexLimits};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Deserialize)]
struct Mutation {
    path: String,
    original: String,
    updated: String,
}

#[derive(Debug, Deserialize)]
struct Edit {
    name: String,
    changes: Vec<Mutation>,
}

struct Witness(Sha256);

impl fmt::Write for Witness {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.0.update(value);
        Ok(())
    }
}

fn digest_debug(value: &impl Debug) -> Result<String> {
    let mut witness = Witness(Sha256::new());
    write!(witness, "{value:?}")?;
    Ok(data_encoding::HEXLOWER.encode(&witness.0.finalize()))
}

fn build(root: &Path) -> Result<WorkspaceIndex> {
    Ok(WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?)
}

fn witness(index: &WorkspaceIndex) -> Result<serde_json::Value> {
    let graph = index.normalized_graph();
    Ok(serde_json::json!({
        "revision": index.tree_revision(),
        "rows": graph.publications().contribution_count(),
        "records": graph.records().len(),
        "sources": digest_debug(&index.digests())?,
        "normalized": digest_debug(&graph)?,
        "adjacency": digest_debug(&index.edges())?,
        "omitted": digest_debug(&index.static_targets_omitted())?,
        "warnings": digest_debug(&index.warnings())?,
    }))
}

fn modify(root: &Path, directory: &Path, edit: &Edit) -> Result<()> {
    if !(1..=50).contains(&edit.changes.len()) {
        return Err("expected one through fifty changed paths".into());
    }
    for row in &edit.changes {
        let path = ProjectPath::new(&row.path)?;
        let expected = ProjectPath::new(&row.original)?;
        let updated = ProjectPath::new(&row.updated)?;
        let destination = root.join(path.as_str());
        let before = fs::read(directory.join(expected.as_str()))?;
        if fs::read(&destination)? != before {
            return Err(format!("unexpected original bytes at {path}").into());
        }
        let changed = fs::read(directory.join(updated.as_str()))?;
        if changed == before {
            return Err(format!("identical bytes at {path}").into());
        }
        fs::write(destination, changed)?;
    }
    Ok(())
}

fn changed_paths(root: &Path, previous: &WorkspaceIndex, edit: &Edit) -> Result<PathChanges> {
    let policy = rift_index::WorkspaceSourcePolicy::build(
        root,
        previous.limits(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?;
    let observed = edit
        .changes
        .iter()
        .map(|row| -> Result<_> {
            let path = ProjectPath::new(&row.path)?;
            let digest = policy.visible_digest(&root.join(path.as_str()))?;
            Ok((path, digest.map(FileRecord::Digest)))
        })
        .collect::<Result<Vec<_>>>()?;
    let changes = PathChanges::resolve(observed, |path| previous.record(path));
    if changes.len() != edit.changes.len() {
        return Err(format!("{} observed {} changed paths", edit.name, changes.len()).into());
    }
    Ok(changes)
}

fn main() -> Result<()> {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.len() != 3 {
        return Err("expected checkout, edit plan and named or cold".into());
    }
    let root = fs::canonicalize(&arguments[0])?;
    let plan = PathBuf::from(&arguments[1]);
    let directory = plan.parent().ok_or("edit plan has no folder")?;
    let input = fs::read(&plan)?;
    if input.len() > 256 * 1024 {
        return Err("edit plan exceeds 256 KiB".into());
    }
    let edits: Vec<Edit> = serde_json::from_slice(&input)?;
    if edits.len() != 4 {
        return Err("expected single, single restore, burst and burst restore".into());
    }
    let named = match arguments[2].to_str() {
        Some("named") => true,
        Some("cold") => false,
        _ => return Err("expected named or cold".into()),
    };
    let initial = build(&root)?;
    let baseline = witness(&initial)?;
    let view = initial.symbol_view()?;
    println!(
        "{}",
        serde_json::json!({
            "phase": "cold",
            "witness": baseline,
            "retained_view_bytes": view.retained_material_bytes(),
        })
    );
    drop(view);
    let mut held = named.then_some(initial);
    for edit in &edits {
        modify(&root, directory, edit)?;
        let (index, additional_held_bytes) = if let Some(previous) = held.take() {
            let before = witness(&previous)?;
            let changed = changed_paths(&root, &previous, edit)?;
            let current = previous.rebuilt(&changed)?;
            if before != witness(&previous)? {
                return Err(format!("{} modified the held view", edit.name).into());
            }
            let current_material = current.retained_view_baseline();
            let (held_bytes, _) = previous
                .symbol_view()?
                .get_size_with_tracker(current_material);
            (current, Some(held_bytes))
        } else {
            (build(&root)?, None)
        };
        let material = witness(&index)?;
        if edit.name.ends_with("restore") && material != baseline {
            return Err(format!("{} failed to restore the original witness", edit.name).into());
        }
        println!(
            "{}",
            serde_json::json!({
                "phase": edit.name,
                "witness": material,
                "retained_view_bytes": index.symbol_view()?.retained_material_bytes(),
                "additional_held_bytes": additional_held_bytes,
            })
        );
        held = named.then_some(index);
    }
    Ok(())
}
