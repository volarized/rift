//! Maps one qualifying call to the `search` arguments answering the same question.
//!
//! A `Grep` call, and the `grep` or `rg` command [`super::bash`] reads as one, maps to a
//! `search` call carrying `pattern`. A `Glob` call, and a `find` command, list files by
//! name, so they map to a `search` call whose `pattern` matches once in every file, over the
//! `paths.include` globs selecting the same names. A call holding a field `search` has no
//! form for maps to nothing, so the steer lets it through without a deny. Sans-I/O: every
//! path arrives classified as a [`GrepPath`], which the shell probes.

use ignore::types::TypesBuilder;
use rift_protocol::read::{PAGE_LIMIT_MAX, PathPattern, SearchParamsTarget};
use serde::Serialize;
use serde_json::Value;

/// UTF-8 bytes of the caller's own regex a deny reason carries. A longer `Grep` pattern
/// passes without a deny, since a truncated regex is another regex.
pub(super) const DENY_PATTERN_BYTES_MAX: usize = 256;

/// The `pattern` a file listing searches with. `\A` matches once, at the start of each
/// file's text, so the answer holds one file hit per file its `paths` select.
const EVERY_FILE_PATTERN: &str = r"\A";

/// Bytes of one serialized `search` suggestion. A call whose arguments run longer passes
/// without a deny, so the deny reason stays bounded whatever the call names.
const SUGGESTION_BYTES_MAX: usize = 1_024;

/// Globs one suggestion's `paths.include` lists at most. A call selecting more passes
/// without a deny, so joining them stays bounded whatever `glob` holds.
const INCLUDE_GLOBS_MAX: usize = 64;

/// Entries Claude Code's `Grep` returns when a call names no `head_limit` (`250` in Claude
/// Code 2.1.280), the page size an `offset` pages by.
const GREP_HEAD_LIMIT_DEFAULT: u64 = 250;

/// Where one `Grep` `path`, or one path a command names, sits in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GrepPath {
    /// The workspace root itself.
    Root,
    /// A directory below the root, as a project-relative `/` path.
    Directory(String),
    /// A file below the root, as a project-relative `/` path.
    File(String),
    /// Outside the root, missing, or holding a glob character `paths.include` would read.
    Unmapped,
}

impl GrepPath {
    /// Whether the path names a directory, the root included.
    pub(super) const fn is_directory(&self) -> bool {
        matches!(self, Self::Root | Self::Directory(_))
    }

    /// The `paths.include` globs selecting `globs` below this path; an empty list selects
    /// every file. `None` when no glob list states the selection: an unmapped path, a file
    /// beside a glob, a file at the root, whose slashless name a glob would match in every
    /// directory, and a glob holding `/` below a directory.
    fn include(&self, globs: &[String]) -> Option<Vec<String>> {
        match self {
            Self::Unmapped => None,
            Self::File(file) if !globs.is_empty() || !file.contains('/') => None,
            Self::File(file) => Some(vec![file.clone()]),
            Self::Root => Some(globs.to_vec()),
            Self::Directory(directory) if globs.is_empty() => Some(vec![format!("{directory}/**")]),
            Self::Directory(directory) => globs
                .iter()
                .map(|glob| (!glob.contains('/')).then(|| format!("{directory}/**/{glob}")))
                .collect(),
        }
    }
}

/// The `Grep` fields steer maps, read from one `Grep` call's input or from the `grep` or
/// `rg` command line equal to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GrepRequest {
    /// The regex, in the syntax of the `regex` crate that ripgrep and `pattern` read.
    pub(super) pattern: String,
    /// The `rg --glob` values, each selecting files by name or path; several select their
    /// union.
    pub(super) globs: Vec<String>,
    /// A ripgrep type name, such as `rust`.
    pub(super) file_type: Option<String>,
    /// The match ignores case.
    pub(super) case_insensitive: bool,
    /// The call lists matching files rather than matching lines.
    pub(super) files_only: bool,
    /// Entries the call returns at most; `0` returns every entry.
    pub(super) head_limit: Option<u64>,
    /// Entries the call skips before `head_limit` applies.
    pub(super) offset: Option<u64>,
}

impl GrepRequest {
    /// A request for `pattern` alone, listing matching lines of every file.
    pub(super) const fn lines(pattern: String) -> Self {
        Self {
            pattern,
            globs: Vec::new(),
            file_type: None,
            case_insensitive: false,
            files_only: false,
            head_limit: None,
            offset: None,
        }
    }

    /// A request listing each file `globs` select once, as a `Glob` call or a `find DIRECTORY
    /// -name NAME` command asks: [`EVERY_FILE_PATTERN`], answered as file hits.
    pub(super) fn files(globs: Vec<String>) -> Self {
        Self {
            globs,
            files_only: true,
            ..Self::lines(EVERY_FILE_PATTERN.to_owned())
        }
    }

    /// Reads one `Glob` call's input, the `pattern` and `path` fields Claude Code 2.1.280's
    /// `Glob` schema declares; `path` is resolved by the caller. The pattern is one
    /// `rg --glob` value, which reads it with the `.gitignore` rules `paths.include` reads.
    /// `None` for a pattern that is not a string, or a field steer does not know.
    pub(super) fn from_glob_input(input: &Value) -> Option<Self> {
        let fields = input.as_object()?;
        if fields
            .keys()
            .any(|key| !matches!(key.as_str(), "pattern" | "path"))
        {
            return None;
        }
        let pattern = fields.get("pattern")?.as_str()?;
        Some(Self::files(vec![pattern.to_owned()]))
    }

    /// Reads one `Grep` call's input, the fields Claude Code 2.1.280's `Grep` schema
    /// declares. An absent `output_mode` is `Grep`'s own default, `files_with_matches`.
    /// `None` when a field has no `search` form: `multiline`, context lines, `-o`,
    /// `output_mode: "count"`, or a field steer does not know.
    pub(super) fn from_input(input: &Value) -> Option<Self> {
        let mut request = Self {
            files_only: true,
            ..Self::lines(String::new())
        };
        for (key, value) in input.as_object()? {
            request.read(key, value)?;
        }
        Some(request)
    }

    /// Reads one input field. `None` when the field has no `search` form.
    fn read(&mut self, key: &str, value: &Value) -> Option<()> {
        match key {
            "pattern" => value.as_str()?.clone_into(&mut self.pattern),
            "path" => {}
            "glob" => self.globs = grep_globs(value.as_str()?),
            "type" => self.file_type = Some(value.as_str()?.to_owned()),
            "-i" => self.case_insensitive = flag(value)?,
            "-n" => {
                flag(value)?;
            }
            "output_mode" => {
                self.files_only = match value.as_str()? {
                    "content" => false,
                    "files_with_matches" => true,
                    _ => return None,
                };
            }
            "multiline" | "-o" => (!flag(value)?).then_some(())?,
            "-A" | "-B" | "-C" | "context" => (count(value)? == 0).then_some(())?,
            "head_limit" => self.head_limit = Some(count(value)?),
            "offset" => self.offset = Some(count(value)?),
            _ => return None,
        }
        Some(())
    }

    /// The serialized `search` arguments answering this request over `paths`, each one
    /// `Grep` `path`; several paths select the union of their files. `None` when a part has
    /// no `search` form: an empty pattern, one past [`DENY_PATTERN_BYTES_MAX`], one the
    /// `regex` crate refuses, a selection no glob list states, an offset inside a page, or
    /// arguments past `SUGGESTION_BYTES_MAX`.
    pub(super) fn suggestion(&self, paths: &[GrepPath]) -> Option<String> {
        let within_bound = !self.pattern.is_empty() && self.pattern.len() <= DENY_PATTERN_BYTES_MAX;
        let pattern = if self.case_insensitive {
            format!("(?i){}", self.pattern)
        } else {
            self.pattern.clone()
        };
        if !within_bound || regex_syntax::parse(&pattern).is_err() {
            return None;
        }
        let include = included(&self.selected_globs()?, paths)?;
        let (limit, page_index) = paging(self.head_limit, self.offset)?;
        let suggestion = SearchSuggestion {
            pattern,
            paths: (!include.is_empty()).then_some(SuggestedPaths { include }),
            target: self.files_only.then_some(SearchParamsTarget::File),
            limit,
            page_index,
        };
        let rendered = serde_json::to_string(&suggestion).ok()?;
        (rendered.len() <= SUGGESTION_BYTES_MAX).then_some(rendered)
    }

    /// The globs `globs` and `file_type` select. `None` for a negated glob, which drops
    /// files rather than selecting them, an unknown type, and a glob beside a type, whose
    /// intersection no glob list states.
    fn selected_globs(&self) -> Option<Vec<String>> {
        let negated = self.globs.iter().any(|glob| glob.starts_with('!'));
        match (self.globs.as_slice(), &self.file_type) {
            _ if negated => None,
            ([_, ..], Some(_)) => None,
            ([], Some(name)) => type_globs(name),
            (globs, None) => Some(globs.to_vec()),
        }
    }
}

/// The `paths.include` globs selecting `globs` below each of `paths`, joined in order
/// without repeats; an empty list, as `paths.include` reads it, selects every file, and
/// one path selecting every file makes the union select every file too. `None` when a
/// path has no glob form, a glob breaks the `paths.include` contract, or the globs pass
/// `INCLUDE_GLOBS_MAX`.
fn included(globs: &[String], paths: &[GrepPath]) -> Option<Vec<PathPattern>> {
    let selections = paths
        .iter()
        .map(|path| path.include(globs))
        .collect::<Option<Vec<_>>>()?;
    if selections.iter().any(Vec::is_empty) {
        return Some(Vec::new());
    }
    if selections.iter().map(Vec::len).sum::<usize>() > INCLUDE_GLOBS_MAX {
        return None;
    }
    let mut include: Vec<PathPattern> = Vec::new();
    for glob in selections.into_iter().flatten() {
        let pattern = PathPattern(glob);
        if pattern.violation().is_some() {
            return None;
        }
        if !include.contains(&pattern) {
            include.push(pattern);
        }
    }
    Some(include)
}

/// `head_limit` and `offset` as `limit` and `page_index`, `head_limit` capped at
/// [`PAGE_LIMIT_MAX`]. `None` when the offset falls inside a page or pages an unlimited
/// answer.
fn paging(head_limit: Option<u64>, offset: Option<u64>) -> Option<(Option<u64>, Option<u64>)> {
    let limit = match head_limit {
        Some(0) | None => None,
        Some(entries) => Some(entries.min(PAGE_LIMIT_MAX)),
    };
    match offset {
        None | Some(0) => Some((limit, None)),
        Some(_) if head_limit == Some(0) => None,
        Some(skipped) => {
            let size = limit.unwrap_or(GREP_HEAD_LIMIT_DEFAULT);
            (skipped % size == 0).then_some((Some(size), Some(skipped / size)))
        }
    }
}

/// The `search` arguments one `Grep` call maps to, in the `search` schema's field order.
#[derive(Debug, Serialize)]
struct SearchSuggestion {
    pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    paths: Option<SuggestedPaths>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<SearchParamsTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_index: Option<u64>,
}

/// The `paths` selector a suggestion carries: `include` alone.
#[derive(Debug, Serialize)]
struct SuggestedPaths {
    include: Vec<PathPattern>,
}

/// Splits one `Grep` `glob` as Claude Code 2.1.280 does before passing each piece to
/// `rg --glob`: on whitespace, then on commas, except that a piece holding both `{` and `}`
/// stays whole.
fn grep_globs(glob: &str) -> Vec<String> {
    glob.split_whitespace()
        .flat_map(|piece| {
            if piece.contains('{') && piece.contains('}') {
                vec![piece.to_owned()]
            } else {
                piece
                    .split(',')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
        })
        .collect()
}

/// The globs ripgrep's built-in type table holds for `name`, read from `ignore`'s default
/// types, the table `rg --type` reads. Each glob matches a file name, never a path.
fn type_globs(name: &str) -> Option<Vec<String>> {
    let mut builder = TypesBuilder::new();
    builder.add_defaults();
    builder
        .definitions()
        .into_iter()
        .find(|definition| definition.name() == name)
        .map(|definition| definition.globs().to_vec())
}

/// A boolean `Grep` flag, sent as a JSON boolean or its string spelling.
fn flag(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(set) => Some(*set),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// A non-negative `Grep` count, sent as a JSON number or its digits.
fn count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use rift_protocol::read::SearchParams;
    use serde_json::{Value, json};

    use super::{
        DENY_PATTERN_BYTES_MAX, EVERY_FILE_PATTERN, GrepPath, GrepRequest, INCLUDE_GLOBS_MAX,
        SUGGESTION_BYTES_MAX, grep_globs,
    };

    /// Proves one suggestion is a `search` call the server takes: it deserializes into
    /// `SearchParams`, which refuses an unknown field, and validates against the served
    /// `search` input schema. Returns the arguments as JSON.
    pub(in crate::steer) fn served_search_call(suggestion: &str) -> Value {
        let arguments: Value = serde_json::from_str(suggestion).expect("a suggestion is JSON");
        serde_json::from_value::<SearchParams>(arguments.clone())
            .unwrap_or_else(|error| panic!("{suggestion} must be SearchParams: {error}"));
        let tools = rift_mcp::schema::tool_listing();
        let search = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "search")
            .expect("the served surface carries search");
        let schema = Value::Object(search.input_schema.as_ref().clone());
        let validator = jsonschema::validator_for(&schema).expect("the served schema compiles");
        let errors: Vec<String> = validator
            .iter_errors(&arguments)
            .map(|error| error.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{suggestion} must validate against the served search schema: {errors:?}"
        );
        arguments
    }

    /// The `search` arguments one `Grep` input maps to at `path`, proven a served call, or
    /// `None` when the call passes.
    fn mapped(input: &Value, path: &GrepPath) -> Option<Value> {
        let request = GrepRequest::from_input(input)?;
        let suggestion = request.suggestion(std::slice::from_ref(path))?;
        Some(served_search_call(&suggestion))
    }

    #[test]
    fn the_recorded_grep_calls_map_to_their_pattern() {
        for pattern in ["tokenize\\(", "ParsedQuery", "TODO|FIXME"] {
            let input = json!({"pattern": pattern, "output_mode": "content", "-n": true});
            assert_eq!(
                mapped(&input, &GrepPath::Root),
                Some(json!({"pattern": pattern}))
            );
        }
    }

    #[test]
    fn an_absent_output_mode_lists_files_as_grep_does() {
        assert_eq!(
            mapped(&json!({"pattern": "TODO"}), &GrepPath::Root),
            Some(json!({"pattern": "TODO", "target": "file"}))
        );
    }

    #[test]
    fn a_pattern_holding_a_quote_or_backslash_stays_valid_json() {
        let pattern = r#"say\("hi"\)"#;
        let input = json!({"pattern": pattern, "output_mode": "content"});
        let arguments = mapped(&input, &GrepPath::Root).expect("the pattern maps");
        assert_eq!(arguments["pattern"], json!(pattern));
    }

    #[test]
    fn calls_search_has_no_form_for_pass_without_a_suggestion() {
        for extra in [
            json!({"multiline": true}),
            json!({"-A": 2}),
            json!({"-B": 1}),
            json!({"-C": 3}),
            json!({"context": 1}),
            json!({"-o": true}),
            json!({"output_mode": "count"}),
            json!({"output_mode": 1}),
            json!({"-n": 1}),
            json!({"unknown_field": 1}),
            json!({"type": "rs"}),
            json!({"type": "rust", "glob": "*.rs"}),
            json!({"glob": "!target/**"}),
        ] {
            let mut input = json!({"pattern": "TODO"});
            for (key, value) in extra.as_object().expect("an object") {
                input[key] = value.clone();
            }
            assert_eq!(mapped(&input, &GrepPath::Root), None, "{input}");
        }
        assert_eq!(GrepRequest::from_input(&json!(["pattern"])), None);
    }

    #[test]
    fn switched_off_flags_and_zero_context_still_map() {
        let input = json!({
            "pattern": "TODO", "output_mode": "content",
            "multiline": false, "-o": "false", "-C": 0, "-A": "0", "-n": false
        });
        assert_eq!(
            mapped(&input, &GrepPath::Root),
            Some(json!({"pattern": "TODO"}))
        );
    }

    #[test]
    fn case_insensitive_and_file_output_map() {
        let input = json!({"pattern": "todo", "-i": true, "output_mode": "files_with_matches"});
        assert_eq!(
            mapped(&input, &GrepPath::Root),
            Some(json!({"pattern": "(?i)todo", "target": "file"}))
        );
    }

    #[test]
    fn a_type_maps_to_the_ripgrep_default_globs() {
        let input = json!({"pattern": "fn", "type": "rust", "output_mode": "content"});
        assert_eq!(
            mapped(&input, &GrepPath::Root),
            Some(json!({"pattern": "fn", "paths": {"include": ["*.rs"]}}))
        );
        let input = json!({"pattern": "fn", "type": "py", "output_mode": "content"});
        assert_eq!(
            mapped(&input, &GrepPath::Directory("tools".to_owned())),
            Some(
                json!({"pattern": "fn", "paths": {"include": ["tools/**/*.py", "tools/**/*.pyi"]}})
            )
        );
    }

    #[test]
    fn a_glob_splits_as_claude_code_splits_it() {
        assert_eq!(grep_globs("*.ts *.tsx"), vec!["*.ts", "*.tsx"]);
        assert_eq!(grep_globs("*.ts,,*.tsx"), vec!["*.ts", "*.tsx"]);
        assert_eq!(grep_globs(" *.{ts,tsx} *.js "), vec!["*.{ts,tsx}", "*.js"]);
        let input = json!({"pattern": "x", "glob": "*.rs,*.toml", "output_mode": "content"});
        assert_eq!(
            mapped(&input, &GrepPath::Directory("crates".to_owned())),
            Some(
                json!({"pattern": "x", "paths": {"include": ["crates/**/*.rs", "crates/**/*.toml"]}})
            )
        );
    }

    #[test]
    fn a_path_and_a_glob_map_to_include() {
        let directory = GrepPath::Directory("crates/rift".to_owned());
        let file = GrepPath::File("src/lib.rs".to_owned());
        let bare = json!({"pattern": "x", "output_mode": "content"});
        let globbed = json!({"pattern": "x", "glob": "*.rs", "output_mode": "content"});
        assert_eq!(
            mapped(&bare, &directory),
            Some(json!({"pattern": "x", "paths": {"include": ["crates/rift/**"]}}))
        );
        assert_eq!(
            mapped(&globbed, &directory),
            Some(json!({"pattern": "x", "paths": {"include": ["crates/rift/**/*.rs"]}}))
        );
        assert_eq!(
            mapped(&globbed, &GrepPath::Root),
            Some(json!({"pattern": "x", "paths": {"include": ["*.rs"]}}))
        );
        assert_eq!(
            mapped(&bare, &file),
            Some(json!({"pattern": "x", "paths": {"include": ["src/lib.rs"]}}))
        );
        assert_eq!(mapped(&globbed, &file), None);
        assert_eq!(mapped(&bare, &GrepPath::File("README.md".to_owned())), None);
        assert_eq!(mapped(&bare, &GrepPath::Unmapped), None);
        let slashed = json!({"pattern": "x", "glob": "src/*.rs", "output_mode": "content"});
        assert_eq!(mapped(&slashed, &directory), None);
        let backslashed = json!({"pattern": "x", "glob": r"a\b", "output_mode": "content"});
        assert_eq!(mapped(&backslashed, &GrepPath::Root), None);
    }

    #[test]
    fn several_paths_join_and_the_root_selects_every_file() {
        let request = GrepRequest::lines("x".to_owned());
        let src = GrepPath::Directory("src".to_owned());
        let examples = GrepPath::Directory("examples".to_owned());
        let joined = request
            .suggestion(&[src.clone(), examples, src.clone()])
            .expect("directories map");
        assert_eq!(
            served_search_call(&joined),
            json!({"pattern": "x", "paths": {"include": ["src/**", "examples/**"]}})
        );
        let whole = request
            .suggestion(&[src.clone(), GrepPath::Root])
            .expect("the root maps");
        assert_eq!(served_search_call(&whole), json!({"pattern": "x"}));
        assert_eq!(request.suggestion(&[src, GrepPath::Unmapped]), None);
    }

    #[test]
    fn head_limit_and_offset_map_to_paging() {
        let cases = [
            (json!({"head_limit": 50}), Some(json!({"limit": 50}))),
            (
                json!({"head_limit": 50, "offset": 100}),
                Some(json!({"limit": 50, "page_index": 2})),
            ),
            (
                json!({"offset": 250}),
                Some(json!({"limit": 250, "page_index": 1})),
            ),
            (json!({"head_limit": 0}), Some(json!({}))),
            (
                json!({"head_limit": 20_000}),
                Some(json!({"limit": 10_000})),
            ),
            (json!({"head_limit": 50, "offset": 30}), None),
            (json!({"head_limit": 0, "offset": 10}), None),
            (json!({"head_limit": -1}), None),
        ];
        for (paging, expected) in cases {
            let mut input = paging.clone();
            input["pattern"] = json!("x");
            input["output_mode"] = json!("content");
            let expected = expected.map(|mut expected| {
                expected["pattern"] = json!("x");
                expected
            });
            assert_eq!(mapped(&input, &GrepPath::Root), expected, "{paging}");
        }
    }

    #[test]
    fn a_missing_oversized_or_refused_pattern_passes() {
        let content = |pattern: Value| json!({"pattern": pattern, "output_mode": "content"});
        assert_eq!(
            mapped(&json!({"output_mode": "content"}), &GrepPath::Root),
            None
        );
        assert_eq!(mapped(&content(json!("")), &GrepPath::Root), None);
        let long = "a".repeat(DENY_PATTERN_BYTES_MAX + 1);
        assert_eq!(mapped(&content(json!(long)), &GrepPath::Root), None);
        let longest = "a".repeat(DENY_PATTERN_BYTES_MAX);
        assert!(mapped(&content(json!(longest)), &GrepPath::Root).is_some());
        assert_eq!(mapped(&content(json!("tokenize(")), &GrepPath::Root), None);
        assert_eq!(mapped(&content(json!(7)), &GrepPath::Root), None);
    }

    #[test]
    fn a_suggestion_past_its_byte_bound_passes() {
        let globs: Vec<String> = (0..64)
            .map(|index| format!("module_{index:02}/*.rs"))
            .collect();
        let request = GrepRequest {
            globs,
            ..GrepRequest::lines("x".to_owned())
        };
        let everything = request.suggestion(&[GrepPath::Root]);
        assert_eq!(
            everything, None,
            "the arguments pass {SUGGESTION_BYTES_MAX} bytes"
        );
        let request = GrepRequest {
            globs: vec!["*.rs".to_owned()],
            ..request
        };
        assert!(request.suggestion(&[GrepPath::Root]).is_some());
    }

    #[test]
    fn a_selection_past_its_glob_bound_passes() {
        let globs = |count: usize| (0..count).map(|index| format!("*.e{index}")).collect();
        let at_bound = GrepRequest {
            globs: globs(INCLUDE_GLOBS_MAX),
            ..GrepRequest::lines("x".to_owned())
        };
        assert!(at_bound.suggestion(&[GrepPath::Root]).is_some());
        let past_bound = GrepRequest {
            globs: globs(INCLUDE_GLOBS_MAX + 1),
            ..at_bound
        };
        assert_eq!(past_bound.suggestion(&[GrepPath::Root]), None);
    }

    /// The `search` arguments one `Glob` input maps to at `path`, proven a served call, or
    /// `None` when the call passes.
    fn listed(input: &Value, path: &GrepPath) -> Option<Value> {
        let request = GrepRequest::from_glob_input(input)?;
        let suggestion = request.suggestion(std::slice::from_ref(path))?;
        Some(served_search_call(&suggestion))
    }

    /// The listing call selecting `include`, as a `Glob` call or a `find` command maps.
    fn listing(include: &[&str]) -> Value {
        json!({"pattern": EVERY_FILE_PATTERN, "paths": {"include": include}, "target": "file"})
    }

    #[test]
    fn a_glob_maps_to_a_listing_of_the_files_it_selects() {
        let rust = json!({"pattern": "**/*.rs"});
        assert_eq!(listed(&rust, &GrepPath::Root), Some(listing(&["**/*.rs"])));
        let quoted = json!({"pattern": r#"**/say"hi".rs"#});
        assert_eq!(
            listed(&quoted, &GrepPath::Root),
            Some(listing(&[r#"**/say"hi".rs"#]))
        );
        let below = json!({"pattern": "*.rs", "path": "src"});
        assert_eq!(
            listed(&below, &GrepPath::Directory("src".to_owned())),
            Some(listing(&["src/**/*.rs"]))
        );
    }

    #[test]
    fn a_glob_no_include_list_states_passes() {
        let cases = [
            (json!({"pattern": "/etc/**"}), GrepPath::Root),
            (json!({"pattern": ""}), GrepPath::Root),
            (json!({"pattern": 7}), GrepPath::Root),
            (json!({"path": "src"}), GrepPath::Root),
            (json!({"pattern": "*.rs", "limit": 10}), GrepPath::Root),
            (
                json!({"pattern": "lib/*.rs"}),
                GrepPath::Directory("src".to_owned()),
            ),
            (
                json!({"pattern": "*.rs"}),
                GrepPath::File("src/lib.rs".to_owned()),
            ),
            (json!({"pattern": "*.rs"}), GrepPath::Unmapped),
        ];
        for (input, path) in cases {
            assert_eq!(listed(&input, &path), None, "{input} at {path:?}");
        }
    }

    #[test]
    fn a_find_name_lists_the_files_below_its_start_directory() {
        let find = |directory: &GrepPath, name: &str| {
            GrepRequest::files(vec![name.to_owned()])
                .suggestion(std::slice::from_ref(directory))
                .map(|suggestion| served_search_call(&suggestion))
        };
        assert_eq!(find(&GrepPath::Root, "*.rs"), Some(listing(&["*.rs"])));
        assert_eq!(
            find(&GrepPath::Directory("src".to_owned()), "*.rs"),
            Some(listing(&["src/**/*.rs"]))
        );
        assert_eq!(find(&GrepPath::File("src/lib.rs".to_owned()), "*.rs"), None);
        assert_eq!(find(&GrepPath::Unmapped, "*.rs"), None);
        assert_eq!(find(&GrepPath::Root, r"a\b"), None);
    }

    #[test]
    fn grep_path_is_directory_names_the_root_and_directories() {
        assert!(GrepPath::Root.is_directory());
        assert!(GrepPath::Directory("src".to_owned()).is_directory());
        assert!(!GrepPath::File("src/lib.rs".to_owned()).is_directory());
        assert!(!GrepPath::Unmapped.is_directory());
    }
}
