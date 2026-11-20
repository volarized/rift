//! Declared type positions and inferred type requests for engine inventories.

use std::collections::BTreeMap;

use lsp_types::Position;
use rift_error::RiftError;
use rift_lsp::{EngineSession, PositionEncoding};
use rift_syntax::{ByteRange, RustQuery};

use super::{InventoryPosition, InventoryReport};

const TYPE_CAPTURES_MAX: usize = super::TRAVERSAL_SYMBOLS_MAX * 4;

pub(super) fn declared_types(
    file: &rift_index::IndexedFile,
) -> Result<BTreeMap<u64, Vec<usize>>, RiftError> {
    let mut types = if file.syntax().language().name == "rust" {
        rust_types(file.source())?
    } else {
        BTreeMap::new()
    };
    if let Some(facts) = file.syntax().static_facts() {
        for reference in facts
            .references()
            .iter()
            .filter(|reference| reference.role == rift_core::ReferenceRole::Type)
        {
            let Some(owner) = file
                .enclosing_symbol(reference.target.start, reference.target.end)
                .and_then(|owner| owner.name_range)
            else {
                continue;
            };
            let offset = reference.target.start;
            let offset = usize::try_from(offset).map_err(|_| super::graph_call_coverage())?;
            types.entry(owner.start).or_default().push(offset);
        }
    }
    for offsets in types.values_mut() {
        offsets.sort_unstable();
        offsets.dedup();
    }
    Ok(types)
}

fn rust_types(source: &str) -> Result<BTreeMap<u64, Vec<usize>>, RiftError> {
    let query = RustQuery::new(
        "[(const_item name: (identifier) @declaration type: (_) @annotation)
          (static_item name: (identifier) @declaration type: (_) @annotation)
          (field_declaration name: (field_identifier) @declaration type: (_) @annotation)]
         [(type_identifier) (primitive_type)] @type_name",
    )?;
    let mut declaration = None;
    let mut annotations = BTreeMap::<u64, ByteRange>::new();
    let mut names = BTreeMap::new();
    for capture in query.captures(source, TYPE_CAPTURES_MAX)? {
        match capture.name.as_str() {
            "declaration" => declaration = Some(capture.range.start),
            "annotation" => {
                if let Some(name) = declaration.take() {
                    annotations.insert(name, capture.range);
                }
            }
            "type_name" => {
                names.insert(capture.range.start, capture.range.end);
            }
            _ => {}
        }
    }
    annotations
        .into_iter()
        .map(|(name, annotation)| {
            let offsets = names
                .range(annotation.start..annotation.end)
                .filter(|(_, end)| **end <= annotation.end)
                .map(|(start, _)| usize::try_from(*start).map_err(|_| super::graph_call_coverage()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((name, offsets))
        })
        .collect()
}

pub(super) async fn read(
    session: &mut EngineSession,
    path: &rift_core::ProjectPath,
    positions: &[InventoryPosition],
    requests: &[(rift_core::ProjectPath, Position)],
    maximum: usize,
    report: &mut InventoryReport,
) -> Result<(), RiftError> {
    let inferred: Vec<_> = positions
        .iter()
        .zip(requests)
        .filter(|(position, _)| position.declared_type.is_none())
        .collect();
    if session.capabilities().definitions.type_definition && !inferred.is_empty() {
        let requests: Vec<_> = inferred
            .iter()
            .map(|(_, request)| (*request).clone())
            .collect();
        let answers = session.type_definition_batch(&requests).await?;
        for ((position, _), answer) in inferred.into_iter().zip(answers) {
            report.charge(answer.len().saturating_add(1))?;
            report.types.push((position.symbol.clone(), answer));
        }
    }
    if !session.capabilities().definitions.definition {
        return Ok(());
    }
    let encoding = session.capabilities().position_encoding;
    let declared: Vec<_> = positions
        .iter()
        .flat_map(|position| {
            position
                .declared_type
                .iter()
                .flatten()
                .map(move |(utf8, utf16)| {
                    let at = match encoding {
                        PositionEncoding::Utf8 => *utf8,
                        PositionEncoding::Utf16 => *utf16,
                    };
                    (&position.symbol, (path.clone(), at))
                })
        })
        .collect();
    for chunk in declared.chunks(maximum) {
        let requests: Vec<_> = chunk.iter().map(|(_, request)| request.clone()).collect();
        let answers = session.definition_batch(&requests).await?;
        for ((seed, _), answer) in chunk.iter().zip(answers) {
            report.charge(answer.len().saturating_add(1))?;
            report.types.push(((*seed).clone(), answer));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_function_annotations_are_requested_at_their_type_positions()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join("src"))?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='types'\nversion='0.1.0'\n",
        )?;
        let source =
            "pub struct Input; pub struct Output; pub fn owner(arg: Input) -> Output { Output }\n";
        std::fs::write(directory.path().join("src/lib.rs"), source)?;
        let reads = crate::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )?;
        let file = reads
            .index()
            .file(&rift_core::ProjectPath::new("src/lib.rs")?)
            .ok_or("file")?;
        let types = super::declared_types(file)?;
        let owner = source.find("owner").ok_or("owner")? as u64;
        let offsets = types.get(&owner).ok_or("function type positions")?;
        let names: Vec<_> = offsets
            .iter()
            .map(|offset| {
                source[*offset..]
                    .split(|c: char| !c.is_alphanumeric())
                    .next()
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(names, ["Input", "Output"]);
        Ok(())
    }

    #[test]
    fn nested_values_select_their_own_declared_types() -> Result<(), Box<dyn std::error::Error>> {
        let source = "struct Outer; struct Inner;\nfn owner() -> Outer {\n const LIMIT: usize = 32;\n const VALUE: Inner = Inner;\n static POINTER: Option<Inner> = None;\n Outer\n}\nstruct Holder { field: Inner }\n";
        let types = super::rust_types(source)?;
        for (name, expected) in [
            ("LIMIT", vec!["usize"]),
            ("VALUE", vec!["Inner"]),
            ("POINTER", vec!["Option", "Inner"]),
            ("field", vec!["Inner"]),
        ] {
            let offsets = &types[&(source.find(name).ok_or("name")? as u64)];
            let actual: Vec<_> = offsets
                .iter()
                .map(|offset| {
                    source[*offset..]
                        .split(|c: char| !c.is_alphanumeric())
                        .next()
                        .unwrap_or_default()
                })
                .collect();
            assert_eq!(actual, expected, "{name}");
        }
        assert_eq!(types.len(), 4);
        Ok(())
    }
}
