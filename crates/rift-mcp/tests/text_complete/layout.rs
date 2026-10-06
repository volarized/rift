//! Check of what the compact text of a `search`, `get_symbol`, or `nodes` answer promises.
//!
//! The text is a curated layout, so it does not state every field of the structured answer: the
//! normal state is implicit. This check asserts only what `crates/rift-mcp/src/output/render.rs`
//! and `render/{layout,facts,search,symbol,nodes,warning}.rs` write for the exceptional state, and
//! never asserts the absence of other text.
//!
//! The text is a sequence of counted sections. A section is a title at column 0, then its
//! entries indented by one level. Any non-empty line at column 0 is a title. A level is one tab.
//!
//! - Items section, always first: the title is `N result` or `N results` (`N node` or `N nodes`
//!   for `nodes`), then ` · page P/T` when `total_pages > 1` or the page lies past the end. A
//!   `nodes` answer has no page. No blank line follows the title or separates two items.
//! - Warnings section, second and only when the answer has warnings: the title `N warning` or
//!   `N warnings`, then one line per warning at one level that opens with its `code`. Without
//!   warnings the text has no second section.
//! - One item has no marker and indents every line by one level. Several items each start with
//!   `[n] ` at one level and indent their further lines by two.
//! - Per `search` hit, in order: the location, every `matched_by` name, and `score <value>` on the
//!   location line; the symbol, node, or documented-symbol identity as a line; the returned source
//!   line by line. A commit hit states its revision cut to 8 characters, author, subject, paths,
//!   and both truncation flags.
//! - A `search` hit whose target is `symbol` and whose `traversal_path` has a hop is a walk hit.
//!   It is no item: the walk hits form a tree after the items, checked by [`walk`]. The items are
//!   the other hits, and only they take markers. A blank line separates the last item from the
//!   first root.
//! - Per `get_symbol` hit: the location line, the symbol identity, every history version, the
//!   `history (incomplete):` label exactly when the history is incomplete, and the source.
//!   A single-item answer ends its items section with its source.
//! - Per `nodes` entry: the kind on the head line, ` · <L> lines` exactly when the excerpt has
//!   more than one line, `generated` and `test` when the facet holds, the node identity on the
//!   next line, the symbol identity and the regions line when present. The innermost excerpt ends
//!   the items section, line by line, after one blank line; outer excerpts are not checked.

mod walk;

use serde_json::Value;

use walk::Plan;

/// Text of one indent level. Every indent of the text is a run of this unit; the text an answer
/// copies keeps its own whitespace after the indent.
const INDENT_UNIT: &str = "\t";
/// Levels that indent an entry of a section.
const SECTION_LEVELS: usize = 1;
/// Levels, past the section level, that indent the further lines of an item among several.
const MARKER_LEVELS: usize = 1;
/// Separates the facts of one line.
const SEPARATOR: &str = " · ";
/// Characters of a hash that stay when it is cut.
const HASH_CUT: usize = 8;
/// Relative difference under which a score token states a score.
const SCORE_TOLERANCE: f64 = 1e-6;
/// Label of the history section of a complete history.
const HISTORY_LABEL: &str = "history:";
/// Label of the history section of an incomplete history.
const HISTORY_INCOMPLETE_LABEL: &str = "history (incomplete):";
/// Line after the message of a cut commit.
const MESSAGE_TRUNCATED_LINE: &str = "message truncated";
/// Tail of the label of a cut path list.
const PATHS_TRUNCATED_TAIL: &str = "+ paths · truncated";
/// Noun counted by the title of the warnings section.
const WARNING_NOUN: &str = "warning";
/// Facets of a node that the head line states.
const NODE_FACT_FACETS: [&str; 2] = ["generated", "test"];

/// The tool whose answer the text states.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Tool {
    Search,
    GetSymbol,
    Nodes,
}

impl Tool {
    /// The key of the root array that holds the items.
    fn items_key(self) -> &'static str {
        match self {
            Self::Search => "results",
            Self::GetSymbol => "hits",
            Self::Nodes => "nodes",
        }
    }

    /// What the title of the items section counts.
    fn noun(self) -> &'static str {
        match self {
            Self::Search | Self::GetSymbol => "result",
            Self::Nodes => "node",
        }
    }
}

/// One section of the text: its title line and the lines under it.
struct Section<'a> {
    title: &'a str,
    entries: Vec<&'a str>,
}

/// Splits the text into sections. Any non-empty line at column 0 opens a section.
fn sections_of(text: &str) -> Result<Vec<Section<'_>>, String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut sections: Vec<Section<'_>> = Vec::new();
    for line in lines {
        if !line.is_empty() && !line.starts_with(INDENT_UNIT) {
            sections.push(Section {
                title: line,
                entries: Vec::new(),
            });
        } else if let Some(section) = sections.last_mut() {
            section.entries.push(line);
        } else {
            return Err(format!(
                "$: the first line must be a title at column 0, got `{line}`"
            ));
        }
    }
    Ok(sections)
}

/// The count of `count` things named `noun`, in the singular for one.
fn counted(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// Checks `text` against what the layout promises for the answer `structured`.
///
/// The failure names the JSON path of the first fact the text does not state.
pub(super) fn text_states(tool: Tool, text: &str, structured: &Value) -> Result<(), String> {
    let key = tool.items_key();
    let items = structured
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("$.{key}: must be an array"))?;
    let sections = sections_of(text)?;
    let Some((first, rest)) = sections.split_first() else {
        return Err("$: the text is empty".to_owned());
    };
    title_states(first.title, items.len(), tool, structured)?;
    warnings_states(rest, structured.get("warnings"))?;
    items_states(tool, &first.entries, items, structured)
}

/// Checks the items section: its markers, indents, blank lines, every item, and the walk tree.
fn items_states(
    tool: Tool,
    entries: &[&str],
    items: &[Value],
    structured: &Value,
) -> Result<(), String> {
    let key = tool.items_key();
    require(entries.first() != Some(&""), &format!("$.{key}"), || {
        "no blank line may follow the title".to_owned()
    })?;
    let plan = Plan::of(items, tool == Tool::Search)?;
    let (item_entries, nodes) = plan.split(entries)?;
    let listed = plan.outside();
    let chunks = chunk_items(&item_entries, listed.len(), key)?;
    for ((index, hit), chunk) in listed.iter().zip(&chunks) {
        let at = format!("$.{key}[{index}]");
        let may_end_blank = may_end_blank(tool, hit, structured, *index, items.len());
        require(
            may_end_blank || chunk.len() < 2 || chunk.last() != Some(&""),
            &at,
            || "a blank line may not end an item, before the next item or section".to_owned(),
        )?;
        let item = Item::new(chunk, listed.len() > 1, &at)?;
        match tool {
            Tool::Search => search_hit(&item, hit, &at)?,
            Tool::GetSymbol => symbol_hit(&item, hit, &at)?,
            Tool::Nodes => node_entry(&item, hit, structured, *index, &at)?,
        }
    }
    plan.states(&nodes)?;
    match tool {
        Tool::Search => Ok(()),
        Tool::GetSymbol => single_symbol_ends_with_source(entries, items),
        Tool::Nodes => innermost_ends_section(entries, structured, items.len()),
    }
}

/// Whether the text of item `index` may end in an empty line.
///
/// That is the case when the source the item writes last is empty or ends in a line feed. A
/// `nodes` answer writes only the innermost excerpt, so only its last item may.
fn may_end_blank(tool: Tool, hit: &Value, structured: &Value, index: usize, count: usize) -> bool {
    let source = match tool {
        Tool::Nodes if index + 1 == count => structured.pointer(&format!("/source/{index}")),
        Tool::Nodes => None,
        Tool::Search | Tool::GetSymbol => hit.get("source"),
    };
    source_may_end_blank(source)
}

/// Whether a hit with this `source` may end in an empty line: the source is empty or ends in a
/// line feed.
fn source_may_end_blank(source: Option<&Value>) -> bool {
    source
        .and_then(Value::as_str)
        .is_some_and(|source| source.is_empty() || source.ends_with('\n'))
}

/// Whether `entries` end with `expected`.
fn ends_with_lines(entries: &[&str], expected: &[String]) -> bool {
    entries.len() >= expected.len()
        && entries
            .iter()
            .skip(entries.len() - expected.len())
            .zip(expected)
            .all(|(line, wanted)| *line == wanted.as_str())
}

/// `levels` indent levels as text.
fn indent(levels: usize) -> String {
    INDENT_UNIT.repeat(levels)
}

/// The lines of `text` as an item writes them, `levels` deep; empty lines stay empty.
fn indented_lines(text: &str, levels: usize) -> Vec<String> {
    text.split('\n')
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{}{line}", indent(levels))
            }
        })
        .collect()
}

/// Levels that indent the further lines of an item, among several or alone.
fn item_indent(several: bool) -> usize {
    if several {
        SECTION_LEVELS + MARKER_LEVELS
    } else {
        SECTION_LEVELS
    }
}

/// A single `get_symbol` hit writes its source last, so the items section ends with it.
fn single_symbol_ends_with_source(entries: &[&str], items: &[Value]) -> Result<(), String> {
    let source = match items {
        [hit] => hit.get("source").and_then(Value::as_str),
        _ => None,
    };
    match source {
        Some(source) if !ends_with_lines(entries, &indented_lines(source, item_indent(false))) => {
            Err("$.hits[0].source: the items section must end with the source".to_owned())
        }
        _ => Ok(()),
    }
}

/// Requires `holds`, else the failure `at: what`.
fn require(holds: bool, at: &str, what: impl FnOnce() -> String) -> Result<(), String> {
    if holds {
        Ok(())
    } else {
        Err(format!("{at}: {}", what()))
    }
}

/// Checks the count and, for a paged tool, the page of the title of the items section.
fn title_states(title: &str, count: usize, tool: Tool, structured: &Value) -> Result<(), String> {
    let counted = counted(count, tool.noun());
    let mut facts = title.split(SEPARATOR);
    require(
        facts.next() == Some(counted.as_str()),
        &format!("$.{}", tool.items_key()),
        || format!("the title must open with `{counted}`, got `{title}`"),
    )?;
    if tool == Tool::Nodes {
        return require(
            facts.next().is_none(),
            &format!("$.{}", tool.items_key()),
            || format!("a nodes title states no page, got `{title}`"),
        );
    }
    let number = |name: &str| {
        structured
            .pointer(&format!("/pagination/{name}"))
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("$.pagination.{name}: must be an integer"))
    };
    let (page_index, total_pages) = (number("page_index")?, number("total_pages")?);
    let past_the_end = total_pages > 0 && page_index >= total_pages;
    if total_pages <= 1 && !past_the_end {
        return Ok(());
    }
    let page = format!("page {}/{total_pages}", page_index + 1);
    require(facts.any(|fact| fact == page), "$.pagination", || {
        format!("the title must state `{page}`, got `{title}`")
    })
}

/// Checks the sections after the items: one warnings section exactly when there are warnings.
fn warnings_states(rest: &[Section<'_>], warnings: Option<&Value>) -> Result<(), String> {
    let mut codes = Vec::new();
    for (index, warning) in warnings
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let at = format!("$.warnings[{index}].code");
        codes.push(
            warning
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{at}: must be a string"))?,
        );
    }
    match (codes.is_empty(), rest) {
        (true, []) => Ok(()),
        (true, [extra, ..]) => Err(format!(
            "$.warnings: the answer has no warnings, but the text has the section `{}`",
            extra.title
        )),
        (false, []) => Err(format!(
            "$.warnings[0].code: the answer has {} warnings, but the text has no warnings section",
            codes.len()
        )),
        (false, [section]) => warning_section_states(section, &codes),
        (false, [_, extra, ..]) => Err(format!(
            "$.warnings: unexpected section `{}` after the warnings",
            extra.title
        )),
    }
}

/// Checks the warnings section: its title count, its entries, and each code in order.
///
/// The code ends the line, or is followed by ` · ` for evidence or `:` for the detail.
fn warning_section_states(section: &Section<'_>, codes: &[&str]) -> Result<(), String> {
    let counted = counted(codes.len(), WARNING_NOUN);
    require(section.title == counted, "$.warnings", || {
        format!(
            "the second section must be titled `{counted}`, got `{}`",
            section.title
        )
    })?;
    require(section.entries.len() == codes.len(), "$.warnings", || {
        format!(
            "the title counts {} but the section holds {} lines",
            codes.len(),
            section.entries.len()
        )
    })?;
    for (index, (code, line)) in codes.iter().zip(&section.entries).enumerate() {
        let at = format!("$.warnings[{index}].code");
        let entry = line
            .strip_prefix(INDENT_UNIT)
            .filter(|entry| !entry.starts_with(INDENT_UNIT))
            .ok_or_else(|| format!("{at}: the line `{line}` must be indented by one level"))?;
        let stated = entry
            .strip_prefix(code)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with([' ', ':']));
        require(stated, &at, || {
            format!("the line must open with `{code}`, got `{entry}`")
        })?;
    }
    Ok(())
}

/// The lines of one item, indent removed.
struct Item<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Item<'a> {
    /// The item of `chunk`, whose head has no marker left when `several`.
    ///
    /// Every further non-empty line must carry the item indent.
    fn new(chunk: &[&'a str], several: bool, at: &str) -> Result<Self, String> {
        let levels = item_indent(several);
        let mut lines = Vec::with_capacity(chunk.len());
        for (index, line) in chunk.iter().enumerate() {
            if line.is_empty() || (several && index == 0) {
                lines.push(*line);
                continue;
            }
            let plain = line.strip_prefix(indent(levels).as_str()).ok_or_else(|| {
                format!("{at}: the line `{line}` must be indented by {levels} levels")
            })?;
            lines.push(plain);
        }
        Ok(Self { lines })
    }

    /// The item of the lines of a tree node, whose `head` has its indent and lead removed.
    ///
    /// Every further non-empty line must carry `levels` levels.
    fn node(chunk: &[&'a str], head: &'a str, levels: usize, at: &str) -> Result<Self, String> {
        let mut lines = vec![head];
        for line in chunk.iter().skip(1) {
            if line.is_empty() {
                lines.push(*line);
                continue;
            }
            let plain = line.strip_prefix(indent(levels).as_str()).ok_or_else(|| {
                format!("{at}: the line `{line}` must be indented by {levels} levels")
            })?;
            lines.push(plain);
        }
        Ok(Self { lines })
    }

    /// Line `index`, or an empty line when there is none.
    fn plain(&self, index: usize) -> &'a str {
        self.lines.get(index).copied().unwrap_or_default()
    }

    /// Every line.
    fn plain_lines(&self) -> Vec<&'a str> {
        self.lines.clone()
    }

    /// Whether a line equals `wanted`.
    fn has_line(&self, wanted: &str) -> bool {
        self.lines.contains(&wanted)
    }
}

/// What follows the marker `  [n]` on a line that opens an item, or `None` for any other line.
///
/// An item whose first line is empty writes the marker alone.
fn after_marker(line: &str, number: usize) -> Option<&str> {
    let rest = line
        .strip_prefix(INDENT_UNIT)?
        .strip_prefix(&format!("[{number}]"))?;
    match rest.strip_prefix(' ') {
        Some(first) => Some(first),
        None if rest.is_empty() => Some(rest),
        None => None,
    }
}

/// Splits the entries of the items section into one slice of lines per item.
///
/// Several items start at the lines `  [1] `, `  [2] `, and so on, in order. The head of each
/// has its marker removed. One item holds every entry.
fn chunk_items<'a>(
    entries: &[&'a str],
    count: usize,
    key: &str,
) -> Result<Vec<Vec<&'a str>>, String> {
    match count {
        0 => {
            require(entries.is_empty(), &format!("$.{key}"), || {
                "the title counts 0 but the section holds entries".to_owned()
            })?;
            return Ok(Vec::new());
        }
        1 => return Ok(vec![entries.to_vec()]),
        _ => {}
    }
    let mut starts = Vec::with_capacity(count);
    let mut from = 0;
    for number in 1..=count {
        let found = entries
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, line)| after_marker(line, number).is_some());
        let (at, _) = found.ok_or_else(|| {
            format!(
                "$.{key}[{}]: no line starts with `  [{number}]`",
                number - 1
            )
        })?;
        starts.push(at);
        from = at + 1;
    }
    require(starts.first() == Some(&0), &format!("$.{key}[0]"), || {
        "the first entry must start with `  [1]`".to_owned()
    })?;
    let stray = count + 1;
    require(
        !entries
            .iter()
            .skip(from)
            .any(|line| after_marker(line, stray).is_some()),
        &format!("$.{key}"),
        || format!("the title counts {count} but the section holds an item `[{stray}]`"),
    )?;
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .copied()
        .chain([entries.len()])
        .collect();
    Ok(starts
        .iter()
        .zip(ends)
        .enumerate()
        .map(|(index, (start, end))| {
            let mut lines = entries.get(*start..end).unwrap_or_default().to_vec();
            if let Some(first) = lines.first_mut() {
                *first = after_marker(first, index + 1).unwrap_or(first);
            }
            lines
        })
        .collect())
}

/// The string at the JSON pointer.
fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer)?.as_str()
}

/// `place:line`, or the place alone without a line. A path wins over a unit.
fn place_of(path: Option<&str>, unit: Option<&str>, line: Option<u64>) -> Option<String> {
    let place = path.or(unit)?;
    Some(match line {
        Some(line) => format!("{place}:{line}"),
        None => place.to_owned(),
    })
}

/// A single-line fact as the layout writes it: control characters become escapes.
fn visible(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' => "\\n".to_owned(),
            '\t' => "\\t".to_owned(),
            '\r' => "\\r".to_owned(),
            other if other.is_control() => format!("\\u{{{:x}}}", u32::from(other)),
            other => other.to_string(),
        })
        .collect()
}

/// The first 8 characters of lowercase hex of at least 8 characters, else the text itself.
fn cut_hash(text: &str) -> &str {
    let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    if text.len() >= HASH_CUT && text.bytes().all(hex) {
        text.get(..HASH_CUT).unwrap_or(text)
    } else {
        text
    }
}

/// Checks one `search` hit against its lines.
fn search_hit(item: &Item<'_>, hit: &Value, at: &str) -> Result<(), String> {
    let target =
        text_at(hit, "/hit/target").ok_or_else(|| format!("{at}.hit.target: must be a string"))?;
    let facts_index = usize::from(matches!(target, "symbol" | "documentation"));
    let facts: Vec<&str> = item.plain(facts_index).split(SEPARATOR).collect();
    score_states(&facts, hit, at)?;
    if target == "commit" {
        commit_states(item, &facts, hit, at)?;
    } else {
        located_states(item, &facts, hit, target, at, None)?;
    }
    source_states(item, hit.get("source"), at)
}

/// Checks `score <value>` among the facts of the location line.
///
/// `distance` is not written: the depth of a node in the tree shows it.
fn score_states(facts: &[&str], hit: &Value, at: &str) -> Result<(), String> {
    if let Some(score) = hit.get("score").and_then(Value::as_f64) {
        let stated = facts
            .iter()
            .filter_map(|fact| fact.strip_prefix("score "))
            .filter_map(|value| value.parse::<f64>().ok())
            .any(|value| (value - score).abs() <= SCORE_TOLERANCE * score.abs().max(value.abs()));
        require(stated, &format!("{at}.score"), || {
            format!("no `score {score}` fact on the location line {facts:?}")
        })?;
    }
    Ok(())
}

/// The location a documentation hit writes: its block's source identity and line.
fn documentation_place(hit: &Value) -> Option<String> {
    let source = "/hit/documentation/block/source/source";
    let path = text_at(hit, &format!("{source}/path"));
    let unit = text_at(hit, &format!("{source}/unit"));
    let line = hit
        .pointer("/hit/documentation/block/line")
        .and_then(Value::as_u64);
    place_of(path, unit, line)
}

/// Checks the location, the matched fields, and the identity of a hit that is not a commit.
///
/// The matched field `left_out`, when there is one, is not written: the facts must not name it.
fn located_states(
    item: &Item<'_>,
    facts: &[&str],
    hit: &Value,
    target: &str,
    at: &str,
    left_out: Option<&str>,
) -> Result<(), String> {
    let place = if target == "documentation" {
        documentation_place(hit)
    } else {
        place_of(
            text_at(hit, "/path"),
            text_at(hit, "/unit"),
            hit.get("line").and_then(Value::as_u64),
        )
    };
    if let Some(place) = place {
        require(facts.contains(&place.as_str()), at, || {
            format!("no `{place}` fact on the location line {facts:?}")
        })?;
    }
    let names: Vec<&str> = facts.iter().flat_map(|fact| fact.split(", ")).collect();
    let matched = hit.get("matched_by").and_then(Value::as_array);
    if let Some(left_out) = left_out {
        require(!names.contains(&left_out), at, || {
            format!("`{left_out}` must not be on the location line {facts:?}")
        })?;
    }
    for (index, name) in matched
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .enumerate()
        .filter(|(_, name)| Some(*name) != left_out)
    {
        require(
            names.contains(&name),
            &format!("{at}.matched_by[{index}]"),
            || format!("no `{name}` on the location line {facts:?}"),
        )?;
    }
    let identity = match target {
        "symbol" => text_at(hit, "/hit/symbol/id"),
        "node" => text_at(hit, "/hit/node"),
        "documentation" => text_at(hit, "/hit/documentation/block/symbol"),
        _ => None,
    };
    match identity {
        Some(id) => require(item.has_line(id), &format!("{at}.hit"), || {
            format!("no line equals the identity `{id}`")
        }),
        None => Ok(()),
    }
}

/// Checks that the returned source appears line by line, at the indent of the item.
///
/// An empty source has no line to check.
fn source_states(item: &Item<'_>, source: Option<&Value>, at: &str) -> Result<(), String> {
    let Some(source) = source
        .and_then(Value::as_str)
        .filter(|source| !source.is_empty())
    else {
        return Ok(());
    };
    let expected: Vec<&str> = source.split('\n').collect();
    let found = item
        .lines
        .windows(expected.len())
        .any(|window| window == expected);
    require(found, &format!("{at}.source"), || {
        format!("the source does not appear line by line: {source:?}")
    })
}

/// Checks the revision, author, message, and paths of a commit hit.
fn commit_states(item: &Item<'_>, facts: &[&str], hit: &Value, at: &str) -> Result<(), String> {
    let at = format!("{at}.hit.commit");
    let commit = hit
        .pointer("/hit/commit")
        .ok_or_else(|| format!("{at}: missing"))?;
    let revision =
        text_at(commit, "/revision").ok_or_else(|| format!("{at}.revision: must be a string"))?;
    let cut = cut_hash(revision);
    require(facts.contains(&cut), &format!("{at}.revision"), || {
        format!("no `{cut}` fact on the first line {facts:?}")
    })?;
    let name = text_at(commit, "/author/name").unwrap_or_default();
    let email = text_at(commit, "/author/email").unwrap_or_default();
    let author = visible(&format!("{name} <{email}>"));
    require(
        item.plain(0).contains(&author),
        &format!("{at}.author"),
        || format!("no `{author}` on the first line"),
    )?;
    commit_message_states(item, commit, &at)?;
    commit_paths_states(item, commit, &at)
}

/// Checks the subject line, and the truncation line exactly when the message was cut.
fn commit_message_states(item: &Item<'_>, commit: &Value, at: &str) -> Result<(), String> {
    let message = text_at(commit, "/message").unwrap_or_default();
    let subject = visible(message.split('\n').next().unwrap_or_default());
    let subject = subject.trim_end_matches(' ');
    if !subject.is_empty() {
        require(item.has_line(subject), &format!("{at}.message"), || {
            format!("no line equals the subject `{subject}`")
        })?;
    }
    let cut = commit
        .get("message_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    require(
        item.has_line(MESSAGE_TRUNCATED_LINE) == cut,
        &format!("{at}.message_truncated"),
        || format!("`{MESSAGE_TRUNCATED_LINE}` must appear exactly when the flag is {cut}"),
    )
}

/// Checks every path, and the cut label exactly when the paths were cut.
fn commit_paths_states(item: &Item<'_>, commit: &Value, at: &str) -> Result<(), String> {
    let paths: Vec<&str> = commit
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (index, path) in paths.iter().enumerate() {
        let wanted = format!("{INDENT_UNIT}{}", visible(path));
        require(
            item.has_line(&wanted),
            &format!("{at}.paths[{index}]"),
            || format!("no line equals `{wanted}`"),
        )?;
    }
    let cut = commit
        .get("paths_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let label_written = item
        .plain_lines()
        .iter()
        .any(|line| line.ends_with(PATHS_TRUNCATED_TAIL));
    let label = format!("{}{PATHS_TRUNCATED_TAIL}", paths.len());
    // The layout writes no paths block for an empty list, so a cut flag there states nothing.
    let expected = cut && !paths.is_empty();
    require(
        label_written == expected && (!expected || item.has_line(&label)),
        &format!("{at}.paths_truncated"),
        || format!("`{label}` must appear exactly when the flag is {cut}"),
    )
}

/// Checks one `get_symbol` hit against its lines.
fn symbol_hit(item: &Item<'_>, hit: &Value, at: &str) -> Result<(), String> {
    let place = place_of(
        text_at(hit, "/path"),
        text_at(hit, "/unit"),
        hit.get("line").and_then(Value::as_u64),
    );
    if let Some(place) = place {
        require(item.plain(1) == place, at, || {
            format!("the second line must be `{place}`, got `{}`", item.plain(1))
        })?;
    }
    if let Some(id) = text_at(hit, "/symbol/id") {
        require(item.has_line(id), &format!("{at}.symbol.id"), || {
            format!("no line equals `{id}`")
        })?;
    }
    if let Some(history) = hit.get("history") {
        history_states(item, history, &format!("{at}.history"))?;
    }
    source_states(item, hit.get("source"), at)
}

/// Checks the history label, which says whether it is complete, and every version.
fn history_states(item: &Item<'_>, history: &Value, at: &str) -> Result<(), String> {
    let complete = history
        .get("complete")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{at}.complete: must be a bool"))?;
    let (label, other) = if complete {
        (HISTORY_LABEL, HISTORY_INCOMPLETE_LABEL)
    } else {
        (HISTORY_INCOMPLETE_LABEL, HISTORY_LABEL)
    };
    require(
        item.has_line(label) && !item.has_line(other),
        &format!("{at}.complete"),
        || format!("the section label must be `{label}` and not `{other}`"),
    )?;
    let lines = item.plain_lines();
    let entries: Vec<&str> = lines
        .iter()
        .skip_while(|line| **line != label)
        .skip(1)
        .take_while(|line| line.starts_with(INDENT_UNIT))
        .filter(|line| !line.starts_with(indent(2).as_str()))
        .copied()
        .collect();
    let versions = history.get("versions").and_then(Value::as_array);
    for (index, version) in versions.into_iter().flatten().enumerate() {
        version_states(
            entries.get(index).copied(),
            version,
            &format!("{at}.versions[{index}]"),
        )?;
    }
    Ok(())
}

/// Checks that an entry states the cut revision and the kind of a version.
fn version_states(entry: Option<&str>, version: &Value, at: &str) -> Result<(), String> {
    let entry = entry.ok_or_else(|| format!("{at}: no entry line under the history label"))?;
    let facts: Vec<&str> = entry.trim_start().split(SEPARATOR).collect();
    let revision = text_at(version, "/revision").unwrap_or_default();
    let kind = text_at(version, "/kind")
        .unwrap_or_default()
        .replace('_', " ");
    for (name, wanted) in [("revision", cut_hash(revision)), ("kind", kind.as_str())] {
        require(facts.contains(&wanted), &format!("{at}.{name}"), || {
            format!("no `{wanted}` fact on the entry `{entry}`")
        })?;
    }
    Ok(())
}

/// Checks one `nodes` entry against its lines: head, identity, symbol, and regions.
fn node_entry(
    item: &Item<'_>,
    node: &Value,
    structured: &Value,
    index: usize,
    at: &str,
) -> Result<(), String> {
    let id = text_at(node, "/id").ok_or_else(|| format!("{at}.id: must be a string"))?;
    let excerpt = structured
        .pointer(&format!("/source/{index}"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("$.source[{index}]: must be a string"))?;
    node_head_states(item, node, excerpt, (at, &format!("$.source[{index}]")))?;
    require(item.plain(1) == id, &format!("{at}.id"), || {
        format!("the second line must be `{id}`, got `{}`", item.plain(1))
    })?;
    if let Some(symbol) = text_at(node, "/symbol") {
        require(item.has_line(symbol), &format!("{at}.symbol"), || {
            format!("no line equals the symbol `{symbol}`")
        })?;
    }
    regions_states(item, node, at)
}

/// Checks the head line: the kind, the line count, and the `generated` and `test` facts.
///
/// `(at, excerpt_at)` are the JSON paths of the node and of its excerpt.
fn node_head_states(
    item: &Item<'_>,
    node: &Value,
    excerpt: &str,
    (at, excerpt_at): (&str, &str),
) -> Result<(), String> {
    let kind = text_at(node, "/kind").ok_or_else(|| format!("{at}.kind: must be a string"))?;
    let head: Vec<&str> = item.plain(0).split(SEPARATOR).collect();
    let kind = visible(kind);
    require(head.first() == Some(&kind.as_str()), at, || {
        format!("the head line must open with the kind `{kind}`, got {head:?}")
    })?;
    let count = excerpt.lines().count();
    let counted = format!("{count} lines");
    require(
        head.contains(&counted.as_str()) == (count > 1),
        excerpt_at,
        || format!("`{counted}` must appear exactly when the excerpt has several lines: {head:?}"),
    )?;
    let facet_list = node.get("facets").and_then(Value::as_array);
    for name in NODE_FACT_FACETS {
        let holds = facet_list
            .into_iter()
            .flatten()
            .any(|facet| facet.as_str() == Some(name));
        require(
            head.iter().skip(1).any(|fact| *fact == name) == holds,
            &format!("{at}.facets"),
            || format!("`{name}` must appear exactly when the facet holds: {head:?}"),
        )?;
    }
    Ok(())
}

/// Checks the regions line: `<role> <START>..<END>` per region, joined by ` · `.
fn regions_states(item: &Item<'_>, node: &Value, at: &str) -> Result<(), String> {
    let regions: Vec<String> = node
        .get("regions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|region| {
            let number = |name: &str| region.pointer(&format!("/range/{name}"))?.as_u64();
            Some(format!(
                "{} {}..{}",
                text_at(region, "/role")?,
                number("start")?,
                number("end")?
            ))
        })
        .collect::<Option<_>>()
        .ok_or_else(|| format!("{at}.regions: each region needs a role and a range"))?;
    if regions.is_empty() {
        return Ok(());
    }
    let line = regions.join(SEPARATOR);
    require(item.has_line(&line), &format!("{at}.regions"), || {
        format!("no line equals the regions `{line}`")
    })
}

/// Checks that the items section ends with the innermost excerpt, after one blank line.
///
/// The innermost node is the last. Its lines carry the item indent. An answer without nodes
/// has no excerpt.
fn innermost_ends_section(
    entries: &[&str],
    structured: &Value,
    count: usize,
) -> Result<(), String> {
    let Some(last) = count.checked_sub(1) else {
        return Ok(());
    };
    let at = format!("$.source[{last}]");
    let excerpt = structured
        .pointer(&format!("/source/{last}"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{at}: must be a string"))?;
    let levels = item_indent(count > 1);
    let mut expected = vec![String::new()];
    expected.extend(indented_lines(excerpt, levels));
    require(ends_with_lines(entries, &expected), &at, || {
        format!(
            "the items section must end with a blank line and the excerpt indented by {levels} levels"
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A `search` answer with a symbol hit, a commit hit, and a walk hit two hops from `main`, on
    /// page 2 of 3, and a warning.
    fn search_answer() -> Value {
        json!({
            "results": [
                {
                    "hit": {
                        "target": "symbol",
                        "symbol": { "id": "rift://symbol/rust/src/a.rs/a", "name": "a" }
                    },
                    "score": 2.0,
                    "matched_by": ["name", "documentation"],
                    "source": "pub fn a() {\n    1\n}",
                    "range": { "start": 0, "end": 20 },
                    "line": 10,
                    "path": "src/a.rs",
                    "distance": 2
                },
                {
                    "hit": {
                        "target": "commit",
                        "commit": {
                            "revision": "9c1d4e7a1b2c3d4e5f60",
                            "message": "Bound comparisons\n\nA body line",
                            "message_truncated": true,
                            "author": { "name": "Alice", "email": "alice@example.com" },
                            "timestamp": "2026-09-22T14:03:00+02:00",
                            "paths": ["a.rs", "b.rs"],
                            "paths_truncated": true
                        }
                    }
                },
                {
                    "hit": {
                        "target": "symbol",
                        "symbol": { "id": "rift://symbol/rust/src/d.rs/d", "name": "d" }
                    },
                    "score": 1.5,
                    "matched_by": ["relationship", "name"],
                    "source": "pub fn d() {\n    2\n}",
                    "line": 30,
                    "path": "src/d.rs",
                    "distance": 2,
                    "traversal_path": [
                        {
                            "direction": "outgoing",
                            "relationship": {
                                "from": "rift://symbol/rust/src/m.rs/main",
                                "to": "rift://symbol/rust/src/b.rs/b",
                                "facets": ["calls"],
                                "derivation": "resolution"
                            }
                        },
                        {
                            "direction": "outgoing",
                            "relationship": {
                                "from": "rift://symbol/rust/src/b.rs/b",
                                "to": "rift://symbol/rust/src/d.rs/d",
                                "facets": ["calls", "has_type"],
                                "derivation": "heuristic"
                            }
                        }
                    ]
                }
            ],
            "pagination": { "page_index": 1, "total_pages": 3 },
            "warnings": [{ "code": "stale_index", "detail": "index lags" }]
        })
    }

    /// The text the layout writes for [`search_answer`].
    const SEARCH_TEXT: &str = "\
3 results · page 2/3
\t[1] pub fn a()
\t\tsrc/a.rs:10 · name, documentation · score 2
\t\trift://symbol/rust/src/a.rs/a

\t\tpub fn a() {
\t\t    1
\t\t}
\t[2] 9c1d4e7a · Alice <alice@example.com> · 2026-09-22 14:03 +02:00
\t\tBound comparisons

\t\tA body line
\t\tmessage truncated

\t\t2+ paths · truncated
\t\t\ta.rs
\t\t\tb.rs
\t\t\t...

\tat rift://symbol/rust/src/m.rs/main

\t\t↳ calls
\t\t\tat rift://symbol/rust/src/b.rs/b

\t\t\t↳ probably calls, has type
\t\t\t\tpub fn d()
\t\t\t\tin src/d.rs:30 · name · score 1.5
\t\t\t\tat rift://symbol/rust/src/d.rs/d

\t\t\t\tpub fn d() {
\t\t\t\t    2
\t\t\t\t}
1 warning
\tstale_index: index lags
";

    /// A `get_symbol` answer with an incomplete history and a source.
    fn symbol_answer() -> Value {
        json!({
            "hits": [{
                "symbol": { "id": "rift://symbol/rust/src/a.rs/a", "name": "a" },
                "path": "src/a.rs",
                "range": { "start": 0, "end": 12 },
                "line": 3,
                "source": "pub fn a() {}",
                "history": {
                    "symbol": "rift://symbol/rust/src/a.rs/a",
                    "complete": false,
                    "versions": [
                        {
                            "revision": "1f2080e5aa",
                            "path": "src/a.rs",
                            "kind": "signature_changed",
                            "timestamp": "2026-08-21T10:00:00+00:00",
                            "summary": "Return an error",
                            "author": { "name": "Alice", "email": "alice@example.com" }
                        },
                        {
                            "revision": "HEAD~2",
                            "path": "src/a.rs",
                            "kind": "introduced",
                            "timestamp": "2026-08-17T09:41:05+00:00",
                            "author": { "name": "Alice", "email": "alice@example.com" }
                        }
                    ]
                }
            }],
            "pagination": { "page_index": 0, "total_pages": 1 }
        })
    }

    /// The text the layout writes for [`symbol_answer`].
    const SYMBOL_TEXT: &str = "\
1 result
\tpub fn a()
\tsrc/a.rs:3
\trift://symbol/rust/src/a.rs/a

\thistory (incomplete):
\t\t2026-08-21 · 1f2080e5 · signature changed · Alice <alice@example.com>
\t\t\tReturn an error
\t\t2026-08-17 · HEAD~2 · introduced · Alice <alice@example.com>

\tpub fn a() {}
";

    /// `base` with `from` replaced by `to`, refused when `from` is not in `base`.
    fn edited(base: &str, from: &str, to: &str) -> Result<String, String> {
        if base.contains(from) {
            Ok(base.replace(from, to))
        } else {
            Err(format!("the base text has no `{from}`"))
        }
    }

    /// The failure of a check that must refuse, so a test can assert on its JSON path.
    fn refused(tool: Tool, text: &str, answer: &Value) -> Result<String, String> {
        match text_states(tool, text, answer) {
            Ok(()) => Err(format!("the check must refuse:\n{text}")),
            Err(failure) => Ok(failure),
        }
    }

    /// `answer` with the value at `pointer` replaced.
    fn with(mut answer: Value, pointer: &str, value: Value) -> Result<Value, String> {
        *answer
            .pointer_mut(pointer)
            .ok_or_else(|| format!("no value at {pointer}"))? = value;
        Ok(answer)
    }

    /// Asserts the failure of the search text edited by `from` to `to` names `path`.
    fn search_refusal(from: &str, to: &str, path: &str) -> Result<(), String> {
        let text = edited(SEARCH_TEXT, from, to)?;
        let failure = refused(Tool::Search, &text, &search_answer())?;
        assert!(failure.contains(path), "{path} not in {failure}");
        Ok(())
    }

    #[test]
    fn the_layout_text_of_both_tools_passes() {
        assert_eq!(
            text_states(Tool::Search, SEARCH_TEXT, &search_answer()),
            Ok(())
        );
        assert_eq!(
            text_states(Tool::GetSymbol, SYMBOL_TEXT, &symbol_answer()),
            Ok(())
        );
    }

    #[test]
    fn an_empty_answer_needs_only_its_header() {
        let answer = json!({
            "results": [],
            "pagination": { "page_index": 0, "total_pages": 0 }
        });
        assert_eq!(text_states(Tool::Search, "0 results\n", &answer), Ok(()));
        assert!(text_states(Tool::Search, "0 results\n\t[1] a\n", &answer).is_err());
    }

    #[test]
    fn a_page_past_the_end_states_its_page() -> Result<(), String> {
        let answer = json!({
            "results": [],
            "pagination": { "page_index": 4, "total_pages": 2 }
        });
        assert_eq!(
            text_states(Tool::Search, "0 results · page 5/2\n", &answer),
            Ok(())
        );
        let failure = refused(Tool::Search, "0 results\n", &answer)?;
        assert!(failure.contains("$.pagination"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_wrong_count_in_the_title_is_refused() -> Result<(), String> {
        search_refusal("3 results", "4 results", "$.results")?;
        let failure = refused(Tool::GetSymbol, "2 results\n", &symbol_answer())?;
        assert!(failure.contains("$.hits"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_title_without_its_page_is_refused() -> Result<(), String> {
        search_refusal(" · page 2/3", "", "$.pagination")?;
        search_refusal("page 2/3", "page 1/3", "$.pagination")
    }

    #[test]
    fn a_missing_warnings_section_is_refused() -> Result<(), String> {
        search_refusal(
            "1 warning\n\tstale_index: index lags\n",
            "",
            "$.warnings[0].code",
        )?;
        search_refusal("\tstale_index: index lags\n", "", "$.warnings")
    }

    #[test]
    fn a_warnings_section_without_warnings_is_refused() -> Result<(), String> {
        let answer = with(search_answer(), "/warnings", json!([]))?;
        let failure = refused(Tool::Search, SEARCH_TEXT, &answer)?;
        assert!(failure.contains("$.warnings"), "{failure}");
        let absent = json!({
            "results": [],
            "pagination": { "page_index": 0, "total_pages": 0 }
        });
        let failure = refused(Tool::Search, "0 results\n1 warning\n\tx\n", &absent)?;
        assert!(failure.contains("$.warnings"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_warning_under_the_header_in_the_old_form_is_refused() -> Result<(), String> {
        let bare = edited(SEARCH_TEXT, "1 warning\n\tstale_index: index lags\n", "")?;
        let old = edited(&bare, "page 2/3\n", "page 2/3\n! stale_index: index lags\n")?;
        let failure = refused(Tool::Search, &old, &search_answer())?;
        assert!(failure.contains("$.warnings"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_warnings_title_that_disagrees_with_its_lines_is_refused() -> Result<(), String> {
        search_refusal("1 warning\n", "2 warnings\n", "$.warnings")?;
        search_refusal("index lags\n", "index lags\n\tstale_index\n", "$.warnings")
    }

    #[test]
    fn a_warning_line_at_a_wrong_indent_or_before_the_items_is_refused() -> Result<(), String> {
        search_refusal(
            "\tstale_index: index lags",
            "\t\tstale_index: index lags",
            "$.warnings[0].code",
        )?;
        search_refusal(
            "\tstale_index: index lags",
            "stale_index: index lags",
            "$.warnings",
        )?;
        let bare = edited(SEARCH_TEXT, "1 warning\n\tstale_index: index lags\n", "")?;
        let moved = edited(
            &bare,
            "page 2/3\n",
            "page 2/3\n1 warning\n\tstale_index: index lags\n",
        )?;
        let failure = refused(Tool::Search, &moved, &search_answer())?;
        assert!(failure.contains("$.warnings"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_blank_line_after_a_title_between_items_or_between_sections_is_refused()
    -> Result<(), String> {
        search_refusal("page 2/3\n\t[1]", "page 2/3\n\n\t[1]", "$.results")?;
        search_refusal("\t\t}\n\t[2]", "\t\t}\n\n\t[2]", "$.results[0]")?;
        search_refusal(
            "\t\t\t\t}\n1 warning",
            "\t\t\t\t}\n\n1 warning",
            "$.results[2]",
        )
    }

    #[test]
    fn a_blank_line_may_follow_a_source_that_ends_in_a_line_feed() -> Result<(), String> {
        let source = json!("pub fn a() {\n    1\n}\n");
        let answer = with(search_answer(), "/results/0/source", source)?;
        let text = edited(SEARCH_TEXT, "\t\t}\n\t[2]", "\t\t}\n\n\t[2]")?;
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        Ok(())
    }

    #[test]
    fn an_item_line_at_a_wrong_indent_is_refused() -> Result<(), String> {
        search_refusal(
            "\t\trift://symbol/rust/src/a.rs/a\n",
            "\trift://symbol/rust/src/a.rs/a\n",
            "$.results[0]",
        )?;
        search_refusal("\t\tb.rs\n", "\tb.rs\n", "$.results[1]")?;
        let text = edited(SYMBOL_TEXT, "\tsrc/a.rs:3", "src/a.rs:3")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("src/a.rs:3"), "{failure}");
        let text = edited(SYMBOL_TEXT, "\trift://symbol", "\t\trift://symbol")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("$.hits[0].symbol.id"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_line_indented_by_spaces_is_refused() {
        let spaced = SEARCH_TEXT.replace(INDENT_UNIT, "  ");
        assert!(text_states(Tool::Search, &spaced, &search_answer()).is_err());
        let walk = WALK_TEXT.replace(INDENT_UNIT, "  ");
        assert!(text_states(Tool::Search, &walk, &walk_answer()).is_err());
    }

    #[test]
    fn a_marker_at_a_wrong_indent_or_an_extra_item_is_refused() -> Result<(), String> {
        search_refusal("\t[2] 9c1d", "\t\t[2] 9c1d", "$.results[1]")?;
        let bare = edited(SEARCH_TEXT, "1 warning\n\tstale_index: index lags\n", "")?;
        let extra = format!("{bare}\t[3] x\n1 warning\n\tstale_index: index lags\n");
        let failure = refused(Tool::Search, &extra, &search_answer())?;
        assert!(failure.contains("[3]"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_missing_location_is_refused() -> Result<(), String> {
        search_refusal("src/a.rs:10", "src/a.rs:11", "$.results[0]")?;
        let text = edited(SYMBOL_TEXT, "src/a.rs:3", "src/a.rs:4")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("$.hits[0]"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_missing_symbol_id_is_refused() -> Result<(), String> {
        search_refusal(
            "\t\trift://symbol/rust/src/a.rs/a\n",
            "",
            "$.results[0].hit",
        )?;
        let text = edited(SYMBOL_TEXT, "\trift://symbol/rust/src/a.rs/a\n", "")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("$.hits[0].symbol.id"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_missing_matched_field_is_refused() -> Result<(), String> {
        search_refusal("name, documentation", "name", "$.results[0].matched_by[1]")
    }

    #[test]
    fn a_wrong_score_is_refused_and_a_distance_is_not_required() -> Result<(), String> {
        search_refusal(" · score 2", " · score 3", "$.results[0].score")?;
        assert!(!SEARCH_TEXT.contains("distance"));
        assert_eq!(
            text_states(Tool::Search, SEARCH_TEXT, &search_answer()),
            Ok(())
        );
        Ok(())
    }

    #[test]
    fn a_warning_line_names_its_code_whole() -> Result<(), String> {
        search_refusal(
            "\tstale_index: index lags\n",
            "\tstale_index_other: index lags\n",
            "$.warnings[0].code",
        )?;
        let spelled = edited(SEARCH_TEXT, "\tstale_index: index lags", "\tstale_index")?;
        assert_eq!(
            text_states(Tool::Search, &spelled, &search_answer()),
            Ok(())
        );
        let evidence = edited(
            SEARCH_TEXT,
            "\tstale_index: index lags",
            "\tstale_index · index_tree_revision 3f9a1c2e",
        )?;
        assert_eq!(
            text_states(Tool::Search, &evidence, &search_answer()),
            Ok(())
        );
        Ok(())
    }

    /// The identity of the symbol `name`.
    fn id_of(name: &str) -> String {
        format!("rift://symbol/rust/src/{name}.rs/{name}")
    }

    /// A hop over the edge `from` to `to`, followed in `direction`.
    fn hop(direction: &str, (from, to): (&str, &str), facets: &[&str], derivation: &str) -> Value {
        json!({
            "direction": direction,
            "relationship": {
                "from": id_of(from),
                "to": id_of(to),
                "facets": facets,
                "derivation": derivation
            }
        })
    }

    /// A symbol hit for `name` at line `line`, a walk hit when `hops` is not empty.
    fn named_hit(name: &str, line: u64, matched: &[&str], hops: Vec<Value>) -> Value {
        let mut hit = json!({
            "hit": { "target": "symbol", "symbol": { "id": id_of(name), "name": name } },
            "matched_by": matched,
            "path": format!("src/{name}.rs"),
            "line": line
        });
        if !hops.is_empty() {
            hit["traversal_path"] = Value::Array(hops);
        }
        hit
    }

    /// `hit` with the score `score`.
    fn scored(mut hit: Value, score: f64) -> Value {
        hit["score"] = json!(score);
        hit
    }

    /// An answer of one page that holds all of `results`.
    fn page_of(results: &[Value]) -> Value {
        json!({ "results": results, "pagination": { "page_index": 0, "total_pages": 1 } })
    }

    /// A `search` answer with one item, a file hit, and two trees. `load_config` is a hit outside
    /// the walk and the root of the first tree. `run` is reached once, `main` through it, and
    /// `reload_config` by its own hop. The second tree starts from `serve` and passes `handler`,
    /// which has no hit.
    fn walk_answer() -> Value {
        let file = json!({
            "hit": { "target": "file", "size": 3 },
            "matched_by": ["content"],
            "path": "README.md",
            "line": 3,
            "source": "Readme line"
        });
        let to_run = || {
            hop(
                "incoming",
                ("run", "load_config"),
                &["references"],
                "resolution",
            )
        };
        page_of(&[
            file,
            named_hit("load_config", 10, &["name"], Vec::new()),
            named_hit("run", 20, &["relationship"], vec![to_run()]),
            scored(
                named_hit(
                    "main",
                    9,
                    &["relationship", "name"],
                    vec![
                        to_run(),
                        hop("incoming", ("main", "run"), &["calls"], "syntax"),
                    ],
                ),
                0.5,
            ),
            named_hit(
                "reload_config",
                41,
                &["relationship"],
                vec![hop(
                    "incoming",
                    ("reload_config", "load_config"),
                    &["calls"],
                    "resolution",
                )],
            ),
            named_hit(
                "parse",
                4,
                &["relationship"],
                vec![
                    hop("outgoing", ("serve", "handler"), &["calls"], "resolution"),
                    hop(
                        "outgoing",
                        ("handler", "parse"),
                        &["calls", "reads"],
                        "heuristic",
                    ),
                ],
            ),
        ])
    }

    /// The text the layout writes for [`walk_answer`].
    const WALK_TEXT: &str = "\
6 results
\tREADME.md:3 · content
\tReadme line

\tpub fn load_config()
\tin src/load_config.rs:10 · name
\tat rift://symbol/rust/src/load_config.rs/load_config

\t\t↳ referenced
\t\t\tby fn run()
\t\t\tin src/run.rs:20
\t\t\tat rift://symbol/rust/src/run.rs/run

\t\t\t↳ called
\t\t\t\tby fn main()
\t\t\t\tin src/main.rs:9 · name · score 0.5
\t\t\t\tat rift://symbol/rust/src/main.rs/main

\t\t↳ called
\t\t\tby fn reload_config()
\t\t\tin src/reload_config.rs:41
\t\t\tat rift://symbol/rust/src/reload_config.rs/reload_config

\tat rift://symbol/rust/src/serve.rs/serve

\t\t↳ calls
\t\t\tat rift://symbol/rust/src/handler.rs/handler

\t\t\t↳ probably calls, reads
\t\t\t\tfn parse()
\t\t\t\tin src/parse.rs:4
\t\t\t\tat rift://symbol/rust/src/parse.rs/parse
";

    /// The block of the node `reload_config` of [`WALK_TEXT`], with the blank line before it.
    const RELOAD_BLOCK: &str = "\n\t\t↳ called\n\t\t\tby fn reload_config()\n\t\t\tin src/reload_config.rs:41\n\t\t\tat rift://symbol/rust/src/reload_config.rs/reload_config\n";

    /// Asserts the failure of the walk text edited by `from` to `to` names `path`.
    fn walk_refusal(from: &str, to: &str, path: &str) -> Result<(), String> {
        let text = edited(WALK_TEXT, from, to)?;
        let failure = refused(Tool::Search, &text, &walk_answer())?;
        assert!(failure.contains(path), "{path} not in {failure}");
        Ok(())
    }

    #[test]
    fn the_indent_unit_is_one_tab() {
        assert_eq!(INDENT_UNIT, "\t");
    }

    #[test]
    fn the_walk_tree_text_passes() {
        assert_eq!(text_states(Tool::Search, WALK_TEXT, &walk_answer()), Ok(()));
    }

    #[test]
    fn a_walk_without_items_starts_at_the_first_entry_and_the_root_is_no_item() -> Result<(), String>
    {
        let mut answer = walk_answer();
        let results = answer["results"]
            .as_array_mut()
            .ok_or("the results must be an array")?;
        results.remove(0);
        let text = edited(
            WALK_TEXT,
            "6 results\n\tREADME.md:3 · content\n\tReadme line\n\n",
            "5 results\n",
        )?;
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        let blank = edited(&text, "5 results\n", "5 results\n\n")?;
        let failure = refused(Tool::Search, &blank, &answer)?;
        assert!(failure.contains("$.results"), "{failure}");
        let marked = edited(
            &text,
            "\tpub fn load_config()",
            "\t[1] pub fn load_config()",
        )?;
        assert!(text_states(Tool::Search, &marked, &answer).is_err());
        Ok(())
    }

    #[test]
    fn a_wrong_relationship_word_is_refused() -> Result<(), String> {
        walk_refusal(
            "\t\t↳ referenced\n",
            "\t\t↳ calls\n",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "\t\t↳ referenced\n",
            "\t\t↳ referenced by\n",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "\n\t\t↳ called\n\t\t\tby fn reload_config()",
            "\n\t\t↳ calls\n\t\t\tby fn reload_config()",
            "$.results[4].traversal_path[0]",
        )?;
        walk_refusal(
            "↳ probably calls, reads\n",
            "↳ calls, reads\n",
            "$.results[5].traversal_path[1]",
        )?;
        walk_refusal(
            "↳ probably calls, reads\n",
            "↳ probably calls\n",
            "$.results[5].traversal_path[1]",
        )?;
        walk_refusal(
            "\t\t↳ calls\n",
            "\t\t↳ called\n",
            "$.results[5].traversal_path[0]",
        )
    }

    #[test]
    fn the_by_rule_moves_the_word_to_the_declaration_line() -> Result<(), String> {
        walk_refusal(
            "\t\t\tby fn run()",
            "\t\t\tfn run()",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "\t\t\t\tfn parse()",
            "\t\t\t\tby fn parse()",
            "$.results[5].traversal_path[1]",
        )?;
        Ok(())
    }

    #[test]
    fn the_in_rule_needs_a_location_exactly() -> Result<(), String> {
        walk_refusal(
            "\t\t\tin src/run.rs:20\n",
            "\t\t\tsrc/run.rs:20\n",
            "$.results[2]",
        )?;
        let mut answer = walk_answer();
        let hit = &mut answer["results"][4];
        hit["matched_by"] = json!(["relationship", "name"]);
        hit.as_object_mut()
            .ok_or("the hit must be an object")?
            .retain(|key, _| key != "path" && key != "line");
        let located = edited(WALK_TEXT, "in src/reload_config.rs:41\n", "name\n")?;
        assert_eq!(text_states(Tool::Search, &located, &answer), Ok(()));
        let marked = edited(&located, "\t\t\tname\n", "\t\t\tin name\n")?;
        let failure = refused(Tool::Search, &marked, &answer)?;
        assert!(failure.contains("$.results[4]"), "{failure}");
        Ok(())
    }

    #[test]
    fn the_identity_line_of_a_node_starts_with_at() -> Result<(), String> {
        walk_refusal(
            "\t\t\tat rift://symbol/rust/src/run.rs/run\n",
            "\t\t\trift://symbol/rust/src/run.rs/run\n",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "\t\t\tat rift://symbol/rust/src/handler.rs/handler\n",
            "\t\t\trift://symbol/rust/src/handler.rs/handler\n",
            "$.results[5].traversal_path[0]",
        )
    }

    #[test]
    fn a_root_that_is_a_hit_needs_in_and_at_and_a_root_that_is_no_hit_needs_at()
    -> Result<(), String> {
        walk_refusal(
            "\tin src/load_config.rs:10 · name\n",
            "\tsrc/load_config.rs:10 · name\n",
            "$.results[1]",
        )?;
        walk_refusal(
            "\tat rift://symbol/rust/src/load_config.rs/load_config\n",
            "\trift://symbol/rust/src/load_config.rs/load_config\n",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "\tat rift://symbol/rust/src/serve.rs/serve\n",
            "\trift://symbol/rust/src/serve.rs/serve\n",
            "$.results[5].traversal_path[0]",
        )
    }

    #[test]
    fn an_item_symbol_hit_carries_neither_in_nor_at() -> Result<(), String> {
        let mut answer = walk_answer();
        answer["results"]
            .as_array_mut()
            .ok_or("the results must be an array")?
            .insert(1, named_hit("lone", 7, &["name"], Vec::new()));
        let text = edited(
            WALK_TEXT,
            "6 results\n\tREADME.md:3 · content\n\tReadme line\n",
            "7 results\n\t[1] README.md:3 · content\n\t\tReadme line\n\t[2] fn lone()\n\t\tsrc/lone.rs:7 · name\n\t\trift://symbol/rust/src/lone.rs/lone\n",
        )?;
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        let worded = edited(&text, "\t\tsrc/lone.rs:7", "\t\tin src/lone.rs:7")?;
        assert!(text_states(Tool::Search, &worded, &answer).is_err());
        let at = edited(
            &text,
            "\t\trift://symbol/rust/src/lone.rs/lone",
            "\t\tat rift://symbol/rust/src/lone.rs/lone",
        )?;
        assert!(text_states(Tool::Search, &at, &answer).is_err());
        Ok(())
    }

    #[test]
    fn a_node_at_a_wrong_indent_is_refused() -> Result<(), String> {
        walk_refusal(
            "\n\t\t↳ called\n\t\t\tby fn reload_config()",
            "\n\t↳ called\n\t\t\tby fn reload_config()",
            "$.results[4].traversal_path[0]",
        )?;
        walk_refusal(
            "\n\t\t\t↳ called\n",
            "\n\t\t↳ called\n",
            "$.results[3].traversal_path[1]",
        )?;
        walk_refusal(
            "\t\t\tin src/reload_config.rs:41",
            "\t\tin src/reload_config.rs:41",
            "$.results[4]",
        )?;
        walk_refusal(
            "\tat rift://symbol/rust/src/serve.rs/serve",
            "\t\tat rift://symbol/rust/src/serve.rs/serve",
            "$.results[5].traversal_path[0]",
        )?;
        walk_refusal(
            "\tin src/load_config.rs:10 · name",
            "\t\t\tsrc/load_config.rs:10 · name",
            "$.results[1]",
        )
    }

    #[test]
    fn a_missing_node_is_refused() -> Result<(), String> {
        walk_refusal(RELOAD_BLOCK, "", "$.results[4].traversal_path[0]")?;
        walk_refusal(
            "\n\t\t↳ calls\n\t\t\tat rift://symbol/rust/src/handler.rs/handler\n",
            "",
            "$.results[5].traversal_path[0]",
        )?;
        walk_refusal(
            "\tat rift://symbol/rust/src/serve.rs/serve\n\n",
            "",
            "$.results[5].traversal_path[0]",
        )?;
        walk_refusal(
            "\n\t\t\t↳ called\n\t\t\t\tby fn main()\n\t\t\t\tin src/main.rs:9 · name · score 0.5\n\t\t\t\tat rift://symbol/rust/src/main.rs/main\n",
            "",
            "$.results[3].traversal_path[1]",
        )
    }

    #[test]
    fn a_node_that_comes_before_its_turn_is_refused() -> Result<(), String> {
        let swapped = edited(WALK_TEXT, RELOAD_BLOCK, "")?;
        let text = edited(
            &swapped,
            "\n\t\t↳ referenced\n",
            &format!("{RELOAD_BLOCK}\n\t\t↳ referenced\n"),
        )?;
        let failure = refused(Tool::Search, &text, &walk_answer())?;
        assert!(failure.contains("$.results["), "{failure}");
        Ok(())
    }

    #[test]
    fn a_missing_blank_line_before_a_node_is_refused() -> Result<(), String> {
        walk_refusal(
            "\n\n\t\t↳ called\n\t\t\tby fn reload_config()",
            "\n\t\t↳ called\n\t\t\tby fn reload_config()",
            "$.results[4].traversal_path[0]",
        )?;
        walk_refusal(
            "main\n\n\t\t↳",
            "main\n\t\t↳",
            "$.results[4].traversal_path[0]",
        )?;
        walk_refusal(
            "\n\n\tat rift://symbol/rust/src/serve.rs/serve",
            "\n\tat rift://symbol/rust/src/serve.rs/serve",
            "$.results[5].traversal_path[0]",
        )?;
        walk_refusal(
            "Readme line\n\n\tpub fn load_config()",
            "Readme line\n\tpub fn load_config()",
            "$.results[2].traversal_path[0]",
        )?;
        walk_refusal(
            "handler\n\n\t\t\t↳",
            "handler\n\t\t\t↳",
            "$.results[5].traversal_path[1]",
        )
    }

    #[test]
    fn a_blank_line_after_the_last_node_is_refused() -> Result<(), String> {
        let text = format!("{WALK_TEXT}\n");
        let failure = refused(Tool::Search, &text, &walk_answer())?;
        assert!(failure.contains("$.results[5]"), "{failure}");
        Ok(())
    }

    #[test]
    fn the_marker_counts_only_the_items_outside_the_walk() -> Result<(), String> {
        walk_refusal("\tREADME.md:3", "\t[1] README.md:3", "$.results[0]")?;
        let mut answer = walk_answer();
        answer["results"]
            .as_array_mut()
            .ok_or("the results must be an array")?
            .insert(1, named_hit("lone", 7, &["name"], Vec::new()));
        let text = edited(
            WALK_TEXT,
            "6 results\n\tREADME.md:3 · content\n\tReadme line\n",
            "7 results\n\t[1] README.md:3 · content\n\t\tReadme line\n\t[2] fn lone()\n\t\tsrc/lone.rs:7 · name\n\t\trift://symbol/rust/src/lone.rs/lone\n",
        )?;
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        let unmarked = edited(&text, "\t[2] fn lone()", "\tfn lone()")?;
        let failure = refused(Tool::Search, &unmarked, &answer)?;
        assert!(failure.contains("[2]"), "{failure}");
        Ok(())
    }

    #[test]
    fn two_walk_hits_that_reach_one_symbol_under_one_parent_each_get_a_node() -> Result<(), String>
    {
        let reach = |line| {
            named_hit(
                "x",
                line,
                &["relationship"],
                vec![hop("incoming", ("x", "root"), &["calls"], "resolution")],
            )
        };
        let answer = page_of(&[reach(1), reach(2)]);
        let text = "\
2 results
\tat rift://symbol/rust/src/root.rs/root

\t\t↳ called
\t\t\tby fn x()
\t\t\tin src/x.rs:1
\t\t\tat rift://symbol/rust/src/x.rs/x

\t\t↳ called
\t\t\tby fn x()
\t\t\tin src/x.rs:2
\t\t\tat rift://symbol/rust/src/x.rs/x
";
        assert_eq!(text_states(Tool::Search, text, &answer), Ok(()));
        let one = edited(
            text,
            "\n\t\t↳ called\n\t\t\tby fn x()\n\t\t\tin src/x.rs:2\n\t\t\tat rift://symbol/rust/src/x.rs/x\n",
            "",
        )?;
        assert!(text_states(Tool::Search, &one, &answer).is_err());
        Ok(())
    }

    #[test]
    fn a_node_leaves_out_the_relationship_field_and_keeps_the_others() -> Result<(), String> {
        walk_refusal(
            "in src/main.rs:9 · name",
            "in src/main.rs:9 · relationship, name",
            "$.results[3]",
        )?;
        walk_refusal(
            "in src/main.rs:9 · name · score 0.5",
            "in src/main.rs:9 · score 0.5",
            "$.results[3].matched_by[1]",
        )?;
        walk_refusal(
            "in src/main.rs:9 · name · score 0.5",
            "in src/main.rs:9 · name",
            "$.results[3].score",
        )?;
        walk_refusal(
            "\t\t\tin src/run.rs:20",
            "\t\t\tin src/run.rs:21",
            "$.results[2]",
        )?;
        walk_refusal(
            "\t\t\tat rift://symbol/rust/src/run.rs/run\n",
            "",
            "$.results[2].traversal_path[0]",
        )
    }

    #[test]
    fn a_node_without_a_hit_writes_its_identity_alone() -> Result<(), String> {
        walk_refusal(
            "handler.rs/handler\n",
            "handler.rs/handler\n\t\t\tsrc/x.rs:1\n",
            "$.results[5].traversal_path[0]",
        )?;
        walk_refusal(
            "\t\t\tat rift://symbol/rust/src/handler.rs/handler",
            "\t\t\tat rift://symbol/rust/src/other.rs/other",
            "$.results[5].traversal_path[0]",
        )
    }

    #[test]
    fn the_source_of_a_node_follows_a_blank_line_at_its_further_line_indent() -> Result<(), String>
    {
        let mut answer = walk_answer();
        answer["results"][5]["source"] = json!("fn parse() {\n    1\n}");
        let text = edited(
            WALK_TEXT,
            "parse.rs/parse\n",
            "parse.rs/parse\n\n\t\t\t\tfn parse() {\n\t\t\t\t    1\n\t\t\t\t}\n",
        )?;
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        let joined = edited(&text, "parse.rs/parse\n\n", "parse.rs/parse\n")?;
        let failure = refused(Tool::Search, &joined, &answer)?;
        assert!(failure.contains("$.results[5].source"), "{failure}");
        let shallow = edited(&text, "\t\t\t\tfn parse() {", "\t\t\tfn parse() {")?;
        let failure = refused(Tool::Search, &shallow, &answer)?;
        assert!(failure.contains("$.results[5]"), "{failure}");
        let changed = edited(&text, "    1", "    2")?;
        let failure = refused(Tool::Search, &changed, &answer)?;
        assert!(failure.contains("$.results[5].source"), "{failure}");
        Ok(())
    }

    #[test]
    fn copied_source_keeps_its_own_leading_spaces_after_the_tab_indent() {
        let mut answer = walk_answer();
        answer["results"][5]["source"] = json!("  a\n\n\tb");
        let text = WALK_TEXT.replace(
            "parse.rs/parse\n",
            "parse.rs/parse\n\n\t\t\t\t  a\n\n\t\t\t\t\tb\n",
        );
        assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()));
        let reindented = text.replace("\t\t\t\t  a", "\t\t\t\t\t\ta");
        assert!(text_states(Tool::Search, &reindented, &answer).is_err());
    }

    #[test]
    fn a_hit_of_another_target_with_a_traversal_path_stays_an_item() {
        let mut file = json!({
            "hit": { "target": "file", "size": 3 },
            "matched_by": ["content"],
            "path": "a.rs",
            "line": 2
        });
        file["traversal_path"] = json!([hop("outgoing", ("a", "b"), &["calls"], "resolution")]);
        let answer = page_of(&[file]);
        assert_eq!(
            text_states(Tool::Search, "1 result\n\ta.rs:2 · content\n", &answer),
            Ok(())
        );
        let tree = "1 result\n\trift://symbol/rust/src/a.rs/a\n";
        assert!(text_states(Tool::Search, tree, &answer).is_err());
    }

    #[test]
    fn a_hit_with_an_empty_traversal_path_is_an_item() {
        let mut answer = search_answer();
        answer["results"][0]["traversal_path"] = json!([]);
        assert_eq!(text_states(Tool::Search, SEARCH_TEXT, &answer), Ok(()));
    }

    #[test]
    fn the_tree_of_the_search_text_is_checked_through_its_hops() -> Result<(), String> {
        let path = "$.results[2].traversal_path";
        search_refusal(
            "\t\t↳ calls\n\t\t\tat rift://symbol/rust/src/b.rs/b\n\n",
            "",
            &format!("{path}[0]"),
        )?;
        search_refusal(
            "↳ probably calls, has type\n",
            "↳ calls, has type\n",
            &format!("{path}[1]"),
        )?;
        search_refusal(
            "↳ probably calls, has type\n",
            "↳ probably calls, type of\n",
            &format!("{path}[1]"),
        )?;
        search_refusal(
            "\tat rift://symbol/rust/src/m.rs/main\n",
            "\tat rift://symbol/rust/src/n.rs/n\n",
            &format!("{path}[0]"),
        )?;
        search_refusal(
            "\t\t\t...\n\n\tat rift",
            "\t\t\t...\n\t[3] x\n\n\tat rift",
            "[3]",
        )?;
        search_refusal(
            "\t\t\t...\n\n\tat rift",
            "\t\t\t...\n\tat rift",
            &format!("{path}[0]"),
        )
    }

    #[test]
    fn a_hop_reads_a_facet_by_its_direction() {
        // Direction, facet, head line words, and whether the declaration line starts with `by `.
        let cases = [
            ("incoming", "has_type", "type of", false),
            ("incoming", "annotated_by", "annotates", false),
            ("incoming", "mixes_in", "mixed in", true),
            ("incoming", "depends_on", "dependency of", false),
            ("incoming", "calls", "called", true),
            ("outgoing", "has_type", "has type", false),
            ("outgoing", "annotated_by", "annotated", true),
            ("outgoing", "mixes_in", "mixes in", false),
            ("outgoing", "calls", "calls", false),
        ];
        for (direction, facet, words, by) in cases {
            let answer = page_of(&[named_hit(
                "x",
                1,
                &["relationship"],
                vec![hop(direction, ("x", "x"), &[facet], "resolution")],
            )]);
            let word = if by { "by " } else { "" };
            let text = format!(
                "1 result\n\tat rift://symbol/rust/src/x.rs/x\n\n\t\t↳ {words}\n\t\t\t{word}fn x()\n\t\t\tin src/x.rs:1\n\t\t\tat rift://symbol/rust/src/x.rs/x\n"
            );
            assert_eq!(text_states(Tool::Search, &text, &answer), Ok(()), "{facet}");
            let wrong = text.replace(words, "touches");
            assert!(
                text_states(Tool::Search, &wrong, &answer).is_err(),
                "{facet}"
            );
        }
    }

    #[test]
    fn several_facets_move_by_only_when_every_phrase_ends_in_it() {
        let hit = |facets: &[&str]| {
            page_of(&[named_hit(
                "x",
                1,
                &["relationship"],
                vec![hop("incoming", ("x", "root"), facets, "resolution")],
            )])
        };
        let all = "1 result\n\tat rift://symbol/rust/src/root.rs/root\n\n\t\t↳ called, referenced\n\t\t\tby fn x()\n\t\t\tin src/x.rs:1\n\t\t\tat rift://symbol/rust/src/x.rs/x\n";
        assert_eq!(
            text_states(Tool::Search, all, &hit(&["calls", "references"])),
            Ok(())
        );
        let one = "1 result\n\tat rift://symbol/rust/src/root.rs/root\n\n\t\t↳ called by, type of\n\t\t\tfn x()\n\t\t\tin src/x.rs:1\n\t\t\tat rift://symbol/rust/src/x.rs/x\n";
        assert_eq!(
            text_states(Tool::Search, one, &hit(&["calls", "has_type"])),
            Ok(())
        );
        assert!(text_states(Tool::Search, all, &hit(&["calls", "has_type"])).is_err());
        assert!(text_states(Tool::Search, one, &hit(&["calls", "references"])).is_err());
    }

    #[test]
    fn a_score_is_compared_numerically() {
        let answer = with(search_answer(), "/results/0/score", json!(0.5));
        let text = SEARCH_TEXT.replace("score 2", "score 0.50");
        assert_eq!(
            answer.and_then(|answer| text_states(Tool::Search, &text, &answer)),
            Ok(())
        );
    }

    #[test]
    fn source_that_is_changed_or_not_indented_is_refused() -> Result<(), String> {
        search_refusal("    1\n", "    2\n", "$.results[0].source")?;
        search_refusal("\t\tpub fn a() {\n", "\tpub fn a() {\n", "$.results[0]")
    }

    #[test]
    fn single_source_is_unindented_and_ends_a_get_symbol_text() -> Result<(), String> {
        let text = edited(SYMBOL_TEXT, "\tpub fn a() {}\n", "\t\tpub fn a() {}\n")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("$.hits[0].source"), "{failure}");
        let trailing = format!("{SYMBOL_TEXT}\trift://symbol/rust/src/a.rs/a\n");
        let failure = refused(Tool::GetSymbol, &trailing, &symbol_answer())?;
        assert!(failure.contains("must end with the source"), "{failure}");
        let after = format!("{SYMBOL_TEXT}rift://symbol/rust/src/a.rs/a\n");
        let failure = refused(Tool::GetSymbol, &after, &symbol_answer())?;
        assert!(failure.contains("$.warnings"), "{failure}");
        Ok(())
    }

    #[test]
    fn an_empty_source_has_no_line_to_check() {
        let answer = with(symbol_answer(), "/hits/0/source", json!(""));
        let text = SYMBOL_TEXT.replace("\tpub fn a() {}\n", "\n");
        assert_eq!(
            answer.and_then(|answer| text_states(Tool::GetSymbol, &text, &answer)),
            Ok(())
        );
    }

    #[test]
    fn commit_facts_are_checked_one_by_one() -> Result<(), String> {
        search_refusal("9c1d4e7a", "9c1d4e7", "$.results[1].hit.commit.revision")?;
        search_refusal("Alice <", "Bob <", "$.results[1].hit.commit.author")?;
        search_refusal(
            "\t\tBound comparisons\n",
            "\t\tOther\n",
            "$.results[1].hit.commit.message",
        )?;
        search_refusal("\t\t\tb.rs\n", "", "$.results[1].hit.commit.paths[1]")
    }

    #[test]
    fn truncation_flags_state_their_lines_exactly() -> Result<(), String> {
        let message = "$.results[1].hit.commit.message_truncated";
        let paths = "$.results[1].hit.commit.paths_truncated";
        search_refusal("\t\tmessage truncated\n", "", message)?;
        search_refusal("2+ paths · truncated", "2 paths:", paths)?;
        let clear = |pointer: &str| with(search_answer(), pointer, json!(false));
        let failure = refused(
            Tool::Search,
            SEARCH_TEXT,
            &clear("/results/1/hit/commit/message_truncated")?,
        )?;
        assert!(failure.contains(message), "{failure}");
        let failure = refused(
            Tool::Search,
            SEARCH_TEXT,
            &clear("/results/1/hit/commit/paths_truncated")?,
        )?;
        assert!(failure.contains(paths), "{failure}");
        Ok(())
    }

    #[test]
    fn a_history_version_without_its_entry_is_refused() -> Result<(), String> {
        let text = edited(SYMBOL_TEXT, " · signature changed", " · signature_changed")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(
            failure.contains("$.hits[0].history.versions[0].kind"),
            "{failure}"
        );
        let text = edited(SYMBOL_TEXT, "1f2080e5", "1f2080e")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("versions[0].revision"), "{failure}");
        let text = edited(
            SYMBOL_TEXT,
            "\t\t2026-08-17 · HEAD~2 · introduced · Alice <alice@example.com>\n",
            "",
        )?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("versions[1]"), "{failure}");
        Ok(())
    }

    #[test]
    fn the_history_label_follows_the_complete_flag() -> Result<(), String> {
        let text = edited(SYMBOL_TEXT, "history (incomplete):", "history:")?;
        let failure = refused(Tool::GetSymbol, &text, &symbol_answer())?;
        assert!(failure.contains("$.hits[0].history.complete"), "{failure}");
        let complete = with(symbol_answer(), "/hits/0/history/complete", json!(true))?;
        let failure = refused(Tool::GetSymbol, SYMBOL_TEXT, &complete)?;
        assert!(failure.contains("$.hits[0].history.complete"), "{failure}");
        assert_eq!(text_states(Tool::GetSymbol, &text, &complete), Ok(()));
        Ok(())
    }

    #[test]
    fn a_documentation_node_and_file_hit_state_their_place() -> Result<(), String> {
        let answer = json!({
            "results": [
                {
                    "hit": { "target": "node", "node": "rift://node/rust/a.rs@0-9#3f9a1c2e" },
                    "matched_by": ["content"],
                    "path": "a.rs",
                    "line": 2
                },
                {
                    "hit": {
                        "target": "documentation",
                        "documentation": {
                            "block": {
                                "source": { "source": { "kind": "project", "path": "docs/g.md" } },
                                "line": 7,
                                "symbol": "rift://symbol/rust/a.rs/a"
                            }
                        }
                    },
                    "matched_by": ["documentation"],
                    "source": "# Guide"
                },
                { "hit": { "target": "file", "size": 3 }, "unit": "rift://source/p/a.rs" }
            ],
            "pagination": { "page_index": 0, "total_pages": 1 }
        });
        let text = "\
3 results
\t[1] a.rs:2 · content
\t\trift://node/rust/a.rs@0-9#3f9a1c2e
\t[2] Guide
\t\tdocs/g.md:7 · documentation
\t\trift://symbol/rust/a.rs/a
\t\t# Guide
\t[3] rift://source/p/a.rs
";
        assert_eq!(text_states(Tool::Search, text, &answer), Ok(()));
        let moved = edited(text, "docs/g.md:7", "docs/g.md:8")?;
        let failure = refused(Tool::Search, &moved, &answer)?;
        assert!(failure.contains("$.results[1]"), "{failure}");
        let unnumbered = edited(text, "\t[3] ", "\t")?;
        let failure = refused(Tool::Search, &unnumbered, &answer)?;
        assert!(failure.contains("$.results[2]"), "{failure}");
        Ok(())
    }

    #[test]
    fn control_characters_in_a_single_line_fact_are_escaped() {
        assert_eq!(visible("a\tb\nc\rd\u{1}"), "a\\tb\\nc\\rd\\u{1}");
        assert_eq!(cut_hash("0123456789abcdef"), "01234567");
        assert_eq!(cut_hash("main"), "main");
        assert_eq!(cut_hash("0123456"), "0123456");
    }

    /// A `nodes` answer: an outer node with a symbol, regions, and facets, and an inner node.
    fn nodes_answer() -> Value {
        json!({
            "nodes": [
                {
                    "id": "rift://node/rust/a.rs@0-20#aaaaaaaa",
                    "symbol": "rift://symbol/rust/a.rs/A",
                    "kind": "function_item",
                    "facets": ["declaration", "test", "generated"],
                    "range": { "start": 0, "end": 20 },
                    "regions": [
                        { "role": "name", "range": { "start": 3, "end": 4 } },
                        { "role": "body", "range": { "start": 8, "end": 20 } }
                    ]
                },
                {
                    "id": "rift://node/rust/a.rs@3-9#bbbbbbbb",
                    "kind": "identifier",
                    "range": { "start": 3, "end": 9 }
                }
            ],
            "source": ["fn a() {\n    b\n}\n// tail", "a\n\nb"],
            "warnings": [{ "code": "stale_index", "detail": "index lags" }]
        })
    }

    /// The text the layout writes for [`nodes_answer`].
    const NODES_TEXT: &str = "\
2 nodes
\t[1] function_item · 4 lines · generated · test
\t\trift://node/rust/a.rs@0-20#aaaaaaaa
\t\trift://symbol/rust/a.rs/A
\t\tname 3..4 · body 8..20
\t[2] identifier · 3 lines
\t\trift://node/rust/a.rs@3-9#bbbbbbbb

\t\ta

\t\tb
1 warning
\tstale_index: index lags
";

    /// Asserts the failure of the nodes text edited by `from` to `to` names `path`.
    fn nodes_refusal(from: &str, to: &str, path: &str) -> Result<(), String> {
        let text = edited(NODES_TEXT, from, to)?;
        let failure = refused(Tool::Nodes, &text, &nodes_answer())?;
        assert!(failure.contains(path), "{path} not in {failure}");
        Ok(())
    }

    #[test]
    fn the_layout_text_of_nodes_passes() {
        assert_eq!(
            text_states(Tool::Nodes, NODES_TEXT, &nodes_answer()),
            Ok(())
        );
    }

    #[test]
    fn one_node_has_no_marker_and_no_indent() {
        let answer = json!({
            "nodes": [{
                "id": "rift://node/rust/a.rs@3-9#bbbbbbbb",
                "kind": "identifier",
                "range": { "start": 3, "end": 9 }
            }],
            "source": ["name"]
        });
        let text = "1 node\n\tidentifier\n\trift://node/rust/a.rs@3-9#bbbbbbbb\n\n\tname\n";
        assert_eq!(text_states(Tool::Nodes, text, &answer), Ok(()));
        let indented = text.replace("\n\tname\n", "\n\t\tname\n");
        assert!(text_states(Tool::Nodes, &indented, &answer).is_err());
    }

    #[test]
    fn an_empty_nodes_answer_needs_only_its_header() {
        let answer = json!({ "nodes": [], "source": [] });
        assert_eq!(text_states(Tool::Nodes, "0 nodes\n", &answer), Ok(()));
        assert!(text_states(Tool::Nodes, "1 node\n", &answer).is_err());
    }

    #[test]
    fn a_wrong_node_count_or_a_page_in_the_header_is_refused() -> Result<(), String> {
        nodes_refusal("2 nodes", "3 nodes", "$.nodes")?;
        nodes_refusal("2 nodes", "2 results", "$.nodes")
    }

    #[test]
    fn a_node_head_states_its_kind_and_identity_in_order() -> Result<(), String> {
        nodes_refusal("[1] function_item", "[1] block", "$.nodes[0]")?;
        nodes_refusal("#aaaaaaaa", "#cccccccc", "$.nodes[0].id")?;
        nodes_refusal("[2] identifier", "[2] function_item", "$.nodes[1]")?;
        nodes_refusal("#bbbbbbbb", "#cccccccc", "$.nodes[1].id")?;
        nodes_refusal("\t\trift://symbol/rust/a.rs/A\n", "", "$.nodes[0].symbol")?;
        nodes_refusal("\t\tname 3..4 · body 8..20\n", "", "$.nodes[0].regions")?;
        nodes_refusal("name 3..4", "name 3..5", "$.nodes[0].regions")
    }

    #[test]
    fn the_line_count_and_the_facets_appear_exactly_when_they_hold() -> Result<(), String> {
        nodes_refusal(" · 4 lines", "", "$.source[0]")?;
        nodes_refusal(" · 3 lines", "", "$.source[1]")?;
        nodes_refusal("function_item · 4 lines", "function_item", "$.source[0]")?;
        nodes_refusal(" · generated", "", "$.nodes[0].facets")?;
        nodes_refusal(" · test", "", "$.nodes[0].facets")?;
        let answer = with(nodes_answer(), "/nodes/1/kind", json!("block"))?;
        let text = edited(NODES_TEXT, "[2] identifier", "[2] block")?;
        assert_eq!(text_states(Tool::Nodes, &text, &answer), Ok(()));
        let spurious = edited(
            NODES_TEXT,
            "[2] identifier · 3 lines",
            "[2] identifier · 2 lines",
        )?;
        let failure = refused(Tool::Nodes, &spurious, &nodes_answer())?;
        assert!(failure.contains("$.source[1]"), "{failure}");
        Ok(())
    }

    #[test]
    fn only_the_innermost_excerpt_ends_the_text_and_outer_excerpts_are_not_checked()
    -> Result<(), String> {
        nodes_refusal("\t\ta\n\n\t\tb\n", "\t\ta\n\n\t\tc\n", "$.source[1]")?;
        nodes_refusal("\t\ta\n\n\t\tb\n", "\t\ta\n\n\t\t b\n", "$.source[1]")?;
        nodes_refusal("\t\ta\n\n\t\tb\n", "\t\ta\n\nb\n", "$.warnings")?;
        let blank = edited(NODES_TEXT, "#bbbbbbbb\n\n\t\ta", "#bbbbbbbb\n\t\ta")?;
        let failure = refused(Tool::Nodes, &blank, &nodes_answer())?;
        assert!(failure.contains("$.source[1]"), "{failure}");
        Ok(())
    }

    #[test]
    fn a_missing_nodes_warnings_section_is_refused() -> Result<(), String> {
        nodes_refusal(
            "1 warning\n\tstale_index: index lags\n",
            "",
            "$.warnings[0].code",
        )
    }
}
