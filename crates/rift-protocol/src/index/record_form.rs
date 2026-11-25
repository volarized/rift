//! Serialization of package publications in source unit blocks.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, value::RawValue};

use super::{
    LEGACY_PACKAGE_PUBLICATION_FORMAT_REVISION, PACKAGE_DOCUMENTS_CEILING,
    PACKAGE_PUBLICATION_FORMAT_REVISION, PACKAGE_SOURCE_BYTES_CEILING, PACKAGE_SYMBOLS_CEILING,
    PACKAGE_UNITS_CEILING, PackageAnalysisWarning, PackageArtifact, PackageCoverage,
    PackageDocument, PackageOccurrence, PackagePublication, PackageSourceUnit, PackageSymbol,
    StoredEdge,
};
use crate::canonical::{canonical_json, record_digest_without_member};
use crate::documentation::DocumentationIndex;
use crate::identity::{SymbolOwner, released_source_identity};
use crate::read::{
    Digest, PackageIdentity, RuntimeIdentity, SourceKind, SourceLocationKind, Symbol, SymbolId,
    SymbolKind, SymbolOrigin,
};
use crate::revision::Revision;

#[cfg(test)]
mod tests;

const UNIT_ROWS: [&str; 4] = ["declarations", "edges", "occurrences", "documents"];

type Object = Map<String, Value>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRecordsInput<'input> {
    header: Header,
    #[serde(borrow)]
    records: &'input RawValue,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format_revision: u32,
    identity_format: u32,
    analyzer_revision: Digest,
    owner: SymbolOwner,
    revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    source_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact: Option<PackageArtifact>,
    coverage: Vec<PackageCoverage>,
    documentation: DocumentationIndex,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<PackageAnalysisWarning>,
    counts: Counts,
    source_bytes: u64,
    canonical_bytes: usize,
}

impl Header {
    fn new(publication: &PackagePublication, counts: Counts, source_bytes: u64) -> Self {
        Self {
            format_revision: PACKAGE_PUBLICATION_FORMAT_REVISION,
            identity_format: publication.identity_format,
            analyzer_revision: publication.analyzer_revision.clone(),
            owner: publication.owner.clone(),
            revision: publication.revision.clone(),
            target: publication.target.clone(),
            source_digest: publication.source_digest.clone(),
            artifact: publication.artifact.clone(),
            coverage: publication.coverage.clone(),
            documentation: publication.documentation.clone(),
            warnings: publication.warnings.clone(),
            counts,
            source_bytes,
            canonical_bytes: 0,
        }
    }

    fn restore(self, records: Expanded) -> PackagePublication {
        PackagePublication {
            format_revision: self.format_revision,
            identity_format: self.identity_format,
            analyzer_revision: self.analyzer_revision,
            owner: self.owner,
            revision: self.revision,
            target: self.target,
            source_digest: self.source_digest,
            artifact: self.artifact,
            coverage: self.coverage,
            documentation: self.documentation,
            warnings: self.warnings,
            units: records.units,
            objects: records.objects,
            declarations: records.declarations,
            edges: records.edges,
            occurrences: records.occurrences,
            documents: records.documents,
        }
    }
}

#[derive(Serialize)]
struct Records {
    units: Vec<Object>,
    objects: Vec<Object>,
    edges: Vec<Object>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordsInput<'input> {
    #[serde(borrow)]
    units: Vec<&'input RawValue>,
    #[serde(borrow)]
    objects: Vec<&'input RawValue>,
    #[serde(borrow)]
    edges: Vec<&'input RawValue>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    units: usize,
    objects: usize,
    declarations: usize,
    edges: usize,
    occurrences: usize,
    documents: usize,
}

impl Counts {
    fn validate(&self, source_bytes: u64) -> Result<(), RiftError> {
        let units = u64::try_from(self.units).unwrap_or(u64::MAX);
        if units > u64::from(PACKAGE_UNITS_CEILING)
            || self.objects > PACKAGE_SYMBOLS_CEILING as usize
            || self.declarations > PACKAGE_SYMBOLS_CEILING as usize
            || self.documents > PACKAGE_DOCUMENTS_CEILING as usize
            || u64::try_from(self.edges).unwrap_or(u64::MAX) > crate::source::SOURCE_EDGES_MAX
            || u64::try_from(self.occurrences).unwrap_or(u64::MAX) > crate::source::SOURCE_EDGES_MAX
            || source_bytes > units.saturating_mul(u64::from(PACKAGE_SOURCE_BYTES_CEILING))
        {
            return Err(invalid("publication record counts exceed their bounds"));
        }
        Ok(())
    }
}

fn invalid(violation: &'static str) -> RiftError {
    errors::analysis::package_artifact_invalid()
        .field("records")
        .violation(violation)
        .error()
}

fn json_error(error: &serde_json::Error) -> RiftError {
    errors::analysis::package_artifact_invalid()
        .field("records")
        .violation(format!(
            "invalid publication JSON on line {} at column {}",
            error.line(),
            error.column()
        ))
        .error()
}

/// Encodes a package publication in the canonical form for its revision.
///
/// A retained flat publication keeps its original format so an exact read can verify
/// its stored artifact address. Fresh publications write records under source units.
///
/// # Errors
/// Refuses an unsupported revision or records the selected format cannot represent.
pub fn encode_package_publication(publication: &PackagePublication) -> Result<String, RiftError> {
    match publication.format_revision {
        PACKAGE_PUBLICATION_FORMAT_REVISION => encode_package_records(publication),
        LEGACY_PACKAGE_PUBLICATION_FORMAT_REVISION if publication.target.is_none() => {
            canonical_json(publication).map_err(|error| json_error(&error))
        }
        _ => Err(invalid("publication record format revision differs")),
    }
}

#[derive(Deserialize)]
struct EncodedHeader<'input> {
    #[serde(default, borrow)]
    header: Option<&'input RawValue>,
}

/// Reads a canonical package publication and restores its logical records.
///
/// Callers apply their artifact byte bound before decoding. The returned publication
/// keeps the format revision of the stored bytes.
///
/// # Errors
/// Refuses an unsupported revision or a malformed publication.
pub fn decode_package_publication(bytes: &[u8]) -> Result<PackagePublication, RiftError> {
    let encoded: EncodedHeader<'_> =
        serde_json::from_slice(bytes).map_err(|error| json_error(&error))?;
    if encoded.header.is_some() {
        return decode_package_records(bytes);
    }
    let publication: PackagePublication =
        serde_json::from_slice(bytes).map_err(|error| json_error(&error))?;
    if publication.format_revision != LEGACY_PACKAGE_PUBLICATION_FORMAT_REVISION
        || publication.target.is_some()
    {
        return Err(invalid("publication record format revision differs"));
    }
    Ok(publication)
}

/// Writes a publication with its rows grouped by source unit.
///
/// # Errors
/// Refuses a row that names an absent source unit, or a field that the record form cannot write.
fn encode_package_records(publication: &PackagePublication) -> Result<String, RiftError> {
    let context = Context::new(&publication.owner, &publication.revision)?;
    let mut objects = encode_rows(&publication.objects)?;
    let mut declarations = encode_rows(&publication.declarations)?;
    let mut edges = encode_rows(&publication.edges)?;
    let mut occurrences = encode_rows(&publication.occurrences)?;
    let mut documents = encode_rows(&publication.documents)?;
    let counts = Counts {
        units: publication.units.len(),
        objects: objects.len(),
        declarations: declarations.len(),
        edges: edges.len(),
        occurrences: occurrences.len(),
        documents: documents.len(),
    };
    let base = unit_bases(publication);
    let EncodedUnits {
        mut units,
        by_unit,
        source_bytes,
    } = encode_units(publication, &context, &declarations, &base)?;
    counts.validate(source_bytes)?;
    for row in &mut objects {
        context.shorten(row, &["id", "container"], None)?;
        shorten_origin(row, &context.origin, true)?;
    }
    for row in &mut declarations {
        let id = take_text(row, "unit")?;
        let unit = unit_for(&mut units, &by_unit, &id)?;
        let module = base.get(id.as_str()).copied();
        context.shorten(row, &["symbol"], module)?;
        shorten_origin(row, unit.get("origin").unwrap_or(&context.origin), false)?;
        shorten_source(row, unit)?;
        shorten_range(row, "range")?;
        set_record_digest(row)?;
        append_row(unit, "declarations", std::mem::take(row))?;
    }
    let mut loose = Vec::new();
    for row in &mut edges {
        if let Some(occurrence) = row.remove("occurrence") {
            let mut occurrence = object(occurrence)?;
            let id = take_text(&mut occurrence, "unit")?;
            let unit = unit_for(&mut units, &by_unit, &id)?;
            let module = base.get(id.as_str()).copied();
            context.shorten(row, &["from", "to"], module)?;
            shorten_range(&mut occurrence, "range")?;
            row.insert(
                "occurrence".into(),
                occurrence
                    .remove("range")
                    .ok_or_else(|| invalid("record has no range"))?,
            );
            append_row(unit, "edges", std::mem::take(row))?;
        } else {
            context.shorten(row, &["from", "to"], None)?;
            loose.push(std::mem::take(row));
        }
    }
    for row in &mut occurrences {
        let id = take_text(row, "unit")?;
        let unit = unit_for(&mut units, &by_unit, &id)?;
        context.shorten(row, &["symbol"], base.get(id.as_str()).copied())?;
        shorten_range(row, "range")?;
        append_row(unit, "occurrences", std::mem::take(row))?;
    }
    for row in &mut documents {
        let id = take_text(row, "unit")?;
        let unit = unit_for(&mut units, &by_unit, &id)?;
        if text(row, "kind")? == "file" {
            if take_text(row, "identity")? != id {
                return Err(invalid("file document identity differs from its unit"));
            }
            if row.get("file_content") == unit.get("source") {
                row.remove("file_content");
                row.insert("file_content_from_unit".into(), true.into());
            }
        } else {
            context.shorten(row, &["identity"], base.get(id.as_str()).copied())?;
        }
        set_record_digest(row)?;
        append_row(unit, "documents", std::mem::take(row))?;
    }
    for unit in &mut units {
        set_record_digest(unit)?;
    }
    encode_frame(
        Header::new(publication, counts, source_bytes),
        &Records {
            units,
            objects,
            edges: loose,
        },
    )
}

fn encode_frame(mut header: Header, records: &Records) -> Result<String, RiftError> {
    const PREFIX: &str = "{\"header\":";
    const RECORDS: &str = ",\"records\":";
    let records = canonical_json(records).map_err(|error| json_error(&error))?;
    let first = canonical_json(&header).map_err(|error| json_error(&error))?;
    let fixed_bytes = PREFIX.len() + first.len() - 1 + RECORDS.len() + records.len() + 1;
    let mut length = fixed_bytes + 1;
    loop {
        let next = fixed_bytes + length.to_string().len();
        if next == length {
            break;
        }
        length = next;
    }
    header.canonical_bytes = length;
    let header = canonical_json(&header).map_err(|error| json_error(&error))?;
    let mut artifact = String::with_capacity(length);
    artifact.push_str(PREFIX);
    artifact.push_str(&header);
    artifact.push_str(RECORDS);
    artifact.push_str(&records);
    artifact.push('}');
    Ok(artifact)
}

struct EncodedUnits {
    units: Vec<Object>,
    by_unit: BTreeMap<String, usize>,
    source_bytes: u64,
}

fn encode_units(
    publication: &PackagePublication,
    context: &Context,
    declarations: &[Object],
    base: &BTreeMap<&str, &str>,
) -> Result<EncodedUnits, RiftError> {
    let mut units = encode_rows(&publication.units)?;
    let mut by_unit = BTreeMap::new();
    let mut source_bytes = 0_u64;
    for (index, unit) in units.iter_mut().enumerate() {
        let id = take_text(unit, "unit")?;
        let path = text(unit, "path")?;
        if released_source_identity(&publication.owner, path)? != id
            || by_unit.insert(id, index).is_some()
        {
            return Err(invalid("source unit path does not establish one identity"));
        }
        source_bytes = count_unit_source_bytes(unit, source_bytes)?;
    }
    let mut origins = BTreeMap::<usize, Option<Value>>::new();
    for row in declarations {
        let index = *by_unit
            .get(text(row, "unit")?)
            .ok_or_else(|| invalid("record source unit is absent"))?;
        let origin = row
            .get("origin")
            .ok_or_else(|| invalid("declaration has no origin"))?;
        origins
            .entry(index)
            .and_modify(|prior| {
                if prior.as_ref() != Some(origin) {
                    *prior = None;
                }
            })
            .or_insert_with(|| Some(origin.clone()));
    }
    for (index, unit) in units.iter_mut().enumerate() {
        if let Some(base) = base.get(publication.units[index].unit.0.as_str()) {
            unit.insert("base".into(), context.short_symbol(base, None).into());
        }
        if let Some(Some(origin)) = origins.remove(&index)
            && origin != context.origin
        {
            unit.insert("origin".into(), origin);
        }
    }
    Ok(EncodedUnits {
        units,
        by_unit,
        source_bytes,
    })
}

/// Reads a unit-block publication with the original source and declaration identities.
///
/// # Errors
/// Refuses a different format revision, invalid counts, or a record with invalid ownership,
/// source ranges, or fields.
fn decode_package_records(bytes: &[u8]) -> Result<PackagePublication, RiftError> {
    let PublicationRecordsInput { header, records } =
        serde_json::from_slice(bytes).map_err(|error| json_error(&error))?;
    if header.format_revision != PACKAGE_PUBLICATION_FORMAT_REVISION {
        return Err(invalid("publication record format revision differs"));
    }
    if header.canonical_bytes != bytes.len() {
        return Err(invalid("publication byte count differs"));
    }
    let counts = &header.counts;
    counts.validate(header.source_bytes)?;
    let context = Context::new(&header.owner, &header.revision)?;
    let mut expanded = Expanded::default();
    // The raw records value was validated as JSON without allocating its rows.
    // The header bounds are checked first. Each object or unit block is decoded
    // as it is restored to the logical publication.
    let records: RecordsInput<'_> =
        serde_json::from_str(records.get()).map_err(|error| json_error(&error))?;
    if records.units.len() != counts.units
        || records.objects.len() != counts.objects
        || records.edges.len() > counts.edges
    {
        return Err(invalid("publication record counts differ"));
    }
    for raw in records.objects {
        let mut row = serde_json::from_str(raw.get()).map_err(|error| json_error(&error))?;
        context.expand(&mut row, &["id", "container"], None)?;
        expand_origin(&mut row, &context.origin);
        expanded.objects.push(decode_row(row)?);
    }
    for raw in records.edges {
        let mut row: Object =
            serde_json::from_str(raw.get()).map_err(|error| json_error(&error))?;
        if row.contains_key("occurrence") {
            return Err(invalid("top-level edge has a source occurrence"));
        }
        context.expand(&mut row, &["from", "to"], None)?;
        expanded.edges.push(decode_row(row)?);
    }
    let total_source = restore_units(records.units, &header.owner, &context, &mut expanded)?;
    if expanded.counts() != *counts || total_source != header.source_bytes {
        return Err(invalid("publication record counts differ"));
    }
    let mut publication = header.restore(expanded);
    publication.declarations.sort_by(|a, b| {
        (&a.symbol, &a.unit, a.range.start, a.range.end).cmp(&(
            &b.symbol,
            &b.unit,
            b.range.start,
            b.range.end,
        ))
    });
    publication.occurrences.sort_by(|a, b| {
        (&a.unit, a.range.start, a.range.end, &a.symbol, &a.producer).cmp(&(
            &b.unit,
            b.range.start,
            b.range.end,
            &b.symbol,
            &b.producer,
        ))
    });
    publication
        .documents
        .sort_by(|a, b| (a.kind, &a.identity).cmp(&(b.kind, &b.identity)));
    publication.validate_facts()?;
    Ok(publication)
}

fn restore_units(
    blocks: Vec<&RawValue>,
    owner: &SymbolOwner,
    context: &Context,
    expanded: &mut Expanded,
) -> Result<u64, RiftError> {
    let mut total_source = 0_u64;
    let mut units = BTreeSet::new();
    for raw in blocks {
        let mut unit = serde_json::from_str(raw.get()).map_err(|error| json_error(&error))?;
        verify_record_digest(&mut unit)?;
        let path = text(&unit, "path")?;
        let id = released_source_identity(owner, path)?;
        if !units.insert(id.clone()) {
            return Err(invalid("publication repeats a source unit"));
        }
        let module = unit
            .remove("base")
            .map(|module| {
                let value = module
                    .as_str()
                    .ok_or_else(|| invalid("unit base is not a symbol"))?;
                let base = context.full_symbol(value, None)?;
                if !base.starts_with(&context.owner_prefix) {
                    return Err(invalid("unit base belongs to another owner"));
                }
                SymbolId::parse(&base)?;
                Ok(base)
            })
            .transpose()?;
        total_source = count_unit_source_bytes(&unit, total_source)?;
        let origin = unit
            .remove("origin")
            .unwrap_or_else(|| context.origin.clone());
        for field in UNIT_ROWS {
            let rows = take_rows(&mut unit, field)?;
            expanded.extend(
                field,
                rows,
                &UnitRows {
                    unit: &unit,
                    id: &id,
                    base: module.as_deref(),
                    origin: &origin,
                },
                context,
            )?;
        }
        insert_derived(&mut unit, "unit", id.into())?;
        set_record_digest(&mut unit)?;
        expanded.units.push(decode_row(unit)?);
    }
    Ok(total_source)
}

struct Context {
    owner_prefix: String,
    origin: Value,
}

impl Context {
    fn new(owner: &SymbolOwner, revision: &Revision) -> Result<Self, RiftError> {
        if !revision.answers_for(owner) {
            return Err(invalid("publication revision belongs to another owner"));
        }
        let version = revision
            .release_version()
            .ok_or_else(|| invalid("publication does not name a release"))?;
        let (location, package, runtime) = match owner {
            SymbolOwner::Package {
                manager,
                registry,
                name,
            } => (
                SourceLocationKind::Dependency,
                Some(PackageIdentity {
                    manager: manager.clone(),
                    registry: registry.clone(),
                    name: name.clone(),
                    version: version.to_owned(),
                }),
                None,
            ),
            SymbolOwner::Runtime { runtime } => (
                SourceLocationKind::Stdlib,
                None,
                Some(RuntimeIdentity {
                    runtime: runtime.clone(),
                    version: version.to_owned(),
                }),
            ),
            _ => return Err(invalid("publication owner is not a release owner")),
        };
        let origin = serde_json::to_value(SymbolOrigin {
            location: Some(location),
            package,
            runtime,
            source_kind: SourceKind::Authored,
        })
        .map_err(|error| json_error(&error))?;
        Ok(Self {
            owner_prefix: format!("rift://symbol/{}/", owner.wire_owner()),
            origin,
        })
    }

    fn short_symbol(&self, id: &str, base: Option<&str>) -> String {
        if let Some(suffix) = base.and_then(|base| id.strip_prefix(base))
            && (suffix.is_empty() || suffix.starts_with('/'))
        {
            format!("~{suffix}")
        } else {
            id.strip_prefix(&self.owner_prefix).unwrap_or(id).to_owned()
        }
    }

    fn full_symbol(&self, id: &str, base: Option<&str>) -> Result<String, RiftError> {
        if let Some(suffix) = id.strip_prefix('~') {
            let base = base.ok_or_else(|| invalid("unit-relative symbol has no base"))?;
            if !suffix.is_empty() && !suffix.starts_with('/') {
                return Err(invalid("unit-relative symbol has no path separator"));
            }
            Ok(format!("{base}{suffix}"))
        } else if id.starts_with("rift://") {
            Ok(id.to_owned())
        } else {
            Ok(format!("{}{id}", self.owner_prefix))
        }
    }

    fn shorten(
        &self,
        row: &mut Object,
        fields: &[&str],
        base: Option<&str>,
    ) -> Result<(), RiftError> {
        for field in fields {
            if let Some(value) = row.get_mut(*field) {
                let id = value
                    .as_str()
                    .ok_or_else(|| invalid("identity is not a symbol"))?;
                *value = self.short_symbol(id, base).into();
            }
        }
        Ok(())
    }

    fn expand(
        &self,
        row: &mut Object,
        fields: &[&str],
        base: Option<&str>,
    ) -> Result<(), RiftError> {
        for field in fields {
            if let Some(value) = row.get_mut(*field) {
                let id = value
                    .as_str()
                    .ok_or_else(|| invalid("identity is not a symbol"))?;
                *value = self.full_symbol(id, base)?.into();
            }
        }
        Ok(())
    }
}

fn unit_bases(publication: &PackagePublication) -> BTreeMap<&str, &str> {
    let full_units: BTreeMap<_, _> = publication
        .units
        .iter()
        .filter(|unit| unit.source_complete)
        .map(|unit| (unit.unit.0.as_str(), unit.source.len()))
        .collect();
    let modules: BTreeSet<_> = publication
        .objects
        .iter()
        .filter(|symbol| symbol.kinds.contains(&SymbolKind::Module))
        .filter_map(|symbol| symbol.id.as_ref().map(|id| id.0.as_str()))
        .collect();
    let mut bases = BTreeMap::new();
    for declaration in &publication.declarations {
        let unit = declaration.unit.0.as_str();
        let symbol = declaration.symbol.0.as_str();
        if declaration.range.start != 0
            || usize::try_from(declaration.range.end).ok().as_ref() != full_units.get(unit)
            || !modules.contains(symbol)
        {
            continue;
        }
        bases
            .entry(unit)
            .and_modify(|previous| {
                if *previous != Some(symbol) {
                    *previous = None;
                }
            })
            .or_insert(Some(symbol));
    }
    bases
        .into_iter()
        .filter_map(|(unit, base)| Some((unit, base?)))
        .collect()
}

fn shorten_origin(row: &mut Object, inherited: &Value, object: bool) -> Result<(), RiftError> {
    if row.get("origin") == Some(inherited) {
        row.remove("origin");
    } else if object && !row.contains_key("origin") {
        let project = SymbolOrigin {
            location: Some(SourceLocationKind::Project),
            package: None,
            runtime: None,
            source_kind: SourceKind::Authored,
        };
        row.insert(
            "origin".into(),
            serde_json::to_value(project).map_err(|error| json_error(&error))?,
        );
    }
    Ok(())
}

fn expand_origin(row: &mut Object, inherited: &Value) {
    if !row.contains_key("origin") {
        row.insert("origin".into(), inherited.clone());
    }
}

fn count_unit_source_bytes(unit: &Object, total: u64) -> Result<u64, RiftError> {
    let bytes = u64::try_from(text(unit, "source")?.len()).unwrap_or(u64::MAX);
    if bytes > u64::from(PACKAGE_SOURCE_BYTES_CEILING) {
        return Err(invalid("unit source byte count exceeds its bound"));
    }
    total
        .checked_add(bytes)
        .ok_or_else(|| invalid("publication source byte count exceeds its bound"))
}

fn shorten_source(row: &mut Object, unit: &Object) -> Result<(), RiftError> {
    if !complete(row) || !complete(unit) {
        return Ok(());
    }
    let range = row
        .get("range")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("declaration has no range"))?;
    if text(unit, "source")?.get(source_range(range)?) == Some(text(row, "source")?) {
        row.remove("source");
    }
    Ok(())
}

fn source_range(range: &Object) -> Result<std::ops::Range<usize>, RiftError> {
    let offset = |field| {
        range
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| invalid("source range has an invalid offset"))
    };
    Ok(offset("start")?..offset("end")?)
}

fn complete(row: &Object) -> bool {
    row.get("source_complete").and_then(Value::as_bool) == Some(true)
}

fn shorten_range(row: &mut Object, field: &str) -> Result<(), RiftError> {
    let range = row
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("record has no range"))?;
    let range = source_range(range)?;
    let length = range
        .end
        .checked_sub(range.start)
        .ok_or_else(|| invalid("range is reversed"))?;
    row.insert(field.into(), serde_json::json!([range.start, length]));
    Ok(())
}

fn full_range(value: Value) -> Result<Value, RiftError> {
    let [start, length] =
        serde_json::from_value::<[u64; 2]>(value).map_err(|error| json_error(&error))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| invalid("range end exceeds its bound"))?;
    Ok(serde_json::json!({"start": start, "end": end}))
}

fn expand_range(row: &mut Object, field: &str) -> Result<(), RiftError> {
    let value = row
        .remove(field)
        .ok_or_else(|| invalid("record has no range"))?;
    row.insert(field.into(), full_range(value)?);
    Ok(())
}

#[derive(Default)]
struct Expanded {
    units: Vec<PackageSourceUnit>,
    objects: Vec<Symbol>,
    declarations: Vec<PackageSymbol>,
    edges: Vec<StoredEdge>,
    occurrences: Vec<PackageOccurrence>,
    documents: Vec<PackageDocument>,
}

struct UnitRows<'a> {
    unit: &'a Object,
    id: &'a str,
    base: Option<&'a str>,
    origin: &'a Value,
}

impl Expanded {
    fn extend(
        &mut self,
        field: &str,
        rows: Vec<Object>,
        block: &UnitRows<'_>,
        context: &Context,
    ) -> Result<(), RiftError> {
        for mut row in rows {
            match field {
                "declarations" => {
                    verify_record_digest(&mut row)?;
                    insert_derived(&mut row, "unit", block.id.into())?;
                    context.expand(&mut row, &["symbol"], block.base)?;
                    expand_origin(&mut row, block.origin);
                    expand_range(&mut row, "range")?;
                    if !row.contains_key("source") {
                        restore_source(&mut row, block.unit)?;
                    }
                    set_record_digest(&mut row)?;
                    self.declarations.push(decode_row(row)?);
                }
                "edges" => {
                    context.expand(&mut row, &["from", "to"], block.base)?;
                    let range = row
                        .remove("occurrence")
                        .ok_or_else(|| invalid("unit edge has no occurrence"))?;
                    row.insert(
                        "occurrence".into(),
                        serde_json::json!({"unit": block.id, "range": full_range(range)?}),
                    );
                    self.edges.push(decode_row(row)?);
                }
                "occurrences" => {
                    insert_derived(&mut row, "unit", block.id.into())?;
                    context.expand(&mut row, &["symbol"], block.base)?;
                    expand_range(&mut row, "range")?;
                    self.occurrences.push(decode_row(row)?);
                }
                "documents" => {
                    verify_record_digest(&mut row)?;
                    restore_document(&mut row, block, context)?;
                    set_record_digest(&mut row)?;
                    self.documents.push(decode_row(row)?);
                }
                _ => return Err(invalid("unknown unit record list")),
            }
        }
        Ok(())
    }

    fn counts(&self) -> Counts {
        Counts {
            units: self.units.len(),
            objects: self.objects.len(),
            declarations: self.declarations.len(),
            edges: self.edges.len(),
            occurrences: self.occurrences.len(),
            documents: self.documents.len(),
        }
    }
}

fn set_record_digest(row: &mut Object) -> Result<(), RiftError> {
    row.remove("digest");
    let digest = record_digest_without_member(row).map_err(|error| json_error(&error))?;
    row.insert("digest".into(), digest.0.into());
    Ok(())
}

fn verify_record_digest(row: &mut Object) -> Result<(), RiftError> {
    let digest = take_text(row, "digest")?;
    let actual = record_digest_without_member(row).map_err(|error| json_error(&error))?;
    if digest != actual.0 {
        return Err(invalid("publication record digest differs"));
    }
    Ok(())
}

fn restore_source(row: &mut Object, unit: &Object) -> Result<(), RiftError> {
    if !complete(row) || !complete(unit) {
        return Err(invalid("declaration source is missing from a cut unit"));
    }
    let range = row
        .get("range")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("declaration has no range"))?;
    let source = text(unit, "source")?
        .get(source_range(range)?)
        .ok_or_else(|| invalid("declaration range is outside unit source"))?;
    row.insert("source".into(), source.into());
    Ok(())
}

fn restore_document(
    row: &mut Object,
    block: &UnitRows<'_>,
    context: &Context,
) -> Result<(), RiftError> {
    insert_derived(row, "unit", block.id.into())?;
    if text(row, "kind")? == "file" {
        insert_derived(row, "identity", block.id.into())?;
        if let Some(from_unit) = row.remove("file_content_from_unit") {
            if from_unit != Value::Bool(true) || row.contains_key("file_content") {
                return Err(invalid("file document has two content values"));
            }
            row.insert("file_content".into(), text(block.unit, "source")?.into());
        }
    } else {
        context.expand(row, &["identity"], block.base)?;
    }
    Ok(())
}

fn insert_derived(row: &mut Object, field: &str, value: Value) -> Result<(), RiftError> {
    if row.contains_key(field) {
        return Err(invalid("record repeats a field supplied by its unit"));
    }
    row.insert(field.into(), value);
    Ok(())
}

fn object(value: Value) -> Result<Object, RiftError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(invalid("record is not an object")),
    }
}

fn take_rows(row: &mut Object, field: &str) -> Result<Vec<Object>, RiftError> {
    match row.remove(field) {
        Some(Value::Array(rows)) => rows.into_iter().map(object).collect(),
        Some(_) => Err(invalid("unit record list is not an array")),
        None => Ok(Vec::new()),
    }
}

fn append_row(unit: &mut Object, field: &str, row: Object) -> Result<(), RiftError> {
    let rows = unit
        .entry(field)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| invalid("unit record list is not an array"))?;
    rows.push(Value::Object(row));
    Ok(())
}

fn text<'row>(row: &'row Object, field: &str) -> Result<&'row str, RiftError> {
    row.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("record text is absent"))
}

fn take_text(row: &mut Object, field: &str) -> Result<String, RiftError> {
    match row.remove(field) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(invalid("record text is absent")),
    }
}

fn unit_for<'units>(
    units: &'units mut [Object],
    by_unit: &BTreeMap<String, usize>,
    id: &str,
) -> Result<&'units mut Object, RiftError> {
    let index = *by_unit
        .get(id)
        .ok_or_else(|| invalid("record source unit is absent"))?;
    units
        .get_mut(index)
        .ok_or_else(|| invalid("source unit index exceeds its bound"))
}

fn encode_rows<T: Serialize>(rows: &[T]) -> Result<Vec<Object>, RiftError> {
    rows.iter()
        .map(|row| object(serde_json::to_value(row).map_err(|error| json_error(&error))?))
        .collect()
}

fn decode_row<T: DeserializeOwned>(row: Object) -> Result<T, RiftError> {
    serde_json::from_value(Value::Object(row)).map_err(|error| json_error(&error))
}
