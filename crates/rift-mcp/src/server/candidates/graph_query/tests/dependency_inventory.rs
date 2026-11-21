use super::*;
use std::path::Path;

fn installed_python_workspace(root: &Path) -> TestResult {
    reverse_graph_workspace(root, "127.0.0.1:9".parse()?)?;
    crate::server::hermetic_workspace(
        root,
        "[source]\nexclude=['.venv/**']\n[languages.toml]\nenabled=false\n[languages.python]\nstdlib=false\n[languages.python.lsp]\nembedded='ty'\n[global]\nenabled=false\n",
    )?;
    let site = if cfg!(windows) {
        ".venv/Lib/site-packages"
    } else {
        ".venv/lib/python3.12/site-packages"
    };
    std::fs::write(
        root.join(site).join("greeting/core.py"),
        "class Base:\n    pass\n\nclass Client:\n    def get(self, value: str) -> str:\n        return value\n",
    )?;
    std::fs::write(
        root.join(site).join("greeting/api.py"),
        "from greeting.core import Base, Client\n",
    )?;
    std::fs::write(
        root.join("app.py"),
        "from greeting.api import Base, Client\n\nclass Child(Base):\n    client: Client\n    def __init__(self):\n        self.client = Client()\n    def fetch(self):\n        return self.client.get('rift')\n\ndef hello():\n    client = Client()\n    return client.get('rift')\n",
    )?;
    Ok(())
}

async fn query(
    server: &RiftMcp,
    query: String,
) -> Result<rift_protocol::query::QueryResult, Box<dyn std::error::Error + Send + Sync>> {
    let answer = server
        .graph_query(QueryParams {
            query: query.clone(),
            max_results: 100,
            ..Default::default()
        })
        .await
        .map_err(|error| format!("{error:?}"))?
        .0
        .result;
    let QueryReadResult::Results(result) = answer else {
        return Err(format!("expected inventory rows for {query}: {answer:?}").into());
    };
    Ok(*result)
}

async fn targets(
    server: &RiftMcp,
    kind: &str,
    provider: &str,
) -> Result<std::collections::BTreeSet<String>, Box<dyn std::error::Error + Send + Sync>> {
    let predicate = if kind == "has_type" { "hasType" } else { kind };
    let result = query(server, format!("SELECT DISTINCT ?target {{ ?caller :{predicate} ?target . ?edge a :Edge; :from ?caller; :to ?target; :kinds :{kind}; :provider '{provider}' }}")).await?;
    let QueryAnswer::Bindings { bindings, .. } = result.answer else {
        return Err("bindings".into());
    };
    Ok(bindings
        .into_iter()
        .filter_map(|row| match row.get("target") {
            Some(rift_protocol::query::RdfTerm::Iri { value }) => Some(value.clone()),
            _ => None,
        })
        .collect())
}

#[tokio::test]
async fn python_inventory_names_dependency_calls_heritage_references_and_types() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    installed_python_workspace(&root)?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    for (kind, suffix) in [
        ("calls", "Client/get"),
        ("extends", "Base"),
        ("references", "Client"),
        ("has_type", "Client"),
    ] {
        let ids = targets(&server, kind, "engine").await?;
        let expected =
            format!("rift://symbol/pypi/pypi.org/greeting/python/greeting/core/{suffix}");
        assert!(ids.contains(&expected), "{kind}: {ids:?}");
        assert!(
            ids.iter().all(|id| !id.contains("local/python/greeting")),
            "{ids:?}"
        );
    }
    let all = query(
        &server,
        "SELECT DISTINCT ?target { <rift://symbol/local/python/app/Child> :extends ?target }"
            .to_owned(),
    )
    .await?;
    let QueryAnswer::Bindings { bindings, .. } = all.answer else {
        return Err("bindings".into());
    };
    let ids = bindings
        .iter()
        .filter_map(|row| match &row["target"] {
            rift_protocol::query::RdfTerm::Iri { value } => Some(value.as_str()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        ids.contains("rift://symbol/pypi/pypi.org/greeting/python/greeting/api/Base"),
        "{ids:?}"
    );
    assert!(
        ids.contains("rift://symbol/pypi/pypi.org/greeting/python/greeting/core/Base"),
        "{ids:?}"
    );
    server.engine_pool().await.shutdown().await;
    Ok(())
}

pub(super) fn observed_python_environment(root: &Path) -> TestResult {
    let home = root.join("base/bin");
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join("python3.12"), "")?;
    for path in [
        "base/lib/python3.12",
        "base/Lib",
        "base/bin/Lib",
        ".venv/lib/python3.12/site-packages",
        ".venv/Lib/site-packages",
    ] {
        std::fs::create_dir_all(root.join(path))?;
    }
    std::fs::write(
        root.join(".venv/pyvenv.cfg"),
        format!(
            "home = {}\nimplementation = CPython\nversion_info = 3.12.3\n",
            home.display()
        ),
    )?;
    let project = root.join("pyproject.toml");
    std::fs::write(
        &project,
        format!(
            "{}\n[tool.ty.environment]\npython = '.venv'\n",
            std::fs::read_to_string(&project)?
        ),
    )?;
    Ok(())
}

#[tokio::test]
async fn python_inventory_names_observed_standard_library_targets() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    installed_python_workspace(&root)?;
    observed_python_environment(&root)?;
    std::fs::write(
        root.join("app.py"),
        "class Buffer(list):\n    pass\n\ndef size():\n    return len([])\n\ndef numbers(value: list[int]) -> list[int]:\n    return value\n",
    )?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    for (kind, name) in [
        ("calls", "len"),
        ("references", "len"),
        ("extends", "list"),
        ("has_type", "list"),
    ] {
        let ids = targets(&server, kind, "engine").await?;
        let expected = format!("rift://symbol/stdlib/cpython/python/builtins/{name}");
        assert!(ids.contains(&expected), "{kind}: {ids:?}");
    }
    server.engine_pool().await.shutdown().await;
    Ok(())
}

#[cfg(unix)]
pub(in crate::server::candidates::graph_query) fn recorded_rust_workspace(
    root: &Path,
    cache: &Path,
    missing: bool,
) -> TestResult {
    let package = cache.join("registry/src/index.crates.io-1949cf8c6b5b557f/serde_json-1.0.0");
    std::fs::create_dir_all(package.join("src"))?;
    std::fs::write(
        package.join("Cargo.toml"),
        "[package]\nname='serde_json'\nversion='1.0.0'\n",
    )?;
    std::fs::write(
        package.join("src/lib.rs"),
        "pub mod ser;\npub use ser::to_string;\n",
    )?;
    std::fs::write(
        package.join("src/ser.rs"),
        "pub fn to_string(value: &i32) -> String { String::new() }\n",
    )?;
    let uri = |path: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(reqwest::Url::from_file_path(package.join(path))
            .map_err(|()| "package URI")?
            .to_string())
    };
    let script = root.join("engine.py");
    std::fs::write(&script, include_str!("definition_fixture.py"))?;
    let command = serde_json::to_string(&vec![
        "python3".to_owned(),
        script.to_string_lossy().into_owned(),
        uri("src/ser.rs")?,
        uri("src/missing.rs")?,
    ])?;
    let home = serde_json::to_string(&cache.to_string_lossy())?;
    crate::server::hermetic_workspace(
        root,
        &format!(
            "[source]\nexclude=['engine.py']\n[languages.toml]\nenabled=false\n[languages.rust]\nstdlib=false\n[languages.rust.lsp]\ncommand={command}\nenvironment={{CARGO_HOME={home}}}\n[global]\nenabled=false\n"
        ),
    )?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n[dependencies]\nserde_json='=1.0.0'\n",
    )?;
    std::fs::write(
        root.join("Cargo.lock"),
        "version=4\n[[package]]\nname='fixture'\nversion='0.1.0'\ndependencies=['serde_json']\n[[package]]\nname='serde_json'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
    )?;
    let extra = if missing {
        " serde_json::unknown();"
    } else {
        ""
    };
    std::fs::write(
        root.join("lib.rs"),
        format!(
            "pub fn local() {{}}\npub fn encode() {{ serde_json::to_string(&1); local();{extra} }}\n"
        ),
    )?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rust_inventory_uses_the_engines_accepted_cache_and_keeps_static_targets() -> TestResult {
    let directory = tempfile::tempdir()?;
    let cache = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    recorded_rust_workspace(&root, cache.path(), false)?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    let named = targets(&server, "calls", "lsp").await?;
    assert!(
        named.contains("rift://symbol/cargo/crates.io/serde_json/rust/serde_json/ser/to_string"),
        "{named:?}"
    );
    assert!(
        named.contains("rift://symbol/local/rust/fixture/local"),
        "{named:?}"
    );
    let spelled = targets(&server, "calls", "static").await?;
    assert!(
        spelled.contains("rift://symbol/cargo/crates.io/serde_json/rust/serde_json/to_string"),
        "{spelled:?}"
    );
    server.engine_pool().await.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn inventory_keeps_named_calls_when_another_engine_target_has_no_source() -> TestResult {
    let directory = tempfile::tempdir()?;
    let cache = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    recorded_rust_workspace(&root, cache.path(), true)?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    let result = query(&server, "SELECT DISTINCT ?target { ?caller :calls ?target . ?edge a :Edge; :from ?caller; :to ?target; :kinds :calls; :provider 'lsp' }".to_owned()).await?;
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::CalleesDropped { callees: 1, .. })),
        "{:?}",
        result.warnings
    );
    let QueryAnswer::Bindings { bindings, .. } = result.answer else {
        return Err("bindings".into());
    };
    assert!(bindings.iter().any(|row| matches!(&row["target"], rift_protocol::query::RdfTerm::Iri { value } if value.ends_with("/ser/to_string"))), "{bindings:?}");
    server.engine_pool().await.shutdown().await;
    Ok(())
}

/// A Python workspace whose engine places standard-library targets in the bundled stubs,
/// with no observed runtime to name their release.
fn unobserved_python_workspace(root: &Path, source: &str) -> TestResult {
    crate::server::hermetic_workspace(
        root,
        "[languages.toml]\nenabled=false\n[languages.python]\nstdlib=false\n[languages.python.lsp]\nembedded='ty'\n[global]\nenabled=false\n",
    )?;
    std::fs::write(
        root.join("pyproject.toml"),
        "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['service']\n",
    )?;
    std::fs::write(root.join("service.py"), source)?;
    Ok(())
}

fn reference_pairs(
    result: &rift_protocol::query::QueryResult,
) -> Result<std::collections::BTreeSet<(String, String)>, Box<dyn std::error::Error + Send + Sync>>
{
    let QueryAnswer::Bindings { bindings, .. } = &result.answer else {
        return Err("bindings".into());
    };
    let iri = |row: &std::collections::BTreeMap<String, rift_protocol::query::RdfTerm>,
               name: &str| match row.get(name) {
        Some(rift_protocol::query::RdfTerm::Iri { value }) => Some(value.clone()),
        _ => None,
    };
    Ok(bindings
        .iter()
        .filter_map(|row| Some((iri(row, "caller")?, iri(row, "target")?)))
        .collect())
}

#[tokio::test]
async fn inventory_names_engine_targets_at_sites_static_named_externally() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    unobserved_python_workspace(
        &root,
        "def serve(port: int) -> int:\n    return port\n\n\ndef caller() -> int:\n    return serve(8080)\n",
    )?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    let result = query(
        &server,
        "SELECT ?caller ?target { ?caller :references ?target }".to_owned(),
    )
    .await?;
    assert!(
        result.warnings.is_empty(),
        "engine targets at statically named sites count as unnamed: {:?}",
        result.warnings
    );
    let serve = "rift://symbol/local/python/service/serve";
    let caller = "rift://symbol/local/python/service/caller";
    let int = "rift://symbol/stdlib/cpython/python/builtins/int";
    let expected = [(caller, serve), (caller, int), (serve, int)]
        .into_iter()
        .map(|(from, to)| (from.to_owned(), to.to_owned()))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(reference_pairs(&result)?, expected);
    let ids = targets(&server, "references", "engine").await?;
    assert!(
        ids.contains(int),
        "the engine target takes the static name: {ids:?}"
    );
    server.engine_pool().await.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn inventory_reports_engine_targets_static_did_not_name() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    // `_typeshed` exists only in the bundled stubs: no runtime module the static pass admits.
    unobserved_python_workspace(
        &root,
        "from _typeshed import SupportsRead\n\n\ndef reader() -> SupportsRead[str]:\n    raise NotImplementedError\n",
    )?;
    let server = RiftMcp::build_settled(&root, rift_index::WorkspaceIndexLimits::default()).await?;
    let result = query(
        &server,
        "SELECT ?caller ?target { ?caller :references ?target }".to_owned(),
    )
    .await?;
    assert!(
        result.warnings.iter().any(|warning| matches!(
            warning,
            ReadWarning::EdgeCoverageMissing { kinds, detail }
                if kinds == &[rift_protocol::read::EdgeKind::References]
                    && detail.contains("engine targets for references could not be named")
        )),
        "an engine target neither static nor its release names stays unnamed: {:?}",
        result.warnings
    );
    assert!(
        reference_pairs(&result)?
            .iter()
            .all(|(_, target)| !target.contains("_typeshed")),
        "{result:?}"
    );
    server.engine_pool().await.shutdown().await;
    Ok(())
}
