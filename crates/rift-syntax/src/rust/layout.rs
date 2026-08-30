//! Rust crate layout: package roots, crate roots, and refined module candidates.
//!
//! [`RustCrateLayout`] classifies the project path set once: every directory holding a
//! `Cargo.toml` is a package root, and the Cargo target files under it are crate roots.
//! [`RustCrateLayout::refined_facts`] then recomputes each module declaration's candidate
//! paths under that layout: a crate root or a `mod.rs` file resolves modules beside
//! itself, any other file resolves them under its own stem, and each enclosing inline
//! module adds one directory. A declaration inside a block gains no candidates, and a
//! candidate that is itself a crate root is discarded, so a `mod` declaration never
//! adopts another crate's root.

use std::collections::{BTreeMap, BTreeSet};

use rift_binding::{
    BindingError, BindingLimits, Name, ScopeKind, UnitBindingFacts, UnitModuleDeclaration,
    UnitScopeIndex,
};

/// Manifest file name whose directory is a package root.
const CARGO_MANIFEST_FILE_NAME: &str = "Cargo.toml";
/// Package-relative paths of the primary library and binary crate roots.
const PRIMARY_TARGET_PATHS: [&str; 2] = ["src/lib.rs", "src/main.rs"];
/// Package-relative directories whose direct `.rs` files and `<name>/main.rs` files are
/// crate roots.
const TARGET_DIRECTORIES: [&str; 4] = ["src/bin", "tests", "examples", "benches"];
/// File name of a multi-file target's crate root inside its target directory.
const MAIN_FILE_NAME: &str = "main.rs";
/// File name of a module body held in the module's own directory.
const MOD_FILE_NAME: &str = "mod.rs";
/// File names whose `mod` declarations resolve beside the file when no package root
/// governs the unit.
pub(super) const DIRECTORY_OWNING_FILE_NAMES: [&str; 3] = ["lib.rs", "main.rs", "mod.rs"];
/// Extension a module candidate file carries.
pub(super) const RUST_FILE_SUFFIX: &str = ".rs";

/// Crate roots and module-candidate rules derived from the project path set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustCrateLayout {
    package_roots: BTreeSet<String>,
    crate_roots: BTreeSet<String>,
}

impl RustCrateLayout {
    /// Classifies `paths` once; the caller supplies every indexed project path.
    ///
    /// One pass collects package roots from `Cargo.toml` paths and a second classifies
    /// crate roots under the nearest package root, so the work is linear in the supplied
    /// set, whose size the caller bounds.
    #[must_use]
    pub fn new<Path: AsRef<str>>(paths: &[Path]) -> Self {
        let mut package_roots = BTreeSet::new();
        for path in paths {
            let (directory, file) = split_file(path.as_ref());
            if file == CARGO_MANIFEST_FILE_NAME {
                package_roots.insert(directory.to_owned());
            }
        }
        let mut layout = Self {
            package_roots,
            crate_roots: BTreeSet::new(),
        };
        for path in paths {
            let path = path.as_ref();
            if layout.classifies_as_crate_root(path) {
                layout.crate_roots.insert(path.to_owned());
            }
        }
        layout
    }

    /// Whether `path` is one crate's root source file.
    #[must_use]
    pub fn is_crate_root(&self, path: &str) -> bool {
        self.crate_roots.contains(path)
    }

    /// Whether module candidates for `path` resolve beside the file itself.
    ///
    /// A crate root and a `mod.rs` file own their directory; with no package root above
    /// the file, the file names `lib.rs`, `main.rs`, and `mod.rs` keep the parsed rule.
    #[must_use]
    pub fn owns_directory(&self, path: &str) -> bool {
        if self.is_crate_root(path) {
            return true;
        }
        let (_, file) = split_file(path);
        if file == MOD_FILE_NAME {
            return true;
        }
        self.package_root_of(path).is_none() && DIRECTORY_OWNING_FILE_NAMES.contains(&file)
    }

    /// The unit's facts with every module declaration's candidates recomputed.
    ///
    /// A declaration whose scope chain leaves module scopes - a `mod` inside a block -
    /// gains no candidates and is dropped; `rustc` refuses a file module inside a block
    /// without a `path` attribute. A candidate that is itself a crate root is discarded,
    /// so a stray `mod` declaration cannot adopt another crate's root; with no surviving
    /// candidate the declaration is dropped and its module link never forms.
    ///
    /// # Errors
    ///
    /// Returns [`BindingError`] when the replaced declarations do not validate against
    /// `limits`; the facts they derive from make any other refusal a programmer error.
    pub fn refined_facts(
        &self,
        unit_path: &str,
        facts: &UnitBindingFacts,
        limits: &BindingLimits,
    ) -> Result<UnitBindingFacts, BindingError> {
        let module_names = declared_module_names(facts);
        let mut declarations = Vec::with_capacity(facts.module_declarations().len());
        for declaration in facts.module_declarations() {
            let refined = self.refined_declaration(unit_path, facts, &module_names, declaration);
            declarations.extend(refined);
        }
        facts.with_module_declarations(declarations, limits)
    }

    /// One declaration's candidates under the layout; `None` drops the declaration.
    fn refined_declaration(
        &self,
        unit_path: &str,
        facts: &UnitBindingFacts,
        module_names: &BTreeMap<UnitScopeIndex, &Name>,
        declaration: &UnitModuleDeclaration,
    ) -> Option<UnitModuleDeclaration> {
        let definition = facts.definition(declaration.definition())?;
        let segments = inline_module_segments(facts, module_names, definition.scope())?;
        let mut directory = candidate_directory(unit_path, self.owns_directory(unit_path));
        for segment in segments {
            directory = joined(&directory, segment);
        }
        let candidates: Vec<String> =
            module_body_candidates(&directory, definition.name().as_str())
                .into_iter()
                .filter(|candidate| !self.is_crate_root(candidate))
                .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(UnitModuleDeclaration::new(
            declaration.definition(),
            candidates,
        ))
    }

    fn classifies_as_crate_root(&self, path: &str) -> bool {
        let Some(package) = self.package_root_of(path) else {
            return false;
        };
        let relative = if package.is_empty() {
            path
        } else {
            let Some(relative) = strip_directory(path, package) else {
                return false;
            };
            relative
        };
        crate_root_form(relative)
    }

    /// The nearest package root holding `path`, by longest directory prefix.
    ///
    /// Each step drops one path segment, so the walk ends within the segment count.
    fn package_root_of(&self, path: &str) -> Option<&str> {
        let mut directory = parent_directory(path);
        while let Some(current) = directory {
            if let Some(root) = self.package_roots.get(current) {
                return Some(root);
            }
            directory = parent_directory(current);
        }
        self.package_roots.get("").map(String::as_str)
    }
}

/// Whether a package-relative path is one of Cargo's discovered target files.
fn crate_root_form(relative: &str) -> bool {
    if PRIMARY_TARGET_PATHS.contains(&relative) {
        return true;
    }
    TARGET_DIRECTORIES.iter().any(|directory| {
        let Some(rest) = strip_directory(relative, directory) else {
            return false;
        };
        single_target_file(rest) || directory_target_main(rest)
    })
}

/// A direct `.rs` file of a target directory, such as `tests/it.rs`.
fn single_target_file(rest: &str) -> bool {
    !rest.contains('/') && rest.ends_with(RUST_FILE_SUFFIX)
}

/// A multi-file target's root, such as `src/bin/tool/main.rs`.
fn directory_target_main(rest: &str) -> bool {
    rest.split_once('/')
        .is_some_and(|(name, file)| !name.is_empty() && file == MAIN_FILE_NAME)
}

/// Module scopes each definition opens, keyed by the opened scope's index.
fn declared_module_names(facts: &UnitBindingFacts) -> BTreeMap<UnitScopeIndex, &Name> {
    let mut names = BTreeMap::new();
    for definition in facts.definitions() {
        if let Some(declared) = definition.declares() {
            names.entry(declared).or_insert_with(|| definition.name());
        }
    }
    names
}

/// Names of the inline modules between the unit scope and `scope`, outermost first.
///
/// `None` when the chain passes a non-module scope or a module scope no definition
/// names. A scope's parent precedes it in the unit table, so the walk strictly decreases
/// and the loop's bound cannot trip on builder-validated facts.
fn inline_module_segments<'facts>(
    facts: &UnitBindingFacts,
    module_names: &BTreeMap<UnitScopeIndex, &'facts Name>,
    scope: UnitScopeIndex,
) -> Option<Vec<&'facts str>> {
    let mut segments = Vec::new();
    let mut index = scope;
    for _ in 0..=facts.scopes().len() {
        let current = facts.scope(index)?;
        let Some(parent) = current.parent() else {
            if current.kind() != ScopeKind::Module {
                return None;
            }
            segments.reverse();
            return Some(segments);
        };
        if current.kind() != ScopeKind::Module {
            return None;
        }
        segments.push(module_names.get(&index)?.as_str());
        index = parent;
    }
    None
}

/// The directory a file's module candidates resolve under.
pub(super) fn candidate_directory(unit_path: &str, owns_directory: bool) -> String {
    let (directory, file) = split_file(unit_path);
    if owns_directory {
        directory.to_owned()
    } else {
        let stem = file.strip_suffix(RUST_FILE_SUFFIX).unwrap_or(file);
        joined(directory, stem)
    }
}

/// The file and directory bodies that could hold `module`, strongest first.
pub(super) fn module_body_candidates(directory: &str, module: &str) -> [String; 2] {
    [
        joined(directory, &format!("{module}{RUST_FILE_SUFFIX}")),
        joined(directory, &format!("{module}/{MOD_FILE_NAME}")),
    ]
}

/// The path's directory and file name; the directory is empty for a bare file name.
fn split_file(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn parent_directory(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(directory, _)| directory)
}

/// The path under `directory`, when the directory is a proper prefix.
fn strip_directory<'path>(path: &'path str, directory: &str) -> Option<&'path str> {
    path.strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix('/'))
}

fn joined(directory: &str, tail: &str) -> String {
    if directory.is_empty() {
        tail.to_owned()
    } else {
        format!("{directory}/{tail}")
    }
}

#[cfg(test)]
mod tests {
    use rift_binding::{
        BindingGraph, BindingLimits, LinkedGraph, NeverCancelled, ResolutionSet, UnitBindingFacts,
        assemble, resolve_all,
    };
    use rift_core::{ContributionOrigin, SourceUnitId};

    use super::super::fixture::{analyze, offset, origin, source_unit, targets_at};
    use super::RustCrateLayout;
    use crate::SyntaxDocument;

    fn layout(paths: &[&str]) -> RustCrateLayout {
        RustCrateLayout::new(paths)
    }

    /// Refines, assembles, links, and resolves the documents under the layout the
    /// documents' paths and `manifests` describe.
    fn resolved(
        documents: &[(&str, &SyntaxDocument)],
        manifests: &[&str],
    ) -> (BindingGraph, ResolutionSet) {
        let limits = BindingLimits::default();
        let mut paths: Vec<String> = manifests.iter().map(ToString::to_string).collect();
        paths.extend(documents.iter().map(|(path, _)| (*path).to_owned()));
        let layout = RustCrateLayout::new(&paths);
        let refined: Vec<(SourceUnitId, ContributionOrigin, UnitBindingFacts)> = documents
            .iter()
            .map(|(path, document)| {
                let facts = document.binding().expect("binding facts extracted");
                let facts = layout
                    .refined_facts(path, facts, &limits)
                    .expect("facts refine");
                (source_unit(path), origin(), facts)
            })
            .collect();
        let units: Vec<_> = refined
            .iter()
            .map(|(unit, origin, facts)| (unit.clone(), origin.clone(), facts))
            .collect();
        let graph = assemble(&units, &limits).expect("facts assemble");
        let set = {
            let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
            resolve_all(&linked, &limits, &NeverCancelled).expect("resolution completes")
        };
        (graph, set)
    }

    #[test]
    fn test_layout_primary_targets_classified_crate_roots() {
        let layout = layout(&["Cargo.toml", "src/lib.rs", "src/main.rs", "src/other.rs"]);
        assert!(layout.is_crate_root("src/lib.rs"));
        assert!(layout.is_crate_root("src/main.rs"));
        assert!(!layout.is_crate_root("src/other.rs"));
        assert!(layout.owns_directory("src/lib.rs"));
        assert!(!layout.owns_directory("src/other.rs"));
    }

    #[test]
    fn test_layout_target_directories_classify_files_and_main_directories() {
        let roots = [
            "src/bin/tool.rs",
            "src/bin/multi/main.rs",
            "tests/it.rs",
            "tests/multi/main.rs",
            "examples/simple.rs",
            "examples/multi/main.rs",
            "benches/large.rs",
            "benches/multi/main.rs",
        ];
        let others = [
            "src/bin/multi/helper.rs",
            "src/bin/deep/nested/main.rs",
            "tests/helper/util.rs",
            "src/tests/it.rs",
            "src/binx.rs",
        ];
        let mut paths = vec!["Cargo.toml"];
        paths.extend(roots);
        paths.extend(others);
        let layout = layout(&paths);
        for path in roots {
            assert!(layout.is_crate_root(path), "{path} is a crate root");
        }
        for path in others {
            assert!(!layout.is_crate_root(path), "{path} is not a crate root");
        }
    }

    #[test]
    fn test_layout_nearest_package_root_wins() {
        let layout = layout(&[
            "Cargo.toml",
            "a/Cargo.toml",
            "a/src/lib.rs",
            "src/lib.rs",
            "b/src/lib.rs",
        ]);
        assert!(layout.is_crate_root("a/src/lib.rs"));
        assert!(layout.is_crate_root("src/lib.rs"));
        assert!(
            !layout.is_crate_root("b/src/lib.rs"),
            "no package root holds b, so the workspace root's src rule does not reach it"
        );
    }

    #[test]
    fn test_layout_without_manifest_keeps_file_name_rule() {
        let layout = layout(&["src/lib.rs", "src/worker.rs", "nested/mod.rs"]);
        assert!(!layout.is_crate_root("src/lib.rs"));
        assert!(layout.owns_directory("src/lib.rs"));
        assert!(layout.owns_directory("main.rs"));
        assert!(!layout.owns_directory("src/worker.rs"));
        assert!(layout.owns_directory("nested/mod.rs"));
    }

    #[test]
    fn test_layout_manifest_narrows_directory_owners_to_roots_and_mod_files() {
        let layout = layout(&[
            "Cargo.toml",
            "src/lib.rs",
            "src/nested/mod.rs",
            "src/x/lib.rs",
        ]);
        assert!(layout.owns_directory("src/nested/mod.rs"));
        assert!(layout.owns_directory("src/lib.rs"));
        assert!(
            !layout.owns_directory("src/x/lib.rs"),
            "under a package root only a crate root or mod.rs owns its directory"
        );
    }

    #[test]
    fn test_layout_refined_block_module_declaration_dropped() {
        let document = analyze("src/lib.rs", "fn f() { mod x; }\n");
        let facts = document.binding().expect("binding facts extracted");
        assert_eq!(facts.module_declarations().len(), 1);
        let layout = layout(&["Cargo.toml", "src/lib.rs"]);
        let refined = layout.refined_facts("src/lib.rs", facts, &BindingLimits::default());
        let refined = refined.expect("facts refine");
        assert_eq!(refined.module_declarations(), &[]);
    }

    #[test]
    fn test_layout_refined_block_rooted_unit_drops_declaration() {
        use rift_binding::{
            DefinitionOrder, ScopeKind, UnitDefinition, UnitModuleDeclaration, VisibilitySpelling,
        };
        use rift_core::{ExactKind, SourceRange};
        let limits = BindingLimits::default();
        let mut builder = rift_binding::UnitBindingFacts::builder(limits);
        let range = SourceRange::new(0, 20).expect("fixture range");
        let root = builder
            .scope(ScopeKind::Block, range, None)
            .expect("root scope accepted");
        let name = rift_binding::Name::new("x").expect("fixture name");
        let definition = UnitDefinition::new(
            root,
            name,
            range,
            ExactKind("rust.module".to_owned()),
            DefinitionOrder::Item,
            VisibilitySpelling::Private,
        );
        let definition = builder.definition(definition).expect("definition accepted");
        let declaration = UnitModuleDeclaration::new(definition, vec!["src/x.rs".to_owned()]);
        builder
            .module_declaration(declaration)
            .expect("declaration accepted");
        let facts = builder.build();
        let layout = layout(&["Cargo.toml", "src/lib.rs"]);
        let refined = layout.refined_facts("src/lib.rs", &facts, &limits);
        let refined = refined.expect("facts refine");
        assert_eq!(
            refined.module_declarations(),
            &[],
            "a unit rooted in a block scope resolves no file modules"
        );
    }

    #[test]
    fn test_layout_refined_crate_root_candidate_discarded() {
        let document = analyze("src/lib.rs", "mod main;\n");
        let facts = document.binding().expect("binding facts extracted");
        let layout = layout(&["Cargo.toml", "src/lib.rs", "src/main.rs"]);
        let refined = layout.refined_facts("src/lib.rs", facts, &BindingLimits::default());
        let refined = refined.expect("facts refine");
        assert_eq!(refined.module_declarations().len(), 1);
        assert_eq!(
            refined.module_declarations()[0].candidates(),
            ["src/main/mod.rs".to_owned()],
            "the file candidate is another crate's root and is discarded"
        );
    }

    #[test]
    fn test_layout_two_packages_same_name_definitions_stay_isolated() {
        let first = analyze("a/src/lib.rs", "pub fn run() {}\n");
        let text = "pub fn run() {}\nfn h() { run(); }\n";
        let second = analyze("b/src/lib.rs", text);
        let documents = [("a/src/lib.rs", &first), ("b/src/lib.rs", &second)];
        let (graph, set) = resolved(&documents, &["a/Cargo.toml", "b/Cargo.toml"]);
        let targets = targets_at(&graph, &set, "b/src/lib.rs", offset(text, "run();"));
        assert_eq!(targets, [("run".to_owned(), "b/src/lib.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_layout_super_import_from_child_unit_resolves_parent_definition() {
        let lib = "mod child;\npub fn helper() {}\n";
        let child = "use super::helper;\nfn h() { helper(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let child_document = analyze("src/child.rs", child);
        let documents = [
            ("src/lib.rs", &lib_document),
            ("src/child.rs", &child_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(&graph, &set, "src/child.rs", offset(child, "helper();"));
        let helper_at = offset(lib, "pub fn helper");
        assert_eq!(
            targets,
            [("helper".to_owned(), "src/lib.rs".to_owned(), helper_at)]
        );
    }

    #[test]
    fn test_layout_reexport_chain_across_three_units_resolves() {
        let lib = "mod middle;\nmod deep;\nuse middle::item;\nfn h() { item(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let middle_document = analyze("src/middle.rs", "pub use crate::deep::item;\n");
        let deep_document = analyze("src/deep.rs", "pub fn item() {}\n");
        let documents = [
            ("src/lib.rs", &lib_document),
            ("src/middle.rs", &middle_document),
            ("src/deep.rs", &deep_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "item();"));
        assert_eq!(targets, [("item".to_owned(), "src/deep.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_layout_import_cycle_across_units_terminates_unresolved() {
        let first = "pub use crate::b::x;\nfn h() { x(); }\n";
        let lib_document = analyze("src/lib.rs", "mod a;\nmod b;\n");
        let first_document = analyze("src/a.rs", first);
        let second_document = analyze("src/b.rs", "pub use crate::a::x;\n");
        let documents = [
            ("src/lib.rs", &lib_document),
            ("src/a.rs", &first_document),
            ("src/b.rs", &second_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(&graph, &set, "src/a.rs", offset(first, "x();"));
        assert_eq!(targets, [], "the import cycle terminates with no target");
    }

    /// Targets of `a::x::run()` in a `src/lib.rs` declaring `mod a { pub mod x; }`, with
    /// the module body at `body_path`.
    fn inline_module_targets(body_path: &str) -> Vec<(String, String, u64)> {
        let lib = "mod a { pub mod x; }\nfn h() { a::x::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let body_document = analyze(body_path, "pub fn run() {}\n");
        let documents = [("src/lib.rs", &lib_document), (body_path, &body_document)];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        targets_at(&graph, &set, "src/lib.rs", offset(lib, "a::x::run"))
    }

    #[test]
    fn test_layout_inline_module_declaration_resolves_file_candidate() {
        let targets = inline_module_targets("src/a/x.rs");
        assert_eq!(targets, [("run".to_owned(), "src/a/x.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_layout_inline_module_declaration_resolves_directory_candidate() {
        let targets = inline_module_targets("src/a/x/mod.rs");
        assert_eq!(
            targets,
            [("run".to_owned(), "src/a/x/mod.rs".to_owned(), 0)]
        );
    }

    #[test]
    fn test_layout_inline_module_in_named_file_prefixes_its_stem() {
        let lib = "mod holder;\nfn h() { holder::ha::hx::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let holder_document = analyze("src/holder.rs", "pub mod ha { pub mod hx; }\n");
        let body_document = analyze("src/holder/ha/hx.rs", "pub fn run() {}\n");
        let documents = [
            ("src/lib.rs", &lib_document),
            ("src/holder.rs", &holder_document),
            ("src/holder/ha/hx.rs", &body_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(&graph, &set, "src/lib.rs", offset(lib, "holder::ha::hx"));
        assert_eq!(
            targets,
            [("run".to_owned(), "src/holder/ha/hx.rs".to_owned(), 0)]
        );
    }

    #[test]
    fn test_layout_bin_crate_root_resolves_modules_beside_itself() {
        let tool = "mod x;\nfn main() { x::run(); }\n";
        let tool_document = analyze("src/bin/tool.rs", tool);
        let body_document = analyze("src/bin/x/mod.rs", "pub fn run() {}\n");
        let documents = [
            ("src/bin/tool.rs", &tool_document),
            ("src/bin/x/mod.rs", &body_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(&graph, &set, "src/bin/tool.rs", offset(tool, "x::run"));
        assert_eq!(
            targets,
            [("run".to_owned(), "src/bin/x/mod.rs".to_owned(), 0)]
        );
    }

    #[test]
    fn test_layout_multi_file_bin_root_resolves_sibling_module() {
        let main = "mod x;\nfn main() { x::run(); }\n";
        let main_document = analyze("src/bin/multi/main.rs", main);
        let body_document = analyze("src/bin/multi/x.rs", "pub fn run() {}\n");
        let documents = [
            ("src/bin/multi/main.rs", &main_document),
            ("src/bin/multi/x.rs", &body_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let targets = targets_at(
            &graph,
            &set,
            "src/bin/multi/main.rs",
            offset(main, "x::run"),
        );
        assert_eq!(
            targets,
            [("run".to_owned(), "src/bin/multi/x.rs".to_owned(), 0)]
        );
    }

    #[test]
    fn test_layout_stray_module_declaration_cannot_adopt_crate_root() {
        let lib = "mod main;\nfn h() { main::run(); }\n";
        let main = "pub fn run() {}\nfn h() { crate::run(); }\n";
        let lib_document = analyze("src/lib.rs", lib);
        let main_document = analyze("src/main.rs", main);
        let documents = [
            ("src/lib.rs", &lib_document),
            ("src/main.rs", &main_document),
        ];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let adopted = targets_at(&graph, &set, "src/lib.rs", offset(lib, "main::run"));
        assert_eq!(
            adopted,
            [],
            "the binary crate root is not adopted as a module"
        );
        let own = targets_at(&graph, &set, "src/main.rs", offset(main, "crate::run"));
        assert_eq!(own, [("run".to_owned(), "src/main.rs".to_owned(), 0)]);
    }

    #[test]
    fn test_layout_tests_unit_is_its_own_crate_root() {
        let it = "pub fn local() {}\nfn h() { crate::local(); crate::x(); }\n";
        let lib_document = analyze("src/lib.rs", "pub fn x() {}\n");
        let it_document = analyze("tests/it.rs", it);
        let documents = [("src/lib.rs", &lib_document), ("tests/it.rs", &it_document)];
        let (graph, set) = resolved(&documents, &["Cargo.toml"]);
        let local = targets_at(&graph, &set, "tests/it.rs", offset(it, "crate::local"));
        assert_eq!(local, [("local".to_owned(), "tests/it.rs".to_owned(), 0)]);
        let reached = targets_at(&graph, &set, "tests/it.rs", offset(it, "crate::x"));
        assert_eq!(reached, [], "crate paths in a test crate stay inside it");
    }
}
