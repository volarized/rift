//! Model of the workspace configuration file `rift.toml`.
//!
//! Every type here is a contract: serde attributes define exactly what the
//! file may say, and the exported `rift.schema.json` derives from these
//! definitions. The numeric bounds the types advertise are enforced by
//! [`WorkspaceConfiguration::validate`]; a file that breaks one is refused
//! whole as `configuration_invalid`.

use std::collections::BTreeMap;

use crate::read::{CoverageScope, ProjectPath};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// Bytes one submitted execution block may hold, at most.
pub const EXECUTION_CODE_BYTES_MAX: u64 = 32 << 10;
/// Milliseconds one evaluation may run, at most: one day.
pub const EXECUTION_TIMEOUT_MS_MAX: u64 = 86_400_000;
/// Bytes one captured execution stream may keep, at most.
pub const EXECUTION_OUTPUT_BYTES_MAX: u64 = 16 << 10;
/// Evaluations admitted concurrently across the workspace, at most.
pub const EXECUTION_CONCURRENT_MAX: u64 = 64;
/// Entries `execution.allow` may hold, at most.
pub const EXECUTION_ALLOW_ITEMS_MAX: usize = 64;
/// Revisions the history provider may walk from the current head, at most.
pub const HISTORY_REVISIONS_MAX: u64 = 100_000;
/// Bytes `search.embedding` may hold, at most.
pub const EMBEDDING_MODEL_BYTES_MAX: usize = 128;
/// Configured hooks one workspace may declare, at most.
pub const HOOKS_MAX: usize = 32;
/// Bytes one hook's `id` may hold, at most.
pub const HOOK_ID_BYTES_MAX: usize = 64;
/// Entries one hook's `arguments` may hold, at most.
pub const HOOK_ARGUMENTS_MAX: usize = 64;
/// Entries one hook's `environment` may hold, at most.
pub const HOOK_ENVIRONMENT_ENTRIES_MAX: usize = 64;
/// Milliseconds one hook may run before Rift kills it, at most: one hour.
pub const HOOK_TIMEOUT_MS_MAX: u64 = 3_600_000;
/// Bytes of each hook stream Rift keeps, at least.
pub const HOOK_OUTPUT_BYTES_MIN: u64 = 256;
/// Bytes of each hook stream Rift keeps, at most.
pub const HOOK_OUTPUT_BYTES_MAX: u64 = 4_096;
/// Guarantees one hook may declare, at most.
pub const HOOK_GUARANTEES_MAX: usize = 16;
/// Bytes one guarantee's `detail` may hold, at most.
pub const HOOK_GUARANTEE_DETAIL_BYTES_MAX: usize = 1_024;

/// The spelling a [`ByteSize`] value must match.
const BYTE_SIZE_PATTERN: &str = "^(?:0|[1-9][0-9]*)(?:b|kb|mb|gb|tb)$";
/// The spelling a [`Duration`] value must match.
const DURATION_PATTERN: &str = "^(?:0|[1-9][0-9]*)(?:ms|s|m|h|d)$";

/// A byte size: an integer magnitude with a required unit suffix `b`, `kb`,
/// `mb`, `gb`, or `tb`. Units are binary: `1kb` is 1024 bytes, and every
/// larger unit scales by 1024 again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ByteSize(u64);

impl ByteSize {
    /// A size stated directly in bytes.
    #[must_use]
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The size in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Parses the file spelling: digits, then one of `b`, `kb`, `mb`, `gb`,
    /// or `tb`.
    ///
    /// # Errors
    ///
    /// Returns [`UnitParseError`] when the magnitude or suffix breaks the
    /// documented form, or the product overflows.
    pub fn parse(text: &str) -> Result<Self, UnitParseError> {
        let (magnitude, unit) = split_magnitude(text, ByteSize::EXPECTED)?;
        let scale: u64 = match unit {
            "b" => 1,
            "kb" => 1 << 10,
            "mb" => 1 << 20,
            "gb" => 1 << 30,
            "tb" => 1 << 40,
            _ => return Err(UnitParseError::new(text, ByteSize::EXPECTED)),
        };
        let bytes = magnitude
            .checked_mul(scale)
            .ok_or_else(|| UnitParseError::new(text, ByteSize::EXPECTED))?;
        Ok(Self(bytes))
    }

    /// The documented form, named in every parse failure.
    const EXPECTED: &'static str =
        "an integer magnitude followed by b, kb, mb, gb, or tb (binary units), such as `16kb`";
}

impl TryFrom<String> for ByteSize {
    type Error = UnitParseError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<ByteSize> for String {
    fn from(size: ByteSize) -> Self {
        let units = [
            (1_u64 << 40, "tb"),
            (1 << 30, "gb"),
            (1 << 20, "mb"),
            (1 << 10, "kb"),
        ];
        for (scale, suffix) in units {
            if size.0 != 0 && size.0.is_multiple_of(scale) {
                return format!("{}{suffix}", size.0 / scale);
            }
        }
        format!("{}b", size.0)
    }
}

impl JsonSchema for ByteSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ByteSize".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": BYTE_SIZE_PATTERN,
            "description": "A byte size: an integer magnitude with a required unit \
                            suffix `b`, `kb`, `mb`, `gb`, or `tb`; units are binary, \
                            so `1kb` is 1024 bytes."
        })
    }
}

/// A duration: an integer magnitude with a required unit suffix `ms`, `s`,
/// `m`, `h`, or `d`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Duration(u64);

impl Duration {
    /// A duration stated directly in milliseconds.
    #[must_use]
    pub const fn from_millis(milliseconds: u64) -> Self {
        Self(milliseconds)
    }

    /// The duration in milliseconds.
    #[must_use]
    pub const fn milliseconds(self) -> u64 {
        self.0
    }

    /// Parses the file spelling: digits, then one of `ms`, `s`, `m`, `h`,
    /// or `d`.
    ///
    /// # Errors
    ///
    /// Returns [`UnitParseError`] when the magnitude or suffix breaks the
    /// documented form, or the product overflows.
    pub fn parse(text: &str) -> Result<Self, UnitParseError> {
        let (magnitude, unit) = split_magnitude(text, Duration::EXPECTED)?;
        let scale: u64 = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return Err(UnitParseError::new(text, Duration::EXPECTED)),
        };
        let milliseconds = magnitude
            .checked_mul(scale)
            .ok_or_else(|| UnitParseError::new(text, Duration::EXPECTED))?;
        Ok(Self(milliseconds))
    }

    /// The documented form, named in every parse failure.
    const EXPECTED: &'static str =
        "an integer magnitude followed by ms, s, m, h, or d, such as `30s`";
}

impl TryFrom<String> for Duration {
    type Error = UnitParseError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Duration> for String {
    fn from(duration: Duration) -> Self {
        let units = [
            (86_400_000_u64, "d"),
            (3_600_000, "h"),
            (60_000, "m"),
            (1_000, "s"),
        ];
        for (scale, suffix) in units {
            if duration.0 != 0 && duration.0.is_multiple_of(scale) {
                return format!("{}{suffix}", duration.0 / scale);
            }
        }
        format!("{}ms", duration.0)
    }
}

impl JsonSchema for Duration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": DURATION_PATTERN,
            "description": "A duration: an integer magnitude with a required \
                            unit suffix `ms`, `s`, `m`, `h`, or `d`."
        })
    }
}

/// A magnitude-with-unit value that breaks its documented form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitParseError {
    text: String,
    expected: &'static str,
}

impl UnitParseError {
    fn new(text: &str, expected: &'static str) -> Self {
        Self {
            text: text.to_owned(),
            expected,
        }
    }

    /// The value that failed to parse, as the file spelled it.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.text
    }

    /// The documented form the value must match.
    #[must_use]
    pub const fn expected(&self) -> &'static str {
        self.expected
    }
}

impl std::fmt::Display for UnitParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "value {:?} does not match the documented form: expected {}",
            self.text, self.expected
        )
    }
}

impl std::error::Error for UnitParseError {}

/// Splits `<digits><unit>`. Digits are `0` or start with a nonzero digit, so
/// `007ms` is refused rather than silently normalized.
fn split_magnitude<'text>(
    text: &'text str,
    expected: &'static str,
) -> Result<(u64, &'text str), UnitParseError> {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(end);
    let leading_zero = digits.len() > 1 && digits.starts_with('0');
    if digits.is_empty() || leading_zero {
        return Err(UnitParseError::new(text, expected));
    }
    let magnitude = digits
        .parse::<u64>()
        .map_err(|_| UnitParseError::new(text, expected))?;
    Ok((magnitude, unit))
}

/// The whole `rift.toml`. Every table is optional and defaults to the
/// behavior documented on it; any key outside these definitions refuses the
/// file as `configuration_invalid`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfiguration {
    /// Bounds and switches for the built-in providers.
    pub providers: ProvidersConfiguration,
    /// Enablement and limits for caller-provided code.
    pub execution: ExecutionConfiguration,
    /// The embedding model that adds dense ranking to lexical search.
    pub search: SearchConfiguration,
    /// Hooks run in the changed tree, in list order, each time a change
    /// applies.
    #[schemars(length(max = 32))]
    pub hooks: Vec<CommandHook>,
}

impl WorkspaceConfiguration {
    /// Checks every bound the schema advertises.
    ///
    /// # Errors
    ///
    /// Returns the first [`ConfigurationViolation`] in declaration order, so
    /// a caller fixing the file converges one refusal at a time.
    pub fn validate(&self) -> Result<(), ConfigurationViolation> {
        match self.violation() {
            Some(violation) => Err(violation),
            None => Ok(()),
        }
    }

    /// The first violated bound, in the order the file declares its tables.
    fn violation(&self) -> Option<ConfigurationViolation> {
        self.execution
            .violation()
            .or_else(|| self.providers.history.violation())
            .or_else(|| self.search.violation())
            .or_else(|| hooks_violation(&self.hooks))
    }
}

/// The `[providers]` table: the built-in providers run without
/// configuration, and this table bounds or disables them.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfiguration {
    /// The history provider's budget.
    pub history: HistoryConfiguration,
}

/// The `[providers.history]` table. The history provider's cost scales with
/// how far back it walks, so its depth is the budget worth setting.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfiguration {
    /// Whether the history provider runs at all.
    pub enabled: bool,
    /// Revisions the walk covers from the current head, newest first.
    #[schemars(range(min = 1, max = 100_000))]
    pub max_revisions: u64,
}

impl Default for HistoryConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            max_revisions: 500,
        }
    }
}

impl HistoryConfiguration {
    fn violation(&self) -> Option<ConfigurationViolation> {
        out_of_range(
            "providers.history.max_revisions",
            self.max_revisions,
            1,
            HISTORY_REVISIONS_MAX,
        )
    }
}

/// The `[execution]` table. `execute` stays off until `allow` names the
/// languages caller-provided code may run as.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_execution_ranges)]
pub struct ExecutionConfiguration {
    /// Language selectors enabled for `execute`: a language name, or
    /// `name:dialect` to pin a dialect. Empty keeps execution off.
    #[schemars(length(max = 64))]
    pub allow: Vec<String>,
    /// Bytes one submitted block may hold, 1b to 32kb.
    pub max_code: ByteSize,
    /// Wall-clock bound per evaluation, 1ms to 1d.
    pub max_timeout: Duration,
    /// Bytes each captured stream keeps, up to 16kb.
    pub max_output: ByteSize,
    /// Evaluations running at once across the whole workspace.
    #[schemars(range(min = 1, max = 64))]
    pub max_concurrent: u64,
}

impl Default for ExecutionConfiguration {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            max_code: ByteSize::from_bytes(16 << 10),
            max_timeout: Duration::from_millis(30_000),
            max_output: ByteSize::from_bytes(8 << 10),
            max_concurrent: 2,
        }
    }
}

impl ExecutionConfiguration {
    /// The table's bounds in key order, then the selector rule.
    fn violation(&self) -> Option<ConfigurationViolation> {
        let limits = [
            (
                "execution.allow",
                self.allow.len() as u64,
                0,
                EXECUTION_ALLOW_ITEMS_MAX as u64,
            ),
            (
                "execution.max_code",
                self.max_code.bytes(),
                1,
                EXECUTION_CODE_BYTES_MAX,
            ),
            (
                "execution.max_timeout",
                self.max_timeout.milliseconds(),
                1,
                EXECUTION_TIMEOUT_MS_MAX,
            ),
            (
                "execution.max_output",
                self.max_output.bytes(),
                0,
                EXECUTION_OUTPUT_BYTES_MAX,
            ),
            (
                "execution.max_concurrent",
                self.max_concurrent,
                1,
                EXECUTION_CONCURRENT_MAX,
            ),
        ];
        first_out_of_range(limits).or_else(|| {
            self.allow
                .iter()
                .find(|selector| !is_language_selector(selector))
                .map(|selector| ConfigurationViolation::LanguageSelectorInvalid {
                    selector: selector.clone(),
                })
        })
    }
}

/// The `[search]` table. Search runs on a lexical index with nothing to
/// configure; `embedding` names the model that adds dense ranking on top.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfiguration {
    /// The embedding model identifier. Vectors are stored per model, so
    /// changing the value rebuilds the dense index; absent keeps search
    /// lexical.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub embedding: Option<String>,
}

impl SearchConfiguration {
    /// The model-identifier rules, one named clause each.
    fn violation(&self) -> Option<ConfigurationViolation> {
        let embedding = self.embedding.as_deref()?;
        let nonempty = !embedding.is_empty();
        let within_length = embedding.len() <= EMBEDDING_MODEL_BYTES_MAX;
        let charset_admitted = embedding.chars().all(is_model_identifier_character);
        let valid = nonempty && within_length && charset_admitted;
        (!valid).then(|| ConfigurationViolation::EmbeddingModelInvalid {
            value: embedding.to_owned(),
        })
    }
}

/// Whether `character` may appear in an embedding model identifier.
fn is_model_identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/')
}

/// One `[[hooks]]` block: an executable Rift starts directly — no shell —
/// inside the changed tree each time a change applies. Every key is
/// required; the schema carries no defaults.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandHook {
    /// How the hook runs; `command` is the only type.
    pub r#type: HookType,
    /// Label for this hook's results, unique within the list.
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    /// What the hook is: a test suite, a linter, a build, or something else.
    pub kind: HookKind,
    /// The executable Rift starts. An absolute path is refused; a bare name
    /// resolves through `PATH`, a relative one below `working_directory`.
    #[schemars(length(min = 1))]
    pub program: String,
    /// The program's literal arguments, in order. May be empty.
    #[schemars(length(max = 64))]
    pub arguments: Vec<String>,
    /// Whether the changed project paths are appended after `arguments` in
    /// byte order.
    pub changed_paths: ChangedPaths,
    /// Directory the process starts in, relative to the changed tree's
    /// root. Empty selects the root.
    pub working_directory: ProjectPath,
    /// Environment values added on top of the environment the server
    /// inherited.
    pub environment: BTreeMap<String, String>,
    /// Milliseconds before Rift kills the process, 1 to 3600000.
    #[schemars(range(min = 1, max = 3_600_000))]
    pub timeout_ms: u64,
    /// Bytes of each output stream Rift keeps, 256 to 4096. The full size
    /// is still reported.
    #[schemars(range(min = 256, max = 4_096))]
    pub output_limit_bytes: u64,
    /// What a passing run establishes. Each entry becomes evidence on the
    /// change the hook checked.
    #[schemars(length(max = 16))]
    pub guarantees: Vec<HookGuarantee>,
    /// Whether an identical tree and environment are expected to reproduce
    /// the result.
    pub determinism: Determinism,
}

/// How a hook runs; `command` — an executable started directly, without a
/// shell — is the only type.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    /// An executable Rift starts directly inside the changed tree.
    Command,
}

/// What a hook is, as workspace configuration presents it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// A test suite.
    Test,
    /// A linter.
    Lint,
    /// A build.
    Build,
    /// A hook none of the other kinds describe.
    Other,
}

/// Whether the changed project paths ride the hook's command line.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChangedPaths {
    /// The command runs exactly as configured.
    None,
    /// The changed project paths follow the configured `arguments`, in byte
    /// order, for a tool that takes files.
    Append,
}

/// Whether an identical tree and environment are expected to reproduce a
/// hook's result.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Determinism {
    /// The same tree and environment give the same answer.
    Deterministic,
    /// The result may vary between identical runs.
    BestEffort,
}

/// What a passing run of one hook establishes about the change it checked.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookGuarantee {
    /// Which property a pass establishes.
    pub kind: GuaranteeKind,
    /// What the check covers.
    pub scope: CoverageScope,
    /// The exact property the hook checks, and the limits on reading a pass.
    #[schemars(length(min = 1, max = 1_024))]
    pub detail: String,
}

/// A property a verifier can establish about an applied change.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GuaranteeKind {
    /// The changed source parses.
    SyntaxValidated,
    /// Names still resolve to what they resolved to before the change.
    BindingsPreserved,
    /// References to the changed declarations were updated with them.
    ReferencesUpdated,
    /// The change's behavior was exercised, such as by a test suite.
    BehaviorChecked,
}

/// The first bound a configuration file breaks. Field paths name keys as
/// the file spells them, so the refusal points at the line to fix.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationViolation {
    /// A numeric key sits outside its documented range.
    LimitOutOfRange {
        /// The key's path in the file, such as `execution.max_code`.
        field: &'static str,
        /// The configured value, in the key's base unit.
        value: u64,
        /// The smallest admitted value.
        min: u64,
        /// The largest admitted value.
        max: u64,
    },
    /// An `execution.allow` entry is not `name` or `name:dialect`.
    LanguageSelectorInvalid {
        /// The rejected entry.
        selector: String,
    },
    /// The `search.embedding` value is not a model identifier.
    EmbeddingModelInvalid {
        /// The rejected value.
        value: String,
    },
    /// Two hooks share one id, so their results could not be told apart.
    HookIdDuplicate {
        /// The id both hooks claim.
        id: String,
    },
    /// A hook id is empty, too long, or uses characters outside
    /// `A-Z a-z 0-9 . _ -`.
    HookIdInvalid {
        /// The rejected id.
        id: String,
    },
    /// A hook's `program` is empty, so there is nothing to run.
    HookProgramEmpty {
        /// The hook missing its executable.
        id: String,
    },
    /// A hook names its executable by absolute path, which would escape the
    /// changed tree's `PATH` policy.
    HookExecutableAbsolute {
        /// The hook naming the executable.
        id: String,
        /// The refused executable path.
        program: String,
    },
    /// A hook environment key is empty, or carries `=` or a NUL byte.
    HookEnvironmentKeyInvalid {
        /// The hook declaring the entry.
        id: String,
        /// The rejected key.
        key: String,
    },
}

impl ConfigurationViolation {
    /// The violation's evidence as stable key-value pairs, for error
    /// context.
    #[must_use]
    pub fn evidence(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::LimitOutOfRange {
                field,
                value,
                min,
                max,
            } => vec![
                ("field", (*field).to_owned()),
                ("value", value.to_string()),
                ("range", format!("{min}..={max}")),
            ],
            Self::LanguageSelectorInvalid { selector } => {
                vec![("selector", selector.clone())]
            }
            Self::EmbeddingModelInvalid { value } => vec![("value", value.clone())],
            Self::HookIdDuplicate { id } | Self::HookIdInvalid { id } => {
                vec![("id", id.clone())]
            }
            Self::HookProgramEmpty { id } => vec![("id", id.clone())],
            Self::HookExecutableAbsolute { id, program } => {
                vec![("id", id.clone()), ("program", program.clone())]
            }
            Self::HookEnvironmentKeyInvalid { id, key } => {
                vec![("id", id.clone()), ("key", key.clone())]
            }
        }
    }
}

/// The bound check every numeric configuration key runs through.
fn out_of_range(
    field: &'static str,
    value: u64,
    min: u64,
    max: u64,
) -> Option<ConfigurationViolation> {
    (value < min || value > max).then_some(ConfigurationViolation::LimitOutOfRange {
        field,
        value,
        min,
        max,
    })
}

/// The first table row whose value sits outside its range, rows in key
/// order.
fn first_out_of_range<const ROWS: usize>(
    limits: [(&'static str, u64, u64, u64); ROWS],
) -> Option<ConfigurationViolation> {
    limits
        .into_iter()
        .find_map(|(field, value, min, max)| out_of_range(field, value, min, max))
}

/// Whether `selector` is a language name or `name:dialect`, both in the
/// lowercase form language identities use.
fn is_language_selector(selector: &str) -> bool {
    let (name, dialect) = match selector.split_once(':') {
        Some((name, dialect)) => (name, Some(dialect)),
        None => (selector, None),
    };
    is_language_word(name) && dialect.is_none_or(is_language_word)
}

/// Whether `word` matches the lowercase language-identifier form.
fn is_language_word(word: &str) -> bool {
    let mut characters = word.chars();
    let starts_lowercase = characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase());
    starts_lowercase
        && word.len() <= 64
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        })
}

/// The first violated hook bound, hooks in list order.
fn hooks_violation(hooks: &[CommandHook]) -> Option<ConfigurationViolation> {
    if hooks.len() > HOOKS_MAX {
        return out_of_range("hooks", hooks.len() as u64, 0, HOOKS_MAX as u64);
    }
    let mut seen = std::collections::BTreeSet::new();
    for hook in hooks {
        if let Some(violation) = hook_violation(hook) {
            return Some(violation);
        }
        if !seen.insert(hook.id.as_str()) {
            return Some(ConfigurationViolation::HookIdDuplicate {
                id: hook.id.clone(),
            });
        }
    }
    None
}

/// The first bound one hook breaks, rules in key order.
fn hook_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    identity_violation(hook)
        .or_else(|| command_violation(hook))
        .or_else(|| environment_violation(hook))
        .or_else(|| guarantee_violation(hook))
        .or_else(|| hook_bounds_violation(hook))
}

/// The `id` rule: the label every result of this hook carries.
fn identity_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    (!is_hook_id(&hook.id)).then(|| ConfigurationViolation::HookIdInvalid {
        id: hook.id.clone(),
    })
}

/// The rules on what the hook runs: a present, non-absolute program.
fn command_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    if hook.program.is_empty() {
        return Some(ConfigurationViolation::HookProgramEmpty {
            id: hook.id.clone(),
        });
    }
    is_absolute_program(&hook.program).then(|| ConfigurationViolation::HookExecutableAbsolute {
        id: hook.id.clone(),
        program: hook.program.clone(),
    })
}

/// The rule on every `environment` entry's key.
fn environment_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    let key = hook
        .environment
        .keys()
        .find(|key| !is_environment_key(key))?;
    Some(ConfigurationViolation::HookEnvironmentKeyInvalid {
        id: hook.id.clone(),
        key: key.clone(),
    })
}

/// The `detail` length rule on every declared guarantee.
fn guarantee_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    hook.guarantees.iter().find_map(|guarantee| {
        out_of_range(
            "hooks.guarantees.detail",
            guarantee.detail.len() as u64,
            1,
            HOOK_GUARANTEE_DETAIL_BYTES_MAX as u64,
        )
    })
}

/// The numeric bounds one hook carries, as a table in key order.
fn hook_bounds_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    let limits = [
        (
            "hooks.arguments",
            hook.arguments.len() as u64,
            0,
            HOOK_ARGUMENTS_MAX as u64,
        ),
        (
            "hooks.environment",
            hook.environment.len() as u64,
            0,
            HOOK_ENVIRONMENT_ENTRIES_MAX as u64,
        ),
        ("hooks.timeout_ms", hook.timeout_ms, 1, HOOK_TIMEOUT_MS_MAX),
        (
            "hooks.output_limit_bytes",
            hook.output_limit_bytes,
            HOOK_OUTPUT_BYTES_MIN,
            HOOK_OUTPUT_BYTES_MAX,
        ),
        (
            "hooks.guarantees",
            hook.guarantees.len() as u64,
            0,
            HOOK_GUARANTEES_MAX as u64,
        ),
    ];
    first_out_of_range(limits)
}

/// Whether `id` labels a hook: nonempty, at most
/// [`HOOK_ID_BYTES_MAX`] bytes, characters from `A-Z a-z 0-9 . _ -`.
fn is_hook_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= HOOK_ID_BYTES_MAX
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

/// Whether `key` can name an environment entry: nonempty, without `=` or a
/// NUL byte.
fn is_environment_key(key: &str) -> bool {
    !key.is_empty() && !key.contains('=') && !key.contains('\0')
}

/// Whether `program` is an absolute path on any platform the workspace can
/// run on: a `/` root, a backslash, or a drive prefix.
fn is_absolute_program(program: &str) -> bool {
    match program.as_bytes() {
        [b'/' | b'\\', ..] => true,
        [drive, b':', ..] if drive.is_ascii_alphabetic() => true,
        _ => program.contains('\\'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook() -> CommandHook {
        CommandHook {
            r#type: HookType::Command,
            id: "tests".to_owned(),
            kind: HookKind::Test,
            program: "cargo".to_owned(),
            arguments: vec!["test".to_owned()],
            changed_paths: ChangedPaths::None,
            working_directory: ProjectPath(String::new()),
            environment: BTreeMap::new(),
            timeout_ms: 120_000,
            output_limit_bytes: 4_096,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
    }

    #[test]
    fn test_byte_size_parses_every_documented_unit() {
        let cases = [
            ("0b", 0),
            ("1b", 1),
            ("16kb", 16 << 10),
            ("3mb", 3 << 20),
            ("2gb", 2 << 30),
            ("1tb", 1 << 40),
        ];
        for (text, bytes) in cases {
            assert_eq!(
                ByteSize::parse(text),
                Ok(ByteSize::from_bytes(bytes)),
                "{text}"
            );
        }
    }

    #[test]
    fn test_byte_size_refuses_malformed_spellings() {
        for text in [
            "", "16", "kb", "016kb", "16KB", "16KiB", "16kib", "16 kb", "-1b", "1.5kb",
        ] {
            assert!(ByteSize::parse(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn test_byte_size_refuses_overflow() {
        assert!(ByteSize::parse("99999999999tb").is_err());
        assert!(ByteSize::parse("999999999999999999999b").is_err());
    }

    #[test]
    fn test_base_unit_accessors_return_the_scaled_magnitude() {
        assert_eq!(ByteSize::parse("2kb").expect("spelling").bytes(), 2 << 10);
        assert_eq!(
            Duration::parse("2m").expect("spelling").milliseconds(),
            120_000
        );
    }

    #[test]
    fn test_byte_size_renders_largest_exact_unit() {
        let cases = [
            (0, "0b"),
            (1, "1b"),
            (16 << 10, "16kb"),
            ((16 << 10) + 1, "16385b"),
        ];
        for (bytes, text) in cases {
            assert_eq!(String::from(ByteSize::from_bytes(bytes)), text);
        }
    }

    #[test]
    fn test_duration_parses_every_documented_unit() {
        let cases = [
            ("0ms", 0),
            ("250ms", 250),
            ("30s", 30_000),
            ("5m", 300_000),
            ("2h", 7_200_000),
            ("1d", 86_400_000),
        ];
        for (text, milliseconds) in cases {
            assert_eq!(
                Duration::parse(text),
                Ok(Duration::from_millis(milliseconds)),
                "{text}"
            );
        }
    }

    #[test]
    fn test_duration_refuses_malformed_spellings_and_overflow() {
        for text in [
            "",
            "30",
            "s",
            "030s",
            "30S",
            "30 s",
            "1w",
            "99999999999999999d",
        ] {
            assert!(Duration::parse(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn test_duration_renders_largest_exact_unit() {
        let cases = [(0, "0ms"), (250, "250ms"), (30_000, "30s"), (90_000, "90s")];
        for (milliseconds, text) in cases {
            assert_eq!(String::from(Duration::from_millis(milliseconds)), text);
        }
    }

    #[test]
    fn test_unit_parse_error_names_the_expected_form() {
        let error = ByteSize::parse("16KiB").expect_err("an uppercase unit must be refused");
        let message = error.to_string();
        assert!(
            message.contains("16KiB") && message.contains("16kb"),
            "the failure must show the value and the documented form: {message}"
        );
        assert_eq!(error.value(), "16KiB");
        assert_eq!(error.expected(), ByteSize::EXPECTED);
    }

    #[test]
    fn test_values_round_trip_through_serde_in_canonical_spelling() {
        let size: ByteSize = serde_json::from_value(json!("16kb")).expect("spelling must admit");
        assert_eq!(serde_json::to_value(size).expect("render"), json!("16kb"));
        let duration: Duration = serde_json::from_value(json!("90s")).expect("spelling must admit");
        assert_eq!(
            serde_json::to_value(duration).expect("render"),
            json!("90s")
        );
        let refused = serde_json::from_value::<ByteSize>(json!("16KiB"))
            .expect_err("the serde boundary must refuse what parse refuses");
        assert!(
            refused.to_string().contains("16kb"),
            "the serde failure must carry the documented form: {refused}"
        );
    }

    #[test]
    fn test_empty_configuration_carries_the_documented_defaults() {
        let configuration: WorkspaceConfiguration =
            serde_json::from_value(json!({})).expect("empty configuration must deserialize");
        assert_eq!(configuration, WorkspaceConfiguration::default());
        let execution = &configuration.execution;
        assert_eq!(execution.max_code, ByteSize::from_bytes(16 << 10));
        assert_eq!(execution.max_timeout, Duration::from_millis(30_000));
        assert_eq!(execution.max_output, ByteSize::from_bytes(8 << 10));
        assert_eq!(execution.max_concurrent, 2);
        assert!(configuration.providers.history.enabled);
        assert_eq!(configuration.providers.history.max_revisions, 500);
        assert_eq!(configuration.search.embedding, None);
        assert!(configuration.hooks.is_empty());
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_unknown_keys_are_refused_at_every_level() {
        let cases = [
            json!({ "unknown": true }),
            json!({ "execution": { "unknown": true } }),
            json!({ "providers": { "unknown": {} } }),
            json!({ "providers": { "history": { "unknown": 1 } } }),
            json!({ "search": { "unknown": "x" } }),
        ];
        for case in cases {
            assert!(
                serde_json::from_value::<WorkspaceConfiguration>(case.clone()).is_err(),
                "{case} must be refused"
            );
        }
    }

    #[test]
    fn test_hook_block_requires_every_key() {
        let complete = serde_json::to_value(hook()).expect("serialize");
        let object = complete.as_object().expect("hook serializes to an object");
        for missing in object.keys() {
            let mut trimmed = object.clone();
            trimmed.remove(missing);
            assert!(
                serde_json::from_value::<CommandHook>(serde_json::Value::Object(trimmed)).is_err(),
                "a hook without {missing} must be refused"
            );
        }
    }

    /// One way to break an execution bound, and the field the refusal names.
    type ExecutionBoundCase = (fn(&mut ExecutionConfiguration), &'static str);

    #[test]
    fn test_execution_bounds_are_enforced() {
        let cases: [ExecutionBoundCase; 4] = [
            (
                |execution| execution.max_code = ByteSize::from_bytes(EXECUTION_CODE_BYTES_MAX + 1),
                "execution.max_code",
            ),
            (
                |execution| {
                    execution.max_timeout = Duration::from_millis(EXECUTION_TIMEOUT_MS_MAX + 1);
                },
                "execution.max_timeout",
            ),
            (
                |execution| {
                    execution.max_output = ByteSize::from_bytes(EXECUTION_OUTPUT_BYTES_MAX + 1);
                },
                "execution.max_output",
            ),
            (
                |execution| execution.max_concurrent = 0,
                "execution.max_concurrent",
            ),
        ];
        for (break_bound, expected_field) in cases {
            let mut configuration = WorkspaceConfiguration::default();
            break_bound(&mut configuration.execution);
            let violation = configuration
                .validate()
                .expect_err("the broken bound must refuse the configuration");
            let named_field = violation
                .evidence()
                .first()
                .map(|(key, value)| (*key, value.clone()));
            assert_eq!(
                named_field,
                Some(("field", (*expected_field).to_owned())),
                "unexpected violation {violation:?}"
            );
        }
    }

    #[test]
    fn test_execution_bounds_admit_their_edges() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.execution.max_code = ByteSize::from_bytes(EXECUTION_CODE_BYTES_MAX);
        configuration.execution.max_timeout = Duration::from_millis(EXECUTION_TIMEOUT_MS_MAX);
        configuration.execution.max_output = ByteSize::from_bytes(0);
        configuration.execution.max_concurrent = EXECUTION_CONCURRENT_MAX;
        configuration.providers.history.max_revisions = HISTORY_REVISIONS_MAX;
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_history_depth_bound_is_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.providers.history.max_revisions = 0;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "providers.history.max_revisions",
                ..
            })
        ));
    }

    #[test]
    fn test_language_selectors_admit_names_and_dialects() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.execution.allow = vec!["python".to_owned(), "sql:postgresql".to_owned()];
        assert_eq!(configuration.validate(), Ok(()));
        for selector in ["", "Python", "sql:", ":postgresql", "sql:Post", "a b"] {
            configuration.execution.allow = vec![selector.to_owned()];
            let violation = configuration
                .validate()
                .expect_err("the selector must refuse the configuration");
            let evidence_key = violation.evidence().first().map(|(key, _)| *key);
            assert_eq!(
                evidence_key,
                Some("selector"),
                "{selector:?} gave {violation:?}"
            );
        }
    }

    #[test]
    fn test_embedding_model_identifier_is_checked() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.embedding = Some("potion-retrieval-32M".to_owned());
        assert_eq!(configuration.validate(), Ok(()));
        configuration.search.embedding = Some("spaced out".to_owned());
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::EmbeddingModelInvalid { .. })
        ));
    }

    /// One way to break a hook bound, and the check its refusal must pass.
    type HookBoundCase = (fn(&mut CommandHook), fn(&ConfigurationViolation) -> bool);

    /// Proves each broken hook is refused with the expected violation, and
    /// that the matcher rejects an unrelated one.
    fn assert_hooks_refused<const CASES: usize>(break_and_expect: [HookBoundCase; CASES]) {
        let unrelated = ConfigurationViolation::HookIdDuplicate {
            id: "unrelated".to_owned(),
        };
        for (break_bound, expected) in break_and_expect {
            let mut broken = hook();
            break_bound(&mut broken);
            let configuration = WorkspaceConfiguration {
                hooks: vec![broken],
                ..WorkspaceConfiguration::default()
            };
            let violation = configuration
                .validate()
                .expect_err("the broken hook must be refused");
            assert!(expected(&violation), "unexpected violation {violation:?}");
            assert!(
                !expected(&unrelated),
                "the matcher must reject an unrelated violation"
            );
        }
    }

    #[test]
    fn test_hook_command_rules_are_enforced() {
        let break_and_expect: [HookBoundCase; 4] = [
            (
                |hook| hook.id = "spaced id".to_owned(),
                |violation| matches!(violation, ConfigurationViolation::HookIdInvalid { .. }),
            ),
            (
                |hook| hook.program = String::new(),
                |violation| matches!(violation, ConfigurationViolation::HookProgramEmpty { .. }),
            ),
            (
                |hook| hook.program = "/bin/echo".to_owned(),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::HookExecutableAbsolute { .. }
                    )
                },
            ),
            (
                |hook| hook.program = "C:\\tools\\echo.exe".to_owned(),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::HookExecutableAbsolute { .. }
                    )
                },
            ),
        ];
        assert_hooks_refused(break_and_expect);
    }

    #[test]
    fn test_hook_bounds_are_enforced() {
        let break_and_expect: [HookBoundCase; 5] = [
            (
                |hook| hook.arguments = vec![String::new(); HOOK_ARGUMENTS_MAX + 1],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.arguments",
                            ..
                        }
                    )
                },
            ),
            (
                |hook| {
                    hook.guarantees = vec![
                        HookGuarantee {
                            kind: GuaranteeKind::BehaviorChecked,
                            scope: CoverageScope::Reach {
                                reach: crate::read::CoverageReach::Project,
                            },
                            detail: "the suite ran".to_owned(),
                        };
                        HOOK_GUARANTEES_MAX + 1
                    ];
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.guarantees",
                            ..
                        }
                    )
                },
            ),
            (
                |hook| {
                    hook.guarantees = vec![HookGuarantee {
                        kind: GuaranteeKind::BehaviorChecked,
                        scope: CoverageScope::Reach {
                            reach: crate::read::CoverageReach::Project,
                        },
                        detail: String::new(),
                    }];
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.guarantees.detail",
                            ..
                        }
                    )
                },
            ),
            (
                |hook| hook.timeout_ms = 0,
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.timeout_ms",
                            ..
                        }
                    )
                },
            ),
            (
                |hook| hook.output_limit_bytes = HOOK_OUTPUT_BYTES_MIN - 1,
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.output_limit_bytes",
                            ..
                        }
                    )
                },
            ),
        ];
        assert_hooks_refused(break_and_expect);
    }

    #[test]
    fn test_duplicate_hook_ids_are_refused() {
        let configuration = WorkspaceConfiguration {
            hooks: vec![hook(), hook()],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::HookIdDuplicate { .. })
        ));
    }

    #[test]
    fn test_hook_environment_keys_are_checked() {
        let mut broken = hook();
        broken
            .environment
            .insert("BAD=KEY".to_owned(), "1".to_owned());
        let configuration = WorkspaceConfiguration {
            hooks: vec![broken],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::HookEnvironmentKeyInvalid { .. })
        ));
    }

    #[test]
    fn test_violation_evidence_names_field_value_and_range() {
        let violation = ConfigurationViolation::LimitOutOfRange {
            field: "hooks.timeout_ms",
            value: 0,
            min: 1,
            max: HOOK_TIMEOUT_MS_MAX,
        };
        assert_eq!(
            violation.evidence(),
            vec![
                ("field", "hooks.timeout_ms".to_owned()),
                ("value", "0".to_owned()),
                ("range", "1..=3600000".to_owned()),
            ]
        );
    }

    #[test]
    fn test_every_violation_variant_carries_its_evidence() {
        let id = || "tests".to_owned();
        let cases = [
            (
                ConfigurationViolation::LanguageSelectorInvalid {
                    selector: "Python".to_owned(),
                },
                vec![("selector", "Python".to_owned())],
            ),
            (
                ConfigurationViolation::EmbeddingModelInvalid {
                    value: "spaced out".to_owned(),
                },
                vec![("value", "spaced out".to_owned())],
            ),
            (
                ConfigurationViolation::HookIdDuplicate { id: id() },
                vec![("id", id())],
            ),
            (
                ConfigurationViolation::HookIdInvalid { id: id() },
                vec![("id", id())],
            ),
            (
                ConfigurationViolation::HookProgramEmpty { id: id() },
                vec![("id", id())],
            ),
            (
                ConfigurationViolation::HookExecutableAbsolute {
                    id: id(),
                    program: "/bin/echo".to_owned(),
                },
                vec![("id", id()), ("program", "/bin/echo".to_owned())],
            ),
            (
                ConfigurationViolation::HookEnvironmentKeyInvalid {
                    id: id(),
                    key: "BAD=KEY".to_owned(),
                },
                vec![("id", id()), ("key", "BAD=KEY".to_owned())],
            ),
        ];
        for (violation, expected) in cases {
            assert_eq!(violation.evidence(), expected, "{violation:?}");
        }
    }

    #[test]
    fn test_hook_count_above_the_cap_is_refused() {
        let hooks = (0..=HOOKS_MAX)
            .map(|index| {
                let mut numbered = hook();
                numbered.id = format!("hook-{index}");
                numbered
            })
            .collect();
        let configuration = WorkspaceConfiguration {
            hooks,
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange { field: "hooks", .. })
        ));
    }

    #[test]
    fn test_hook_round_trips_through_json_with_exact_wire_names() {
        let value = serde_json::to_value(hook()).expect("serialize");
        assert_eq!(value["type"], json!("command"));
        assert_eq!(value["changed_paths"], json!("none"));
        assert_eq!(value["determinism"], json!("deterministic"));
        let round_tripped: CommandHook = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, hook());
    }

    #[test]
    fn test_size_and_duration_schemas_are_pattern_bound_strings() {
        let byte_size = serde_json::to_value(schemars::schema_for!(ByteSize)).expect("schema");
        assert_eq!(byte_size["type"], json!("string"));
        assert_eq!(byte_size["pattern"], json!(BYTE_SIZE_PATTERN));
        let duration = serde_json::to_value(schemars::schema_for!(Duration)).expect("schema");
        assert_eq!(duration["type"], json!("string"));
        assert_eq!(duration["pattern"], json!(DURATION_PATTERN));
    }

    /// Attribute arguments take only literals, so the schema attributes
    /// restate the bound constants; this pins each advertised bound to the
    /// constant `validate` enforces.
    #[test]
    fn test_schema_attribute_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let definitions = &schema["$defs"];
        let execution = &definitions["ExecutionConfiguration"]["properties"];
        let history = &definitions["HistoryConfiguration"]["properties"];
        let search = &definitions["SearchConfiguration"]["properties"];
        let hook = &definitions["CommandHook"]["properties"];
        let guarantee = &definitions["HookGuarantee"]["properties"];
        let cases = [
            (
                "hooks max",
                &schema["properties"]["hooks"]["maxItems"],
                json!(HOOKS_MAX),
            ),
            (
                "allow max",
                &execution["allow"]["maxItems"],
                json!(EXECUTION_ALLOW_ITEMS_MAX),
            ),
            (
                "concurrent min",
                &execution["max_concurrent"]["minimum"],
                json!(1),
            ),
            (
                "concurrent max",
                &execution["max_concurrent"]["maximum"],
                json!(EXECUTION_CONCURRENT_MAX),
            ),
            (
                "revisions min",
                &history["max_revisions"]["minimum"],
                json!(1),
            ),
            (
                "revisions max",
                &history["max_revisions"]["maximum"],
                json!(HISTORY_REVISIONS_MAX),
            ),
            ("embedding min", &search["embedding"]["minLength"], json!(1)),
            (
                "embedding max",
                &search["embedding"]["maxLength"],
                json!(EMBEDDING_MODEL_BYTES_MAX),
            ),
            ("id min", &hook["id"]["minLength"], json!(1)),
            ("id max", &hook["id"]["maxLength"], json!(HOOK_ID_BYTES_MAX)),
            ("program min", &hook["program"]["minLength"], json!(1)),
            (
                "arguments max",
                &hook["arguments"]["maxItems"],
                json!(HOOK_ARGUMENTS_MAX),
            ),
            ("timeout min", &hook["timeout_ms"]["minimum"], json!(1)),
            (
                "timeout max",
                &hook["timeout_ms"]["maximum"],
                json!(HOOK_TIMEOUT_MS_MAX),
            ),
            (
                "output min",
                &hook["output_limit_bytes"]["minimum"],
                json!(HOOK_OUTPUT_BYTES_MIN),
            ),
            (
                "output max",
                &hook["output_limit_bytes"]["maximum"],
                json!(HOOK_OUTPUT_BYTES_MAX),
            ),
            (
                "guarantees max",
                &hook["guarantees"]["maxItems"],
                json!(HOOK_GUARANTEES_MAX),
            ),
            ("detail min", &guarantee["detail"]["minLength"], json!(1)),
            (
                "detail max",
                &guarantee["detail"]["maxLength"],
                json!(HOOK_GUARANTEE_DETAIL_BYTES_MAX),
            ),
        ];
        for (name, advertised, enforced) in cases {
            assert_eq!(
                advertised, &enforced,
                "the schema's {name} bound must equal the enforced constant"
            );
        }
    }
}
