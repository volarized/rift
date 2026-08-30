//! Generic syntax facts one provider emits for one source file.

use std::collections::{HashMap, HashSet};

use rift_binding::UnitBindingFacts;
use rift_core::ProjectPath;
use rift_protocol::read::{Documentation, Language, Signature, SymbolFacet};

/// The character a qualified name carries its disambiguating number after,
/// the `~N` suffix `SymbolId` advertises.
const DUPLICATE_SUFFIX_MARKER: char = '~';

/// Half-open UTF-8 byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    /// First included byte.
    pub start: u64,
    /// First excluded byte.
    pub end: u64,
}

impl ByteRange {
    /// Reports whether byte position belongs to range.
    #[must_use]
    pub const fn contains(self, position: u64) -> bool {
        self.start <= position && position < self.end
    }
}

/// One named syntax-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNode {
    /// Grammar node kind, in the producing provider's vocabulary.
    pub kind: String,
    /// Node byte range.
    pub range: ByteRange,
    /// Parent index in the document node vector; `None` for the root.
    pub parent: Option<usize>,
    /// Whether the parser marked the node erroneous or missing.
    pub has_error: bool,
}

/// One named declaration extracted from a source file.
///
/// `Eq` is not derived: `signatures` and `documentation` carry
/// [`rift_protocol::read::Extensions`], whose `serde_json::Value` payload
/// implements only `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxSymbol {
    /// Declared short name.
    pub name: String,
    /// Container-qualified name, unique within the file's symbol space.
    pub qualified_name: String,
    /// The containing symbol's qualified name; `None` for a declaration at
    /// the top level of the file.
    pub container: Option<String>,
    /// The provider's kind word, such as `function`; the wire kind composes
    /// it as `{language}.{kind}`.
    pub kind: &'static str,
    /// Portable categories this declaration falls into, in the provider's
    /// declared order.
    pub facets: Vec<SymbolFacet>,
    /// Authored visibility spelling, such as `pub(crate)`; `None` when the
    /// language states no visibility.
    pub visibility: Option<String>,
    /// Complete declaration byte range, extended over attached outer
    /// attributes and outer doc comments.
    pub range: ByteRange,
    /// The item node's own byte range, excluding any attached outer
    /// attributes and doc comments. Equal to `range` when nothing attaches.
    pub item_range: ByteRange,
    /// The implementation part: the grammar's body or value field; `None`
    /// for a declaration without one.
    pub body_range: Option<ByteRange>,
    /// Callable forms this declaration renders as. Empty for a declaration
    /// the grammar does not mark callable, or one with no attached
    /// implementation.
    pub signatures: Vec<Signature>,
    /// Doc comments the grammar attaches to this declaration, stripped of
    /// comment syntax. Empty when nothing attaches.
    pub documentation: Vec<Documentation>,
}

/// Immutable syntax facts for one source file.
///
/// `Eq` is not derived: [`SyntaxSymbol`] is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxDocument {
    language: Language,
    path: ProjectPath,
    nodes: Vec<SyntaxNode>,
    symbols: Vec<SyntaxSymbol>,
    has_errors: bool,
    binding: Option<UnitBindingFacts>,
}

/// Suffixes every repeated qualified name apart, in source order.
///
/// A qualified name is the address half of a declaration's identity, so two
/// declarations spelling one name would mint one identity. A name exactly
/// one declaration spells keeps its spelling. A name several declarations
/// spell is held by none of them: each takes a `~N` suffix counting from 1
/// in source order, so a hand-built address carrying the bare name resolves
/// to nothing rather than silently to the first declaration. Only
/// `qualified_name` changes: ranges, containers, and kinds stay as the
/// provider extracted them. Whole names are compared, never parsed, so a
/// name the source already spells with a `~` is an ordinary name here.
///
/// Every provider funnels through [`SyntaxDocument::new`], so the rule holds
/// for every language without a provider restating it.
///
/// The pass needs no budget of its own: the provider's node bound already
/// fixed how many declarations reach here, and the search for an unused
/// number cannot outrun the names already taken.
fn suffix_duplicate_qualified_names(symbols: &mut [SyntaxSymbol]) {
    let occurrences = qualified_name_occurrences(symbols);
    let mut taken = kept_names(&occurrences);
    let mut counts: HashMap<String, u32> = HashMap::new();
    for symbol in symbols {
        if occurrences.get(&symbol.qualified_name) == Some(&1) {
            continue;
        }
        let count = counts.entry(symbol.qualified_name.clone()).or_default();
        let suffixed = unused_suffixed_name(&symbol.qualified_name, count, &taken);
        taken.insert(suffixed.clone());
        symbol.qualified_name = suffixed;
    }
}

/// How many declarations spell each qualified name in one document.
fn qualified_name_occurrences(symbols: &[SyntaxSymbol]) -> HashMap<String, usize> {
    let mut occurrences: HashMap<String, usize> = HashMap::with_capacity(symbols.len());
    for symbol in symbols {
        *occurrences
            .entry(symbol.qualified_name.clone())
            .or_default() += 1;
    }
    occurrences
}

/// The names one declaration each spells, which keep their spelling and are
/// therefore unavailable to a suffix.
fn kept_names(occurrences: &HashMap<String, usize>) -> HashSet<String> {
    occurrences
        .iter()
        .filter(|(_, occurrence_count)| **occurrence_count == 1)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The first `{name}~{number}` past `count` that no declaration holds,
/// leaving `count` at the number it took.
///
/// Counting continues past a number another declaration keeps, so a suffix
/// this pass writes can never repeat a name the source authored.
fn unused_suffixed_name(name: &str, count: &mut u32, taken: &HashSet<String>) -> String {
    loop {
        *count += 1;
        let suffixed = format!("{name}{DUPLICATE_SUFFIX_MARKER}{count}");
        if !taken.contains(&suffixed) {
            return suffixed;
        }
    }
}

impl SyntaxDocument {
    /// Assembles one document from a provider's extracted facts, suffixing
    /// every repeated qualified name apart so each declaration addresses one
    /// identity.
    pub(crate) fn new(
        language: Language,
        path: ProjectPath,
        nodes: Vec<SyntaxNode>,
        mut symbols: Vec<SyntaxSymbol>,
        has_errors: bool,
    ) -> Self {
        suffix_duplicate_qualified_names(&mut symbols);
        Self {
            language,
            path,
            nodes,
            symbols,
            has_errors,
            binding: None,
        }
    }

    /// Attaches the unit's extracted name-binding facts.
    #[must_use]
    pub(crate) fn with_binding(mut self, facts: UnitBindingFacts) -> Self {
        self.binding = Some(facts);
        self
    }

    /// Returns the unit's name-binding facts; `None` when the provider extracts none.
    #[must_use]
    pub const fn binding(&self) -> Option<&UnitBindingFacts> {
        self.binding.as_ref()
    }

    /// Returns the language identity these facts are filed under.
    #[must_use]
    pub const fn language(&self) -> &Language {
        &self.language
    }

    /// Returns source path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Returns every named syntax node in pre-order.
    #[must_use]
    pub fn nodes(&self) -> &[SyntaxNode] {
        &self.nodes
    }

    /// Returns extracted declarations in source order.
    #[must_use]
    pub fn symbols(&self) -> &[SyntaxSymbol] {
        &self.symbols
    }

    /// Reports whether parser observed malformed syntax.
    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Returns nodes covering byte position, outermost first.
    #[must_use]
    pub fn nodes_at(&self, position: u64) -> Vec<&SyntaxNode> {
        self.nodes
            .iter()
            .filter(|node| node.range.contains(position))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use rift_core::{encode_path, symbol_identity};

    use super::*;

    /// The literal characters `SymbolId`'s advertised pattern accepts in the
    /// segments after the language, transcribed from `rift-protocol`, with
    /// `%` for the escape form it also allows.
    const SYMBOL_ID_TAIL_CHARACTERS: &str = "._~!$&'()*+,;=:/@-%";

    fn language() -> Language {
        Language {
            name: "yaml".to_owned(),
            dialect: None,
        }
    }

    fn path() -> ProjectPath {
        ProjectPath::new(".github/dependabot.yml").expect("valid fixture path")
    }

    /// One declaration spelling `qualified_name`, spanning one byte at
    /// `start` so source order stays readable in an assertion.
    fn symbol(qualified_name: &str, start: u64) -> SyntaxSymbol {
        let range = ByteRange {
            start,
            end: start + 1,
        };
        SyntaxSymbol {
            name: qualified_name.to_owned(),
            qualified_name: qualified_name.to_owned(),
            container: None,
            kind: "mapping_entry",
            facets: Vec::new(),
            visibility: None,
            range,
            item_range: range,
            body_range: None,
            signatures: Vec::new(),
            documentation: Vec::new(),
        }
    }

    /// A document whose declarations spell `names` in source order.
    fn document(names: &[&str]) -> SyntaxDocument {
        let symbols = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let start = u64::try_from(index).expect("fixture index fits a byte offset");
                symbol(name, start)
            })
            .collect();
        SyntaxDocument::new(language(), path(), Vec::new(), symbols, false)
    }

    fn qualified_names(document: &SyntaxDocument) -> Vec<&str> {
        document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect()
    }

    #[test]
    fn test_a_document_without_repeated_names_keeps_every_qualified_name() {
        let document = document(&["version", "updates", "updates > directory"]);
        assert_eq!(
            qualified_names(&document),
            ["version", "updates", "updates > directory"]
        );
    }

    #[test]
    fn test_an_empty_symbol_list_stays_empty() {
        let document = document(&[]);
        assert!(document.symbols().is_empty());
    }

    #[test]
    fn test_a_single_declaration_keeps_its_qualified_name() {
        let document = document(&["updates"]);
        assert_eq!(qualified_names(&document), ["updates"]);
    }

    /// No member of a repeated name holds the bare name: both take a
    /// suffix, so an address carrying the bare name resolves to neither.
    #[test]
    fn test_two_declarations_under_one_name_both_take_suffixes() {
        let document = document(&["port", "port"]);
        assert_eq!(qualified_names(&document), ["port~1", "port~2"]);
    }

    #[test]
    fn test_four_declarations_under_one_name_number_in_source_order() {
        let document = document(&["port", "port", "port", "port"]);
        assert_eq!(
            qualified_names(&document),
            ["port~1", "port~2", "port~3", "port~4"]
        );
    }

    /// A name one declaration spells stays byte-identical beside a repeated
    /// name that every member of suffixes.
    #[test]
    fn test_a_name_one_declaration_spells_stays_bare_beside_a_repeated_name() {
        let document = document(&["version", "port", "port"]);
        assert_eq!(qualified_names(&document), ["version", "port~1", "port~2"]);
    }

    /// Two repeated names interleaved in one document count apart from each
    /// other: each name's own declarations number it.
    #[test]
    fn test_interleaved_repeated_names_number_independently() {
        let document = document(&["port", "host", "port", "host", "port"]);
        assert_eq!(
            qualified_names(&document),
            ["port~1", "host~1", "port~2", "host~2", "port~3"]
        );
    }

    /// The pass compares whole qualified names and appends, so a name the
    /// source already spells with the suffix character is an ordinary name.
    #[test]
    fn test_a_name_already_carrying_the_suffix_character_is_not_parsed() {
        let document = document(&["a~b", "a~b", "a~b"]);
        assert_eq!(qualified_names(&document), ["a~b~1", "a~b~2", "a~b~3"]);
    }

    /// A name one declaration spells as `foo~1` keeps that spelling, so the
    /// repeated `foo` counts past it. The result does not depend on where
    /// the kept name sits in source order.
    #[test]
    fn test_a_repeated_name_counts_past_a_suffix_another_declaration_keeps() {
        let kept_first = document(&["foo~1", "foo", "foo"]);
        assert_eq!(qualified_names(&kept_first), ["foo~1", "foo~2", "foo~3"]);
        let kept_last = document(&["foo", "foo", "foo~1"]);
        assert_eq!(qualified_names(&kept_last), ["foo~2", "foo~3", "foo~1"]);
    }

    /// Source order, not iteration order, decides which declaration takes
    /// which suffix: the suffixes ascend with the declaration spans.
    #[test]
    fn test_source_order_decides_which_declaration_takes_which_suffix() {
        let document = document(&["port", "port", "port"]);
        let numbered = document
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.range.start))
            .collect::<Vec<_>>();
        assert_eq!(numbered, [("port~1", 0), ("port~2", 1), ("port~3", 2)]);
    }

    /// Only the qualified name moves: the container, kind, and every span
    /// stay as the provider extracted them.
    #[test]
    fn test_a_suffixed_declaration_keeps_every_other_fact() {
        let document = document(&["port", "port"]);
        let suffixed = &document.symbols()[1];
        assert_eq!(suffixed.qualified_name, "port~2");
        assert_eq!(suffixed.name, "port");
        assert_eq!(suffixed.container, None);
        assert_eq!(suffixed.kind, "mapping_entry");
        assert_eq!(suffixed.range, ByteRange { start: 1, end: 2 });
        assert_eq!(suffixed.item_range, ByteRange { start: 1, end: 2 });
    }

    /// The suffix marker reaches the wire identity literally: `encode_path`
    /// keeps `~` out of its escape set, and the minted identity carries
    /// only characters `SymbolId`'s pattern accepts.
    #[test]
    fn test_a_suffixed_name_reaches_the_wire_identity_unescaped() {
        let document = document(&["updates > package-ecosystem", "updates > package-ecosystem"]);
        let suffixed = document.symbols()[0].qualified_name.as_str();
        assert_eq!(suffixed, "updates > package-ecosystem~1");
        assert_eq!(
            encode_path(suffixed),
            "updates%20%3E%20package-ecosystem~1",
            "encode_path keeps the suffix marker literal"
        );
        let identity = symbol_identity("yaml", path().as_str(), suffixed);
        assert_eq!(
            identity,
            "rift://symbol/yaml/.github/dependabot.yml/updates%20%3E%20package-ecosystem~1"
        );
        let tail = identity
            .strip_prefix("rift://symbol/yaml/")
            .expect("the identity files under its language segment");
        assert!(
            tail.chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || SYMBOL_ID_TAIL_CHARACTERS.contains(character)),
            "every character must be one SymbolId's pattern accepts: identity={identity}"
        );
    }
}
