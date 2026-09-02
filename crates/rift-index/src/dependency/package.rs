//! One cataloged package's declarations, parsed with the shipped providers and assembled.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;

use rift_core::{
    ContributionOrigin, ErrorContext, ProjectPath, SourceKind, SourceLocation, SourcePath,
    SourceResolverId, SourceUnitId, symbol_identity,
};
use rift_dependency::{CatalogEntry, PackageLocation};
use rift_protocol::read::PackageIdentity;
use rift_syntax::{DocumentPlacement, SyntaxSymbol, registry};

use super::failure::{
    PackageIndexError, PackageIndexFault, PackageIndexViolation, package_segment,
};
use super::walk::{PackageFiles, public_qualified_names};
use crate::semantic::{BindingPolicy, PlacedDocument, WorkspaceSemantics};
use crate::workspace::{
    IndexedFile, ReadableSymbol, SymbolMatch, TextSourceFile, indexed_file_from_catalog,
    symbol_matches_where,
};

/// One indexed package file, its placement, and the declarations it makes public.
#[derive(Debug)]
struct PackageFile {
    file: IndexedFile,
    placement: DocumentPlacement,
    public: BTreeSet<String>,
}

/// One cataloged package's declarations, parsed and assembled; queries answer the public ones.
///
/// The assembled graph holds every declaration, so container references stay
/// whole; the public-declaration rule applies when [`Self::symbols`] answers.
#[derive(Debug)]
pub struct PackageIndex {
    entry: CatalogEntry,
    files: Vec<PackageFile>,
    declaration_count: usize,
    byte_count: u64,
    skipped_binary: usize,
    semantics: WorkspaceSemantics,
}

impl PackageIndex {
    /// Parses `files` with the shipped providers and assembles the package graph.
    ///
    /// Each file is placed under `rift://source/<manager>/<name>@<version>/<path>`
    /// with the identity path `<manager>/<name>@<version>/<path>`, and its origin
    /// is the entry's location: a dependency carrying the package identity, or
    /// the standard library. Name binding does not run for a package.
    ///
    /// # Errors
    ///
    /// Returns [`PackageIndexError`] when the identity cannot spell a resolver
    /// or unit, when no provider parses a file or its parse fails, or when
    /// publication or normalization refuses the package graph.
    pub fn build(
        entry: &CatalogEntry,
        files: &PackageFiles,
        revision: u64,
    ) -> Result<Self, PackageIndexError> {
        let package = entry.identity();
        let resolver = SourceResolverId::new(package.manager.clone()).map_err(|error| {
            PackageIndexFault::new(PackageIndexViolation::Identity, package).caused_by(error)
        })?;
        let byte_count = files.byte_count();
        let skipped_binary = files.skipped_binary();
        let mut indexed = Vec::with_capacity(files.file_count());
        for file in files.files() {
            let parsed = parsed_file(file, package)?;
            let placement = placement_of(&resolver, entry, file.path())?;
            let public = public_qualified_names(entry.language(), parsed.syntax());
            indexed.push(PackageFile {
                file: parsed,
                placement,
                public,
            });
        }
        indexed.sort_by(|left, right| left.file.path().cmp(right.file.path()));
        let placed: Vec<PlacedDocument<'_>> = indexed
            .iter()
            .map(|held| PlacedDocument {
                document: held.file.syntax(),
                placement: held.placement.clone(),
            })
            .collect();
        let semantics = WorkspaceSemantics::build_placed(
            &placed,
            &[],
            revision,
            None,
            &BindingPolicy::disabled(),
        )
        .map_err(|error| {
            PackageIndexFault::new(PackageIndexViolation::Provider, package).caused_by(error)
        })?;
        let declaration_count = indexed
            .iter()
            .map(|held| held.file.syntax().symbols().len())
            .sum();
        Ok(Self {
            entry: entry.clone(),
            files: indexed,
            declaration_count,
            byte_count,
            skipped_binary,
            semantics,
        })
    }

    /// The package as its manager identifies it.
    #[must_use]
    pub const fn identity(&self) -> &PackageIdentity {
        self.entry.identity()
    }

    /// Every indexed file, in path order.
    #[must_use]
    pub fn files(&self) -> impl ExactSizeIterator<Item = &IndexedFile> {
        self.files.iter().map(|held| &held.file)
    }

    /// How many files the index holds.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The bytes the indexed files hold together.
    #[must_use]
    pub const fn byte_count(&self) -> u64 {
        self.byte_count
    }

    /// How many selected files were skipped as binary before parsing.
    #[must_use]
    pub const fn skipped_binary(&self) -> usize {
        self.skipped_binary
    }

    /// How many declarations the files carry, public or not.
    #[must_use]
    pub const fn declaration_count(&self) -> usize {
        self.declaration_count
    }

    /// The source unit `file` is filed under; `None` for a file this index does not hold.
    #[must_use]
    pub fn unit_of(&self, file: &IndexedFile) -> Option<&SourceUnitId> {
        self.held(file.path()).map(|held| held.placement.unit())
    }

    /// Public declarations matching `query`, ranked as the project index ranks.
    ///
    /// The public-declaration rule runs before ranking and truncation, so the
    /// answer fills `limit` from public declarations alone.
    #[must_use]
    pub fn symbols(&self, query: &str, limit: usize) -> Vec<SymbolMatch<'_>> {
        symbol_matches_where(self.files(), query, limit, |file, symbol| {
            self.is_public(file.path(), symbol)
        })
    }

    /// Assembles the readable symbol behind one match through the package graph.
    ///
    /// # Errors
    ///
    /// Returns [`PackageIndexError`] when the match names a file this index does
    /// not hold, or the normalized graph supplies no readable symbol for it.
    pub fn assembled_symbol(
        &self,
        matched: SymbolMatch<'_>,
    ) -> Result<ReadableSymbol, PackageIndexError> {
        let package = self.identity();
        let path = matched.file.path();
        let held = self.held(path).ok_or_else(|| {
            PackageIndexFault::new(PackageIndexViolation::SymbolMissing, package)
                .at(Path::new(path.as_str()))
        })?;
        let identity = symbol_identity(
            &matched.file.syntax().language().identity_segment(),
            held.placement.identity_path(),
            &matched.symbol.qualified_name,
        );
        ReadableSymbol::assembled_by(&self.semantics, &identity).ok_or_else(|| {
            PackageIndexError::new(PackageIndexFault::new(
                PackageIndexViolation::SymbolMissing,
                package,
            ))
            .with_context(ErrorContext::new("identity", identity))
        })
    }

    fn held(&self, path: &ProjectPath) -> Option<&PackageFile> {
        self.files
            .binary_search_by(|held| held.file.path().cmp(path))
            .ok()
            .map(|position| &self.files[position])
    }

    fn is_public(&self, path: &ProjectPath, symbol: &SyntaxSymbol) -> bool {
        self.held(path)
            .is_some_and(|held| held.public.contains(&symbol.qualified_name))
    }
}

/// One package file parsed by the provider its extension names.
fn parsed_file(
    file: &TextSourceFile,
    package: &PackageIdentity,
) -> Result<IndexedFile, PackageIndexError> {
    let context = Path::new(file.path().as_str());
    let extension = context
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let provider = registry::provider_for_extension(extension).ok_or_else(|| {
        PackageIndexFault::new(PackageIndexViolation::Syntax, package).at(context)
    })?;
    indexed_file_from_catalog(file, context, provider).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::Syntax, package)
            .at(context)
            .caused_by(error)
            .into()
    })
}

/// The placement of one package file: its unit, identity path, and origin.
fn placement_of(
    resolver: &SourceResolverId,
    entry: &CatalogEntry,
    path: &ProjectPath,
) -> Result<DocumentPlacement, PackageIndexError> {
    let package = entry.identity();
    let identity_fault = || PackageIndexFault::new(PackageIndexViolation::Identity, package);
    let origin = ContributionOrigin::new(Some(source_location(entry)), SourceKind::Authored)
        .map_err(|error| identity_fault().caused_by(error))?;
    let key = SourcePath::new(format!("{}@{}/{path}", package.name, package.version)).map_err(
        |error| {
            identity_fault()
                .at(Path::new(path.as_str()))
                .caused_by(error)
        },
    )?;
    let unit = SourceUnitId::new(resolver.clone(), key).map_err(|error| {
        identity_fault()
            .at(Path::new(path.as_str()))
            .caused_by(error)
    })?;
    let identity_path = format!("{}/{path}", package_segment(package));
    Ok(DocumentPlacement::new(origin, unit, identity_path))
}

/// The source location an entry's declarations carry.
fn source_location(entry: &CatalogEntry) -> SourceLocation {
    match entry.location() {
        PackageLocation::Dependency => SourceLocation::Dependency {
            package: entry.identity().clone(),
        },
        PackageLocation::Stdlib => SourceLocation::Stdlib {},
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rift_core::{ErrorCode, ErrorName, PathError, SourceLocation, SourceUnitIdError};
    use rift_dependency::{CatalogEntry, PackageLocation};
    use rift_syntax::ShippedLanguage;

    use super::super::fixture::{
        identity, language, names, rust_package, text, tokio, violation_of,
    };
    use super::{PackageFiles, PackageIndex, PackageIndexViolation};

    #[test]
    fn test_package_index_answers_pub_declarations_with_dependency_origin_unit_and_identity() {
        let entry = CatalogEntry::dependency(
            tokio(),
            language(ShippedLanguage::Rust),
            Some(PathBuf::from("/cache/tokio-1.53.1")),
            true,
        );
        let files = PackageFiles::new(
            vec![text(
                "src/lib.rs",
                "pub fn spawn() {}\nfn hidden() {}\npub(crate) struct Inner;\n",
            )],
            1,
        );

        let package = PackageIndex::build(&entry, &files, 7).expect("package builds");

        assert_eq!(package.identity(), &tokio());
        assert_eq!(package.file_count(), 1);
        assert_eq!(package.declaration_count(), 3);
        assert_eq!(package.skipped_binary(), 1);
        assert_eq!(package.byte_count(), files.byte_count());
        let matches = package.symbols("", 10);
        assert_eq!(names(&matches), ["spawn"]);
        let spawn = matches[0];
        assert_eq!(
            package.unit_of(spawn.file).map(ToString::to_string),
            Some("rift://source/cargo/tokio@1.53.1/src/lib.rs".to_owned())
        );
        let readable = package.assembled_symbol(spawn).expect("assembled");
        assert_eq!(
            readable.identity().map(rift_core::SymbolId::as_str),
            Some("rift://symbol/rust/cargo/tokio@1.53.1/src/lib.rs/spawn")
        );
        assert_eq!(
            readable.assembled().origin().location(),
            Some(&SourceLocation::Dependency { package: tokio() })
        );
        assert_eq!(readable.assembled().index_revision().get(), 7);
        assert_eq!(readable.facts().visibility_spelling(), Some("pub"));
    }

    #[test]
    fn test_package_index_symbols_fill_the_limit_from_public_declarations_alone() {
        let package = rust_package("alpha", "fn alpha_hidden() {}\npub fn alpha_shown() {}\n");

        let matches = package.symbols("alpha", 1);

        assert_eq!(names(&matches), ["alpha_shown"]);
    }

    #[test]
    fn test_package_index_keeps_items_of_a_pub_trait_and_drops_private_impl_methods() {
        let package = rust_package(
            "traits",
            "pub trait Read { fn poll(&self) {} }\ntrait Secret { fn hide(&self) {} }\npub struct Client;\nimpl Client { fn helper() {} pub fn open() {} }\n",
        );

        let matches = package.symbols("", 10);

        assert_eq!(
            names(&matches),
            ["Client", "Client::open", "Read", "Read::poll"]
        );
    }

    #[test]
    fn test_package_index_python_stub_package_hides_underscore_names() {
        let entry = CatalogEntry::dependency(
            identity("uv", "six", "1.17.0"),
            language(ShippedLanguage::Python),
            None,
            true,
        );
        let files = PackageFiles::new(
            vec![text(
                "six.pyi",
                "def public() -> None: ...\ndef _private() -> None: ...\nclass _Hidden: ...\nclass Shown:\n    def _inner(self) -> None: ...\n    def run(self) -> None: ...\n",
            )],
            0,
        );

        let package = PackageIndex::build(&entry, &files, 1).expect("package builds");
        let matches = package.symbols("", 10);

        assert_eq!(names(&matches), ["Shown", "Shown.run", "public"]);
        assert_eq!(
            package.unit_of(matches[0].file).map(ToString::to_string),
            Some("rift://source/uv/six@1.17.0/six.pyi".to_owned())
        );
    }

    #[test]
    fn test_package_index_typescript_drops_private_and_protected_members() {
        let entry = CatalogEntry::dependency(
            identity("npm", "client", "2.0.0"),
            language(ShippedLanguage::TypeScript),
            None,
            true,
        );
        // The TypeScript provider extracts no member of an ambient `declare class`,
        // so the accessibility rule is proven through a bodied class the same
        // provider parses; `.ts` reaches the build only through this crate.
        let files = PackageFiles::new(
            vec![text(
                "index.ts",
                "export class Client {\n  private secret(): void {}\n  protected guard(): void {}\n  public open(): void {}\n  close(): void {}\n}\n",
            )],
            0,
        );

        let package = PackageIndex::build(&entry, &files, 1).expect("package builds");
        let matches = package.symbols("", 10);

        assert_eq!(names(&matches), ["Client", "Client.close", "Client.open"]);
    }

    #[test]
    fn test_package_index_stdlib_entry_carries_stdlib_origin() {
        let entry = CatalogEntry::new(
            identity("stdlib", "rust", "1.90.0"),
            PackageLocation::Stdlib,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("core/src/lib.rs", "pub fn hint() {}\n")], 0);

        let package = PackageIndex::build(&entry, &files, 1).expect("package builds");
        let matches = package.symbols("hint", 1);
        let readable = package.assembled_symbol(matches[0]).expect("assembled");

        assert_eq!(
            readable.assembled().origin().location(),
            Some(&SourceLocation::Stdlib {})
        );
        assert_eq!(
            readable.identity().map(rift_core::SymbolId::as_str),
            Some("rift://symbol/rust/stdlib/rust@1.90.0/core/src/lib.rs/hint")
        );
    }

    #[test]
    fn test_package_index_refuses_a_file_no_provider_parses() {
        let entry = CatalogEntry::new(
            tokio(),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("README.unknown", "text")], 0);

        let error = PackageIndex::build(&entry, &files, 1).expect_err("no provider");

        assert_eq!(violation_of(&error), PackageIndexViolation::Syntax);
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::InternalError));
        assert_eq!(error.fault().path(), Some(Path::new("README.unknown")));
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_index_refuses_a_manager_that_is_no_resolver_identity() {
        let entry = CatalogEntry::new(
            identity("Cargo", "tokio", "1.53.1"),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);

        let error = PackageIndex::build(&entry, &files, 1).expect_err("uppercase manager");

        assert_eq!(violation_of(&error), PackageIndexViolation::Identity);
        assert!(std::error::Error::source(&error).is_some());
        assert!(error.to_string().contains("Cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_index_zero_revision_is_a_provider_refusal() {
        let entry = CatalogEntry::new(
            tokio(),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);

        let error = PackageIndex::build(&entry, &files, 0).expect_err("zero revision");

        assert_eq!(violation_of(&error), PackageIndexViolation::Provider);
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_package_index_language_without_visibility_rules_answers_every_declaration() {
        let entry = CatalogEntry::dependency(
            identity("npm", "docs", "1.0.0"),
            language(ShippedLanguage::Markdown),
            None,
            true,
        );
        let files = PackageFiles::new(vec![text("README.md", "# Guide\n\n## Install\n")], 0);

        let package = PackageIndex::build(&entry, &files, 1).expect("package builds");
        let matches = package.symbols("", 10);

        assert_eq!(names(&matches), ["Guide", "Guide > Install"]);
    }

    #[test]
    fn test_package_index_assembled_symbol_refuses_a_match_from_a_file_it_does_not_hold() {
        let alpha = rust_package("alpha", "pub fn spawn() {}\n");
        let entry = CatalogEntry::dependency(
            identity("cargo", "beta", "1.0.0"),
            language(ShippedLanguage::Rust),
            None,
            false,
        );
        let files = PackageFiles::new(vec![text("src/main.rs", "pub fn other() {}\n")], 0);
        let beta = PackageIndex::build(&entry, &files, 1).expect("package builds");
        let spawn = alpha.symbols("spawn", 1)[0];

        let error = beta
            .assembled_symbol(spawn)
            .expect_err("a file another package holds");

        assert_eq!(violation_of(&error), PackageIndexViolation::SymbolMissing);
        assert_eq!(error.fault().package(), &identity("cargo", "beta", "1.0.0"));
        assert_eq!(error.fault().path(), Some(Path::new("src/lib.rs")));
    }

    #[test]
    fn test_package_index_assembled_symbol_refuses_a_declaration_its_graph_does_not_hold() {
        let alpha = rust_package("alpha", "pub fn spawn() {}\n");
        let beta = rust_package("beta", "pub fn other() {}\n");
        let spawn = alpha.symbols("spawn", 1)[0];

        let error = beta
            .assembled_symbol(spawn)
            .expect_err("a declaration another package's graph holds");

        assert_eq!(violation_of(&error), PackageIndexViolation::SymbolMissing);
        assert_eq!(error.fault().path(), None);
        assert!(
            error
                .to_string()
                .contains("identity rift://symbol/rust/cargo/beta@1.0.0/src/lib.rs/spawn"),
            "{error}"
        );
    }

    #[test]
    fn test_package_index_refuses_a_file_whose_parse_fails() {
        let entry = CatalogEntry::new(
            tokio(),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let deep = format!("fn deep() {}{}", "{".repeat(600), "}".repeat(600));
        let files = PackageFiles::new(vec![text("src/lib.rs", &deep)], 0);

        let error =
            PackageIndex::build(&entry, &files, 1).expect_err("nesting past the depth bound");

        assert_eq!(violation_of(&error), PackageIndexViolation::Syntax);
        assert_eq!(error.fault().path(), Some(Path::new("src/lib.rs")));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_package_index_refuses_a_name_that_spells_no_source_path() {
        let entry = CatalogEntry::new(
            identity("cargo", "vendor\\tokio", "1.53.1"),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);

        let error = PackageIndex::build(&entry, &files, 1).expect_err("a backslash in the name");

        assert_eq!(violation_of(&error), PackageIndexViolation::Identity);
        assert_eq!(error.fault().path(), Some(Path::new("src/lib.rs")));
        assert!(
            std::error::Error::source(&error)
                .is_some_and(|source| source.downcast_ref::<PathError>().is_some()),
            "the source path refusal is the cause"
        );
    }

    #[test]
    fn test_package_index_refuses_an_identity_whose_unit_exceeds_the_bound() {
        let entry = CatalogEntry::new(
            identity("cargo", "tokio", &"#".repeat(2_800)),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);

        let error = PackageIndex::build(&entry, &files, 1).expect_err("a unit past the byte bound");

        assert_eq!(violation_of(&error), PackageIndexViolation::Identity);
        assert_eq!(error.fault().path(), Some(Path::new("src/lib.rs")));
        assert!(
            std::error::Error::source(&error)
                .is_some_and(|source| source.downcast_ref::<SourceUnitIdError>().is_some()),
            "the source unit refusal is the cause"
        );
    }
}
