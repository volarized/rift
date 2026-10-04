//! Single-line facts of symbols, places, commits, and documentation blocks.
//!
//! Each function turns typed values into the short forms the text layouts share. Facts that hold
//! the normal state are not written: the structured content carries them.

use rift_protocol::documentation::{
    DocumentationBlock, DocumentationContentIdentity, DocumentationHeading,
    DocumentationSourceIdentity,
};
use rift_protocol::read::{
    CommitAuthor, Documentation, PackageIdentity, ProjectPath, Signature, SourceKind,
    SourceLocationKind, SourceUnitId, Symbol, SymbolFacet, SymbolId, SymbolOrigin,
};
use serde::Serialize;

use crate::output::text::TextError;

/// Separates the facts of one line.
pub(super) const FACT_SEPARATOR: &str = " · ";
/// Characters of a hash that stay when it is cut.
const HASH_CUT: usize = 8;
/// Characters of `YYYY-MM-DDTHH:MM:SS`.
const TIMESTAMP_CLOCK_CHARS: usize = 19;
/// Characters of `YYYY-MM-DD`.
const TIMESTAMP_DATE_CHARS: usize = 10;
/// Characters of `YYYY-MM-DDTHH:MM`.
const TIMESTAMP_MINUTE_CHARS: usize = 16;

/// Facts joined into one line by the fact separator.
#[derive(Debug, Default)]
pub(super) struct Facts {
    text: String,
}

impl Facts {
    /// Appends `fact`. Empty text appends nothing.
    pub(super) fn push(&mut self, fact: &str) {
        if fact.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str(FACT_SEPARATOR);
        }
        self.text.push_str(fact);
    }

    /// Appends every fact of `other`.
    pub(super) fn extend(&mut self, other: &Self) {
        self.push(&other.text);
    }

    /// The facts as one line, empty when there are none.
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }
}

/// The wire spelling of a unit enum variant.
///
/// # Errors
///
/// Returns the failure when `value` does not serialize as one string.
pub(super) fn wire_name<T: Serialize>(value: &T) -> Result<String, TextError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => Ok(name),
        _ => Err(TextError::Unsupported("enum without a unit wire name")),
    }
}

/// The wire spellings of `values` joined by a comma and a space.
///
/// # Errors
///
/// Fails like [`wire_name`].
pub(super) fn wire_names<T: Serialize>(values: &[T]) -> Result<String, TextError> {
    wire_names_joined(values, ", ")
}

/// The wire spellings of `values` joined by `separator`.
///
/// # Errors
///
/// Fails like [`wire_name`].
pub(super) fn wire_names_joined<T: Serialize>(
    values: &[T],
    separator: &str,
) -> Result<String, TextError> {
    let names = values
        .iter()
        .map(wire_name)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.join(separator))
}

/// The wire spelling of `value` with underscores written as spaces.
///
/// # Errors
///
/// Fails like [`wire_name`].
pub(super) fn spaced_name<T: Serialize>(value: &T) -> Result<String, TextError> {
    Ok(wire_name(value)?.replace('_', " "))
}

/// The first 8 characters of `text` when it is lowercase hex of at least 8 characters.
///
/// Anything else, such as a branch name, is returned unchanged.
pub(super) fn cut_hash(text: &str) -> &str {
    let hex = |byte: &u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte);
    if text.len() >= HASH_CUT && text.bytes().all(|byte| hex(&byte)) {
        text.get(..HASH_CUT).unwrap_or(text)
    } else {
        text
    }
}

/// True when `text` has the shape `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`.
fn is_timestamp(text: &str) -> bool {
    let Some(clock) = text.get(..TIMESTAMP_CLOCK_CHARS) else {
        return false;
    };
    let clock_shaped = clock.bytes().enumerate().all(|(index, byte)| match index {
        4 | 7 => byte == b'-',
        10 => byte == b'T',
        13 | 16 => byte == b':',
        _ => byte.is_ascii_digit(),
    });
    let rest = text.get(TIMESTAMP_CLOCK_CHARS..).unwrap_or_default();
    clock_shaped && is_offset(without_fraction(rest))
}

/// `rest` without a leading fraction of seconds, or `rest` itself when it has none.
fn without_fraction(rest: &str) -> &str {
    let Some(fraction) = rest.strip_prefix('.') else {
        return rest;
    };
    let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return rest;
    }
    fraction.get(digits..).unwrap_or_default()
}

/// True when `text` is `Z` or a signed `HH:MM` offset.
fn is_offset(text: &str) -> bool {
    match text.as_bytes() {
        [b'Z'] => true,
        [sign, tens, ones, b':', minute_tens, minute_ones] => {
            matches!(sign, b'+' | b'-')
                && [tens, ones, minute_tens, minute_ones]
                    .iter()
                    .all(|digit| digit.is_ascii_digit())
        }
        _ => false,
    }
}

/// The date of an RFC 3339 timestamp, or the whole text when it has another shape.
pub(super) fn date_of(timestamp: &str) -> &str {
    if is_timestamp(timestamp) {
        timestamp.get(..TIMESTAMP_DATE_CHARS).unwrap_or(timestamp)
    } else {
        timestamp
    }
}

/// An RFC 3339 timestamp as `YYYY-MM-DD HH:MM offset`, or the whole text when it has another
/// shape.
pub(super) fn moment_of(timestamp: &str) -> String {
    if !is_timestamp(timestamp) {
        return timestamp.to_owned();
    }
    let date = timestamp.get(..TIMESTAMP_DATE_CHARS).unwrap_or_default();
    let minute = timestamp
        .get(TIMESTAMP_DATE_CHARS + 1..TIMESTAMP_MINUTE_CHARS)
        .unwrap_or_default();
    let offset = without_fraction(timestamp.get(TIMESTAMP_CLOCK_CHARS..).unwrap_or_default());
    format!("{date} {minute} {offset}")
}

/// Wire spelling of the availability that is the normal state of a package.
pub(super) const CANONICAL_AVAILABILITY: &str = "canonical";

/// A package context entry as `name@version` or `name requirement`, then its manager.
///
/// The manager sits in parentheses, with the availability after it unless the availability is
/// canonical. An empty `version` or `requirement` is absent. Warnings and the map write package
/// entries through this one function.
pub(super) fn package_text(
    (manager, name): (&str, &str),
    (version, requirement): (&str, &str),
    availability: &str,
) -> String {
    let selector = match (version, requirement) {
        ("", "") => name.to_owned(),
        ("", requirement) => format!("{name} {requirement}"),
        (version, _) => format!("{name}@{version}"),
    };
    if availability == CANONICAL_AVAILABILITY {
        format!("{selector} ({manager})")
    } else {
        format!("{selector} ({manager}, {availability})")
    }
}

/// An author as `name <email>`.
pub(super) fn author_of(author: &CommitAuthor) -> String {
    let CommitAuthor { name, email } = author;
    format!("{name} <{email}>")
}

/// `path:line`, `unit:line`, or just the path or unit when `line` is absent.
///
/// Empty when the hit has neither a path nor a unit. A path wins over a unit.
pub(super) fn location(
    path: Option<&ProjectPath>,
    unit: Option<&SourceUnitId>,
    line: Option<u64>,
) -> String {
    let place = path
        .map(|path| path.0.as_str())
        .or_else(|| unit.map(|unit| unit.0.as_str()));
    match (place, line) {
        (Some(place), Some(line)) => format!("{place}:{line}"),
        (Some(place), None) => place.to_owned(),
        (None, _) => String::new(),
    }
}

/// A documentation block as its place, its heading path, and the symbol it documents.
pub(super) struct Block<'a> {
    /// `path:line` or `unit:line` of the block.
    pub(super) place: String,
    /// The headings of the block joined by a greater-than sign, empty without headings.
    pub(super) headings: String,
    /// The symbol an attached comment documents.
    pub(super) symbol: Option<&'a SymbolId>,
}

/// The place, headings, and symbol of `block`.
pub(super) fn block_of(block: &DocumentationBlock) -> Block<'_> {
    let DocumentationBlock {
        identity: _,
        source,
        content_digest: _,
        heading_path,
        range: _,
        line,
        kind: _,
        language: _,
        chunks: _,
        symbol,
    } = block;
    let DocumentationContentIdentity { source, cell: _ } = source;
    let place = match source {
        DocumentationSourceIdentity::Project { path } => path.0.as_str(),
        DocumentationSourceIdentity::Package { unit } => unit.0.as_str(),
    };
    Block {
        place: format!("{place}:{line}"),
        headings: headings(heading_path),
        symbol: symbol.as_ref(),
    }
}

/// The heading names joined by a greater-than sign between spaces.
fn headings(path: &[DocumentationHeading]) -> String {
    let names: Vec<&str> = path
        .iter()
        .map(|heading| {
            let DocumentationHeading { level: _, name } = heading;
            name.as_str()
        })
        .collect();
    names.join(" > ")
}

/// What the text of a symbol says about it beyond its place.
pub(super) struct Described<'a> {
    /// The signature display with visibility, or the visibility, kind, and name.
    pub(super) declaration: String,
    /// The symbol identity, when the symbol has one.
    pub(super) id: Option<&'a SymbolId>,
    /// The first non-empty line of the first documentation text.
    pub(super) summary: Option<&'a str>,
    /// Every documentation text, in order.
    pub(super) documentation: &'a [Documentation],
    /// Origin, generation, deprecation, test, and document-local facts that hold.
    pub(super) facts: Facts,
}

/// Describes `symbol` for the text layouts.
///
/// # Errors
///
/// Fails when an enum field has no unit wire name.
pub(super) fn describe(symbol: &Symbol) -> Result<Described<'_>, TextError> {
    let Symbol {
        id,
        language: _,
        name,
        kind,
        facets,
        origin,
        container: _,
        modifiers: _,
        visibility,
        types: _,
        signatures,
        documentation,
        extensions: _,
        document_local,
    } = symbol;
    let mut facts = Facts::default();
    facts.push(&origin_fact(origin)?);
    facts.push(source_kind_fact(origin.source_kind));
    if facets.contains(&SymbolFacet::Deprecated) {
        facts.push("deprecated");
    }
    if facets.contains(&SymbolFacet::Test) {
        facts.push("test");
    }
    if *document_local {
        facts.push("document local");
    }
    Ok(Described {
        declaration: declaration(visibility.as_deref(), (&kind.0, name), signatures),
        id: id.as_ref(),
        summary: documentation
            .first()
            .and_then(|first| summary_of(&first.text)),
        documentation,
        facts,
    })
}

/// The first line of `text` that holds a character other than whitespace, trimmed.
fn summary_of(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The declaration line: the first signature, or the visibility, kind, and name.
fn declaration(
    visibility: Option<&str>,
    (kind, name): (&str, &str),
    signatures: &[Signature],
) -> String {
    let visibility = visibility.filter(|visibility| !visibility.is_empty());
    match signatures.first() {
        Some(signature) => {
            let display = display_of(signature);
            match visibility {
                Some(visibility) if !starts_with_word(display, visibility) => {
                    format!("{visibility} {display}")
                }
                _ => display.to_owned(),
            }
        }
        None => match visibility {
            Some(visibility) => format!("{visibility} {kind} {name}"),
            None => format!("{kind} {name}"),
        },
    }
}

/// The text a reader sees for `signature`.
fn display_of(signature: &Signature) -> &str {
    let Signature {
        display,
        links: _,
        language: _,
        receiver: _,
        parameters: _,
        returns: _,
        type_parameters: _,
        throws: _,
        effects: _,
        extensions: _,
    } = signature;
    display
}

/// True when `text` starts with `word` and the next character does not continue a word.
fn starts_with_word(text: &str, word: &str) -> bool {
    text.strip_prefix(word).is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || next == '_'))
    })
}

/// The location kind and package of an origin that is not the project default.
///
/// The default is a project declaration without a package; its facts are not written.
fn origin_fact(origin: &SymbolOrigin) -> Result<String, TextError> {
    let SymbolOrigin {
        location,
        package,
        source_kind: _,
    } = origin;
    let shown_location = match location {
        Some(SourceLocationKind::Project) if package.is_none() => None,
        other => other.as_ref(),
    };
    let mut parts = Vec::new();
    if let Some(location) = shown_location {
        parts.push(wire_name(location)?);
    }
    if let Some(PackageIdentity {
        manager: _,
        name,
        version,
    }) = package
    {
        parts.push(format!("{name}@{version}"));
    }
    Ok(parts.join(" "))
}

/// `generated` or `synthetic` for a declaration that is not authored, else empty.
const fn source_kind_fact(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Authored => "",
        SourceKind::Generated => "generated",
        SourceKind::Synthetic => "synthetic",
    }
}
