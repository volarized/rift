//! Callees outside the project: the installed packages an outgoing walk finds them in, and
//! the callee a walk holds until the global API names the declaration at its position.
//!
//! A language engine names each callee by its file and the range of its name. A file below
//! the served tree maps to the project declaration holding that range. A file below an
//! installed package's root is one of the package's files: the local index holds no
//! declaration there, so the walk records the package, the file's path in it, and the
//! position, and the server asks the global API which declaration holds each position, once
//! per walk. The embedded ty engine names a Python standard library callee by its typeshed
//! stub, `vendored://stdlib/<path>`, which the global index holds as `stdlib/python` at
//! `<path>`.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Position, SymbolKind, Uri};
use rift_core::{PackageIdentity, ProjectPath, SymbolId as CoreSymbolId};
use rift_dependency::{
    DependencyContext, InstallFolder, InstallLocation, STANDARD_LIBRARY_MANAGER, StandardLibrary,
};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::uri::{EngineAddress, EngineRoots, PackageRoot, TreeRoot, UriError, UriFault};
use rift_protocol::read::{
    ExactKind, Language, SourceKind, SourceLocationKind, SourceUnitId, Symbol, SymbolId,
    SymbolOrigin,
};

use crate::engine::EnginePool;
use crate::read::ReadService;

/// The scheme and authority the embedded ty engine names a typeshed stub by, before the
/// stub's path below typeshed's `stdlib` folder.
const VENDORED_STDLIB_PREFIX: &str = "vendored://stdlib/";

/// Most registry source folders below `$CARGO_HOME/registry/src` the roots read. Cargo
/// keeps one per registry the machine fetched from, so a machine holds a handful.
const REGISTRY_SOURCES_MAX: usize = 64;

/// Bytes one `typescript` `package.json` may hold before the roots leave it unread.
const PACKAGE_MANIFEST_BYTES_MAX: u64 = 1 << 20;

/// The npm package whose `lib.*.d.ts` files declare the ECMAScript built-ins.
const TYPESCRIPT_PACKAGE: &str = "typescript";

/// The npm package manager name.
const NPM_MANAGER: &str = "npm";

/// The folder Node.js resolves a package name below, at every ancestor of the module
/// asking for it.
const NODE_MODULES_DIRECTORY_NAME: &str = "node_modules";

/// The manifest naming an npm package's version.
const PACKAGE_MANIFEST_FILE_NAME: &str = "package.json";

/// The words the Language Server Protocol names its symbol kinds by, in `SymbolKind`
/// order, so a package callee's hit states the kind its engine named. `language` rides
/// beside the kind, as it does for every symbol, so the word carries no language prefix.
const SYMBOL_KIND_WORDS: [(SymbolKind, &str); 26] = [
    (SymbolKind::FILE, "file"),
    (SymbolKind::MODULE, "module"),
    (SymbolKind::NAMESPACE, "namespace"),
    (SymbolKind::PACKAGE, "package"),
    (SymbolKind::CLASS, "class"),
    (SymbolKind::METHOD, "method"),
    (SymbolKind::PROPERTY, "property"),
    (SymbolKind::FIELD, "field"),
    (SymbolKind::CONSTRUCTOR, "constructor"),
    (SymbolKind::ENUM, "enum"),
    (SymbolKind::INTERFACE, "interface"),
    (SymbolKind::FUNCTION, "function"),
    (SymbolKind::VARIABLE, "variable"),
    (SymbolKind::CONSTANT, "constant"),
    (SymbolKind::STRING, "string"),
    (SymbolKind::NUMBER, "number"),
    (SymbolKind::BOOLEAN, "boolean"),
    (SymbolKind::ARRAY, "array"),
    (SymbolKind::OBJECT, "object"),
    (SymbolKind::KEY, "key"),
    (SymbolKind::NULL, "null"),
    (SymbolKind::ENUM_MEMBER, "enum_member"),
    (SymbolKind::STRUCT, "struct"),
    (SymbolKind::EVENT, "event"),
    (SymbolKind::OPERATOR, "operator"),
    (SymbolKind::TYPE_PARAMETER, "type_parameter"),
];

/// The installed packages an outgoing walk addresses a callee's file through.
///
/// Built from the dependency context's install folders, the Cargo registry source folders,
/// and the `typescript` package a TypeScript engine runs. A package root nested in the
/// served tree, an install under `node_modules` or `.venv`, answers before the tree.
#[derive(Clone, Debug, Default)]
pub struct CalleeRoots {
    packages: Arc<[PackageRoot]>,
}

impl CalleeRoots {
    /// Reads where this machine installed the packages `reads`' dependency context names,
    /// blocking.
    ///
    /// Each install folder becomes one root, and a folder below the root the index reads
    /// the tree by also becomes one below each engine's own spelling of the workspace
    /// root, since an engine names the files it reads by the root it was started at. A
    /// Cargo registry package becomes one root below each registry source folder of
    /// `$CARGO_HOME`, at most `REGISTRY_SOURCES_MAX` of them. A TypeScript engine adds the
    /// `typescript` package its program resolves, whose `lib.*.d.ts` files declare the
    /// built-ins it answers.
    ///
    /// The work is one directory listing, one program lookup and at most one ancestor walk
    /// per TypeScript engine, and one pass over the context's install folders, which the
    /// context bounds.
    #[must_use]
    pub fn read(reads: &ReadService, engines: &EnginePool) -> Self {
        let context: &DependencyContext = reads.dependency_context();
        let index_root = reads.index().root();
        let mut engine_roots: Vec<&Path> = engines
            .served_slots()
            .map(|(_, slot)| slot.workspace_root())
            .filter(|root| *root != index_root)
            .collect();
        engine_roots.sort_unstable();
        engine_roots.dedup();
        let spellings = RootSpellings {
            index_root,
            engine_roots: &engine_roots,
        };
        let registries = registry_sources(cargo_home().as_deref());
        let mut packages: Vec<PackageRoot> = context
            .install_folders()
            .flat_map(|folder| folder_roots(folder, &registries, spellings))
            .collect();
        packages.extend(engine_typescript_roots(engines));
        packages.sort_by(|left, right| root_order(left).cmp(&root_order(right)));
        packages.dedup();
        Self {
            packages: packages.into(),
        }
    }

    /// Roots holding `packages` alone, for a test that names its own.
    #[cfg(test)]
    pub(crate) fn from_packages(packages: Vec<PackageRoot>) -> Self {
        Self {
            packages: packages.into(),
        }
    }

    /// Where `uri` points: a project file below one of `trees`, the first spelling
    /// holding it winning, or an installed package's file. `None` for a URI under no root.
    ///
    /// # Errors
    ///
    /// Returns [`UriError`] for a URI or root the rules refuse other than one falling
    /// outside every root.
    pub(crate) fn address(
        &self,
        trees: &[&Path],
        uri: &Uri,
    ) -> Result<Option<EngineAddress>, UriError> {
        for tree in trees {
            let roots =
                EngineRoots::new(TreeRoot::new(tree)?).with_packages(Arc::clone(&self.packages));
            match roots.address(uri) {
                Ok(address) => return Ok(Some(address)),
                Err(error) if matches!(error.fault(), UriFault::OutsideRoot) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

/// The order package roots keep: root, then manager, name, and version.
fn root_order(root: &PackageRoot) -> (&TreeRoot, &str, &str, &str) {
    let package = root.package();
    (
        root.root(),
        &package.manager,
        &package.name,
        &package.version,
    )
}

/// The spellings one workspace root takes: the one the index reads the tree by, and each
/// one an engine was started at.
#[derive(Clone, Copy)]
struct RootSpellings<'roots> {
    index_root: &'roots Path,
    engine_roots: &'roots [&'roots Path],
}

impl RootSpellings<'_> {
    /// `folder`, then the same folder below each engine's spelling when it lies below the
    /// index root.
    fn of(self, folder: &Path) -> Vec<PathBuf> {
        let mut spelled = vec![folder.to_path_buf()];
        if let Ok(below) = folder.strip_prefix(self.index_root) {
            spelled.extend(self.engine_roots.iter().map(|root| root.join(below)));
        }
        spelled
    }
}

/// The package roots one install folder names: one per spelling, and for a Cargo registry
/// package one per registry source folder.
fn folder_roots(
    folder: &InstallFolder,
    registries: &[PathBuf],
    spellings: RootSpellings<'_>,
) -> Vec<PackageRoot> {
    let package = &folder.package;
    let identity = PackageIdentity {
        manager: package.manager.clone(),
        name: package.name.clone(),
        version: package.version.clone(),
    };
    match &folder.location {
        InstallLocation::Path(path) => spellings
            .of(path)
            .iter()
            .filter_map(|spelled| TreeRoot::new(spelled).ok())
            .map(|root| PackageRoot::new(root, identity.clone()))
            .collect(),
        InstallLocation::ImportRoot {
            site_packages,
            root,
        } => {
            let Ok(within) = ProjectPath::new(root.as_str()) else {
                return Vec::new();
            };
            spellings
                .of(site_packages)
                .iter()
                .filter_map(|spelled| TreeRoot::new(spelled).ok())
                .map(|base| PackageRoot::within(&base, within.clone(), identity.clone()))
                .collect()
        }
        InstallLocation::CargoRegistry(unpacked) => registries
            .iter()
            .filter_map(|source| TreeRoot::new(&source.join(unpacked)).ok())
            .map(|root| PackageRoot::new(root, identity.clone()))
            .collect(),
    }
}

/// `$CARGO_HOME`, or `.cargo` below the home folder when it is unset, as Cargo reads it.
fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
}

/// The registry source folders below `cargo_home`, in name order, at most
/// `REGISTRY_SOURCES_MAX`. Cargo unpacks a registry package below one per registry,
/// `registry/src/index.crates.io-<hash>` for crates.io.
fn registry_sources(cargo_home: Option<&Path>) -> Vec<PathBuf> {
    let Some(sources) = cargo_home.map(|home| home.join("registry").join("src")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&sources) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    folders.sort();
    if folders.len() > REGISTRY_SOURCES_MAX {
        tracing::warn!(
            component = "engine",
            folders = folders.len(),
            bound = REGISTRY_SOURCES_MAX,
            "Cargo registry source folders past the bound address no callee"
        );
        folders.truncate(REGISTRY_SOURCES_MAX);
    }
    folders
}

/// The `typescript` package each engine serving JavaScript or TypeScript runs, as a root.
///
/// typescript-language-server answers from the workspace's own `typescript` when that
/// package ships a server, and from the one its own program resolves otherwise, such as
/// 5.9.3 beside a workspace pinning 7.0.2. The workspace's copy is an install folder the
/// context already names; this names the other.
fn engine_typescript_roots(engines: &EnginePool) -> Vec<PackageRoot> {
    engines
        .served_slots()
        .filter(|(language, _)| {
            StandardLibrary::for_language(language) == Some(StandardLibrary::Node)
        })
        .filter_map(|(_, slot)| {
            let command = slot.configuration().command.as_ref()?;
            let search_path = slot
                .configuration()
                .environment
                .get("PATH")
                .map(std::ffi::OsString::from)
                .or_else(|| std::env::var_os("PATH"));
            let program =
                which::which_in(command.program(), search_path, slot.workspace_root()).ok()?;
            node_package_root(&fs::canonicalize(program).ok()?, TYPESCRIPT_PACKAGE)
        })
        .collect()
}

/// The package `name` Node.js resolves for a module at `module`: the first
/// `node_modules/<name>` holding a `package.json` with a version, over the module's
/// ancestors. The walk is bounded by the module path's own depth.
fn node_package_root(module: &Path, name: &str) -> Option<PackageRoot> {
    module.ancestors().skip(1).find_map(|folder| {
        let package = folder.join(NODE_MODULES_DIRECTORY_NAME).join(name);
        let version = package_version(&package.join(PACKAGE_MANIFEST_FILE_NAME))?;
        let identity = PackageIdentity {
            manager: NPM_MANAGER.to_owned(),
            name: name.to_owned(),
            version,
        };
        Some(PackageRoot::new(TreeRoot::new(&package).ok()?, identity))
    })
}

/// The `version` one `package.json` names, when it is a file of at most
/// `PACKAGE_MANIFEST_BYTES_MAX` bytes naming one.
fn package_version(manifest: &Path) -> Option<String> {
    let file = fs::File::open(manifest).ok()?;
    let mut bytes = Vec::new();
    file.take(PACKAGE_MANIFEST_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > PACKAGE_MANIFEST_BYTES_MAX {
        return None;
    }
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let version = manifest.get("version")?.as_str()?;
    (!version.is_empty()).then(|| version.to_owned())
}

/// The package a callee sits in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalleePackage {
    /// An installed package, at the exact version its install folder names.
    Installed(PackageIdentity),
    /// A standard library whose release the global API resolves from the context's
    /// requirement: Python's, whose typeshed stubs every supported release shares.
    StandardLibrary(StandardLibrary),
}

impl CalleePackage {
    /// The package manager the callee's package belongs to.
    #[must_use]
    pub fn manager(&self) -> &str {
        match self {
            Self::Installed(package) => &package.manager,
            Self::StandardLibrary(_) => STANDARD_LIBRARY_MANAGER,
        }
    }

    /// The package's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Installed(package) => &package.name,
            Self::StandardLibrary(library) => library.name(),
        }
    }
}

/// One callee a walk found in a package file, waiting for the global API to name the
/// declaration at its position.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageCallee {
    caller: CoreSymbolId,
    package: CalleePackage,
    path: ProjectPath,
    position: Position,
    encoding: PositionEncoding,
    name: String,
    kind: SymbolKind,
}

impl PackageCallee {
    /// The package holding the callee.
    #[must_use]
    pub const fn package(&self) -> &CalleePackage {
        &self.package
    }

    /// The callee's file, below the package's root, such as `src/lib.rs`.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// The start of the callee's name in its file, counted in [`Self::encoding`].
    #[must_use]
    pub const fn position(&self) -> Position {
        self.position
    }

    /// How the engine that named the callee counts characters within a line.
    #[must_use]
    pub const fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// The declaration whose call the engine named.
    pub(crate) const fn caller(&self) -> &CoreSymbolId {
        &self.caller
    }

    /// The symbol a walk's hit for this callee carries, once the global API named its
    /// declaration `id` in `package`: the name and kind are the engine's. The origin is
    /// the one package analysis gives the package's declarations: `stdlib` for a standard
    /// library, the Rust one included although its callees reach it through an install
    /// folder, and `dependency` naming the package otherwise. `None` for an id naming no
    /// language, or a kind the protocol does not define.
    pub(crate) fn symbol(&self, id: &SymbolId, package: &PackageIdentity) -> Option<Symbol> {
        let parsed = rift_core::parse_symbol_identity(&id.0).ok()?;
        let language = Language::from_identity_segment(parsed.language_segment()).ok()?;
        let word = SYMBOL_KIND_WORDS
            .iter()
            .find_map(|(kind, word)| (*kind == self.kind).then_some(*word))?;
        let origin = if package.manager == STANDARD_LIBRARY_MANAGER {
            SymbolOrigin {
                location: Some(SourceLocationKind::Stdlib),
                package: None,
                source_kind: SourceKind::Authored,
            }
        } else {
            SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                source_kind: SourceKind::Authored,
            }
        };
        Some(Symbol {
            id: Some(id.clone()),
            kind: ExactKind(word.to_owned()),
            language,
            name: self.name.clone(),
            facets: Vec::new(),
            origin,
            container: None,
            modifiers: Vec::new(),
            visibility: None,
            types: Vec::new(),
            signatures: Vec::new(),
            documentation: Vec::new(),
            extensions: rift_protocol::read::Extensions::default(),
            document_local: false,
        })
    }

    /// The source unit of the callee's file in `package`.
    pub(crate) fn unit(&self, package: &PackageIdentity) -> Option<SourceUnitId> {
        rift_core::SourceUnitId::for_package(package, &self.path)
            .ok()
            .map(|unit| SourceUnitId(unit.to_string()))
    }
}

/// One callee's file and name as the engine named them.
#[derive(Clone, Copy)]
pub(crate) struct NamedCallee<'call> {
    pub(crate) uri: &'call Uri,
    pub(crate) name: &'call str,
    pub(crate) kind: SymbolKind,
    pub(crate) position: Position,
    pub(crate) encoding: PositionEncoding,
}

/// Where one callee's file stands.
pub(crate) enum CalleeFile {
    /// A file below the served tree.
    Project(rift_core::ProjectPath),
    /// A package's file, held until the global API names the declaration.
    Package(PackageCallee),
    /// A file under no root, or a scheme no root reads.
    Unaddressed,
}

/// Classifies one callee's file: a typeshed stub the embedded engine names, a file below
/// the served tree or an installed package's root, or neither.
///
/// # Errors
///
/// Returns [`UriError`] for a `file` URI the rules refuse other than one outside every
/// root.
pub(crate) fn callee_file(
    roots: &CalleeRoots,
    trees: &[&Path],
    caller: &CoreSymbolId,
    call: NamedCallee<'_>,
) -> Result<CalleeFile, UriError> {
    let held = |package: CalleePackage, path: ProjectPath| {
        CalleeFile::Package(PackageCallee {
            caller: caller.clone(),
            package,
            path,
            position: call.position,
            encoding: call.encoding,
            name: call.name.to_owned(),
            kind: call.kind,
        })
    };
    if let Some(stub) = call.uri.as_str().strip_prefix(VENDORED_STDLIB_PREFIX) {
        return Ok(
            ProjectPath::new(stub).map_or(CalleeFile::Unaddressed, |path| {
                held(
                    CalleePackage::StandardLibrary(StandardLibrary::Python),
                    path,
                )
            }),
        );
    }
    let file_scheme = call
        .uri
        .scheme()
        .is_some_and(|scheme| scheme.as_str().eq_ignore_ascii_case("file"));
    if !file_scheme {
        return Ok(CalleeFile::Unaddressed);
    }
    Ok(match roots.address(trees, call.uri)? {
        Some(EngineAddress::Project(path)) => CalleeFile::Project(path),
        Some(EngineAddress::Package(file)) => held(
            CalleePackage::Installed(file.package().clone()),
            file.path().clone(),
        ),
        None => CalleeFile::Unaddressed,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use lsp_types::{Position, SymbolKind, Uri};
    use rift_core::{PackageIdentity, ProjectPath, SymbolId as CoreSymbolId};
    use rift_dependency::{InstallFolder, InstallLocation, StandardLibrary};
    use rift_lsp::capabilities::PositionEncoding;
    use rift_lsp::uri::{EngineAddress, PackageRoot, TreeRoot};
    use rift_protocol::read::SymbolId;

    use super::{
        CalleeFile, CalleePackage, CalleeRoots, NamedCallee, PackageCallee, RootSpellings,
        callee_file, folder_roots, node_package_root, package_version, registry_sources,
    };
    use crate::{EnginePool, LspProcessKey};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn identity(manager: &str, name: &str, version: &str) -> PackageIdentity {
        PackageIdentity {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    fn installed(package: PackageIdentity, location: InstallLocation) -> InstallFolder {
        InstallFolder { package, location }
    }

    /// Each root's slash form and the package it names, as `manager/name@version`.
    fn spelled(roots: &[PackageRoot]) -> Vec<(String, String)> {
        roots
            .iter()
            .map(|root| {
                let uri = root
                    .root()
                    .root_uri()
                    .expect("an absolute root forms a URI");
                let package = root.package();
                (
                    uri.as_str().to_owned(),
                    format!("{}/{}@{}", package.manager, package.name, package.version),
                )
            })
            .collect()
    }

    #[test]
    fn an_install_folder_below_the_index_root_is_spelled_below_each_engine_root_too() {
        let engine_roots = [Path::new("/var/ws")];
        let spellings = RootSpellings {
            index_root: Path::new("/private/var/ws"),
            engine_roots: &engine_roots,
        };
        let npm = installed(
            identity("npm", "nanoid", "5.1.6"),
            InstallLocation::Path(PathBuf::from("/private/var/ws/node_modules/nanoid")),
        );
        let sysroot = installed(
            identity("stdlib", "rust", "1.98.1"),
            InstallLocation::Path(PathBuf::from("/toolchain/lib/rustlib/src/rust/library")),
        );
        assert_eq!(
            spelled(&folder_roots(&npm, &[], spellings)),
            [
                (
                    "file:///private/var/ws/node_modules/nanoid".to_owned(),
                    "npm/nanoid@5.1.6".to_owned()
                ),
                (
                    "file:///var/ws/node_modules/nanoid".to_owned(),
                    "npm/nanoid@5.1.6".to_owned()
                ),
            ]
        );
        assert_eq!(
            spelled(&folder_roots(&sysroot, &[], spellings)),
            [(
                "file:///toolchain/lib/rustlib/src/rust/library".to_owned(),
                "stdlib/rust@1.98.1".to_owned()
            )],
            "a folder outside the tree has one spelling"
        );
    }

    #[test]
    fn a_registry_package_is_rooted_below_every_registry_source_and_an_import_root_below_site_packages()
     {
        let spellings = RootSpellings {
            index_root: Path::new("/ws"),
            engine_roots: &[],
        };
        let registries = [
            PathBuf::from("/cargo/registry/src/index.crates.io-1949cf8c6b5b557f"),
            PathBuf::from("/cargo/registry/src/mirror-0123"),
        ];
        let serde = installed(
            identity("cargo", "serde", "1.0.228"),
            InstallLocation::CargoRegistry("serde-1.0.228".to_owned()),
        );
        assert_eq!(
            spelled(&folder_roots(&serde, &registries, spellings)),
            [
                (
                    "file:///cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228"
                        .to_owned(),
                    "cargo/serde@1.0.228".to_owned()
                ),
                (
                    "file:///cargo/registry/src/mirror-0123/serde-1.0.228".to_owned(),
                    "cargo/serde@1.0.228".to_owned()
                ),
            ]
        );
        let jwt = installed(
            identity("pypi", "pyjwt", "2.10.1"),
            InstallLocation::ImportRoot {
                site_packages: PathBuf::from("/ws/.venv/lib/python3.12/site-packages"),
                root: "jwt".to_owned(),
            },
        );
        let roots = folder_roots(&jwt, &registries, spellings);
        assert_eq!(
            spelled(&roots),
            [(
                "file:///ws/.venv/lib/python3.12/site-packages/jwt".to_owned(),
                "pypi/pyjwt@2.10.1".to_owned()
            )]
        );
        let unrooted = installed(
            identity("pypi", "odd", "1.0.0"),
            InstallLocation::ImportRoot {
                site_packages: PathBuf::from("/ws/site-packages"),
                root: "../escape".to_owned(),
            },
        );
        assert!(
            folder_roots(&unrooted, &registries, spellings).is_empty(),
            "an import root the path rules refuse roots nothing"
        );
    }

    #[test]
    fn registry_sources_list_the_folders_below_registry_src_in_name_order() -> TestResult {
        let home = tempfile::tempdir()?;
        let sources = home.path().join("registry").join("src");
        for folder in ["mirror-b", "index.crates.io-1949cf8c6b5b557f"] {
            std::fs::create_dir_all(sources.join(folder))?;
        }
        std::fs::write(sources.join("stray-file"), "")?;
        assert_eq!(
            registry_sources(Some(home.path())),
            [
                sources.join("index.crates.io-1949cf8c6b5b557f"),
                sources.join("mirror-b")
            ]
        );
        assert!(registry_sources(Some(&home.path().join("absent"))).is_empty());
        assert!(registry_sources(None).is_empty());
        Ok(())
    }

    #[test]
    fn registry_sources_stop_at_their_bound() -> TestResult {
        let home = tempfile::tempdir()?;
        let sources = home.path().join("registry").join("src");
        for index in 0..=super::REGISTRY_SOURCES_MAX {
            std::fs::create_dir_all(sources.join(format!("registry-{index:03}")))?;
        }
        let listed = registry_sources(Some(home.path()));
        assert_eq!(listed.len(), super::REGISTRY_SOURCES_MAX);
        assert_eq!(listed[0], sources.join("registry-000"));
        Ok(())
    }

    #[test]
    fn node_resolves_the_nearest_typescript_package_above_a_module() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let module = root.join("node_modules/typescript-language-server/lib/cli.mjs");
        std::fs::create_dir_all(module.parent().ok_or("module folder")?)?;
        std::fs::write(&module, "")?;
        let typescript = root.join("node_modules/typescript");
        std::fs::create_dir_all(&typescript)?;
        std::fs::write(typescript.join("package.json"), r#"{"version": "5.9.3"}"#)?;

        let found = node_package_root(&module, "typescript").ok_or("typescript resolves")?;
        assert_eq!(found.root(), &TreeRoot::new(&typescript)?);
        assert_eq!(found.package(), &identity("npm", "typescript", "5.9.3"));
        assert!(node_package_root(&module, "absent").is_none());
        Ok(())
    }

    #[test]
    fn a_package_version_is_read_from_a_bounded_manifest_naming_one() -> TestResult {
        let directory = tempfile::tempdir()?;
        let manifest = directory.path().join("package.json");
        std::fs::write(&manifest, r#"{"name": "typescript", "version": "5.9.3"}"#)?;
        assert_eq!(package_version(&manifest), Some("5.9.3".to_owned()));
        for text in [r#"{"version": ""}"#, r#"{"name": "x"}"#, "not json"] {
            std::fs::write(&manifest, text)?;
            assert_eq!(package_version(&manifest), None, "{text}");
        }
        let padding = " ".repeat(usize::try_from(super::PACKAGE_MANIFEST_BYTES_MAX)?);
        std::fs::write(&manifest, format!(r#"{{"version": "1.0.0"}}{padding}"#))?;
        assert_eq!(package_version(&manifest), None, "past the bound");
        assert_eq!(package_version(&directory.path().join("absent.json")), None);
        Ok(())
    }

    /// A TypeScript engine's program resolves to the `typescript` package its own install
    /// holds, while an engine for another language adds no root.
    #[cfg(unix)]
    #[test]
    fn a_typescript_engine_adds_the_typescript_package_its_program_resolves() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let cli = root.join("node_modules/typescript-language-server/lib/cli.mjs");
        std::fs::create_dir_all(cli.parent().ok_or("cli folder")?)?;
        std::fs::write(&cli, "#!/usr/bin/env node\n")?;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755))?;
        let bin = root.join("node_modules/.bin");
        std::fs::create_dir_all(&bin)?;
        std::os::unix::fs::symlink(&cli, bin.join("typescript-language-server"))?;
        let typescript = root.join("node_modules/typescript");
        std::fs::create_dir_all(&typescript)?;
        std::fs::write(typescript.join("package.json"), r#"{"version": "5.9.3"}"#)?;

        let program = "node_modules/.bin/typescript-language-server";
        let configuration = serde_json::from_value(serde_json::json!({"command": [program]}))?;
        let definitions = BTreeMap::from([
            (LspProcessKey::named("typescript"), configuration),
            (
                LspProcessKey::named("rust"),
                serde_json::from_value(serde_json::json!({"command": [program]}))?,
            ),
        ]);
        let bindings = BTreeMap::from([
            ("typescript".to_owned(), LspProcessKey::named("typescript")),
            ("rust".to_owned(), LspProcessKey::named("rust")),
        ]);
        let engines = EnginePool::new(root, definitions, bindings);
        let roots = super::engine_typescript_roots(&engines);
        assert_eq!(
            spelled(&roots),
            [(
                TreeRoot::new(&std::fs::canonicalize(&typescript)?)?
                    .root_uri()?
                    .as_str()
                    .to_owned(),
                "npm/typescript@5.9.3".to_owned()
            )],
            "one root, for the engine serving TypeScript"
        );
        Ok(())
    }

    fn callee(package: CalleePackage, path: &str, kind: SymbolKind) -> PackageCallee {
        PackageCallee {
            caller: CoreSymbolId::new("rift://symbol/python/app.py/hello").expect("caller id"),
            package,
            path: ProjectPath::new(path).expect("callee path"),
            position: Position {
                line: 4,
                character: 8,
            },
            encoding: PositionEncoding::Utf16,
            name: "greet".to_owned(),
            kind,
        }
    }

    #[test]
    fn a_named_callee_carries_the_engines_name_and_kind_and_its_packages_origin() -> TestResult {
        let greeting = identity("pypi", "greeting", "1.0.0");
        let held = callee(
            CalleePackage::Installed(greeting.clone()),
            "greeting/core.py",
            SymbolKind::FUNCTION,
        );
        let id = SymbolId("rift://symbol/python/pypi/greeting@1.0.0/greeting/core.py/greet".into());
        let symbol = held
            .symbol(&id, &greeting)
            .ok_or("the id names a language")?;
        let wire = serde_json::to_value(&symbol)?;
        assert_eq!(
            wire,
            serde_json::json!({
                "id": id.0,
                "language": "python",
                "name": "greet",
                "kind": "function",
                "origin": {
                    "location": "dependency",
                    "package": {"manager": "pypi", "name": "greeting", "version": "1.0.0"},
                    "source_kind": "authored"
                }
            })
        );
        assert_eq!(
            held.unit(&greeting).map(|unit| unit.0),
            Some("rift://source/pypi/greeting@1.0.0/greeting/core.py".to_owned())
        );

        let python = identity("stdlib", "python", "3.12.9");
        let stub = callee(
            CalleePackage::StandardLibrary(StandardLibrary::Python),
            "builtins.pyi",
            SymbolKind::METHOD,
        );
        let len = SymbolId("rift://symbol/python/stdlib/python@3.12.9/builtins.pyi/len".into());
        let symbol = stub
            .symbol(&len, &python)
            .ok_or("the id names a language")?;
        let wire = serde_json::to_value(&symbol)?;
        assert_eq!(wire["kind"], "method");
        assert_eq!(
            wire["origin"],
            serde_json::json!({"location": "stdlib", "source_kind": "authored"})
        );
        assert_eq!(stub.package().manager(), "stdlib");
        assert_eq!(stub.package().name(), "python");

        let unknown = callee(
            CalleePackage::Installed(greeting.clone()),
            "greeting/core.py",
            serde_json::from_value(serde_json::json!(99))?,
        );
        assert!(
            unknown.symbol(&id, &greeting).is_none(),
            "a kind the protocol does not define names no symbol"
        );
        assert!(
            held.symbol(&SymbolId("not an identity".into()), &greeting)
                .is_none()
        );
        Ok(())
    }

    /// A Rust standard library callee reaches the sysroot's folder as an installed
    /// package, and its hit carries the origin package analysis gives the standard library:
    /// `stdlib`, naming no package.
    #[test]
    fn a_standard_library_callee_below_the_sysroot_carries_the_stdlib_origin() -> TestResult {
        let rust = identity("stdlib", "rust", "1.98.1");
        let held = callee(
            CalleePackage::Installed(rust.clone()),
            "std/src/fs.rs",
            SymbolKind::FUNCTION,
        );
        let id =
            SymbolId("rift://symbol/rust/stdlib/rust@1.98.1/std/src/fs.rs/read_to_string".into());
        let wire = serde_json::to_value(held.symbol(&id, &rust).ok_or("the id names a language")?)?;
        assert_eq!(
            wire["origin"],
            serde_json::json!({"location": "stdlib", "source_kind": "authored"})
        );
        assert_eq!(wire["language"], "rust");
        assert_eq!(
            held.unit(&rust).map(|unit| unit.0),
            Some("rift://source/stdlib/rust@1.98.1/std/src/fs.rs".to_owned())
        );
        Ok(())
    }

    fn call(uri: &Uri) -> NamedCallee<'_> {
        NamedCallee {
            uri,
            name: "greet",
            kind: SymbolKind::FUNCTION,
            position: Position {
                line: 4,
                character: 8,
            },
            encoding: PositionEncoding::Utf8,
        }
    }

    #[test]
    fn callee_files_split_by_the_root_holding_them() -> TestResult {
        let caller = CoreSymbolId::new("rift://symbol/python/app.py/hello")?;
        let roots = CalleeRoots::from_packages(vec![PackageRoot::within(
            &TreeRoot::from_slash_form("/ws/.venv/lib/python3.12/site-packages")?,
            ProjectPath::new("greeting")?,
            identity("pypi", "greeting", "1.0.0"),
        )]);
        let trees = [Path::new("/ws")];
        let classify = |text: &str| -> Result<String, Box<dyn std::error::Error>> {
            let uri: Uri = text.parse()?;
            Ok(match callee_file(&roots, &trees, &caller, call(&uri))? {
                CalleeFile::Project(path) => format!("project {path}"),
                CalleeFile::Package(held) => format!(
                    "{}/{} {}",
                    held.package().manager(),
                    held.package().name(),
                    held.path()
                ),
                CalleeFile::Unaddressed => "unaddressed".to_owned(),
            })
        };
        assert_eq!(classify("file:///ws/app.py")?, "project app.py");
        assert_eq!(
            classify("file:///ws/.venv/lib/python3.12/site-packages/greeting/core.py")?,
            "pypi/greeting greeting/core.py"
        );
        assert_eq!(
            classify("vendored://stdlib/json/__init__.pyi")?,
            "stdlib/python json/__init__.pyi"
        );
        assert_eq!(classify("vendored://stdlib/../escape.pyi")?, "unaddressed");
        assert_eq!(classify("untitled:scratch.py")?, "unaddressed");
        assert_eq!(classify("file:///elsewhere/lib.py")?, "unaddressed");
        let hosted: Uri = "file://build-host/ws/app.py".parse()?;
        assert!(
            callee_file(&roots, &trees, &caller, call(&hosted)).is_err(),
            "a URI the rules refuse is the engine's fault, not an absent root"
        );
        let address = roots.address(&trees, &"file:///ws/app.py".parse()?)?;
        assert!(matches!(address, Some(EngineAddress::Project(_))));
        Ok(())
    }
}
