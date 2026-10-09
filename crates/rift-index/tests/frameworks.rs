//! Workspace framework context changes with captured package evidence.

use std::fs;
use std::path::Path;

use rift_core::{LanguageFileSelections, ProjectPath, SourceVisibility, TextFileInclusion};
use rift_index::{WorkspaceIndex, WorkspaceIndexLimits, WorkspaceIndexWarning};
use rift_protocol::configuration::{
    SyntaxFrameworkConfiguration, TailwindVersion, WorkspaceConfiguration,
};
use rift_protocol::read::{PathPattern, SyntaxFramework};
use rift_syntax::{ShippedLanguage, SyntaxLimits, SyntaxSource};

#[test]
fn inline_template_aggregate_bound_refuses_partial_workspace_publication() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let root = directory.path();
    let text = "import {Component} from '@angular/core'; @Component({template:`<section><p>{{title}}</p><p>{{other}}</p></section>`}) export class Beacon {}";
    write(root, "package.json", "{}");
    write(root, "component.ts", text);
    let path = ProjectPath::new("component.ts").expect("source path");
    let raw = ShippedLanguage::TypeScript
        .definition()
        .syntax_provider()
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("raw TypeScript syntax");
    let position =
        u64::try_from(text.find("title").expect("inline expression")).expect("source position");
    let mut configuration = WorkspaceConfiguration::default();
    configuration.providers.syntax.max_nodes =
        u64::try_from(raw.nodes().len()).expect("bounded node count");
    let limits = WorkspaceIndexLimits::default()
        .with_syntax_configuration(&configuration.providers.syntax)
        .expect("raw node bound");
    let index = WorkspaceIndex::build_with_languages(
        root,
        limits,
        &SourceVisibility::default(),
        &TextFileInclusion::from(&configuration),
        &LanguageFileSelections::from(&configuration),
    )
    .expect("bounded workspace index");
    assert!(index.file(&path).is_none());
    assert!(index.text_file(&path).is_none());
    assert!(
        index
            .nodes(&path, position)
            .expect("refused source nodes")
            .is_none()
    );
    assert!(index.warnings().iter().any(|warning| {
        matches!(warning, WorkspaceIndexWarning::SyntaxTooLarge { path: refused, .. } if refused == &path)
    }));

    let accepted = build(root, &WorkspaceConfiguration::default());
    let file = accepted.file(&path).expect("accepted source");
    assert_eq!(file.source(), text);
    assert_eq!(file.syntax().language().identity_segment(), "typescript");
    assert!(
        accepted
            .nodes(&path, position)
            .expect("accepted source nodes")
            .expect("selected source")
            .iter()
            .any(|node| node.kind == "interpolation")
    );
}

fn write(root: &Path, path: &str, source: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().expect("source directory")).expect("source directories");
    fs::write(target, source).expect("source file");
}

fn build(root: &Path, configuration: &WorkspaceConfiguration) -> WorkspaceIndex {
    WorkspaceIndex::build_with_languages(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::from(configuration),
        &LanguageFileSelections::from(configuration),
    )
    .expect("workspace framework index")
}

fn utility(index: &WorkspaceIndex, path: &str, name: &str) -> bool {
    index
        .file(&ProjectPath::new(path).expect("source path"))
        .expect("indexed source")
        .syntax()
        .symbols()
        .iter()
        .any(|symbol| symbol.kind == "utility" && symbol.name == name)
}

#[test]
fn tailwind_package_and_stylesheet_changes_invalidate_context_on_rescan() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let root = directory.path();
    write(
        root,
        "package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"4.1.0\"}}",
    );
    write(root, "theme.css", "@import 'tailwindcss';");
    write(root, "view.html", "<p class=\"flex\"></p>");
    let configured = WorkspaceConfiguration::default();
    let original = build(root, &configured);
    assert!(utility(&original, "view.html", "flex"));
    write(root, "package.json", "{}");
    let removed = original
        .rescanned(&SourceVisibility::default())
        .expect("package evidence rescan");
    assert!(!utility(&removed, "view.html", "flex"));
    assert!(utility(&original, "view.html", "flex"));
    write(
        root,
        "package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"4.1.0\"}}",
    );
    let restored = removed
        .rescanned(&SourceVisibility::default())
        .expect("package evidence restored");
    assert!(utility(&restored, "view.html", "flex"));
    write(root, "theme.css", "p {color:red;}");
    let no_import = restored
        .rescanned(&SourceVisibility::default())
        .expect("stylesheet evidence rescan");
    assert!(!utility(&no_import, "view.html", "flex"));
    assert!(no_import.warnings().iter().any(|warning| matches!(
        warning,
        WorkspaceIndexWarning::FrameworkContextUnresolved {
            framework: SyntaxFramework::Tailwind,
            ..
        }
    )));
    write(
        root,
        "package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"3.4.1\"}}",
    );
    write(root, "tailwind.config.js", "export default {};");
    let legacy = no_import
        .rescanned(&SourceVisibility::default())
        .expect("legacy configuration evidence");
    assert!(utility(&legacy, "view.html", "flex"));
    fs::remove_file(root.join("tailwind.config.js")).expect("remove legacy configuration");
    let no_config = legacy
        .rescanned(&SourceVisibility::default())
        .expect("legacy configuration removal");
    assert!(!utility(&no_config, "view.html", "flex"));
}

#[test]
fn angular_workspace_keeps_owned_templates_separate_from_plain_html() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let root = directory.path();
    write(root, "apps/beacon/package.json", "{}");
    write(
        root,
        "apps/beacon/component.ts",
        "import {Component} from '@angular/core'; @Component({templateUrl:'./owned.html',template:`<p>{{title}}</p>`}) export class Beacon {}",
    );
    write(
        root,
        "apps/beacon/owned.html",
        "@if (ready) {<p>{{title}}</p>}",
    );
    write(root, "apps/beacon/plain.html", "<p>{{title}}</p>");
    write(root, "apps/other/package.json", "{}");
    write(root, "apps/other/plain.html", "<p>{{title}}</p>");
    let index = build(root, &WorkspaceConfiguration::default());
    for (path, language) in [
        ("apps/beacon/owned.html", "html:angular"),
        ("apps/beacon/plain.html", "html"),
        ("apps/other/plain.html", "html"),
        ("apps/beacon/component.ts", "typescript"),
    ] {
        let file = index
            .file(&ProjectPath::new(path).expect("source path"))
            .expect("indexed source");
        assert_eq!(file.syntax().language().identity_segment(), language);
        if path.ends_with("component.ts") {
            let position = u64::try_from(file.source().find("title").expect("inline expression"))
                .expect("source position");
            let nodes = index
                .nodes(file.path(), position)
                .expect("inline template nodes")
                .expect("selected source");
            assert!(nodes.iter().any(|node| node.kind == "interpolation"));
        }
    }
    write(root, "apps/beacon/component.ts", "export class Beacon {}");
    let plain = index
        .rescanned(&SourceVisibility::default())
        .expect("component ownership rescan");
    let file = plain
        .file(&ProjectPath::new("apps/beacon/owned.html").expect("template path"))
        .expect("indexed template");
    assert_eq!(file.syntax().language().identity_segment(), "html");
}

#[test]
fn explicit_framework_context_applies_without_package_evidence_and_keeps_dynamic_warnings() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let root = directory.path();
    write(
        root,
        "view.html",
        "<p class=\"flex\" [attr.class]=\"selected\">{{title}}</p>",
    );
    write(root, "plain.html", "<p class=\"flex\">{{title}}</p>");
    let mut configuration = WorkspaceConfiguration::default();
    configuration.providers.syntax.frameworks = vec![SyntaxFrameworkConfiguration {
        include: vec![PathPattern("view.html".to_owned())],
        angular: true,
        tailwind: Some(TailwindVersion::V4),
    }];
    let index = build(root, &configuration);
    assert!(utility(&index, "view.html", "flex"));
    assert!(!utility(&index, "plain.html", "flex"));
    assert_eq!(
        index
            .file(&ProjectPath::new("view.html").expect("view path"))
            .expect("view source")
            .syntax()
            .language()
            .identity_segment(),
        "html:angular"
    );
    assert!(index.warnings().iter().any(|warning| matches!(
        warning,
        WorkspaceIndexWarning::FrameworkContextUnresolved {
            framework: SyntaxFramework::Tailwind,
            ..
        }
    )));
}

#[test]
fn workspace_tailwind_lockfile_pin_changes_resolved_major_version() {
    let directory = tempfile::tempdir().expect("workspace directory");
    let root = directory.path();
    write(
        root,
        "package.json",
        "{\"devDependencies\":{\"tailwindcss\":\"^4.0.0\"}}",
    );
    write(
        root,
        "package-lock.json",
        "{\"lockfileVersion\":3,\"packages\":{\"node_modules/tailwindcss\":{\"version\":\"4.1.0\"}}}",
    );
    write(root, "theme.css", "@import 'tailwindcss';");
    write(root, "view.html", "<p class=\"flex\"></p>");
    let pinned = build(root, &WorkspaceConfiguration::default());
    assert!(utility(&pinned, "view.html", "flex"));
    write(
        root,
        "package-lock.json",
        "{\"lockfileVersion\":3,\"packages\":{\"node_modules/tailwindcss\":{\"version\":\"3.4.1\"}}}",
    );
    let legacy = pinned
        .rescanned(&SourceVisibility::default())
        .expect("lockfile pin rescan");
    assert!(!utility(&legacy, "view.html", "flex"));
    write(root, "tailwind.config.js", "export default {};");
    let configured = legacy
        .rescanned(&SourceVisibility::default())
        .expect("legacy evidence rescan");
    assert!(utility(&configured, "view.html", "flex"));
}
