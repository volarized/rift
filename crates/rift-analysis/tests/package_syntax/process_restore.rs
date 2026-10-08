//! External callers restore facts without carrying parser or assembly code.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_protocol::read::{Documentation, Language, Signature, SymbolFacet};
use rift_syntax::{ByteRange, SyntaxFacts, SyntaxFactsParts, SyntaxNames, SyntaxSymbol};
use serde::{Deserialize, Serialize};

use super::{PackageSyntax, ShippedLanguage, SyntaxLimits, analyze, canonical};

const SOURCE: &str = "/// Opens a file.\npub mod client { pub fn open() {} pub fn open() {} }\n";

#[derive(Serialize, Deserialize)]
struct CapturedSymbol {
    name: String,
    qualified_name: String,
    container: Option<String>,
    kind: String,
    node_kind: Option<String>,
    facets: Vec<SymbolFacet>,
    visibility: Option<String>,
    range: [u64; 2],
    item_range: [u64; 2],
    name_range: Option<[u64; 2]>,
    body_range: Option<[u64; 2]>,
    signatures: Vec<Signature>,
    documentation: Vec<Documentation>,
    documentation_ranges: Vec<[u64; 2]>,
}

fn captured_range(range: ByteRange) -> [u64; 2] {
    [range.start, range.end]
}
fn restored_range(range: [u64; 2]) -> ByteRange {
    ByteRange {
        start: range[0],
        end: range[1],
    }
}

impl CapturedSymbol {
    fn capture(symbol: &SyntaxSymbol) -> Self {
        Self {
            name: symbol.name.clone(),
            qualified_name: symbol.qualified_name.clone(),
            container: symbol.container.clone(),
            kind: symbol.kind.to_owned(),
            node_kind: symbol.node_kind.map(str::to_owned),
            facets: symbol.facets.clone(),
            visibility: symbol.visibility.clone(),
            range: captured_range(symbol.range),
            item_range: captured_range(symbol.item_range),
            name_range: symbol.name_range.map(captured_range),
            body_range: symbol.body_range.map(captured_range),
            signatures: symbol.signatures.to_vec(),
            documentation: symbol.documentation.to_vec(),
            documentation_ranges: symbol
                .documentation_ranges
                .iter()
                .copied()
                .map(captured_range)
                .collect(),
        }
    }

    fn restore(self, names: &SyntaxNames) -> SyntaxSymbol {
        SyntaxSymbol {
            name: self.name,
            qualified_name: self.qualified_name,
            container: self.container,
            kind: names
                .symbol_kind(&self.kind)
                .expect("recorded provider kind"),
            node_kind: self
                .node_kind
                .map(|kind| names.node_kind(&kind).expect("recorded node kind")),
            facets: self.facets,
            visibility: self.visibility,
            range: restored_range(self.range),
            item_range: restored_range(self.item_range),
            name_range: self.name_range.map(restored_range),
            body_range: self.body_range.map(restored_range),
            signatures: self.signatures.into(),
            documentation: self.documentation.into(),
            documentation_ranges: self
                .documentation_ranges
                .into_iter()
                .map(restored_range)
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CapturedFacts {
    language: Language,
    source_digest: [u8; 32],
    analyzer_digest: String,
    limits: [u64; 3],
    symbols: Vec<CapturedSymbol>,
    has_errors: bool,
    left_out_declarations: u64,
}

impl CapturedFacts {
    fn capture(syntax: &PackageSyntax) -> Self {
        let identity = syntax.identity();
        Self {
            language: identity.language.clone(),
            source_digest: *identity.source_digest.as_bytes(),
            analyzer_digest: identity.analyzer_digest.clone(),
            limits: [
                identity.limits.source_bytes_max(),
                identity.limits.syntax_nodes_max(),
                identity.limits.syntax_depth_max(),
            ]
            .map(|limit| u64::try_from(limit).expect("limit width")),
            symbols: syntax
                .facts()
                .symbols()
                .iter()
                .map(CapturedSymbol::capture)
                .collect(),
            has_errors: syntax.facts().has_errors(),
            left_out_declarations: u64::try_from(syntax.facts().left_out_declaration_count())
                .expect("omission width"),
        }
    }

    fn restore(self) -> PackageSyntax {
        let limits = self
            .limits
            .map(|limit| usize::try_from(limit).expect("bounded limit"));
        let limits = SyntaxLimits::new(limits[0], limits[1], limits[2]).expect("recorded limits");
        let names = SyntaxNames::new(&self.language).expect("recorded provider");
        let digest = rift_core::FileDigest::from_bytes(self.source_digest);
        let facts = SyntaxFacts::from_parts(
            SOURCE,
            limits,
            SyntaxFactsParts {
                language: self.language.clone(),
                symbols: self
                    .symbols
                    .into_iter()
                    .map(|symbol| symbol.restore(&names))
                    .collect(),
                has_errors: self.has_errors,
                left_out_declarations: usize::try_from(self.left_out_declarations)
                    .expect("bounded omissions"),
                markdown_facts: None,
                source_digest: digest,
            },
        )
        .expect("checked facts");
        PackageSyntax::new(
            rift_analysis::PackageSyntaxIdentity {
                source_digest: digest,
                language: self.language,
                limits,
                analyzer_digest: self.analyzer_digest,
            },
            Arc::new(facts),
        )
    }
}

#[test]
fn restored_facts_in_child() {
    let Ok(path) = std::env::var("RIFT_SYNTAX_RESTORE_FIXTURE") else {
        return;
    };
    let metadata = std::fs::metadata(&path).expect("fixture metadata");
    assert!(metadata.len() <= 65_536, "fixture byte bound");
    let captured: CapturedFacts =
        serde_json::from_slice(&std::fs::read(path).expect("fixture bytes")).expect("fixture JSON");
    assert!(captured.symbols.len() <= 16, "fixture declaration bound");
    let syntax = captured.restore();
    let files = [("src/moved.rs", SOURCE)];
    let restored = analyze(
        "2.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |_| Some(syntax.clone()),
    )
    .expect("restored package");
    let fresh = analyze(
        "2.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |_| None,
    )
    .expect("fresh package");
    assert_eq!(canonical(&restored), canonical(&fresh));
    assert_eq!(restored.syntax_work().provider_calls, 0);
    assert_eq!(restored.syntax_work().reused_files, 1);
    assert!(
        restored
            .publication()
            .symbols
            .iter()
            .all(|symbol| symbol.symbol.0.contains("beacon@2.0.0/src/moved.rs"))
    );
}

#[test]
fn facts_restore_in_another_process_with_current_placement() {
    let mut retained = None;
    analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &[("src/first.rs", SOURCE)],
        SyntaxLimits::default(),
        |source| {
            let parsed = source.parse().expect("fixture parse");
            retained = Some(parsed.clone());
            Some(parsed)
        },
    )
    .expect("original package");
    let captured = CapturedFacts::capture(&retained.expect("captured facts"));
    let directory = tempfile::tempdir().expect("fixture directory");
    let path = directory.path().join("syntax.json");
    std::fs::write(&path, serde_json::to_vec(&captured).expect("fixture JSON"))
        .expect("fixture write");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "process_restore::restored_facts_in_child",
            "--nocapture",
        ])
        .env("RIFT_SYNTAX_RESTORE_FIXTURE", path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("restoration child");
    let deadline = Instant::now() + Duration::from_secs(30);
    // At most 3,001 polls: the child receives a 30-second deadline and each pending poll waits 10 ms.
    for _ in 0..=3_000 {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "restoration child: {status}");
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("restoration child did not complete: {result:?}");
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("restoration child exceeded its poll bound");
}
