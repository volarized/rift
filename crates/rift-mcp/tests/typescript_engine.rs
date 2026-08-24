//! The typescript-language-server probe and the real
//! `[engines.typescript]` table.
//!
//! The engine refuses to initialize without a `typescript` package it can
//! resolve from the workspace root (`-32603 Could not find a valid
//! TypeScript installation`), so the fixture carries a `package.json` and
//! a committed `bun.lock`, and the suite installs from that lockfile into
//! its tempdir copy. The install never touches the repository checkout,
//! and a warm bun cache serves it without the registry.

use std::path::Path;
use std::time::Instant;

/// The package manager that installs the fixture's pinned `typescript`.
pub(crate) const BUN_PROGRAM: &str = "bun";

/// The runner that resolves and starts the language server package.
pub(crate) const BUNX_PROGRAM: &str = "bunx";

/// The language server, pinned: an unpinned `bunx` argument would float
/// to whatever the registry publishes next.
pub(crate) const LANGUAGE_SERVER_PACKAGE: &str = "typescript-language-server@6.0.0";

/// Installs the fixture's pinned `typescript` and proves the language
/// server runs, or fails the test with the command's own words.
///
/// Both halves run from the fixture tree, the directory the engine child
/// resolves from. The install reads the committed lockfile, so it resolves
/// nothing and answers from bun's cache when the package is already there.
pub(crate) fn install_typescript_engine(fixture_root: &Path) {
    let started = Instant::now();
    let install = std::process::Command::new(BUN_PROGRAM)
        .args(["install", "--frozen-lockfile"])
        .current_dir(fixture_root)
        .output();
    match install {
        Ok(output) if output.status.success() => {
            eprintln!("bun install: {:?}", started.elapsed());
        }
        Ok(output) => panic!(
            "`bun install --frozen-lockfile` failed in {}: the fixture's lockfile must resolve \
             the pinned typescript. {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{BUN_PROGRAM}` is not on PATH for {}: install bun to run the live typescript \
             suite. {error}",
            fixture_root.display(),
        ),
    }
    let probe = std::process::Command::new(BUNX_PROGRAM)
        .args([LANGUAGE_SERVER_PACKAGE, "--version"])
        .current_dir(fixture_root)
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            eprintln!(
                "typescript-language-server: {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => panic!(
            "`{BUNX_PROGRAM} {LANGUAGE_SERVER_PACKAGE} --version` failed in {}: {}",
            fixture_root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ),
        Err(error) => panic!(
            "`{BUNX_PROGRAM}` is not on PATH for {}: install bun to run the live typescript \
             suite. {error}",
            fixture_root.display(),
        ),
    }
}

/// The real `[engines.typescript]` table: typescript-language-server over
/// the fixture's bun project, serving both TypeScript dialects.
///
/// `tsserver.useSyntaxServer = "never"` keeps the engine to one semantic
/// server. Under the default `auto` the language server also runs a
/// syntax-only server, and that one answers the first rename from the open
/// file alone: the observed cold answer rewrote the declaration and left
/// both importers standing. With one semantic server the rename waits for
/// the loaded project and spans every file.
///
/// `[source] exclude` keeps the installed package out of the index. It is
/// not decoration: without it the walk reaches `typescript`'s own 23mb of
/// sources and the first change refuses with `violation file_too_large,
/// path .../node_modules/typescript/lib/_tsc.js`. Any workspace serving a
/// bun project needs the same entry.
pub(crate) fn typescript_engine_configuration() -> String {
    format!(
        "[source]\nexclude = [\"node_modules/**\"]\n\n\
         [engines.typescript]\nprogram = \"{BUNX_PROGRAM}\"\n\
         arguments = [\"{LANGUAGE_SERVER_PACKAGE}\", \"--stdio\"]\n\
         languages = [\"typescript\", \"typescript:tsx\"]\n\
         startup_timeout = \"2m\"\nrequest_timeout = \"2m\"\n\n\
         [engines.typescript.initialization_options.tsserver]\n\
         useSyntaxServer = \"never\"\n"
    )
}
