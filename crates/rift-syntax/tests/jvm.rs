//! Java and Kotlin declarations and collection positions.

use rift_core::ProjectPath;
use rift_protocol::read::{DocumentationFormat, SymbolKind, SyntaxKind};
use rift_syntax::{
    JvmLanguage, JvmOccurrenceKind, SyntaxDocument, SyntaxLimits, SyntaxNames, SyntaxSource,
    registry,
};

fn analyze(filename: &str, text: &str) -> SyntaxDocument {
    let path = ProjectPath::new(filename).expect("fixture path");
    let extension = filename.rsplit_once('.').expect("fixture extension").1;
    let provider = registry::provider_for_extension(extension).expect("shipped grammar");
    let document = provider
        .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
        .expect("syntax parses within its limits");
    assert!(
        !document.has_errors(),
        "{filename} should have no parse errors"
    );
    document
}

#[test]
fn java_declares_nested_types_members_and_record_components() {
    let text = r"
package example /* scope */ . api;

import java.util.List;
/** The public entry point. */
public final class Client<T> implements Runnable {
    private int first, second;
    public static record Request(String name, int count) {}
    public List<String> café(String... prefix) { return List.of(prefix); }
    public void run() {}
}
";
    let document = analyze("Client.java", text);
    let symbols = document.symbols();
    let symbol = |name: &str| {
        symbols
            .iter()
            .find(|declaration| declaration.qualified_name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}"))
    };
    let class = symbol("example.api.Client");
    assert_eq!(class.documentation.len(), 1);
    assert_eq!(class.documentation[0].text, "The public entry point.");
    assert_eq!(class.documentation[0].format, DocumentationFormat::Plain);
    assert!(class.kinds.contains(&SymbolKind::Public));
    assert_eq!(
        symbol("example.api.Client.first").visibility.as_deref(),
        Some("private")
    );
    assert_eq!(
        symbol("example.api.Client.second").visibility.as_deref(),
        Some("private")
    );
    let record = symbol("example.api.Client.Request");
    assert_eq!(record.kind, "record");
    assert!(record.kinds.contains(&SymbolKind::Static));
    assert_eq!(symbol("example.api.Client.Request.name").kind, "property");
    let method = symbol("example.api.Client.café");
    let name = method.name_range.expect("authored method name range");
    assert_eq!(
        text.get(
            usize::try_from(name.start).expect("fixture offset")
                ..usize::try_from(name.end).expect("fixture offset")
        ),
        Some("café")
    );
    assert_eq!(method.signatures.len(), 1);
    assert_eq!(symbol("example.api.Client.café.prefix").kind, "parameter");
}

#[test]
fn kotlin_declares_classes_companions_and_extensions() {
    let text = r#"
package example.api

import kotlin.collections.List as Items

/** The public [Client]. */
class Client(val label: String) : Runnable {
    companion object {
        fun create(): Client = Client("example")
    }

    override fun run() {
        println(label)
    }

    internal fun stop() {}
}

typealias Names = Items<String>

@Target(AnnotationTarget.CLASS)
annotation class Scope(val modules: Array<String> = [], val excluded: Array<String> = [])

fun String.wrap(): String = trim()
"#;
    let document = analyze("Client.kt", text);
    let symbols = document.symbols();
    let symbol = |name: &str| {
        symbols
            .iter()
            .find(|declaration| declaration.qualified_name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}"))
    };
    let class = symbol("example.api.Client");
    assert_eq!(class.documentation[0].format, DocumentationFormat::Markdown);
    assert!(class.kinds.contains(&SymbolKind::Public));
    assert_eq!(symbol("example.api.Client.label").kind, "property");
    let companion = symbol("example.api.Client.Companion");
    assert_eq!(companion.kind, "object");
    assert_eq!(companion.name, "Companion");
    let name = companion.name_range.expect("authored companion keyword");
    assert_eq!(
        text.get(
            usize::try_from(name.start).expect("fixture offset")
                ..usize::try_from(name.end).expect("fixture offset")
        ),
        Some("companion")
    );
    assert_eq!(symbol("example.api.Client.Companion.create").kind, "method");
    assert_eq!(
        symbol("example.api.Client.stop").visibility.as_deref(),
        Some("internal")
    );
    assert_eq!(symbol("example.api.Names").kind, "type_alias");
    assert_eq!(symbol("example.api.Scope").kind, "annotation");
    assert_eq!(symbol("example.api.Scope.modules").kind, "property");
    assert_eq!(symbol("example.api.Scope.excluded").kind, "property");
    assert_eq!(symbol("example.api.wrap").kind, "function");
    assert_eq!(
        JvmLanguage::from_filename(r"C:\repo\build.gradle.kts"),
        Some(JvmLanguage::Kotlin)
    );
}

fn occurrences(
    text: &str,
    language: JvmLanguage,
    category: JvmOccurrenceKind,
    maximum: usize,
) -> (bool, Vec<&str>) {
    let path = ProjectPath::new("source").expect("fixture path");
    let found = language
        .occurrences(
            SyntaxSource { path: &path, text },
            SyntaxLimits::default(),
            category,
            maximum,
        )
        .expect("syntax parses within its limits");
    let strings = found
        .occurrences
        .into_iter()
        .map(|site| {
            let start = usize::try_from(site.range.start).expect("fixture offset");
            let end = usize::try_from(site.range.end).expect("fixture offset");
            if let Some(enclosing) = site.enclosing {
                assert!(enclosing.start <= site.range.start && site.range.end <= enclosing.end);
            }
            text.get(start..end).expect("valid UTF-8 range")
        })
        .collect();
    (found.complete, strings)
}

#[test]
fn collection_reads_callee_and_import_names_in_source_order() {
    let java = r"
package example;
import java.util.List;
public class Client {
    Client() { super(); }
    void run(String last) { List.of(new String(last)).size(); }
}";
    let kotlin = r"
package example

import kotlin.collections.listOf as items

fun run(last: String) {
    items(last).joinToString()
}
";
    assert_eq!(
        occurrences(java, JvmLanguage::Java, JvmOccurrenceKind::Call, 8),
        (true, vec!["super", "of", "String", "size"])
    );
    assert_eq!(
        occurrences(kotlin, JvmLanguage::Kotlin, JvmOccurrenceKind::Call, 8),
        (true, vec!["items", "joinToString"])
    );
    assert_eq!(
        occurrences(java, JvmLanguage::Java, JvmOccurrenceKind::Import, 1),
        (true, vec!["List"])
    );
    assert_eq!(
        occurrences(kotlin, JvmLanguage::Kotlin, JvmOccurrenceKind::Import, 1),
        (true, vec!["listOf"])
    );
    assert!(!occurrences(java, JvmLanguage::Java, JvmOccurrenceKind::Call, 1).0);
    let (complete, references) =
        occurrences(kotlin, JvmLanguage::Kotlin, JvmOccurrenceKind::Reference, 8);
    assert!(complete);
    assert_eq!(references, ["String", "items", "last", "joinToString"]);
}

#[test]
fn collection_reports_partial_coverage_for_broken_sources() {
    let java = "class Client { void run() { List.of( } }";
    let (complete, sites) = occurrences(java, JvmLanguage::Java, JvmOccurrenceKind::Call, 8);
    assert!(!complete);
    assert!(sites.len() <= 1);
}

#[test]
fn kotlin_scripts_use_the_kotlin_grammar() {
    let text = r#"val greeting = "hello"

println(greeting.uppercase())
"#;
    let document = analyze("build.gradle.kts", text);
    assert!(
        document
            .symbols()
            .iter()
            .any(|symbol| symbol.name == "greeting")
    );
    assert_eq!(
        occurrences(text, JvmLanguage::Kotlin, JvmOccurrenceKind::Call, 4),
        (true, vec!["println", "uppercase"])
    );
}

#[test]
fn java_declares_named_type_and_record_patterns() {
    let text = r"
package example;
import java.util.Optional;
class Samples {
    record Request(String title, int count) {}

    boolean typePattern(Object value) {
        return value instanceof Optional<?> other && other.isPresent();
    }

    boolean recordPattern(Object value) {
        return value instanceof Request(String title, int count)
            && !title.isEmpty() && count > 0;
    }
}";
    let document = analyze("Samples.java", text);
    for (qualified, expected) in [
        ("example.Samples.typePattern.other", "other"),
        ("example.Samples.recordPattern.title", "title"),
        ("example.Samples.recordPattern.count", "count"),
    ] {
        let local = document
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == qualified)
            .unwrap_or_else(|| panic!("missing pattern variable {qualified}"));
        assert_eq!(local.kind, "variable");
        assert!(!local.kinds.contains(&SymbolKind::Public));
        let range = local.name_range.expect("authored pattern variable");
        assert_eq!(
            text.get(
                usize::try_from(range.start).expect("fixture offset")
                    ..usize::try_from(range.end).expect("fixture offset")
            ),
            Some(expected)
        );
    }
    let (_, references) = occurrences(text, JvmLanguage::Java, JvmOccurrenceKind::Reference, 128);
    assert_eq!(
        references.iter().filter(|&&name| name == "other").count(),
        1
    );
}

#[test]
fn jvm_registry_exposes_structural_kinds_and_restores_declaration_kind_names() {
    let java = registry::provider_for_extension("java").expect("Java provider");
    for (kind, portable) in [
        ("class_declaration", &[SyntaxKind::Declaration][..]),
        ("method_declaration", &[SyntaxKind::Declaration]),
        (
            "formal_parameter",
            &[SyntaxKind::Declaration, SyntaxKind::Parameter],
        ),
        ("constructor_body", &[SyntaxKind::Body]),
        ("block", &[SyntaxKind::Block]),
        ("import_declaration", &[SyntaxKind::Import]),
        ("generic_type", &[SyntaxKind::TypeExpression]),
        ("identifier", &[SyntaxKind::Identifier]),
        (
            "type_identifier",
            &[SyntaxKind::TypeExpression, SyntaxKind::Identifier],
        ),
        ("line_comment", &[SyntaxKind::Comment]),
        ("annotation", &[SyntaxKind::Annotation]),
        ("argument_list", &[SyntaxKind::Argument]),
        ("record_pattern_component", &[SyntaxKind::Pattern]),
        ("method_invocation", &[SyntaxKind::Expression]),
        ("program", &[]),
    ] {
        assert_eq!(java.syntax_element_kinds(kind), portable, "{kind}");
    }

    let kotlin = registry::provider_for_extension("kt").expect("Kotlin provider");
    assert_eq!(
        kotlin.syntax_element_kinds("companion_object"),
        [SyntaxKind::Declaration]
    );
    let names = SyntaxNames::new(kotlin.language()).expect("Kotlin names");
    assert_eq!(names.symbol_kind("object"), Some("object"));
    assert_eq!(names.symbol_kind("unknown"), None);
}

#[test]
fn java_declares_inferred_lambda_parameters_and_local_bindings() {
    let text = r"
class Sample {
    void collect(String[] values, AutoCloseable borrowed) throws Exception {
        java.util.function.Function<String, String> plain = plainArg -> plainArg.trim();
        java.util.function.Function<String, String> wrapped = (wrappedArg) -> wrappedArg.trim();
        for (String item : values) { System.out.println(item); }
        try (AutoCloseable opened = borrowed) { opened.close(); }
        try (borrowed) { borrowed.close(); }
    }
}";
    let document = analyze("Sample.java", text);
    for (name, kind) in [
        ("plainArg", "parameter"),
        ("wrappedArg", "parameter"),
        ("item", "variable"),
        ("opened", "variable"),
        ("borrowed", "parameter"),
    ] {
        let mut declarations = document
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == name);
        let symbol = declarations.next().expect("Java declaration");
        assert!(declarations.next().is_none(), "{name} is declared once");
        assert_eq!(symbol.kind, kind, "{name}");
        assert!(!symbol.kinds.contains(&SymbolKind::Public));
        let range = symbol.name_range.expect("declaration name");
        let start = usize::try_from(range.start).expect("fixture offset");
        let end = usize::try_from(range.end).expect("fixture offset");
        assert_eq!(text.get(start..end), Some(name));
    }
    let (complete, references) =
        occurrences(text, JvmLanguage::Java, JvmOccurrenceKind::Reference, 128);
    assert!(complete);
    for name in ["plainArg", "wrappedArg", "item", "opened"] {
        assert_eq!(references.iter().filter(|&&site| site == name).count(), 1);
    }
    assert_eq!(
        references
            .iter()
            .filter(|&&site| site == "borrowed")
            .count(),
        3
    );
}

#[test]
fn kotlin_declares_authored_accessors_and_catch_parameters() {
    let text = r"
package sample

class Options {
    var count = 0
        get() {
            val current = field
            return current
        }
        private set(next) { field = next }

    private val hidden: Int
        get() = 1

    val automatic = 0

    fun read(): Int = try {
        count
    } catch (caught: IllegalStateException) {
        caught.hashCode()
    }
}
";
    let document = analyze("Options.kt", text);
    for (name, token, public) in [
        ("count", "count", true),
        ("count.get", "get", true),
        ("count.get.current", "current", false),
        ("count.set", "set", false),
        ("count.set.next", "next", false),
        ("hidden.get", "get", false),
        ("automatic", "automatic", true),
        ("read.caught", "caught", false),
    ] {
        let qualified = format!("sample.Options.{name}");
        let symbol = document
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == qualified)
            .unwrap_or_else(|| panic!("missing declaration {qualified}"));
        let range = symbol.name_range.expect("authored name");
        let start = usize::try_from(range.start).expect("fixture offset");
        let end = usize::try_from(range.end).expect("fixture offset");
        assert_eq!(text.get(start..end), Some(token));
        assert_eq!(
            symbol.kinds.contains(&SymbolKind::Public),
            public,
            "{qualified}"
        );
    }
    assert!(
        !document
            .symbols()
            .iter()
            .any(|symbol| symbol.qualified_name == "sample.Options.automatic.get")
    );
    let (_, references) = occurrences(text, JvmLanguage::Kotlin, JvmOccurrenceKind::Reference, 128);
    assert_eq!(
        references.iter().filter(|&&name| name == "caught").count(),
        1
    );
}
