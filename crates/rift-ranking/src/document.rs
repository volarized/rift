//! One searchable document, the fields it may carry, and the corpus revision
//! that states what those fields mean.
//!
//! Project search, local package search, and a later global reader publish the
//! same shape. A provider that starts emitting signatures fills a field that
//! was already declared here; it does not introduce a second document type.
//!
//! Absent facts stay absent. Nothing substitutes declaration source into the
//! signature or documentation field, because a reader that weighs those fields
//! differently would then be weighing the same bytes twice.

use data_encoding::HEXLOWER;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::{Language, PackageIdentity, ProjectPath, SourceUnitId};
use sha2::{Digest as _, Sha256};

use crate::error::{RankingError, RankingViolation, refuse, refuse_over_limit};

/// Bytes one document identity may hold.
pub const IDENTITY_BYTES_MAX: usize = 512;
/// Bytes the `name` and `qualified_name` fields may hold, matching the wire's
/// symbol-name maximum.
pub const NAME_BYTES_MAX: usize = 4_096;
/// Bytes the derived `identifier_terms` field may hold.
pub const IDENTIFIER_TERMS_BYTES_MAX: usize = 8_192;
/// Bytes the `signature` field may hold.
pub const SIGNATURE_BYTES_MAX: usize = 8_192;
/// Bytes the `documentation` field may hold.
pub const DOCUMENTATION_BYTES_MAX: usize = 16_384;

/// The FTS tokenizer every Rift corpus is built with.
///
/// Porter stemming stays off until a Rift corpus shows a gain: the workspace
/// mixes languages whose identifiers stem badly, and the retrieval gate has no
/// measurement supporting the change yet.
///
/// Diacritic folding is off as well, so the in-memory adapter can tokenize the
/// same text the same way without carrying a Unicode folding table of its own.
/// A reader that folded on one side and not the other would rank the same
/// publication two ways.
pub const CORPUS_TOKENIZER: &str = "unicode61 remove_diacritics 0";

/// Separates one field's column name from its value in digest material.
const FIELD_NAME_SEPARATOR: u8 = 0;
/// Separates adjacent fields in digest material.
const FIELD_SEPARATOR: u8 = 0xff;

/// How the derived identifier terms are split.
///
/// The corpus revision carries this, so widening the split rule invalidates
/// stored rows rather than leaving half the corpus on the old derivation.
const IDENTIFIER_DERIVATION: &str = "case-acronym-underscore-1";

/// One searchable field of an index document.
///
/// Declaration order is column order in the FTS table and weight order in
/// `bm25`, so the two cannot drift: both read this enum.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SearchableField {
    /// A declaration name, or a text file's final path segment with its
    /// extension.
    Name,
    /// The declaration's qualified name.
    QualifiedName,
    /// Case-split and separator-split terms derived from the names.
    IdentifierTerms,
    /// The rendered signature, when a provider published one.
    Signature,
    /// Attached documentation, when a provider published it.
    Documentation,
    /// The declaration's own source text.
    DeclarationSource,
    /// A visible text file's content.
    FileContent,
}

impl SearchableField {
    /// Every searchable field, in column order.
    pub const ALL: [Self; 7] = [
        Self::Name,
        Self::QualifiedName,
        Self::IdentifierTerms,
        Self::Signature,
        Self::Documentation,
        Self::DeclarationSource,
        Self::FileContent,
    ];

    /// The FTS column this field is stored in.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::QualifiedName => "qualified_name",
            Self::IdentifierTerms => "identifier_terms",
            Self::Signature => "signature",
            Self::Documentation => "documentation",
            Self::DeclarationSource => "declaration_source",
            Self::FileContent => "file_content",
        }
    }

    /// The `bm25` column weight this field ranks under.
    ///
    /// A name hit outranks a qualified-name hit, which outranks a derived
    /// term, and prose fields carry the least. The retrieval gate measures
    /// these on Rift's own corpus; they are the starting point, not a
    /// conclusion.
    #[must_use]
    pub const fn rank_weight(self) -> f64 {
        match self {
            Self::Name => 10.0,
            Self::QualifiedName => 6.0,
            Self::IdentifierTerms => 4.0,
            Self::Signature => 2.0,
            Self::Documentation => 1.5,
            Self::DeclarationSource | Self::FileContent => 1.0,
        }
    }

    /// The byte bound this field's value may reach, or `None` for a field the
    /// shape does not bound.
    ///
    /// A name, a derived term list, a signature, and a doc comment have a
    /// ceiling no source reasonably reaches, and a value past it is a defect
    /// in whatever produced it. The two content fields have none here: the
    /// store the document is published into carries the operator's own byte
    /// bound, and restating it as a second ceiling would mean neither could
    /// ever trip.
    #[must_use]
    pub const fn bytes_max(self) -> Option<usize> {
        match self {
            Self::Name | Self::QualifiedName => Some(NAME_BYTES_MAX),
            Self::IdentifierTerms => Some(IDENTIFIER_TERMS_BYTES_MAX),
            Self::Signature => Some(SIGNATURE_BYTES_MAX),
            Self::Documentation => Some(DOCUMENTATION_BYTES_MAX),
            Self::DeclarationSource | Self::FileContent => None,
        }
    }

    /// This field's position among the FTS columns, counting the leading
    /// `identity` column that `bm25` numbers but never matches.
    #[must_use]
    pub const fn column_position(self) -> usize {
        match self {
            Self::Name => 1,
            Self::QualifiedName => 2,
            Self::IdentifierTerms => 3,
            Self::Signature => 4,
            Self::Documentation => 5,
            Self::DeclarationSource => 6,
            Self::FileContent => 7,
        }
    }
}

/// A set of searchable fields.
///
/// A document states which fields it filled; a ranking states which fields put
/// a candidate in the answer. Both are the same small set, so both use one
/// representation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FieldSet(u8);

impl FieldSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The set holding every searchable field.
    ///
    /// Bit zero stays clear: the FTS `identity` column occupies position zero
    /// and never matches a term, so no field is ever filed under it.
    #[must_use]
    pub const fn all() -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < SearchableField::ALL.len() {
            bits |= Self::of(SearchableField::ALL[index]).0;
            index += 1;
        }
        Self(bits)
    }

    /// The set holding one field.
    #[must_use]
    pub const fn of(field: SearchableField) -> Self {
        Self(1 << field.column_position())
    }

    /// Whether this set holds `field`.
    #[must_use]
    pub const fn holds(self, field: SearchableField) -> bool {
        self.0 & Self::of(field).0 != 0
    }

    /// Whether this set holds no field at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// This set with `field` added.
    #[must_use]
    pub const fn with(self, field: SearchableField) -> Self {
        Self(self.0 | Self::of(field).0)
    }

    /// The union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every field `other` holds is also held here.
    #[must_use]
    pub const fn covers(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The fields in this set, in column order.
    pub fn fields(self) -> impl Iterator<Item = SearchableField> {
        SearchableField::ALL
            .into_iter()
            .filter(move |field| self.holds(*field))
    }
}

impl FromIterator<SearchableField> for FieldSet {
    fn from_iter<I: IntoIterator<Item = SearchableField>>(fields: I) -> Self {
        fields.into_iter().fold(Self::EMPTY, Self::with)
    }
}

/// What one document describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentKind {
    /// One declaration extracted from source.
    Symbol,
    /// A visible text file indexed as one whole document.
    TextFile,
}

impl DocumentKind {
    /// The spelling stored beside the document.
    #[must_use]
    pub const fn stored_value(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::TextFile => "text_file",
        }
    }

    /// The one field a document of this kind carries content in.
    ///
    /// Only this field can reach a megabyte, so it is the one an operator's byte bound
    /// applies to and the one a reader excerpts from.
    #[must_use]
    pub const fn content_field(self) -> SearchableField {
        match self {
            Self::Symbol => SearchableField::DeclarationSource,
            Self::TextFile => SearchableField::FileContent,
        }
    }

    /// Parses a stored spelling back into a kind.
    #[must_use]
    pub fn from_stored(value: &str) -> Option<Self> {
        [Self::Symbol, Self::TextFile]
            .into_iter()
            .find(|kind| kind.stored_value() == value)
    }
}

/// One document's stable key.
///
/// The key addresses the document inside its own index and travels through
/// every ranking input, so fusion compares identities without ever resolving
/// one.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentIdentity(String);

impl DocumentIdentity {
    /// Constructs one document identity.
    ///
    /// # Errors
    ///
    /// Returns [`RankingError`] when the value is empty or runs past
    /// [`IDENTITY_BYTES_MAX`].
    pub fn new(value: impl Into<String>) -> Result<Self, RankingError> {
        let value = value.into();
        if value.is_empty() {
            return Err(refuse(RankingViolation::DocumentIdentityEmpty, "identity"));
        }
        if value.len() > IDENTITY_BYTES_MAX {
            return Err(refuse_over_limit(
                RankingViolation::DocumentFieldLength,
                "identity",
                "document.identity",
                IDENTITY_BYTES_MAX,
                value.len(),
            ));
        }
        Ok(Self(value))
    }

    /// The identity as stored and compared.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The identity, owned.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for DocumentIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where the document's bytes live.
///
/// A project document is addressed by its project-relative path; a package
/// document is addressed by the source unit the dependency lane minted for it.
/// Neither spelling reaches the ranked text: a host-absolute root must not
/// change a rank.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentLocation {
    /// A path inside the served project.
    Project(ProjectPath),
    /// One file of a cataloged package.
    Unit(SourceUnitId),
}

/// The searchable fields one document filled.
///
/// Every field is optional because a provider that publishes no signature
/// leaves that field absent rather than filling it with something else.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DocumentFields {
    name: Option<String>,
    qualified_name: Option<String>,
    identifier_terms: Option<String>,
    signature: Option<String>,
    documentation: Option<String>,
    declaration_source: Option<String>,
    file_content: Option<String>,
}

impl DocumentFields {
    /// A document with no field filled.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            name: None,
            qualified_name: None,
            identifier_terms: None,
            signature: None,
            documentation: None,
            declaration_source: None,
            file_content: None,
        }
    }

    /// Sets one field, dropping an empty value so an empty string and an
    /// absent fact cannot mean two things.
    #[must_use]
    pub fn with(mut self, field: SearchableField, value: impl Into<String>) -> Self {
        let value = value.into();
        let slot = match field {
            SearchableField::Name => &mut self.name,
            SearchableField::QualifiedName => &mut self.qualified_name,
            SearchableField::IdentifierTerms => &mut self.identifier_terms,
            SearchableField::Signature => &mut self.signature,
            SearchableField::Documentation => &mut self.documentation,
            SearchableField::DeclarationSource => &mut self.declaration_source,
            SearchableField::FileContent => &mut self.file_content,
        };
        *slot = (!value.is_empty()).then_some(value);
        self
    }

    /// Sets one field when the caller holds a value for it.
    #[must_use]
    pub fn with_optional(self, field: SearchableField, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.with(field, value),
            None => self,
        }
    }

    /// Reads one field, or `None` when the document did not fill it.
    #[must_use]
    pub fn get(&self, field: SearchableField) -> Option<&str> {
        let slot = match field {
            SearchableField::Name => &self.name,
            SearchableField::QualifiedName => &self.qualified_name,
            SearchableField::IdentifierTerms => &self.identifier_terms,
            SearchableField::Signature => &self.signature,
            SearchableField::Documentation => &self.documentation,
            SearchableField::DeclarationSource => &self.declaration_source,
            SearchableField::FileContent => &self.file_content,
        };
        slot.as_deref()
    }

    /// The fields this document filled.
    #[must_use]
    pub fn filled(&self) -> FieldSet {
        SearchableField::ALL
            .into_iter()
            .filter(|field| self.get(*field).is_some())
            .collect()
    }

    /// The digest of the fields this document filled, in declared column
    /// order.
    ///
    /// Two publications of the same declaration produce one digest, whichever
    /// adapter built them, so a comparison across adapters tells an unchanged
    /// document from a rewritten one without reading every field back. The
    /// column name is folded in beside its value, so moving a value from one
    /// field to another changes the digest.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        for field in SearchableField::ALL {
            hasher.update(field.column().as_bytes());
            hasher.update([FIELD_NAME_SEPARATOR]);
            hasher.update(self.get(field).unwrap_or_default().as_bytes());
            hasher.update([FIELD_SEPARATOR]);
        }
        HEXLOWER.encode(&hasher.finalize())[..DIGEST_WIRE_CHARS].to_owned()
    }

    /// Refuses a field whose value runs past its own byte bound.
    fn violation(&self) -> Option<(SearchableField, usize, usize)> {
        SearchableField::ALL.into_iter().find_map(|field| {
            let bound = field.bytes_max()?;
            let observed = self.get(field)?.len();
            (observed > bound).then_some((field, bound, observed))
        })
    }
}

/// One document every Rift index publishes and ranks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexDocument {
    identity: DocumentIdentity,
    location: DocumentLocation,
    kind: DocumentKind,
    language: Option<Language>,
    package: Option<PackageIdentity>,
    digest: String,
    fields: DocumentFields,
}

impl IndexDocument {
    /// Constructs one index document.
    ///
    /// `digest` addresses the content the document was derived from, so a
    /// refresh can tell an unchanged declaration from a rewritten one without
    /// comparing every field.
    ///
    /// # Errors
    ///
    /// Returns [`RankingError`] when a field runs past its own byte bound.
    pub fn new(
        identity: DocumentIdentity,
        location: DocumentLocation,
        kind: DocumentKind,
        digest: impl Into<String>,
        fields: DocumentFields,
    ) -> Result<Self, RankingError> {
        if let Some((field, bound, observed)) = fields.violation() {
            return Err(refuse_over_limit(
                RankingViolation::DocumentFieldLength,
                field.column(),
                "document.field",
                bound,
                observed,
            ));
        }
        Ok(Self {
            identity,
            location,
            kind,
            language: None,
            package: None,
            digest: digest.into(),
            fields,
        })
    }

    /// Records the language the document's bytes are written in.
    #[must_use]
    pub fn in_language(mut self, language: Language) -> Self {
        self.language = Some(language);
        self
    }

    /// Records the package the document belongs to.
    #[must_use]
    pub fn in_package(mut self, package: PackageIdentity) -> Self {
        self.package = Some(package);
        self
    }

    /// The document's stable key.
    #[must_use]
    pub const fn identity(&self) -> &DocumentIdentity {
        &self.identity
    }

    /// Where the document's bytes live.
    #[must_use]
    pub const fn location(&self) -> &DocumentLocation {
        &self.location
    }

    /// What the document describes.
    #[must_use]
    pub const fn kind(&self) -> DocumentKind {
        self.kind
    }

    /// The language the document's bytes are written in, when known.
    #[must_use]
    pub const fn language(&self) -> Option<&Language> {
        self.language.as_ref()
    }

    /// The package the document belongs to, when it came from one.
    #[must_use]
    pub const fn package(&self) -> Option<&PackageIdentity> {
        self.package.as_ref()
    }

    /// The digest of the content this document was derived from.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The searchable fields.
    #[must_use]
    pub const fn fields(&self) -> &DocumentFields {
        &self.fields
    }

    /// The content this document's kind carries, empty when it carries none.
    #[must_use]
    pub fn content(&self) -> &str {
        self.fields
            .get(self.kind.content_field())
            .unwrap_or_default()
    }

    /// The project path this document is addressed by, or `None` for a package
    /// document, which a source unit addresses instead.
    #[must_use]
    pub const fn project_path(&self) -> Option<&ProjectPath> {
        match &self.location {
            DocumentLocation::Project(path) => Some(path),
            DocumentLocation::Unit(_) => None,
        }
    }
}

/// What the stored corpus means.
///
/// The revision covers the field shape, the derivation the identifier terms
/// were split with, the tokenizer, and the `bm25` weights. A publication
/// stamped with another revision cannot be ranked against this one, so an
/// incompatible change invalidates stored rows and vectors instead of silently
/// mixing two corpora.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CorpusRevision(String);

impl CorpusRevision {
    /// The revision this build derives.
    #[must_use]
    pub fn current() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CORPUS_TOKENIZER.as_bytes());
        hasher.update(b"\n");
        hasher.update(IDENTIFIER_DERIVATION.as_bytes());
        hasher.update(b"\n");
        for field in SearchableField::ALL {
            hasher.update(field.column().as_bytes());
            hasher.update(b"=");
            hasher.update(field.rank_weight().to_le_bytes());
            hasher.update(b"\n");
        }
        let digest = HEXLOWER.encode(&hasher.finalize());
        Self(digest[..DIGEST_WIRE_CHARS].to_owned())
    }

    /// Reads a revision a store already holds.
    #[must_use]
    pub fn stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The revision as stored and compared.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CorpusRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CorpusRevision, DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, FieldSet,
        IDENTITY_BYTES_MAX, IndexDocument, NAME_BYTES_MAX, SearchableField,
    };
    use crate::error::RankingViolation;
    use rift_core::constants::DIGEST_WIRE_CHARS;
    use rift_core::{Language, PackageIdentity, ProjectPath};

    fn identity(value: &str) -> DocumentIdentity {
        DocumentIdentity::new(value).expect("identity must be accepted")
    }

    fn location() -> DocumentLocation {
        DocumentLocation::Project(ProjectPath::new("src/lib.rs").expect("path must be accepted"))
    }

    fn document(fields: DocumentFields) -> Result<IndexDocument, crate::error::RankingError> {
        IndexDocument::new(
            identity("rift://symbol/rust/src%2Flib.rs/beacon"),
            location(),
            DocumentKind::Symbol,
            "0f1e2d3c",
            fields,
        )
    }

    #[test]
    fn test_an_empty_identity_is_refused() {
        assert_eq!(
            DocumentIdentity::new("")
                .expect_err("an empty identity must be refused")
                .fault()
                .violation(),
            RankingViolation::DocumentIdentityEmpty
        );
    }

    #[test]
    fn test_an_identity_past_the_byte_bound_is_refused() {
        assert_eq!(
            DocumentIdentity::new("i".repeat(IDENTITY_BYTES_MAX + 1))
                .expect_err("an overlong identity must be refused")
                .fault()
                .violation(),
            RankingViolation::DocumentFieldLength
        );
    }

    #[test]
    fn test_an_identity_at_the_byte_bound_is_accepted() {
        let value = "i".repeat(IDENTITY_BYTES_MAX);
        assert_eq!(identity(&value).as_str().len(), IDENTITY_BYTES_MAX);
    }

    #[test]
    fn test_an_identity_renders_as_its_own_value() {
        assert_eq!(identity("beacon").to_string(), "beacon");
        assert_eq!(identity("beacon").into_inner(), "beacon");
    }

    #[test]
    fn test_every_field_declares_a_distinct_column_and_position() {
        let mut columns: Vec<&str> = SearchableField::ALL
            .into_iter()
            .map(SearchableField::column)
            .collect();
        columns.sort_unstable();
        columns.dedup();
        assert_eq!(columns.len(), SearchableField::ALL.len());
        let positions: Vec<usize> = SearchableField::ALL
            .into_iter()
            .map(SearchableField::column_position)
            .collect();
        assert_eq!(positions, [1, 2, 3, 4, 5, 6, 7]);
    }

    /// Whether two weights agree within the last bits a literal can carry.
    fn close(computed: f64, expected: f64) -> bool {
        (computed - expected).abs() < 1e-12
    }

    #[test]
    fn test_the_declared_weights_order_names_above_prose() {
        let declared = [
            (SearchableField::Name, 10.0),
            (SearchableField::QualifiedName, 6.0),
            (SearchableField::IdentifierTerms, 4.0),
            (SearchableField::Signature, 2.0),
            (SearchableField::Documentation, 1.5),
            (SearchableField::DeclarationSource, 1.0),
            (SearchableField::FileContent, 1.0),
        ];
        for (field, expected) in declared {
            assert!(
                close(field.rank_weight(), expected),
                "{} must rank at {expected}, not {}",
                field.column(),
                field.rank_weight()
            );
        }
    }

    #[test]
    fn test_a_field_set_holds_only_what_was_added() {
        let set = FieldSet::EMPTY.with(SearchableField::Name);
        assert!(set.holds(SearchableField::Name));
        assert!(!set.holds(SearchableField::Signature));
        assert!(!set.is_empty());
        assert!(FieldSet::EMPTY.is_empty());
    }

    #[test]
    fn test_a_field_set_unions_and_covers() {
        let names = FieldSet::EMPTY
            .with(SearchableField::Name)
            .with(SearchableField::QualifiedName);
        let name = FieldSet::of(SearchableField::Name);
        assert!(names.covers(name));
        assert!(!name.covers(names));
        assert_eq!(
            name.union(FieldSet::of(SearchableField::QualifiedName)),
            names
        );
    }

    #[test]
    fn test_the_full_field_set_holds_every_field() {
        let all = FieldSet::all();
        assert!(
            SearchableField::ALL
                .into_iter()
                .all(|field| all.holds(field))
        );
        assert_eq!(all.fields().count(), SearchableField::ALL.len());
    }

    #[test]
    fn test_a_field_set_collects_from_an_iterator() {
        let set: FieldSet = [SearchableField::Signature, SearchableField::Name]
            .into_iter()
            .collect();
        assert_eq!(
            set.fields().collect::<Vec<_>>(),
            [SearchableField::Name, SearchableField::Signature]
        );
    }

    #[test]
    fn test_a_document_kind_round_trips_through_its_stored_spelling() {
        for kind in [DocumentKind::Symbol, DocumentKind::TextFile] {
            assert_eq!(DocumentKind::from_stored(kind.stored_value()), Some(kind));
        }
        assert_eq!(DocumentKind::from_stored("module"), None);
    }

    #[test]
    fn test_an_empty_field_value_leaves_the_field_absent() {
        let fields = DocumentFields::empty().with(SearchableField::Signature, "");
        assert_eq!(fields.get(SearchableField::Signature), None);
        assert!(fields.filled().is_empty());
    }

    #[test]
    fn test_an_optional_field_is_set_only_when_a_value_exists() {
        let fields = DocumentFields::empty()
            .with_optional(SearchableField::Signature, Some("fn beacon()"))
            .with_optional(SearchableField::Documentation, None::<String>);
        assert_eq!(fields.get(SearchableField::Signature), Some("fn beacon()"));
        assert_eq!(fields.get(SearchableField::Documentation), None);
    }

    #[test]
    fn test_every_field_stores_and_reads_back() {
        let fields = SearchableField::ALL
            .into_iter()
            .fold(DocumentFields::empty(), |held, field| {
                held.with(field, field.column())
            });
        for field in SearchableField::ALL {
            assert_eq!(fields.get(field), Some(field.column()));
        }
        assert_eq!(fields.filled(), FieldSet::all());
    }

    #[test]
    fn test_a_field_past_its_byte_bound_is_refused() {
        let fields =
            DocumentFields::empty().with(SearchableField::Name, "n".repeat(NAME_BYTES_MAX + 1));
        assert_eq!(
            document(fields)
                .expect_err("an overlong field must be refused")
                .fault()
                .violation(),
            RankingViolation::DocumentFieldLength
        );
    }

    #[test]
    fn test_a_content_field_carries_what_the_store_will_bound() {
        let fields = DocumentFields::empty()
            .with(SearchableField::FileContent, "c".repeat(2 * NAME_BYTES_MAX));
        assert!(
            document(fields).is_ok(),
            "the shape states no content ceiling: the store the document lands in \
             carries the operator's own byte bound, and two ceilings would leave \
             neither able to trip"
        );
        assert_eq!(SearchableField::FileContent.bytes_max(), None);
        assert_eq!(SearchableField::DeclarationSource.bytes_max(), None);
    }

    #[test]
    fn test_a_document_carries_its_location_kind_digest_and_fields() {
        let built = document(DocumentFields::empty().with(SearchableField::Name, "beacon"))
            .expect("document must be accepted");
        assert_eq!(built.kind(), DocumentKind::Symbol);
        assert_eq!(built.digest(), "0f1e2d3c");
        assert_eq!(built.location(), &location());
        assert_eq!(built.fields().get(SearchableField::Name), Some("beacon"));
        assert_eq!(built.language(), None);
        assert_eq!(built.package(), None);
    }

    #[test]
    fn test_a_document_records_its_language_and_package() {
        let language = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        let package = PackageIdentity {
            manager: "cargo".to_owned(),
            name: "helper".to_owned(),
            version: "0.1.0".to_owned(),
        };
        let built = document(DocumentFields::empty())
            .expect("document must be accepted")
            .in_language(language.clone())
            .in_package(package.clone());
        assert_eq!(built.language(), Some(&language));
        assert_eq!(built.package(), Some(&package));
        assert_eq!(
            built.identity().as_str(),
            "rift://symbol/rust/src%2Flib.rs/beacon"
        );
    }

    #[test]
    fn test_the_corpus_revision_is_stable_and_wire_sized() {
        let revision = CorpusRevision::current();
        assert_eq!(revision.as_str().len(), DIGEST_WIRE_CHARS);
        assert_eq!(revision, CorpusRevision::current());
        assert_eq!(revision.to_string(), revision.as_str());
    }

    #[test]
    fn test_a_stored_corpus_revision_compares_against_the_current_one() {
        assert_ne!(
            CorpusRevision::stored("00000000"),
            CorpusRevision::current()
        );
    }
}
