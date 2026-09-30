//! Reads one `Bash` tool call's command line as the `Grep` or `Glob` call it equals.
//!
//! The default Claude Code tool set lists no `Grep` and no `Glob`, so an agent searches
//! through `Bash` with `grep`, `rg`, or `find`. Steer maps a command only when it is one
//! simple command whose every word it understands: no pipeline, list, substitution,
//! redirection other than a trailing `2>/dev/null`, or unquoted glob, and only flags with a
//! `search` form. Every other command answers `None`, so the call passes.

use super::CURRENT_DIRECTORY;
use super::suggestion::GrepRequest;

/// Paths one mapped command may name; a command naming more passes, so resolving them
/// stays bounded.
const COMMAND_PATHS_MAX: usize = 32;

/// One command line steer maps.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BashSearch {
    /// `grep` or `rg`: the `Grep` request it equals, and the paths it names, each one
    /// `Grep` `path`. A `grep` without `-r` reads the named files alone, so a named
    /// directory makes the command pass.
    Grep {
        request: GrepRequest,
        paths: Vec<String>,
        recursive: bool,
    },
    /// `find DIRECTORY -name NAME`: the `Glob` selection it equals.
    Find { directory: String, name: String },
}

/// The `grep`, `rg`, or `find` search one `command` runs, or `None` when steer cannot state
/// it exactly.
pub(super) fn bash_search(command: &str) -> Option<BashSearch> {
    let words = shell_words(command)?;
    let (program, arguments) = words.split_first()?;
    match program.as_str() {
        "grep" => grep_search(arguments),
        "rg" => rg_search(arguments),
        "find" => find_search(arguments),
        _ => None,
    }
}

/// Splits one simple command into words, undoing quotes. `None` for a newline, an
/// operator, a substitution, an expansion, an unquoted glob character, or an unterminated
/// quote, since each makes the command something other than the words written. A trailing
/// `2>/dev/null` is dropped, since it changes nothing the search answers.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = ShellWord::default();
    let mut characters = command.trim().chars();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' => words.extend(word.take()),
            '\'' => word.single_quoted(&mut characters)?,
            '"' => word.double_quoted(&mut characters)?,
            '\\' => word.escaped(&mut characters)?,
            '\n' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | '{'
            | '~' | '#' => {
                let redirected = word.is_stderr_redirection(character, characters.as_str());
                return redirected.then_some(words);
            }
            other => word.push(other),
        }
    }
    words.extend(word.take());
    Some(words)
}

/// The word [`shell_words`] is building, and whether any of it was quoted.
#[derive(Debug, Default)]
struct ShellWord {
    text: Option<String>,
    quoted: bool,
}

impl ShellWord {
    fn push(&mut self, character: char) {
        self.text.get_or_insert_with(String::new).push(character);
    }

    /// The finished word, leaving this one empty.
    fn take(&mut self) -> Option<String> {
        self.quoted = false;
        self.text.take()
    }

    /// Whether this word, `operator`, and the `rest` of the command line spell the one
    /// redirection steer drops: an unquoted `2`, then `>/dev/null` to the end.
    fn is_stderr_redirection(&self, operator: char, rest: &str) -> bool {
        let descriptor = !self.quoted && self.text.as_deref() == Some("2");
        descriptor && operator == '>' && rest.trim() == "/dev/null"
    }

    /// Reads the character an unquoted backslash escapes, which no longer counts as a
    /// file descriptor. `None` for a line continuation.
    fn escaped(&mut self, characters: &mut std::str::Chars<'_>) -> Option<()> {
        let escaped = characters.next().filter(|escaped| *escaped != '\n')?;
        self.quoted = true;
        self.push(escaped);
        Some(())
    }

    /// Reads a single-quoted span up to its closing quote, every character literal.
    fn single_quoted(&mut self, characters: &mut std::str::Chars<'_>) -> Option<()> {
        self.quoted = true;
        let text = self.text.get_or_insert_with(String::new);
        loop {
            match characters.next()? {
                '\'' => return Some(()),
                inner => text.push(inner),
            }
        }
    }

    /// Reads a double-quoted span up to its closing quote. `None` for an expansion or a
    /// substitution, whose value the command line does not hold, and for a line
    /// continuation.
    fn double_quoted(&mut self, characters: &mut std::str::Chars<'_>) -> Option<()> {
        self.quoted = true;
        let text = self.text.get_or_insert_with(String::new);
        loop {
            match characters.next()? {
                '"' => return Some(()),
                '\\' => push_double_quoted_escape(text, characters.next()?)?,
                '$' | '`' => return None,
                inner => text.push(inner),
            }
        }
    }
}

/// Appends what a backslash inside double quotes leaves of `escaped`: the character alone
/// for `"`, `\`, `$`, and a backtick, both characters for any other. `None` for a line
/// continuation.
fn push_double_quoted_escape(text: &mut String, escaped: char) -> Option<()> {
    match escaped {
        '\n' => return None,
        '"' | '\\' | '$' | '`' => text.push(escaped),
        other => {
            text.push('\\');
            text.push(other);
        }
    }
    Some(())
}

/// The `Grep` request one `grep` argument list equals.
fn grep_search(arguments: &[String]) -> Option<BashSearch> {
    let mut options = GrepOptions::default();
    let positional = positional_words(arguments, &mut options)?;
    let GrepOptions {
        flags,
        dialect,
        recursive,
    } = options;
    let (written, mut paths) = flags.pattern_and_paths(positional)?;
    if paths.is_empty() {
        // Without `-r`, grep reads stdin when no file is named.
        recursive.then_some(())?;
        paths.push(CURRENT_DIRECTORY.to_owned());
    }
    let pattern = dialect.translated(&written)?;
    Some(flags.into_search(pattern, paths, recursive))
}

/// The `Grep` request one `rg` argument list equals. ripgrep reads the `regex` crate's
/// syntax, which `pattern` reads too, and searches directories recursively.
fn rg_search(arguments: &[String]) -> Option<BashSearch> {
    let mut options = RgOptions::default();
    let positional = positional_words(arguments, &mut options)?;
    let RgOptions { mut flags, fixed } = options;
    let (written, mut paths) = flags.pattern_and_paths(positional)?;
    if paths.is_empty() {
        paths.push(CURRENT_DIRECTORY.to_owned());
    }
    if flags.case == CaseFlag::Smart {
        flags.case = if smart_case_insensitive(&written, fixed)? {
            CaseFlag::Ignore
        } else {
            CaseFlag::Sensitive
        };
    }
    let pattern = if fixed {
        regex_syntax::escape(&written)
    } else {
        written
    };
    Some(flags.into_search(pattern, paths, true))
}

/// The words of one command line after its program, read one at a time.
type Words<'a> = std::slice::Iter<'a, String>;

/// One program's option grammar: what each option word does to the search it builds.
trait ProgramOptions {
    /// Reads one long option, without its leading `--`. `words` yields the value an `=`
    /// did not attach. `None` for an option with no `search` form.
    fn long(&mut self, long: &str, words: &mut Words<'_>) -> Option<()>;

    /// Reads one cluster of short options, without its leading `-`. `words` yields a
    /// value the cluster does not hold. `None` for an option with no `search` form.
    fn short(&mut self, cluster: &str, words: &mut Words<'_>) -> Option<()>;
}

/// Reads `arguments` through `options`, answering the positional words: the pattern
/// unless an `-e` named one, then the paths. `None` for an option `options` refuses.
fn positional_words(
    arguments: &[String],
    options: &mut impl ProgramOptions,
) -> Option<Vec<String>> {
    let mut positional = Vec::new();
    let mut options_ended = false;
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        match option(word, options_ended) {
            Word::OptionsEnd => options_ended = true,
            Word::Positional => positional.push(word.clone()),
            Word::Long(long) => options.long(long, &mut words)?,
            Word::Short(cluster) => options.short(cluster, &mut words)?,
        }
    }
    Some(positional)
}

/// `grep`'s options: the flags it shares with `rg`, the pattern's dialect, and whether
/// it searches directories.
#[derive(Debug, Default)]
struct GrepOptions {
    flags: SearchFlags,
    dialect: Dialect,
    recursive: bool,
}

impl ProgramOptions for GrepOptions {
    fn long(&mut self, long: &str, words: &mut Words<'_>) -> Option<()> {
        let (name, attached) = long_value(long);
        let mut value = || attached.clone().or_else(|| words.next().cloned());
        match name {
            "recursive" | "dereference-recursive" => self.recursive = true,
            "extended-regexp" => self.dialect = Dialect::Extended,
            "fixed-strings" => self.dialect = Dialect::Fixed,
            "basic-regexp" => self.dialect = Dialect::Basic,
            "with-filename" | "no-filename" | "no-messages" => {}
            "binary-files" => (value()? == "without-match").then_some(())?,
            "regexp" => self.flags.set_pattern(value()?)?,
            "include" => self.flags.include(value()?)?,
            _ if attached.is_some() => return None,
            _ => self.flags.long(name)?,
        }
        Some(())
    }

    fn short(&mut self, cluster: &str, words: &mut Words<'_>) -> Option<()> {
        let mut flags = cluster.chars();
        while let Some(flag) = flags.next() {
            match flag {
                'r' | 'R' => self.recursive = true,
                'E' => self.dialect = Dialect::Extended,
                'F' => self.dialect = Dialect::Fixed,
                'G' => self.dialect = Dialect::Basic,
                'H' | 'h' | 's' | 'I' => {}
                'e' => self
                    .flags
                    .set_pattern(attached_or_next(&mut flags, words)?)?,
                other => self.flags.short(other)?,
            }
        }
        Some(())
    }
}

/// `rg`'s options: the flags it shares with `grep`, and whether the pattern is a literal
/// string.
#[derive(Debug, Default)]
struct RgOptions {
    flags: SearchFlags,
    fixed: bool,
}

impl ProgramOptions for RgOptions {
    fn long(&mut self, long: &str, words: &mut Words<'_>) -> Option<()> {
        let (name, attached) = long_value(long);
        let mut value = || attached.clone().or_else(|| words.next().cloned());
        match name {
            "fixed-strings" => self.fixed = true,
            "smart-case" => self.flags.case = CaseFlag::Smart,
            "no-heading" | "with-filename" => {}
            "regexp" => self.flags.set_pattern(value()?)?,
            "glob" => self.flags.globs.push(value()?),
            "type" => self.flags.file_type = Some(value()?),
            _ if attached.is_some() => return None,
            _ => self.flags.long(name)?,
        }
        Some(())
    }

    fn short(&mut self, cluster: &str, words: &mut Words<'_>) -> Option<()> {
        let mut flags = cluster.chars();
        while let Some(flag) = flags.next() {
            match flag {
                'F' => self.fixed = true,
                'S' => self.flags.case = CaseFlag::Smart,
                'H' => {}
                'e' => self
                    .flags
                    .set_pattern(attached_or_next(&mut flags, words)?)?,
                'g' => self.flags.globs.push(attached_or_next(&mut flags, words)?),
                't' => self.flags.file_type = Some(attached_or_next(&mut flags, words)?),
                other => self.flags.short(other)?,
            }
        }
        Some(())
    }
}

/// Whether ripgrep's `--smart-case` searches `pattern` case-insensitively: when no
/// uppercase letter is written in it. ripgrep reads the parsed pattern's literals, so an
/// uppercase letter beside an escape or a group, which may spell a class or a flag rather
/// than a literal, answers `None`.
fn smart_case_insensitive(pattern: &str, fixed: bool) -> Option<bool> {
    let uppercase = pattern.chars().any(char::is_uppercase);
    let literal_only = fixed || !pattern.contains(['\\', '(']);
    match (uppercase, literal_only) {
        (false, _) => Some(true),
        (true, true) => Some(false),
        (true, false) => None,
    }
}

/// The `Glob` selection one `find` argument list equals: a start directory, then
/// `-name NAME` and at most `-type f`, in any order.
fn find_search(arguments: &[String]) -> Option<BashSearch> {
    let (directory, tests) = match arguments.split_first() {
        Some((first, tests)) if !first.starts_with('-') => (first.clone(), tests),
        _ => (CURRENT_DIRECTORY.to_owned(), arguments),
    };
    let mut name = None;
    let mut tests = tests.iter();
    while let Some(test) = tests.next() {
        match test.as_str() {
            "-name" => name = Some(tests.next()?.clone()),
            "-type" => (tests.next()? == "f").then_some(())?,
            _ => return None,
        }
    }
    let name = name.filter(|name| !name.contains('/'))?;
    Some(BashSearch::Find { directory, name })
}

/// One argument word, classified.
#[derive(Debug, PartialEq, Eq)]
enum Word<'a> {
    /// `--`, which ends the options.
    OptionsEnd,
    /// A pattern or a path.
    Positional,
    /// A long option, without its leading `--`.
    Long(&'a str),
    /// A cluster of short options, without its leading `-`.
    Short(&'a str),
}

/// Classifies `word`; after `--` every word is positional, and so is a lone `-`.
fn option(word: &str, options_ended: bool) -> Word<'_> {
    match word.strip_prefix('-') {
        _ if options_ended => Word::Positional,
        Some("-") => Word::OptionsEnd,
        None | Some("") => Word::Positional,
        Some(rest) => rest.strip_prefix('-').map_or(Word::Short(rest), Word::Long),
    }
}

/// A long option's name and the value an `=` attaches to it.
fn long_value(long: &str) -> (&str, Option<String>) {
    match long.split_once('=') {
        Some((name, value)) => (name, Some(value.to_owned())),
        None => (long, None),
    }
}

/// A short option's value: the rest of its cluster, or else the next word.
fn attached_or_next(cluster: &mut std::str::Chars<'_>, words: &mut Words<'_>) -> Option<String> {
    let attached: String = cluster.by_ref().collect();
    if attached.is_empty() {
        words.next().cloned()
    } else {
        Some(attached)
    }
}

/// The case flag a command gave last; grep and ripgrep both read the last one given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CaseFlag {
    /// No case flag: the match is case-sensitive.
    #[default]
    Sensitive,
    /// `-i`: the match ignores case.
    Ignore,
    /// `rg -S`: the match ignores case unless the pattern writes an uppercase letter.
    Smart,
}

/// The flags `grep` and `rg` share, spelled as the `Grep` request's fields.
#[derive(Debug, Default)]
struct SearchFlags {
    pattern: Option<String>,
    globs: Vec<String>,
    file_type: Option<String>,
    case: CaseFlag,
    word: bool,
    files_only: bool,
}

impl SearchFlags {
    /// One short flag both programs spell alike. `None` for a flag with no `search` form.
    fn short(&mut self, flag: char) -> Option<()> {
        match flag {
            'n' => {}
            'i' => self.case = CaseFlag::Ignore,
            'w' => self.word = true,
            'l' => self.files_only = true,
            _ => return None,
        }
        Some(())
    }

    /// One long flag both programs spell alike.
    fn long(&mut self, flag: &str) -> Option<()> {
        match flag {
            "line-number" => {}
            "ignore-case" => self.case = CaseFlag::Ignore,
            "word-regexp" => self.word = true,
            "files-with-matches" => self.files_only = true,
            _ => return None,
        }
        Some(())
    }

    /// Records an `-e` pattern. `None` for a second one, which both programs read as an
    /// alternative the request cannot hold apart from the first.
    fn set_pattern(&mut self, pattern: String) -> Option<()> {
        self.pattern.is_none().then(|| self.pattern = Some(pattern))
    }

    /// Records a `grep --include` glob. `None` for a glob holding `/`, since grep matches
    /// `--include` against a file's base name alone.
    fn include(&mut self, glob: String) -> Option<()> {
        (!glob.contains('/')).then(|| self.globs.push(glob))
    }

    /// The pattern and the paths the command names: an `-e` pattern, or else the first
    /// positional word, and the words after it. `None` for no pattern, a pattern spanning
    /// lines, which both programs read as several patterns, and more than
    /// `COMMAND_PATHS_MAX` paths.
    fn pattern_and_paths(&self, mut positional: Vec<String>) -> Option<(String, Vec<String>)> {
        let pattern = match &self.pattern {
            Some(pattern) => pattern.clone(),
            None if positional.is_empty() => return None,
            None => positional.remove(0),
        };
        if pattern.contains('\n') || positional.len() > COMMAND_PATHS_MAX {
            return None;
        }
        Some((pattern, positional))
    }

    /// The search these flags and `pattern`, already in the `regex` crate's syntax, run
    /// over `paths`, with a smart case flag already resolved. `-w` bounds the match as both
    /// programs do: no word character right before it or right after it.
    fn into_search(self, pattern: String, paths: Vec<String>, recursive: bool) -> BashSearch {
        let pattern = if self.word {
            format!(r"\b{{start-half}}(?:{pattern})\b{{end-half}}")
        } else {
            pattern
        };
        let request = GrepRequest {
            globs: self.globs,
            file_type: self.file_type,
            case_insensitive: self.case == CaseFlag::Ignore,
            files_only: self.files_only,
            ..GrepRequest::lines(pattern)
        };
        BashSearch::Grep {
            request,
            paths,
            recursive,
        }
    }
}

/// Which regex dialect a `grep` pattern is written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Dialect {
    /// POSIX basic, grep's default: `\|`, `\(`, `\{`, `\+`, and `\?` are the operators,
    /// and their bare spellings are literal.
    #[default]
    Basic,
    /// POSIX extended, `-E`: the operators are bare, as in the `regex` crate.
    Extended,
    /// `-F`: the pattern is a literal string.
    Fixed,
}

impl Dialect {
    /// `pattern` in the `regex` crate's syntax, or `None` for a construct the two dialects
    /// do not share: a back-reference, an escape the crate reads otherwise, a collating
    /// element, or a basic-dialect anchor away from the pattern's ends. `\<` and `\>` keep
    /// their spelling, which the crate reads as the same word boundaries.
    fn translated(self, pattern: &str) -> Option<String> {
        if self == Self::Fixed {
            return Some(regex_syntax::escape(pattern));
        }
        let basic = self == Self::Basic;
        let characters: Vec<char> = pattern.chars().collect();
        let mut translated = String::with_capacity(pattern.len());
        let mut index = 0;
        while let Some(&character) = characters.get(index) {
            index += 1;
            match character {
                '\\' => {
                    let escaped = *characters.get(index)?;
                    index += 1;
                    translated.push_str(&escape_translated(escaped, basic)?);
                }
                '|' | '(' | ')' | '{' | '}' | '+' | '?' if basic => {
                    translated.push('\\');
                    translated.push(character);
                }
                '^' if basic && index != 1 => return None,
                '$' if basic && index != characters.len() => return None,
                '[' => index = bracket_translated(&characters, index, &mut translated)?,
                other => translated.push(other),
            }
        }
        Some(translated)
    }
}

/// One backslash escape of a `grep` pattern in the `regex` crate's syntax. `None` for an
/// escape the two read differently, such as a back-reference.
fn escape_translated(escaped: char, basic: bool) -> Option<String> {
    match escaped {
        '|' | '(' | ')' | '{' | '}' | '+' | '?' if basic => Some(escaped.to_string()),
        '/' => Some("/".to_owned()),
        'w' | 'W' | 's' | 'S' | 'b' | 'B' | '<' | '>' | '.' | '*' | '[' | ']' | '^' | '$'
        | '\\' | '|' | '(' | ')' | '{' | '}' | '+' | '?' => Some(format!("\\{escaped}")),
        _ => None,
    }
}

/// Translates one bracket expression whose `[` sits just before `index`, appending it to
/// `translated` and answering the index past its `]`. A POSIX class such as `[:alpha:]`
/// keeps its spelling; a backslash, `[`, `&`, or `~`, literal inside POSIX brackets, is
/// escaped, since the `regex` crate reads them as escapes, nested classes, and set
/// operators. `None` for an unterminated bracket, a collating element or equivalence
/// class, and a `--`, which the crate reads as set difference.
fn bracket_translated(
    characters: &[char],
    mut index: usize,
    translated: &mut String,
) -> Option<usize> {
    translated.push('[');
    let mut first = true;
    loop {
        let inner = *characters.get(index)?;
        index += 1;
        let next = characters.get(index).copied();
        match (inner, next) {
            (']', _) if !first => break,
            ('^', _) if first => {
                translated.push('^');
                continue;
            }
            ('[', Some(':')) => {
                let close = characters[index..]
                    .windows(2)
                    .position(|pair| pair == [':', ']'])?;
                translated.push('[');
                translated.extend(&characters[index..index + close + 2]);
                index += close + 2;
            }
            ('[', Some('.' | '=')) | ('-', Some('-')) => return None,
            ('\\' | '[' | '&' | '~', _) => {
                translated.push('\\');
                translated.push(inner);
            }
            (other, _) => translated.push(other),
        }
        first = false;
    }
    translated.push(']');
    Some(index)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::suggestion::GrepRequest;
    use super::{BashSearch, COMMAND_PATHS_MAX, bash_search, shell_words};

    /// The search a recursive command equals: `input` read as a `Grep` call's input, over
    /// `paths`; `None` when the input does not read as one.
    fn grep(input: &Value, paths: &[&str]) -> Option<BashSearch> {
        searched(input, paths, true)
    }

    /// As [`grep`], for a `grep` without `-r`, which reads its named files alone.
    fn files(input: &Value, paths: &[&str]) -> Option<BashSearch> {
        searched(input, paths, false)
    }

    fn searched(input: &Value, paths: &[&str], recursive: bool) -> Option<BashSearch> {
        let request = GrepRequest::from_input(input);
        assert!(
            request.is_some(),
            "an expected input must read as Grep: {input}"
        );
        request.map(|request| BashSearch::Grep {
            request,
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            recursive,
        })
    }

    /// Commands Claude Code agents ran through `Bash` to search a repository, the ones
    /// that map: all verbatim but the `find`, recorded with a trailing `| head -20`.
    #[test]
    fn recorded_simple_commands_map_to_their_grep_input() {
        assert_eq!(
            bash_search(r#"grep -rn "cfg(test)" src/ examples/"#),
            grep(
                &json!({"pattern": r"cfg\(test\)", "output_mode": "content"}),
                &["src/", "examples/"]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rn "split_identifier_words" . 2>/dev/null"#),
            grep(
                &json!({"pattern": "split_identifier_words", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rn "CORPUS_TOKENIZER" . 2>/dev/null"#),
            grep(
                &json!({"pattern": "CORPUS_TOKENIZER", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(
                r#"grep -r "CORPUS_TOKENIZER" --include="*.py" --include="*.rs" --include="*.ts" --include="*.js" --include="*.go" -n"#
            ),
            grep(
                &json!({
                    "pattern": "CORPUS_TOKENIZER",
                    "glob": "*.py,*.rs,*.ts,*.js,*.go",
                    "output_mode": "content"
                }),
                &["."]
            )
        );
        assert_eq!(
            bash_search(
                r#"grep -n "mod tests\|#\[cfg(test)\]" src/fusion.rs src/query.rs src/memory.rs"#
            ),
            files(
                &json!({"pattern": r"mod tests|#\[cfg\(test\)\]", "output_mode": "content"}),
                &["src/fusion.rs", "src/query.rs", "src/memory.rs"]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rn "impl.*Display\|impl.*Error" --include="*.rs" ."#),
            grep(
                &json!({"pattern": "impl.*Display|impl.*Error", "glob": "*.rs", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rn "Result<Self, RankingError>" --include="*.rs" ."#),
            grep(
                &json!({"pattern": "Result<Self, RankingError>", "glob": "*.rs", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rn "^impl.*Error" --include="*.rs" ."#),
            grep(
                &json!({"pattern": "^impl.*Error", "glob": "*.rs", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(
                r#"grep -n "fn main\|#\[test\]\|#\[tokio::test\]" examples/external_consumer.rs"#
            ),
            files(
                &json!({"pattern": r"fn main|#\[test\]|#\[tokio::test\]", "output_mode": "content"}),
                &["examples/external_consumer.rs"]
            )
        );
        assert_eq!(
            bash_search(r#"find . -type f -name "*.rs""#),
            Some(BashSearch::Find {
                directory: ".".to_owned(),
                name: "*.rs".to_owned(),
            })
        );
    }

    /// Commands that pass: five agents ran verbatim, then the forms steer cannot state
    /// exactly: lists, loops, expansions, unmapped flags, back-references, stdin, several
    /// patterns, and a `find` that is no plain name selection.
    #[test]
    fn pipelines_lists_and_unmapped_flags_pass() {
        for command in [
            r#"grep -rn "tokenize(" --include="*.rs" | grep -v "fn tokenize""#,
            r#"find . -type f -name "*.rs" | head -20"#,
            r#"grep -n "^mod tests" src/*.rs"#,
            r#"grep -B 5 -A 5 "fuse(" src/memory.rs"#,
            "for f in src/*.rs; do grep -n x \"$f\"; done",
            r#"grep -rn "x" . ; echo done"#,
            r#"grep -rn "$PATTERN" ."#,
            r#"grep -rn "x" . 2>&1"#,
            r#"grep -rn "x" . >out.txt"#,
            r#"grep -c "x" src/lib.rs"#,
            r#"grep -rn "\(a\)\1" ."#,
            r#"grep "x""#,
            r"grep -rn -e a -e b .",
            r"grep -rn --include='src/*.rs' x .",
            r"grep -rn --color=always x .",
            r"grep -rn --binary-files=text x .",
            r#"grep -rn "a\d" ."#,
            r#"grep -rn "a^b" ."#,
            r"grep -rn 'a$b' .",
            r#"grep -rn "[[.a.]]" ."#,
            r#"grep -rn "[a--b]" ."#,
            r#"grep -rn "[abc" ."#,
            r#"grep -rn "x\"#,
            "grep -rn \"a\nb\" .",
            "grep -rn x \\\n .",
            "grep -rn 'x",
            r#"grep -rn "x\
y" ."#,
            r#"rg -U "a\nb""#,
            r"rg --max-count=3 x",
            r"rg -r y x",
            r#"rg -S "\WFoo""#,
            r#"find /etc -name "*.conf" -o -name "*.toml""#,
            r#"find . -name "*.rs" -o -name "*.toml""#,
            r"find . -type d -name src",
            r"find . -name src/lib.rs",
            r"find . -type f",
            r"sed -n 1p src/lib.rs",
            "",
        ] {
            assert_eq!(bash_search(command), None, "{command}");
        }
    }

    #[test]
    fn rg_reads_the_regex_crate_syntax_and_maps_its_flags() {
        assert_eq!(
            bash_search(r#"rg -n "fn\s+parse_\w+" -t rust crates"#),
            grep(
                &json!({"pattern": r"fn\s+parse_\w+", "type": "rust", "output_mode": "content"}),
                &["crates"]
            )
        );
        assert_eq!(
            bash_search("rg -S websocket -g '*.ts' -l"),
            grep(
                &json!({"pattern": "websocket", "glob": "*.ts", "-i": true}),
                &["."]
            )
        );
        assert_eq!(
            bash_search("rg -F 'a.b(' src"),
            grep(
                &json!({"pattern": r"a\.b\(", "output_mode": "content"}),
                &["src"]
            )
        );
        assert_eq!(
            bash_search("rg --smart-case --no-heading --type=py --regexp=Load -- src"),
            grep(
                &json!({"pattern": "Load", "type": "py", "output_mode": "content"}),
                &["src"]
            )
        );
        assert_eq!(
            bash_search("rg -wl -tpy -e load"),
            grep(
                &json!({
                    "pattern": r"\b{start-half}(?:load)\b{end-half}",
                    "type": "py"
                }),
                &["."]
            )
        );
    }

    #[test]
    fn rg_takes_the_flags_it_shares_with_grep_and_ignores_with_filename() {
        assert_eq!(
            bash_search("rg --ignore-case --word-regexp -H Load src"),
            grep(
                &json!({
                    "pattern": r"\b{start-half}(?:Load)\b{end-half}",
                    "-i": true,
                    "output_mode": "content"
                }),
                &["src"]
            )
        );
    }

    #[test]
    fn the_last_case_flag_wins_for_rg() {
        assert_eq!(
            bash_search("rg -S -i Load"),
            grep(
                &json!({"pattern": "Load", "-i": true, "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search("rg -i -S Load"),
            grep(
                &json!({"pattern": "Load", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search("rg -SF 'Foo('"),
            grep(
                &json!({"pattern": r"Foo\(", "output_mode": "content"}),
                &["."]
            )
        );
    }

    #[test]
    fn grep_dialects_translate_to_the_regex_crate_syntax() {
        assert_eq!(
            bash_search(r#"grep -rE "TODO|FIXME" ."#),
            grep(
                &json!({"pattern": "TODO|FIXME", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rwi "\(foo\)\{2\}" ."#),
            grep(
                &json!({
                    "pattern": r"\b{start-half}(?:(foo){2})\b{end-half}",
                    "-i": true,
                    "output_mode": "content"
                }),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -r "[[:alpha:]_]\+x[\]" ."#),
            grep(
                &json!({"pattern": r"[[:alpha:]_]+x[\\]", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -r "\<word\>|a+b?" ."#),
            grep(
                &json!({"pattern": r"\<word\>\|a\+b\?", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r#"grep -rF -e "a.b(" --regexp=other ."#),
            None,
            "a second pattern passes"
        );
        assert_eq!(
            bash_search(r#"grep -rF -e "a.b(" ."#),
            grep(
                &json!({"pattern": r"a\.b\(", "output_mode": "content"}),
                &["."]
            )
        );
        assert_eq!(
            bash_search(r"grep -r 'a\/b' ."),
            grep(&json!({"pattern": "a/b", "output_mode": "content"}), &["."])
        );
        assert_eq!(
            bash_search(r#"grep -rG "[^]a&~]" ."#),
            grep(
                &json!({"pattern": r"[^]a\&\~]", "output_mode": "content"}),
                &["."]
            )
        );
    }

    #[test]
    fn grep_long_forms_map_like_their_short_flags() {
        assert_eq!(
            bash_search(
                "grep --recursive --line-number --ignore-case --files-with-matches \
                 --with-filename --no-messages --binary-files=without-match \
                 --extended-regexp --include '*.rs' --regexp 'a|b' src"
            ),
            grep(
                &json!({"pattern": "a|b", "glob": "*.rs", "-i": true}),
                &["src"]
            )
        );
        assert_eq!(
            bash_search("grep --word-regexp -Rh x ."),
            grep(
                &json!({
                    "pattern": r"\b{start-half}(?:x)\b{end-half}",
                    "output_mode": "content"
                }),
                &["."]
            )
        );
        assert_eq!(bash_search("grep --count -r x ."), None);
    }

    #[test]
    fn a_command_naming_too_many_paths_passes() {
        let paths: Vec<String> = (0..=COMMAND_PATHS_MAX)
            .map(|index| format!("f{index}"))
            .collect();
        let command = format!("grep -n x {}", paths.join(" "));
        assert_eq!(bash_search(&command), None);
        let command = format!("grep -n x {}", paths[1..].join(" "));
        assert!(bash_search(&command).is_some());
    }

    #[test]
    fn shell_words_undo_quotes_and_drop_a_trailing_stderr_redirection() {
        assert_eq!(
            shell_words(r#"grep -rn 'a b' "c\"d\$" e\ f 2>/dev/null"#),
            Some(vec![
                "grep".to_owned(),
                "-rn".to_owned(),
                "a b".to_owned(),
                "c\"d$".to_owned(),
                "e f".to_owned(),
            ])
        );
        assert_eq!(shell_words(r#"grep x "2">/dev/null"#), None);
        assert_eq!(shell_words("grep x 2>/dev/null extra"), None);
        assert_eq!(
            shell_words(r#"grep "\q""#),
            Some(vec!["grep".to_owned(), r"\q".to_owned()])
        );
        assert_eq!(shell_words(r#"grep "x"#), None);
        assert_eq!(shell_words(r#"grep "`id`""#), None);
    }

    #[test]
    fn find_takes_its_start_directory_or_the_current_one() {
        assert_eq!(
            bash_search(r#"find src -name "*.rs" -type f"#),
            Some(BashSearch::Find {
                directory: "src".to_owned(),
                name: "*.rs".to_owned(),
            })
        );
        assert_eq!(
            bash_search(r"find -name Cargo.toml"),
            Some(BashSearch::Find {
                directory: ".".to_owned(),
                name: "Cargo.toml".to_owned(),
            })
        );
        assert_eq!(bash_search("find src -name"), None);
    }
}
