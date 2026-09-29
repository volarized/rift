//! A `pattern` search answers as ripgrep does by default: ripgrep matches one line at a
//! time, its `\n` terminator removed and a `\r` before it kept, and the whole-file matcher
//! must find the same matches at the same offsets.

use std::error::Error;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use rift_ranking::Pattern;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A compiled-size bound far above every pattern this suite runs.
const SIZE_LIMIT: usize = 1 << 20;

/// The patterns the dialect comparison of the text-search evaluation ran against
/// ripgrep 15.1.0: anchors, trailing whitespace, classes beside line ends, and a
/// carriage return.
const DIALECT_PATTERNS: [&str; 11] = [
    "^use ",
    r"^\s*pub fn",
    ";$",
    r"^\}$",
    r"fn\s+new",
    r"\s+$",
    r"^#\[test\]",
    r"impl\s+\w+\s+for",
    r"\)\s*\{$",
    r"^\s*//",
    r"\r$",
];

/// Line-anchored patterns every revision of this repository matches once its lines end
/// `\n`: a `use` line, a statement end, a closing brace alone, and a comment line. A
/// whole-file reading of `^` and `$` finds almost none of them.
const ANCHORED_IN_THIS_REPOSITORY: [&str; 4] = ["^use ", ";$", r"^\}$", r"^\s*//"];

/// A CRLF file, its LF twin, a brace alone on the line after `)`, and a whitespace run
/// split by a line feed.
const CRLF_FIXTURE: [(&str, &str); 4] = [
    ("brace.rs", "fn main()\n{\n}\n"),
    ("crlf.rs", "let a = 1;\r\nlet b = 2;\r\n"),
    ("lf.rs", "let a = 1;\nlet b = 2;\n"),
    ("split.rs", "fn  \nnew()\n"),
];

/// The files `rg --no-config -l -e <pattern>` lists over [`CRLF_FIXTURE`], recorded with
/// ripgrep 15.1.0.
const CRLF_FIXTURE_RIPGREP: [(&str, &[&str]); 12] = [
    (";$", &["lf.rs"]),
    (r"\r$", &["crlf.rs"]),
    ("^let b = 2;$", &["lf.rs"]),
    (r"\)\s*\{$", &[]),
    (r"fn\s+new", &[]),
    ("^use ", &[]),
    (r"^\s*pub fn", &[]),
    (r"^\}$", &["brace.rs"]),
    (r"\s+$", &["crlf.rs", "split.rs"]),
    (r"^#\[test\]", &[]),
    (r"impl\s+\w+\s+for", &[]),
    (r"^\s*//", &[]),
];

fn pattern(source: &str) -> TestResult<Pattern> {
    Ok(Pattern::parse(source, SIZE_LIMIT)?)
}

fn whole_file(pattern: &Pattern, text: &str) -> Vec<Range<usize>> {
    pattern.matches(text, 0..text.len()).collect()
}

/// ripgrep's default reading: each line alone, its `\n` removed and any `\r` kept, the
/// matches offset back into the file.
fn line_by_line(oracle: &regex::Regex, text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        found.extend(
            oracle
                .find_iter(body)
                .map(|matched| start + matched.start()..start + matched.end()),
        );
        start += line.len();
    }
    found
}

#[test]
fn the_crlf_fixture_answers_the_files_ripgrep_lists() -> TestResult {
    for (source, expected) in CRLF_FIXTURE_RIPGREP {
        let pattern = pattern(source)?;
        let listed: Vec<&str> = CRLF_FIXTURE
            .iter()
            .filter(|(_, text)| !whole_file(&pattern, text).is_empty())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(listed, expected, "pattern {source:?}");
    }
    Ok(())
}

#[test]
fn the_crlf_fixture_answers_ripgrep_offsets_line_by_line() -> TestResult {
    for source in DIALECT_PATTERNS {
        let pattern = pattern(source)?;
        let oracle = regex::Regex::new(source)?;
        for (name, text) in CRLF_FIXTURE {
            assert_eq!(
                whole_file(&pattern, text),
                line_by_line(&oracle, text),
                "pattern {source:?} over {name}"
            );
        }
    }
    Ok(())
}

/// Every visible UTF-8 file of this repository, as ripgrep walks it by default: hidden
/// entries and build output left out.
fn repository_files(root: &Path) -> TestResult<Vec<(PathBuf, String)>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let bytes = fs::read(entry.path())?;
                if let Ok(text) = String::from_utf8(bytes)
                    && !text.contains('\0')
                {
                    files.push((entry.path(), text));
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// `text` with every line ending `\n`, and the same lines each ending `\r\n`.
///
/// A checkout's line endings follow its host's git configuration: a Windows runner
/// converts every line to `\r\n`, where `;$` and `^\}$` match nothing, as ripgrep's
/// own reading of that file finds nothing. Reading both forms of each file keeps the
/// comparison the same on every host.
fn line_ending_forms(text: &str) -> [String; 2] {
    let lf = text.replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    [lf, crlf]
}

#[test]
fn this_repository_answers_ripgrep_offsets_line_by_line() -> TestResult {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("the crate sits two levels below the repository root")?;
    let files = repository_files(root)?;
    assert!(
        files.len() > 100,
        "the walk reaches the repository: {} files",
        files.len()
    );
    let forms: Vec<(&PathBuf, [String; 2])> = files
        .iter()
        .map(|(path, text)| (path, line_ending_forms(text)))
        .collect();
    for source in DIALECT_PATTERNS {
        let pattern = pattern(source)?;
        let oracle = regex::Regex::new(source)?;
        let mut matched_lf_files = 0_usize;
        for (path, [lf, crlf]) in &forms {
            let found = whole_file(&pattern, lf);
            assert_eq!(
                found,
                line_by_line(&oracle, lf),
                "pattern {source:?} over {} with LF line ends",
                path.display()
            );
            assert_eq!(
                whole_file(&pattern, crlf),
                line_by_line(&oracle, crlf),
                "pattern {source:?} over {} with CRLF line ends",
                path.display()
            );
            matched_lf_files += usize::from(!found.is_empty());
        }
        if ANCHORED_IN_THIS_REPOSITORY.contains(&source) {
            assert!(
                matched_lf_files > 0,
                "pattern {source:?} matches a line of this repository"
            );
        }
    }
    Ok(())
}
