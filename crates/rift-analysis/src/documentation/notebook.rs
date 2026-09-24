//! Bounded decoding of selected notebook cell sources.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_protocol::documentation::{
    DOCUMENTATION_SOURCE_BYTES_MAX, DocumentationContentIdentity, DocumentationCoverage,
    DocumentationWarning, NOTEBOOK_CELL_ID_BYTES_MAX, NotebookCell, NotebookCellIdentity,
    NotebookCellKind,
};
use rift_protocol::read::{Language, TextRange};
use tree_sitter::{Language as Grammar, Node, Parser, Tree};

use super::failure::{DocumentationError, DocumentationViolation, refused};

const DOCUMENT_KIND: &str = "document";
const OBJECT_KIND: &str = "object";
const PAIR_KIND: &str = "pair";
const ARRAY_KIND: &str = "array";
const STRING_KIND: &str = "string";
const KEY_FIELD: &str = "key";
const VALUE_FIELD: &str = "value";

const SOURCE_NODES_MAX: usize = 250_000;
const SOURCE_DEPTH_MAX: usize = 512;

#[derive(Debug)]
struct JsonKinds {
    document: u16,
    object: u16,
    pair: u16,
    array: u16,
    string: u16,
    key: NonZeroU16,
    value: NonZeroU16,
}

impl JsonKinds {
    fn resolve(language: &Grammar) -> Self {
        Self {
            document: kind_id(language, DOCUMENT_KIND),
            object: kind_id(language, OBJECT_KIND),
            pair: kind_id(language, PAIR_KIND),
            array: kind_id(language, ARRAY_KIND),
            string: kind_id(language, STRING_KIND),
            key: field_id(language, KEY_FIELD),
            value: field_id(language, VALUE_FIELD),
        }
    }
}

fn kind_id(language: &Grammar, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(id != 0, "pinned JSON grammar must define node kind: {kind}");
    id
}

fn field_id(language: &Grammar, field: &str) -> NonZeroU16 {
    language
        .field_id_for_name(field)
        .unwrap_or_else(|| panic!("pinned JSON grammar must define field: {field}"))
}

fn json_kinds() -> &'static JsonKinds {
    static KINDS: OnceLock<JsonKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar: Grammar = tree_sitter_json::LANGUAGE.into();
        JsonKinds::resolve(&grammar)
    })
}

/// One selected cell's decoded source and original JSON source ranges.
#[derive(Clone, Debug, PartialEq)]
pub struct NotebookCellContent {
    text: String,
    cell: NotebookCell,
    physical_ranges: Vec<TextRange>,
    declared_language: Option<Language>,
}

impl NotebookCellContent {
    /// Returns exact decoded source bytes as UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns cell identity and content kind.
    #[must_use]
    pub const fn cell(&self) -> &NotebookCell {
        &self.cell
    }

    /// Returns original JSON string-token ranges in source order.
    #[must_use]
    pub fn physical_ranges(&self) -> &[TextRange] {
        &self.physical_ranges
    }

    /// Returns validated language declared by notebook metadata.
    #[must_use]
    pub const fn declared_language(&self) -> Option<&Language> {
        self.declared_language.as_ref()
    }
}

/// Selected notebook cells and parser coverage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotebookContent {
    cells: Vec<NotebookCellContent>,
    coverage: DocumentationCoverage,
    warnings: Vec<DocumentationWarning>,
}

impl NotebookContent {
    /// Returns selected markdown and code cells in notebook order.
    #[must_use]
    pub fn cells(&self) -> &[NotebookCellContent] {
        &self.cells
    }

    /// Returns source parsing coverage.
    #[must_use]
    pub const fn coverage(&self) -> &DocumentationCoverage {
        &self.coverage
    }

    /// Returns bounded parser warnings.
    #[must_use]
    pub fn warnings(&self) -> &[DocumentationWarning] {
        &self.warnings
    }
}

#[derive(Clone, Copy)]
struct SelectedCell<'tree> {
    source: Node<'tree>,
    id: Option<Node<'tree>>,
    index: u32,
    kind: NotebookCellKind,
}

/// Decodes selected markdown and code cells without decoding notebook outputs.
///
/// Tree-sitter owns notebook structure and original token spans. Each selected
/// JSON string is decoded independently; source arrays concatenate without
/// inserted separators. Input bytes, syntax nodes, depth, and retained source
/// ranges stay within declared bounds.
///
/// # Errors
///
/// Returns a typed refusal for malformed JSON, unsupported notebook shape, or
/// a source, syntax, depth, or range bound.
pub fn decode_notebook(
    source: &str,
    notebook_identity: &DocumentationContentIdentity,
) -> Result<NotebookContent, DocumentationError> {
    validate_input(source, notebook_identity)?;
    let tree = parse_tree(source)?;
    decode_tree(tree.root_node(), source)
}

fn validate_input(
    source: &str,
    notebook_identity: &DocumentationContentIdentity,
) -> Result<(), DocumentationError> {
    if source.len() > DOCUMENTATION_SOURCE_BYTES_MAX as usize {
        return Err(refused(
            DocumentationViolation::LimitExceeded,
            "notebook.source_bytes",
        ));
    }
    if notebook_identity.cell.is_some() {
        return Err(refused(
            DocumentationViolation::Notebook,
            "notebook.identity",
        ));
    }
    Ok(())
}

fn parse_tree(source: &str) -> Result<Tree, DocumentationError> {
    let kinds = json_kinds();
    let mut parser = Parser::new();
    let grammar: Grammar = tree_sitter_json::LANGUAGE.into();
    parser
        .set_language(&grammar)
        .map_err(|_| refused(DocumentationViolation::Notebook, "notebook.grammar"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.parse"))?;
    let root = tree.root_node();
    validate_tree(root)?;
    if root.has_error() || root.kind_id() != kinds.document {
        return Err(refused(DocumentationViolation::Notebook, "notebook.json"));
    }
    Ok(tree)
}

fn decode_tree(root: Node<'_>, source: &str) -> Result<NotebookContent, DocumentationError> {
    let kinds = json_kinds();
    let document = root
        .named_child(0)
        .filter(|node| node.kind_id() == kinds.object)
        .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.object"))?;
    let cells_node = object_value(document, "cells", source, kinds)?
        .filter(|node| node.kind_id() == kinds.array)
        .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.cells"))?;
    let language = notebook_language(document, source, kinds)?;
    let cells = decode_cells(cells_node, language.as_ref(), source, kinds)?;
    let selected_count = u32::try_from(cells.len())
        .map_err(|_| refused(DocumentationViolation::LimitExceeded, "notebook.cells"))?;
    Ok(NotebookContent {
        cells,
        coverage: DocumentationCoverage {
            selected: selected_count,
            parsed: 1,
            ..DocumentationCoverage::default()
        },
        warnings: Vec::new(),
    })
}

fn decode_cells(
    cells_node: Node<'_>,
    language: Option<&Language>,
    source: &str,
    kinds: &JsonKinds,
) -> Result<Vec<NotebookCellContent>, DocumentationError> {
    let selected = selected_cells(cells_node, source, kinds)?;
    let id_counts = authored_id_counts(&selected, source, kinds)?;
    let mut cells = Vec::with_capacity(selected.len());
    for selected_cell in selected {
        cells.push(decode_cell(
            selected_cell,
            &id_counts,
            language,
            source,
            kinds,
        )?);
    }
    Ok(cells)
}

fn decode_cell(
    selected: SelectedCell<'_>,
    id_counts: &BTreeMap<String, u32>,
    language: Option<&Language>,
    source: &str,
    kinds: &JsonKinds,
) -> Result<NotebookCellContent, DocumentationError> {
    let authored_id = selected
        .id
        .map(|node| decode_string(source, node, kinds))
        .transpose()?;
    let identity = cell_identity(authored_id, selected.index, id_counts);
    let (text, physical_ranges) = decode_source(selected.source, source, kinds)?;
    Ok(NotebookCellContent {
        text,
        cell: NotebookCell {
            identity,
            kind: selected.kind,
        },
        physical_ranges,
        declared_language: (selected.kind == NotebookCellKind::Code)
            .then(|| language.cloned())
            .flatten(),
    })
}

fn validate_tree(root: Node<'_>) -> Result<(), DocumentationError> {
    let mut pending = vec![(root, 0_usize)];
    let mut nodes_seen = 0_usize;
    while let Some((node, depth)) = pending.pop() {
        nodes_seen = nodes_seen
            .checked_add(1)
            .ok_or_else(|| refused(DocumentationViolation::LimitExceeded, "notebook.nodes"))?;
        if nodes_seen > SOURCE_NODES_MAX {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "notebook.nodes",
            ));
        }
        if depth > SOURCE_DEPTH_MAX {
            return Err(refused(
                DocumentationViolation::LimitExceeded,
                "notebook.depth",
            ));
        }
        let children = node.child_count();
        for index in 0..children {
            let child_index = u32::try_from(index)
                .map_err(|_| refused(DocumentationViolation::LimitExceeded, "notebook.nodes"))?;
            if let Some(child) = node.child(child_index) {
                pending.push((child, depth + 1));
            }
        }
    }
    Ok(())
}

fn selected_cells<'tree>(
    cells: Node<'tree>,
    source: &str,
    kinds: &JsonKinds,
) -> Result<Vec<SelectedCell<'tree>>, DocumentationError> {
    let mut selected = Vec::new();
    for index in 0..cells.named_child_count() {
        let cell_index = u32::try_from(index)
            .map_err(|_| refused(DocumentationViolation::LimitExceeded, "notebook.cells"))?;
        let cell = cells
            .named_child(cell_index)
            .filter(|node| node.kind_id() == kinds.object)
            .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.cell"))?;
        let cell_type = object_value(cell, "cell_type", source, kinds)?;
        let kind = match cell_type {
            Some(node) if node.kind_id() == kinds.string => {
                match decode_string(source, node, kinds)?.as_str() {
                    "markdown" => NotebookCellKind::Markdown,
                    "code" => NotebookCellKind::Code,
                    _ => continue,
                }
            }
            _ => continue,
        };
        let source_node = object_value(cell, "source", source, kinds)?
            .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.cell.source"))?;
        if source_node.kind_id() != kinds.string && source_node.kind_id() != kinds.array {
            return Err(refused(
                DocumentationViolation::Notebook,
                "notebook.cell.source",
            ));
        }
        selected.push(SelectedCell {
            source: source_node,
            id: object_value(cell, "id", source, kinds)?
                .filter(|node| node.kind_id() == kinds.string),
            index: cell_index,
            kind,
        });
    }
    Ok(selected)
}

fn authored_id_counts(
    selected: &[SelectedCell<'_>],
    source: &str,
    kinds: &JsonKinds,
) -> Result<BTreeMap<String, u32>, DocumentationError> {
    let mut counts = BTreeMap::new();
    for cell in selected {
        let Some(id) = cell.id else {
            continue;
        };
        let id = decode_string(source, id, kinds)?;
        if valid_authored_id(&id) {
            let count = counts.entry(id).or_insert(0_u32);
            *count = count.checked_add(1).ok_or_else(|| {
                refused(DocumentationViolation::LimitExceeded, "notebook.cell.id")
            })?;
        }
    }
    Ok(counts)
}

fn cell_identity(
    authored_id: Option<String>,
    index: u32,
    id_counts: &BTreeMap<String, u32>,
) -> NotebookCellIdentity {
    match authored_id {
        Some(id) if valid_authored_id(&id) && id_counts.get(&id) == Some(&1) => {
            NotebookCellIdentity::Authored { id }
        }
        _ => NotebookCellIdentity::Indexed { index },
    }
}

fn valid_authored_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= NOTEBOOK_CELL_ID_BYTES_MAX as usize
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

fn decode_source(
    source_node: Node<'_>,
    source: &str,
    kinds: &JsonKinds,
) -> Result<(String, Vec<TextRange>), DocumentationError> {
    let mut decoded = String::new();
    let mut physical_ranges = Vec::new();
    if source_node.kind_id() == kinds.string {
        append_string(
            source_node,
            source,
            kinds,
            &mut decoded,
            &mut physical_ranges,
        )?;
        return Ok((decoded, physical_ranges));
    }
    for index in 0..source_node.named_child_count() {
        let child_index = u32::try_from(index)
            .map_err(|_| refused(DocumentationViolation::LimitExceeded, "notebook.source"))?;
        let child = source_node
            .named_child(child_index)
            .filter(|node| node.kind_id() == kinds.string)
            .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.source"))?;
        append_string(child, source, kinds, &mut decoded, &mut physical_ranges)?;
    }
    Ok((decoded, physical_ranges))
}

fn append_string(
    node: Node<'_>,
    source: &str,
    kinds: &JsonKinds,
    decoded: &mut String,
    physical_ranges: &mut Vec<TextRange>,
) -> Result<(), DocumentationError> {
    decoded.push_str(&decode_string(source, node, kinds)?);
    let start = u64::try_from(node.start_byte()).map_err(|_| {
        refused(
            DocumentationViolation::LimitExceeded,
            "notebook.source_range",
        )
    })?;
    let end = u64::try_from(node.end_byte()).map_err(|_| {
        refused(
            DocumentationViolation::LimitExceeded,
            "notebook.source_range",
        )
    })?;
    physical_ranges.push(TextRange { start, end });
    Ok(())
}

fn notebook_language(
    document: Node<'_>,
    source: &str,
    kinds: &JsonKinds,
) -> Result<Option<Language>, DocumentationError> {
    let Some(metadata) = object_value(document, "metadata", source, kinds)? else {
        return Ok(None);
    };
    if metadata.kind_id() != kinds.object {
        return Ok(None);
    }
    let Some(language_info) = object_value(metadata, "language_info", source, kinds)? else {
        return Ok(None);
    };
    if language_info.kind_id() != kinds.object {
        return Ok(None);
    }
    let Some(name) = object_value(language_info, "name", source, kinds)? else {
        return Ok(None);
    };
    if name.kind_id() != kinds.string {
        return Ok(None);
    }
    let name = decode_string(source, name, kinds)?;
    let canonical_name = name.to_ascii_lowercase();
    Ok(Language::try_from(canonical_name).ok())
}

fn object_value<'tree>(
    object: Node<'tree>,
    name: &str,
    source: &str,
    kinds: &JsonKinds,
) -> Result<Option<Node<'tree>>, DocumentationError> {
    if object.kind_id() != kinds.object {
        return Ok(None);
    }
    let mut value = None;
    for index in 0..object.named_child_count() {
        let child_index = u32::try_from(index)
            .map_err(|_| refused(DocumentationViolation::LimitExceeded, "notebook.object"))?;
        let pair = object
            .named_child(child_index)
            .filter(|node| node.kind_id() == kinds.pair)
            .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.object"))?;
        let key = pair
            .child_by_field_id(kinds.key.get())
            .filter(|node| node.kind_id() == kinds.string)
            .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.object.key"))?;
        if decode_string(source, key, kinds)? == name {
            if value.is_some() {
                return Err(refused(
                    DocumentationViolation::Notebook,
                    "notebook.duplicate_field",
                ));
            }
            value = pair.child_by_field_id(kinds.value.get());
        }
    }
    Ok(value)
}

fn decode_string(
    source: &str,
    node: Node<'_>,
    kinds: &JsonKinds,
) -> Result<String, DocumentationError> {
    if node.kind_id() != kinds.string {
        return Err(refused(DocumentationViolation::Notebook, "notebook.string"));
    }
    let token = source
        .get(node.byte_range())
        .ok_or_else(|| refused(DocumentationViolation::Notebook, "notebook.string.range"))?;
    serde_json::from_str(token)
        .map_err(|_| refused(DocumentationViolation::Notebook, "notebook.string"))
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationSourceIdentity, NotebookCellIdentity,
        NotebookCellKind,
    };
    use rift_protocol::read::ProjectPath;

    use super::{NotebookCellContent, SOURCE_DEPTH_MAX, SOURCE_NODES_MAX, decode_notebook};
    use rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX;

    fn notebook_identity() -> DocumentationContentIdentity {
        DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath("examples/demo.ipynb".to_owned()),
            },
            cell: None,
        }
    }

    fn cell_identity(cell: &NotebookCellContent) -> &NotebookCellIdentity {
        &cell.cell().identity
    }

    #[test]
    fn notebook_decodes_selected_cell_sources_and_keeps_physical_ranges() {
        let source = r##"{"cells":[{"cell_type":"markdown","id":"intro-1","source":["# Caf\u00e9\r\n","body"]},{"cell_type":"code","source":"print(1)"},{"cell_type":"raw","source":"ignored"}],"metadata":{"language_info":{"name":"Python"}},"outputs":["ignored"]}"##;
        let content = decode_notebook(source, &notebook_identity()).expect("notebook");

        assert_eq!(content.cells().len(), 2);
        assert_eq!(content.cells()[0].text(), "# Café\r\nbody");
        assert_eq!(content.cells()[0].cell().kind, NotebookCellKind::Markdown);
        assert_eq!(
            cell_identity(&content.cells()[0]),
            &NotebookCellIdentity::Authored {
                id: "intro-1".to_owned()
            }
        );
        assert_eq!(content.cells()[0].physical_ranges().len(), 2);
        assert_eq!(
            &source[usize::try_from(content.cells()[0].physical_ranges()[0].start)
                .expect("range start")
                ..usize::try_from(content.cells()[0].physical_ranges()[0].end).expect("range end")],
            r##""# Caf\u00e9\r\n""##
        );
        assert_eq!(content.cells()[1].text(), "print(1)");
        assert_eq!(content.cells()[1].cell().kind, NotebookCellKind::Code);
        assert_eq!(
            content.cells()[1]
                .declared_language()
                .map(rift_core::Language::identity_segment),
            Some("python".to_owned())
        );
        assert_eq!(content.coverage().selected, 2);
        assert_eq!(content.coverage().parsed, 1);
        assert!(content.warnings().is_empty());
    }

    #[test]
    fn missing_invalid_and_duplicate_ids_use_original_cell_indexes() {
        let source = r#"{"cells":[{"cell_type":"markdown","id":"same","source":"a"},{"cell_type":"code","id":"same","source":["b"]},{"cell_type":"markdown","id":"bad id","source":"c"},{"cell_type":"markdown","source":"d"}],"metadata":{}}"#;
        let content = decode_notebook(source, &notebook_identity()).expect("notebook");

        for (position, expected_index) in [(0, 0), (1, 1), (2, 2), (3, 3)] {
            assert_eq!(
                cell_identity(&content.cells()[position]),
                &NotebookCellIdentity::Indexed {
                    index: expected_index
                }
            );
        }
    }

    #[test]
    fn cells_without_string_type_are_skipped() {
        let source = r#"{"cells":[{"source":"missing type"},{"cell_type":7,"source":"wrong type"}],"metadata":{}}"#;
        let content = decode_notebook(source, &notebook_identity()).expect("notebook");

        assert!(content.cells().is_empty());
        assert_eq!(content.coverage().selected, 0);
    }

    #[test]
    fn unique_valid_authored_id_and_string_source_are_preserved() {
        let source = r#"{"cells":[{"cell_type":"markdown","id":"valid_ID-2","source":"exact\ntext"}],"metadata":{}}"#;
        let content = decode_notebook(source, &notebook_identity()).expect("notebook");

        assert_eq!(content.cells()[0].text(), "exact\ntext");
        assert_eq!(content.cells()[0].physical_ranges().len(), 1);
        assert_eq!(
            cell_identity(&content.cells()[0]),
            &NotebookCellIdentity::Authored {
                id: "valid_ID-2".to_owned()
            }
        );
    }

    #[test]
    fn malformed_json_and_invalid_selected_source_refuse_without_partial_cells() {
        let malformed = r#"{"cells":["#;
        assert!(decode_notebook(malformed, &notebook_identity()).is_err());

        let invalid_source = r#"{"cells":[{"cell_type":"markdown","source":42}],"metadata":{}}"#;
        assert!(decode_notebook(invalid_source, &notebook_identity()).is_err());
    }

    #[test]
    fn duplicate_top_level_field_and_excessive_depth_are_refused() {
        let duplicate = r#"{"cells":[],"cells":[],"metadata":{}}"#;
        let duplicate_error =
            decode_notebook(duplicate, &notebook_identity()).expect_err("duplicate cells field");
        assert_eq!(duplicate_error.fault().field(), "notebook.duplicate_field");

        let nested = format!(
            "{{\"cells\":[],\"metadata\":{{}},\"extra\":{}0{}}}",
            "[".repeat(SOURCE_DEPTH_MAX + 1),
            "]".repeat(SOURCE_DEPTH_MAX + 1),
        );
        let depth_error = decode_notebook(&nested, &notebook_identity()).expect_err("depth bound");
        assert_eq!(depth_error.fault().field(), "notebook.depth");
    }

    #[test]
    fn excessive_json_node_count_is_refused_before_cell_selection() {
        let cell_count = SOURCE_NODES_MAX / 4 + 1;
        let raw_cell = r#"{"cell_type":"raw"}"#;
        let cells = std::iter::repeat_n(raw_cell, cell_count)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("{{\"cells\":[{cells}],\"metadata\":{{}}}}");
        assert!(source.len() < DOCUMENTATION_SOURCE_BYTES_MAX as usize);

        let error = decode_notebook(&source, &notebook_identity()).expect_err("node bound");
        assert_eq!(error.fault().field(), "notebook.nodes");
    }

    #[test]
    fn input_bounds_and_invalid_cell_ids_use_safe_fallbacks() {
        let too_large = " ".repeat(DOCUMENTATION_SOURCE_BYTES_MAX as usize + 1);
        let error = decode_notebook(&too_large, &notebook_identity())
            .expect_err("source byte bound is enforced before parsing");
        assert_eq!(error.fault().field(), "notebook.source_bytes");

        let mut with_cell_identity = notebook_identity();
        with_cell_identity.cell = Some(rift_protocol::documentation::NotebookCell {
            identity: NotebookCellIdentity::Indexed { index: 0 },
            kind: NotebookCellKind::Markdown,
        });
        let error = decode_notebook("{}", &with_cell_identity)
            .expect_err("input identity must name source, not one decoded cell");
        assert_eq!(error.fault().field(), "notebook.identity");

        let long_id =
            "a".repeat(rift_protocol::documentation::NOTEBOOK_CELL_ID_BYTES_MAX as usize + 1);
        let source = format!(
            r#"{{"cells":[{{"cell_type":"code","id":"{long_id}","source":"run()"}}],"metadata":{{"language_info":{{"name":7}}}}}}"#
        );
        let content = decode_notebook(&source, &notebook_identity()).expect("notebook");
        assert_eq!(
            cell_identity(&content.cells()[0]),
            &NotebookCellIdentity::Indexed { index: 0 }
        );
        assert_eq!(content.cells()[0].declared_language(), None);
    }
}
