//! The text one declaration is embedded as, and the key that addresses it.
//!
//! A vector is addressed by the digest of the text it came from, not by the
//! declaration's identity. A rename, a move, and a reformat that leaves the
//! declaration's own bytes alone all resolve to the digest already stored, so
//! a refresh embeds only what actually changed.

use data_encoding::HEXLOWER;
use rayon::prelude::*;
use sha2::{Digest as _, Sha256};

/// Bytes of declaration source one document may carry.
///
/// The tokenizer truncates at the encoder's token bound anyway; this only
/// spares it megabyte inputs.
pub const DOCUMENT_SOURCE_BYTES_MAX: usize = 4 * 1024;

/// One declaration, as the text builder sees it.
#[derive(Clone, Copy, Debug)]
pub struct Declaration<'a> {
    kind: &'a str,
    qualified_name: &'a str,
    signature: &'a str,
    documentation: &'a str,
    source: &'a str,
}

impl<'a> Declaration<'a> {
    /// Names the declaration by its kind and qualified name.
    #[must_use]
    pub const fn new(kind: &'a str, qualified_name: &'a str) -> Self {
        Self {
            kind,
            qualified_name,
            signature: "",
            documentation: "",
            source: "",
        }
    }

    /// Adds the rendered signature, which stands in when the source is absent.
    #[must_use]
    pub const fn signature(mut self, signature: &'a str) -> Self {
        self.signature = signature;
        self
    }

    /// Adds the first documentation line, which stands in with the signature.
    #[must_use]
    pub const fn documentation(mut self, documentation: &'a str) -> Self {
        self.documentation = documentation;
        self
    }

    /// Adds the declaration's own source, which the document prefers.
    #[must_use]
    pub const fn source(mut self, source: &'a str) -> Self {
        self.source = source;
        self
    }
}

/// One declaration's embedding text, and the source bytes that did not fit.
///
/// The dropped count is carried rather than discarded so a caller can report a
/// declaration it could not embed whole. A refresh reads it to decide whether
/// the workspace holds declarations larger than the encoder ever sees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    text: String,
    source_bytes_dropped: usize,
}

impl Document {
    /// The text handed to the encoder.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Source bytes cut to reach [`DOCUMENT_SOURCE_BYTES_MAX`], zero when the
    /// declaration fit whole.
    #[must_use]
    pub const fn source_bytes_dropped(&self) -> usize {
        self.source_bytes_dropped
    }

    /// Whether the declaration's source reached the encoder whole.
    #[must_use]
    pub const fn is_source_complete(&self) -> bool {
        self.source_bytes_dropped == 0
    }

    /// The text alone, for a caller that has already read the dropped count.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }
}

/// The text one declaration is embedded as, and what did not fit.
///
/// The declaration's own source is the document wherever it can be read: the
/// encoder was trained on code, and a declaration's identifiers and doc comment
/// are the strongest evidence it has. The qualified name leads, carrying the
/// module that the source alone does not state. Where the source is
/// unavailable, the signature and the first documentation line stand in,
/// because a metadata line still ranks better than an empty document.
///
/// The path is deliberately absent. It is the weakest signal the lexical tier
/// already covers, and it is the furthest thing from the function bodies the
/// encoder was trained against.
///
/// A declaration longer than [`DOCUMENT_SOURCE_BYTES_MAX`] is cut on a
/// character boundary, and the result says how many bytes were dropped. The cut
/// is a work bound rather than a ranking decision: the encoder truncates the
/// same text again at its own token bound, which is the narrower of the two for
/// any declaration this cap reaches.
#[must_use]
pub fn document(declaration: &Declaration<'_>) -> Document {
    let mut text = String::with_capacity(256);
    text.push_str(declaration.kind);
    text.push(' ');
    text.push_str(declaration.qualified_name);
    text.push('\n');
    if declaration.source.is_empty() {
        push_metadata(&mut text, declaration);
        return Document {
            text,
            source_bytes_dropped: 0,
        };
    }
    let kept = bounded_source(declaration.source);
    text.push_str(kept);
    Document {
        text,
        source_bytes_dropped: declaration.source.len() - kept.len(),
    }
}

/// The signature and documentation, for a declaration whose source is absent.
fn push_metadata(text: &mut String, declaration: &Declaration<'_>) {
    if !declaration.signature.is_empty() {
        text.push_str(declaration.signature);
        text.push('\n');
    }
    text.push_str(declaration.documentation);
}

/// The source, cut at [`DOCUMENT_SOURCE_BYTES_MAX`] on a character boundary.
fn bounded_source(source: &str) -> &str {
    if source.len() <= DOCUMENT_SOURCE_BYTES_MAX {
        return source;
    }
    let cut = source
        .char_indices()
        .take_while(|(offset, _)| *offset < DOCUMENT_SOURCE_BYTES_MAX)
        .last()
        .map_or(0, |(offset, character)| offset + character.len_utf8());
    &source[..cut]
}

/// The key one document's vector is stored under.
///
/// Two models' vectors share no space, so the store keys on the model as well;
/// this addresses the text alone.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentDigest([u8; 32]);

impl DocumentDigest {
    /// Digests one document's text.
    #[must_use]
    pub fn of(text: &str) -> Self {
        Self(Sha256::digest(text.as_bytes()).into())
    }

    /// The digest's bytes, for a store that keys on them directly.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest in lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(&self) -> String {
        HEXLOWER.encode(&self.0)
    }
}

impl std::fmt::Display for DocumentDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

/// Digests many documents, in the order given.
///
/// Hashing is per-document and independent, so the work spreads across the
/// rayon pool. The encoder's own passes stay serial: one forward pass at a time
/// bounds the activation memory a workspace-wide build holds.
#[must_use]
pub fn digests(documents: &[String]) -> Vec<DocumentDigest> {
    documents
        .par_iter()
        .map(|text| DocumentDigest::of(text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DOCUMENT_SOURCE_BYTES_MAX, Declaration, DocumentDigest, digests, document};

    #[test]
    fn test_document_is_the_declarations_source_under_its_qualified_name() {
        let declaration = Declaration::new("struct", "rift_search::Encoder")
            .signature("pub struct Encoder")
            .documentation("The BERT encoder.")
            .source("pub struct Encoder {\n    dimension: usize,\n}");
        let built = document(&declaration);
        assert_eq!(
            built.text(),
            "struct rift_search::Encoder\npub struct Encoder {\n    dimension: usize,\n}"
        );
        assert!(built.is_source_complete());
        assert_eq!(built.source_bytes_dropped(), 0);
    }

    #[test]
    fn test_document_falls_back_to_signature_and_documentation() {
        let declaration = Declaration::new("struct", "rift_search::Encoder")
            .signature("pub struct Encoder")
            .documentation("The BERT encoder.");
        let built = document(&declaration);
        assert_eq!(
            built.text(),
            "struct rift_search::Encoder\npub struct Encoder\nThe BERT encoder."
        );
        assert!(
            built.is_source_complete(),
            "an absent source drops no bytes"
        );
    }

    #[test]
    fn test_document_without_signature_or_source_is_the_name_line_alone() {
        let declaration = Declaration::new("struct", "rift_search::Encoder");
        assert_eq!(
            document(&declaration).into_text(),
            "struct rift_search::Encoder\n"
        );
    }

    #[test]
    fn test_document_source_is_cut_on_a_character_boundary_and_says_how_much() {
        let source = "\u{1f600}".repeat(DOCUMENT_SOURCE_BYTES_MAX);
        let declaration = Declaration::new("const", "emoji").source(&source);
        let built = document(&declaration);
        assert!(!built.is_source_complete(), "the source must be cut");
        let cut = built
            .text()
            .strip_prefix("const emoji\n")
            .expect("the name line")
            .to_owned();
        assert!(cut.len() <= DOCUMENT_SOURCE_BYTES_MAX);
        assert!(
            cut.chars().all(|character| character == '\u{1f600}'),
            "the cut must not split a character"
        );
        assert_eq!(
            built.source_bytes_dropped(),
            source.len() - cut.len(),
            "the dropped count is what the text lost"
        );
    }

    #[test]
    fn test_source_at_the_bound_is_carried_whole() {
        let source = "a".repeat(DOCUMENT_SOURCE_BYTES_MAX);
        let declaration = Declaration::new("const", "letters").source(&source);
        let built = document(&declaration);
        assert!(built.text().ends_with(&source));
        assert!(built.is_source_complete());
    }

    #[test]
    fn test_one_byte_past_the_bound_drops_exactly_one_byte() {
        let source = "a".repeat(DOCUMENT_SOURCE_BYTES_MAX + 1);
        let declaration = Declaration::new("const", "letters").source(&source);
        assert_eq!(document(&declaration).source_bytes_dropped(), 1);
    }

    #[test]
    fn test_digest_addresses_the_text_and_renders_as_hexadecimal() {
        let one = DocumentDigest::of("fn load_config");
        let same = DocumentDigest::of("fn load_config");
        let other = DocumentDigest::of("fn read_config");
        assert_eq!(one, same, "one text has one digest");
        assert_ne!(one, other);
        assert_eq!(one.to_hex().len(), 64);
        assert_eq!(one.to_string(), one.to_hex());
        assert_eq!(one.bytes().len(), 32);
    }

    #[test]
    fn test_digests_keep_the_order_they_were_given() {
        let documents = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
        let computed = digests(&documents);
        let expected: Vec<_> = documents
            .iter()
            .map(|text| DocumentDigest::of(text))
            .collect();
        assert_eq!(computed, expected);
        assert!(digests(&[]).is_empty());
    }
}
