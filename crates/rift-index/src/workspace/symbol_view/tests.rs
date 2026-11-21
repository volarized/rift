use std::{collections::BTreeMap, fs, sync::Arc};

use get_size2::{GetSize, StandardTracker};

use super::super::{
    SourceVisibility, SymbolDeclarations, TextFileInclusion, WorkspaceIndex, WorkspaceIndexLimits,
};

#[test]
fn captured_symbol_material_keeps_source_graph_ranking_and_binding_facts() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
    )
    .expect("captured metadata");
    fs::write(
        root.path().join("lib.rs"),
        "/// Original.\npub fn kept() {}\n",
    )
    .expect("captured source");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let matched = index.symbols("kept", 1).expect("physical declaration")[0];
    let object = index.assembled_symbol(matched).expect("canonical facts");
    let identity = object.identity().expect("established identity").clone();
    let view = index.symbol_view().expect("captured material");
    assert!(std::ptr::eq(
        view.normalized_graph(),
        index.normalized_graph()
    ));
    assert!(std::ptr::eq(view.edges(), index.edges()));
    assert_eq!(view.index_documents().len(), index.index_documents().len());
    assert_eq!(
        view.object_by_identity(&identity)
            .expect("object")
            .expect("found")
            .facts(),
        object.facts()
    );
    let (bindings, total) = view
        .symbols_by_identity_page(&identity, 0, 1)
        .expect("page");
    assert_eq!(total, 1);
    assert_eq!(bindings[0].symbol.item_range, matched.symbol.item_range);
    let path = matched.file.path().clone();
    assert!(view.retained_material_bytes() > matched.file.source().len());
    assert_ne!(view.retained_material_bytes(), usize::MAX);
    drop(index);
    fs::remove_file(root.path().join("lib.rs")).expect("remove live source");
    assert_eq!(
        view.file(&path).expect("retained source").source(),
        "/// Original.\npub fn kept() {}\n"
    );
    assert!(
        view.object_by_identity(&identity)
            .expect("retained object")
            .is_some()
    );
}

#[test]
fn captured_symbol_material_charges_shared_generation_once() {
    let root = tempfile::tempdir().expect("workspace");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("empty index");
    let before = super::retained_size::MEASUREMENTS.get();
    let view = index.symbol_view().expect("captured material");
    assert_eq!(super::retained_size::MEASUREMENTS.get(), before + 1);
    let charge = view.retained_material_bytes();
    for _ in 0..256 {
        assert_eq!(view.retained_material_bytes(), charge);
        assert!(Arc::ptr_eq(
            &view,
            &index.symbol_view().expect("same captured material")
        ));
    }
    assert_eq!(super::retained_size::MEASUREMENTS.get(), before + 1);
    assert!(Arc::ptr_eq(
        &view,
        &index.symbol_view().expect("same publication")
    ));
    let (one, _) = view.get_heap_size_with_tracker(StandardTracker::default());
    let pair = (Arc::clone(&view), Arc::clone(&view));
    let (two, _) = pair.get_heap_size_with_tracker(StandardTracker::default());
    assert!(one > 0);
    assert_eq!(one, two);
    assert_ne!(view.retained_material_bytes(), usize::MAX);
}

#[test]
#[ignore = "manual retained material measurement"]
fn captured_symbol_material_measures_existing_workspace() {
    let root = std::env::var_os("RIFT_RETAINED_MATERIAL_ROOT").expect("measurement root");
    let root = std::path::PathBuf::from(root);
    let index = WorkspaceIndex::build(
        &root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("captured workspace");
    let view = index.symbol_view().expect("captured material");
    let bytes = view.retained_material_bytes();
    assert_ne!(
        bytes,
        usize::MAX,
        "ordinary workspace material must be measured"
    );
    println!("retained_material_bytes={bytes} root={}", root.display());
}

#[test]
fn captured_syntax_elements_keep_inline_framework_source_after_current_context_changes() {
    let root = tempfile::tempdir().expect("workspace");
    let text = "import {Component} from '@angular/core'; @Component({template:`<p>{{ title }}</p>`}) export class Beacon {}";
    fs::write(root.path().join("package.json"), "{}").expect("package context");
    fs::write(root.path().join("component.ts"), text).expect("component source");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let path = rift_core::ProjectPath::new("component.ts").expect("path");
    let position = u64::try_from(text.find("title").expect("template bytes")).expect("position");
    let original = index
        .syntax(&path)
        .map(|document| {
            document.map(|document| {
                document
                    .syntax_elements_at(position)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
        })
        .expect("original syntax elements")
        .expect("selected source");
    assert!(
        original
            .iter()
            .any(|syntax_element| syntax_element.kind == "interpolation")
    );
    let view = index.symbol_view().expect("captured material");
    assert_eq!(view.frameworks[&path].templates.len(), 1);
    let charge = view.retained_material_bytes();
    drop(index);
    fs::write(root.path().join("component.ts"), "export class Changed {}").expect("new source");
    fs::write(root.path().join("package.json"), "{\"name\":\"changed\"}")
        .expect("new package context");
    assert_eq!(
        view.syntax(&path)
            .map(|document| document.map(|document| document
                .syntax_elements_at(position)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()))
            .expect("captured syntax elements"),
        Some(original)
    );
    assert_eq!(view.file(&path).expect("captured source").source(), text);
    assert_eq!(view.retained_material_bytes(), charge);
    assert_eq!(
        view.syntax(&path)
            .map(|document| document.map(|document| document
                .syntax_elements_at(u64::MAX)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()))
            .expect("outside source"),
        Some(Vec::new())
    );
    assert!(
        view.syntax(&rift_core::ProjectPath::new("missing.ts").expect("missing path"))
            .map(|document| document.map(|document| document
                .syntax_elements_at(0)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()))
            .expect("missing captured source")
            .is_none()
    );
}

#[test]
fn captured_framework_charge_counts_template_capacity_and_checked_source() {
    let path = rift_core::ProjectPath::new("component.ts").expect("path");
    let text = "import {Component} from '@angular/core'; @Component({template:`<p>{{ title }}</p>`}) class Beacon {}";
    let sources = [rift_analysis::PackageSource::new(&path, text)];
    let context = rift_analysis::FrameworkContext::resolve(
        &sources,
        &[],
        rift_syntax::SyntaxLimits::default(),
        &|| false,
    )
    .expect("framework context");
    let digest = rift_core::FileDigest::of(text.as_bytes());
    let retained = context.for_source(&path, &digest).expect("checked source");
    assert_eq!(retained.templates.len(), 1);
    let (heap, _) = retained.get_heap_size_with_tracker(StandardTracker::default());
    assert_eq!(
        heap,
        retained.templates.capacity() * std::mem::size_of::<rift_syntax::ByteRange>()
    );
    assert!(
        context
            .for_source(&path, &rift_core::FileDigest::of(b"changed"))
            .is_none()
    );
    let empty = rift_analysis::ResolvedFramework::default();
    assert_eq!(
        empty
            .get_heap_size_with_tracker(StandardTracker::default())
            .0,
        0
    );
}

#[test]
fn captured_symbol_material_refuses_unmeasured_portable_facts_without_overflow() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
    )
    .expect("captured metadata");
    fs::write(root.path().join("lib.rs"), "pub fn kept() {}\n").expect("source");
    let index = WorkspaceIndex::build(
        root.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index");
    let matched = index.symbols("kept", 1).expect("physical declaration")[0];
    let mut object = index.assembled_symbol(matched).expect("canonical facts");
    let identity = object.identity().expect("established identity").clone();
    let mut signatures = object.facts.signatures_slice().to_vec();
    signatures[0].extensions = rift_protocol::read::Extensions(BTreeMap::from([(
        rift_protocol::read::ExtensionKey("example.org.fact".to_owned()),
        rift_protocol::read::ExtensionValue {
            version: 1,
            data: serde_json::json!({"held": "value"}),
        },
    )]));
    object.facts = object.facts.clone().signatures(signatures);
    let mut view = index.build_symbol_view().expect("immutable view");
    view.declarations = Arc::new(SymbolDeclarations {
        assembled: BTreeMap::from([(identity, Ok(object))]),
        ..SymbolDeclarations::default()
    });
    assert_eq!(super::retained_size::measure(&view), usize::MAX);
    assert_eq!(
        view.declarations.assembled.len(),
        1,
        "refusal retains object facts"
    );
    assert!(!view.normalized_graph().records().is_empty());
}

/// A framework whose context is unresolved keeps every declaration its files hold, so it
/// leaves the captured object inventory complete; a file left out of the index does not
/// (#759).
#[test]
fn captured_object_inventory_stays_complete_beside_an_unresolved_framework() {
    let root = tempfile::tempdir().expect("workspace");
    fs::create_dir_all(root.path().join("docs")).expect("docs package");
    fs::write(
        root.path().join("core.ts"),
        "export function construct_core(): number {\n  return 1;\n}\n",
    )
    .expect("plain source");
    fs::write(
        root.path().join("docs/package.json"),
        "{\"name\":\"docs\",\"devDependencies\":{\"tailwindcss\":\"^4.0.0\"}}",
    )
    .expect("framework manifest");
    fs::write(
        root.path().join("docs/layout.tsx"),
        "export function DocsLayout() {\n  return <div className=\"flex\" />;\n}\n",
    )
    .expect("framework source");
    let build = || {
        WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )
        .expect("index")
    };
    let index = build();
    assert!(
        index.warnings().iter().any(|warning| matches!(
            warning,
            super::super::WorkspaceIndexWarning::FrameworkContextUnresolved { .. }
        )),
        "{:?}",
        index.warnings()
    );
    let language = index
        .file(&rift_core::ProjectPath::new("docs/layout.tsx").expect("path"))
        .expect("framework source stays indexed")
        .syntax()
        .language()
        .clone();
    assert!(index.symbol_view().expect("view").objects_complete());
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("coverage")
    );
    fs::write(root.path().join("blob.ts"), "export const held = 1;\0\n").expect("binary source");
    let left_out = build();
    assert!(!left_out.symbol_view().expect("view").objects_complete());
    assert!(
        !left_out
            .identity_coverage_complete(&language)
            .expect("coverage")
    );
}

/// `pub fn <name>() {}` padded with a line comment to exactly `bytes` bytes.
fn rust_source_of(name: &str, bytes: usize) -> String {
    let mut source = format!("pub fn {name}() {{}}\n//");
    assert!(
        source.len() < bytes,
        "the fixture must fit its byte count: source={} bytes={bytes}",
        source.len()
    );
    source.push_str(&"x".repeat(bytes - source.len() - 1));
    source.push('\n');
    source
}

fn build_with(root: &std::path::Path, limits: WorkspaceIndexLimits) -> WorkspaceIndex {
    WorkspaceIndex::build(
        root,
        limits,
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )
    .expect("index")
}

/// D37: a file a bound left out is outside the indexed set, as a file the source policy
/// excludes is. A source below and at `[providers.syntax] max_file` is parsed; one byte
/// above it is left out, and the captured inventory stays complete over what the index
/// holds, so enumerations, counts and absences answer rather than refuse.
#[test]
fn a_file_past_the_syntax_bound_leaves_the_object_inventory_complete() {
    const BOUND: usize = 96;
    let root = tempfile::tempdir().expect("workspace");
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
    )
    .expect("captured metadata");
    let mut below = "mod above;\nmod at;\n".to_owned();
    below.push_str(&rust_source_of("below_bound", BOUND - 1 - below.len()));
    fs::write(root.path().join("lib.rs"), below).expect("below");
    fs::write(root.path().join("at.rs"), rust_source_of("at_bound", BOUND)).expect("at");
    fs::write(
        root.path().join("above.rs"),
        rust_source_of("above_bound", BOUND + 1),
    )
    .expect("above");
    let limits = WorkspaceIndexLimits::default()
        .with_syntax(rift_syntax::SyntaxLimits::new(BOUND, 250_000, 256).expect("bounds"));
    let index = build_with(root.path(), limits);
    let left_out: Vec<&str> = index
        .warnings()
        .iter()
        .map(|warning| warning.path().as_str())
        .collect();
    assert_eq!(left_out, ["above.rs"], "{:?}", index.warnings());
    assert!(index.warnings()[0].is_left_out_by_bound());
    assert!(!index.warnings()[0].leaves_inventory_partial());
    let view = index.symbol_view().expect("view");
    assert!(view.objects_complete());
    let language = index
        .file(&rift_core::ProjectPath::new("at.rs").expect("path"))
        .expect("the source at the bound is parsed")
        .syntax()
        .language()
        .clone();
    assert!(view.identity_coverage_complete(&language));
    assert!(
        index
            .identity_coverage_complete(&language)
            .expect("coverage")
    );
    for (name, held) in [
        ("below_bound", true),
        ("at_bound", true),
        ("above_bound", false),
    ] {
        assert_eq!(
            !index.symbols(name, 1).expect("lookup").is_empty(),
            held,
            "{name}"
        );
    }
}

/// D37: a file past the per-file byte bound of `[source]` is left out the same way.
#[test]
fn a_file_past_the_source_byte_bound_leaves_the_object_inventory_complete() {
    const BOUND: usize = 96;
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("at.rs"), rust_source_of("at_bound", BOUND)).expect("at");
    fs::write(
        root.path().join("above.rs"),
        rust_source_of("above_bound", BOUND + 1),
    )
    .expect("above");
    let limits = WorkspaceIndexLimits {
        file_bytes_max: BOUND,
        ..WorkspaceIndexLimits::default()
    };
    let index = build_with(root.path(), limits);
    assert!(
        matches!(
            index.warnings(),
            [super::super::WorkspaceIndexWarning::FileTooLarge { path, .. }]
                if path.as_str() == "above.rs"
        ),
        "{:?}",
        index.warnings()
    );
    assert!(index.symbol_view().expect("view").objects_complete());
    assert!(!index.symbols("at_bound", 1).expect("lookup").is_empty());
}

/// A numeric log field, which the record stores as its rendered text.
fn number(field: &serde_json::Value) -> u64 {
    field
        .as_u64()
        .or_else(|| field.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("a numeric field: {field}"))
}

/// The warning records one build writes for the files a bound left out, by message.
fn omission_records(drain: &mut rift_tracing::LogDrain) -> Vec<serde_json::Value> {
    drain
        .queued_records()
        .into_iter()
        .filter(|record| {
            [
                "file left out of the index",
                "file held unparsed in the index",
                "more files left out of the index or held unparsed than one build names",
            ]
            .contains(&record.message())
        })
        .map(|record| {
            assert_eq!(record.level(), "warn", "{record:?}");
            let mut fields: serde_json::Value =
                serde_json::from_str(record.fields()).expect("fields");
            fields["message"] = serde_json::Value::from(record.message());
            fields
        })
        .collect()
}

/// D37: the log names each file a bound left out once per build, with its bound and measured
/// value, up to a fixed number of records; past them one record counts the rest, so the log
/// volume stays bounded whatever the number of oversized files.
#[test]
fn a_build_names_the_files_a_bound_left_out_up_to_a_bound_and_counts_the_rest() {
    const BOUND: usize = 96;
    let named_max = usize::try_from(super::super::OMITTED_FILE_RECORDS_MAX).expect("fits");
    for (oversized, expected_named, expected_counted) in [
        (1_usize, 1_usize, None),
        (named_max, named_max, None),
        (named_max + 1, named_max, Some(1_u64)),
        (named_max + 40, named_max, Some(40)),
    ] {
        let root = tempfile::tempdir().expect("workspace");
        fs::write(root.path().join("kept.rs"), "pub fn kept() {}\n").expect("kept");
        for index in 0..oversized {
            fs::write(
                root.path().join(format!("wide_{index:03}.rs")),
                rust_source_of(&format!("wide_{index}"), BOUND + 1),
            )
            .expect("oversized");
        }
        let limits = WorkspaceIndexLimits {
            file_bytes_max: BOUND,
            ..WorkspaceIndexLimits::default()
        };
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("warn")
            .install()
            .expect("recorder");
        let index = build_with(root.path(), limits);
        assert_eq!(index.warnings().len(), oversized);
        assert!(index.symbol_view().expect("view").objects_complete());
        let records = omission_records(&mut drain);
        let named: Vec<&serde_json::Value> = records
            .iter()
            .filter(|record| record["message"] == "file left out of the index")
            .collect();
        assert_eq!(named.len(), expected_named, "oversized={oversized}");
        let paths: BTreeMap<&str, usize> =
            named.iter().fold(BTreeMap::new(), |mut seen, record| {
                *seen
                    .entry(record["path"].as_str().expect("path"))
                    .or_default() += 1;
                seen
            });
        assert!(
            paths.values().all(|count| *count == 1),
            "each file is named once per build: {paths:?}"
        );
        let reason = named[0]["reason"].as_str().expect("reason");
        assert!(
            reason.contains(&BOUND.to_string()) && reason.contains(&(BOUND + 1).to_string()),
            "the record carries the bound and the measured value: {reason}"
        );
        let counted: Vec<u64> = records
            .iter()
            .filter(|record| {
                record["message"]
                    == "more files left out of the index or held unparsed than one build names"
            })
            .map(|record| number(&record["counted"]))
            .collect();
        assert_eq!(counted, expected_counted.into_iter().collect::<Vec<_>>());
        assert!(records.len() <= named_max + 1, "oversized={oversized}");
    }
}
