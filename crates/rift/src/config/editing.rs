//! Parsed TOML value replacements preserving unrelated source bytes.

use std::ops::Range;
use std::path::Path;

use rift_core::line::{LineEnding, lines_inclusive};
use rift_error::RiftError;
use toml_edit::{Document, Item, Key, TableLike, Value};

pub(super) fn set(
    path: &Path,
    raw: &str,
    keys: &[&str],
    value: Value,
) -> Result<String, RiftError> {
    let document =
        Document::parse(raw).map_err(|error| super::lsp::invalid(path, &error.to_string()))?;
    let (range, replacement) = replacement(path, &document, keys, value)?;
    let mut rendered = raw.to_owned();
    rendered.replace_range(range, &replacement);
    Ok(rendered)
}

pub(super) fn lookup<'a>(document: &'a Document<&str>, keys: &[&str]) -> Option<&'a Item> {
    let mut table = document.as_table() as &dyn TableLike;
    for (index, key) in keys.iter().enumerate() {
        let item = table.get(key)?;
        if index + 1 == keys.len() {
            return Some(item);
        }
        table = item.as_table_like()?;
    }
    None
}

fn replacement(
    path: &Path,
    document: &Document<&str>,
    keys: &[&str],
    mut value: Value,
) -> Result<(Range<usize>, String), RiftError> {
    value.decor_mut().clear();
    let replacement = value.to_string();
    let mut table = document.as_table() as &dyn TableLike;
    let mut parent = None;
    for (index, key) in keys.iter().enumerate() {
        let Some(item) = table.get(key) else {
            return insert(path, document.raw(), parent, keys, index, &replacement);
        };
        if index + 1 == keys.len() {
            let range = item
                .as_value()
                .and_then(Value::span)
                .ok_or_else(|| super::lsp::invalid(path, "configuration target must be a value"))?;
            return Ok((range, replacement));
        }
        table = item
            .as_table_like()
            .ok_or_else(|| super::lsp::invalid(path, "configuration key must contain a table"))?;
        parent = Some(item);
    }
    unreachable!("configuration edits always carry at least one key")
}

fn insert(
    path: &Path,
    raw: &str,
    parent: Option<&Item>,
    keys: &[&str],
    missing: usize,
    value: &str,
) -> Result<(Range<usize>, String), RiftError> {
    match parent {
        Some(Item::Value(Value::InlineTable(table))) => {
            let range = table.span().ok_or_else(|| {
                super::lsp::invalid(path, "configuration inline table has no parsed span")
            })?;
            let position = range.start + 1;
            let separator = if table.is_empty() { "" } else { "," };
            let spacing = if raw
                .as_bytes()
                .get(position)
                .is_some_and(u8::is_ascii_whitespace)
            {
                ""
            } else {
                " "
            };
            Ok((
                position..position,
                format!(
                    " {} = {value}{separator}{spacing}",
                    key_path(&keys[missing..])
                ),
            ))
        }
        Some(Item::Table(table)) if table.is_dotted() => Ok((
            0..0,
            format!("{} = {value}{}", key_path(keys), super::line_ending(raw)),
        )),
        Some(Item::Table(table)) if !table.is_implicit() && missing + 1 == keys.len() => {
            let header = table.span().ok_or_else(|| {
                super::lsp::invalid(path, "configuration table has no parsed span")
            })?;
            let line = lines_inclusive(&raw[header.start..])
                .next()
                .unwrap_or_default();
            let position = header.start + line.len();
            let ending = super::line_ending(raw);
            let prefix = if LineEnding::of(line).is_some() {
                ""
            } else {
                ending
            };
            Ok((
                position..position,
                format!("{prefix}{} = {value}{ending}", key_path(&keys[missing..])),
            ))
        }
        _ => Ok(append(raw, keys, value)),
    }
}

fn append(raw: &str, keys: &[&str], value: &str) -> (Range<usize>, String) {
    let ending = super::line_ending(raw);
    let prefix = if raw.is_empty() || LineEnding::of(raw).is_some() {
        ""
    } else {
        ending
    };
    let (key, table) = keys
        .split_last()
        .unwrap_or_else(|| unreachable!("edits carry keys"));
    let header = if table.is_empty() {
        String::new()
    } else {
        format!("[{}]{ending}", key_path(table))
    };
    (
        raw.len()..raw.len(),
        format!("{prefix}{header}{} = {value}{ending}", Key::new(*key)),
    )
}

fn key_path(keys: &[&str]) -> String {
    keys.iter()
        .map(|key| Key::new(*key).to_string())
        .collect::<Vec<_>>()
        .join(".")
}

pub(super) fn remove(path: &Path, raw: &str, keys: &[&str]) -> Result<String, RiftError> {
    let document =
        Document::parse(raw).map_err(|error| super::lsp::invalid(path, &error.to_string()))?;
    let Some(range) = lookup(&document, keys)
        .and_then(Item::as_value)
        .and_then(Value::span)
    else {
        return Ok(raw.to_owned());
    };
    let mut offset = 0;
    let mut start = None;
    for line in lines_inclusive(raw) {
        let end = offset + line.len();
        if offset <= range.start && range.start < end {
            start = Some(offset);
        }
        if end >= range.end {
            let start = start.ok_or_else(|| {
                super::lsp::invalid(path, "configuration value has no source line")
            })?;
            let mut rendered = raw.to_owned();
            rendered.replace_range(start..end, "");
            return Ok(rendered);
        }
        offset = end;
    }
    Err(super::lsp::invalid(
        path,
        "configuration value exceeds its source",
    ))
}
