//! Model of the workspace configuration file `rift.toml`.
//!
//! Every type here is a contract: serde attributes define exactly what the
//! file may say, and the exported `rift.schema.json` derives from these
//! definitions. The numeric bounds the types advertise are enforced by
//! [`WorkspaceConfiguration::validate`]; a file that breaks one is refused
//! whole as `configuration_invalid`.

use std::collections::BTreeMap;

use crate::lock::{SERVER_PORT_FLOOR, SERVER_PORT_MAX, SERVER_PORT_MIN};
use crate::read::{CoverageScope, ProjectPath};
use crate::retry::{
    RESTART_ATTEMPTS_MAX, RESTART_ATTEMPTS_MIN, RESTART_WINDOW_MS_MAX, RESTART_WINDOW_MS_MIN,
    RETRY_ATTEMPTS_MAX, RETRY_ATTEMPTS_MIN, RETRY_DELAY_LIMIT_MS_MAX, RETRY_DELAY_LIMIT_MS_MIN,
    RETRY_DELAY_MS_MAX, RETRY_DELAY_MS_MIN, RestartPolicy, RetryPolicy,
};
use crate::search::path_pattern_violation;
use crate::source::SourceConfiguration;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// Workers the server's blocking pool may hold, at most.
pub const SERVER_NUM_WORKERS_MAX: u64 = 64;
/// Milliseconds one request may wait for a free worker, at most: one hour.
pub const SERVER_QUEUE_TIMEOUT_MS_MAX: u64 = 3_600_000;
/// Milliseconds the server serves with no request before it stops, at
/// least: one second.
pub const SERVER_IDLE_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds the server serves with no request before it stops, at
/// most: one day.
pub const SERVER_IDLE_TIMEOUT_MS_MAX: u64 = 86_400_000;
/// Bytes one submitted execution block may hold, at most.
pub const EXECUTION_CODE_BYTES_MAX: u64 = 32 << 10;
/// Milliseconds one evaluation may run, at most: one day.
pub const EXECUTION_TIMEOUT_MS_MAX: u64 = 86_400_000;
/// Bytes one captured execution stream may keep, at most.
pub const EXECUTION_OUTPUT_BYTES_MAX: u64 = 16 << 10;
/// Evaluations running concurrently across the workspace, at most.
pub const EXECUTION_CONCURRENT_MAX: u64 = 64;
/// Entries `execution.allow` may hold, at most.
pub const EXECUTION_ALLOW_ITEMS_MAX: usize = 64;
/// Revisions the history provider may walk from the current head, at most.
pub const HISTORY_REVISIONS_MAX: u64 = 100_000;
/// Bytes `search.semantic.model` may hold, at most.
pub const SEMANTIC_MODEL_BYTES_MAX: usize = 128;
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
/// Configured language engines one workspace may declare, at most.
pub const ENGINES_MAX: usize = 16;
/// Entries one engine's `arguments` may hold, at most.
pub const ENGINE_ARGUMENTS_MAX: usize = 64;
/// Bytes one engine argument may hold, at most.
pub const ENGINE_ARGUMENT_BYTES_MAX: usize = 4_096;
/// Entries one engine's `environment` may hold, at most.
pub const ENGINE_ENVIRONMENT_ENTRIES_MAX: usize = 64;
/// Entries one engine's `languages` may hold, at most.
pub const ENGINE_LANGUAGES_MAX: usize = 16;
/// Milliseconds one engine may take to initialize, at least: one second.
pub const ENGINE_STARTUP_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one engine may take to initialize, at most: ten minutes.
pub const ENGINE_STARTUP_TIMEOUT_MS_MAX: u64 = 600_000;
/// Milliseconds `engines.startup_timeout` holds when the key is absent.
const ENGINE_STARTUP_TIMEOUT_MS_DEFAULT: u64 = 30_000;
/// Milliseconds one engine request may run, at least: one second.
pub const ENGINE_REQUEST_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one engine request may run, at most: ten minutes.
pub const ENGINE_REQUEST_TIMEOUT_MS_MAX: u64 = 600_000;
/// Milliseconds `engines.request_timeout` holds when the key is absent.
const ENGINE_REQUEST_TIMEOUT_MS_DEFAULT: u64 = 60_000;
/// Bytes of each engine's standard error Rift keeps, at least.
pub const ENGINE_OUTPUT_BYTES_MIN: u64 = 1_024;
/// Bytes of each engine's standard error Rift keeps, at most.
pub const ENGINE_OUTPUT_BYTES_MAX: u64 = 8 << 20;
/// Bytes `engines.output_limit` holds when the key is absent.
const ENGINE_OUTPUT_BYTES_DEFAULT: u64 = 4_096;

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
    /// The server's own blocking-work bounds: worker count and queue wait.
    pub server: ServerConfiguration,
    /// Bounds and switches for the built-in providers.
    pub providers: ProvidersConfiguration,
    /// Enablement and limits for caller-provided code.
    pub execution: ExecutionConfiguration,
    /// The lexical search index: which non-source files join it, the `SQLite` bounds behind
    /// it, and the embedding model that adds dense ranking on top.
    pub search: SearchConfiguration,
    /// Which files below the workspace root the index and reads consider visible.
    pub source: SourceConfiguration,
    /// Hooks run in the changed tree, in list order, each time a change
    /// applies.
    #[schemars(length(max = 32))]
    pub hooks: Vec<CommandHook>,
    /// Language engines keyed by name: each `[engines.<name>]` table names
    /// an external LSP server and the languages it serves.
    pub engines: BTreeMap<String, EngineConfiguration>,
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
        self.server
            .violation()
            .or_else(|| self.execution.violation())
            .or_else(|| self.providers.history.violation())
            .or_else(|| self.search.violation())
            .or_else(|| self.source.violation())
            .or_else(|| hooks_violation(&self.hooks))
            .or_else(|| engines_violation(&self.engines))
    }
}

/// The `[server]` table. Filesystem scans and parses run on a bounded
/// worker pool; this table sets the worker count, bounds the queue wait,
/// sets how long the server serves with no request before it stops, and
/// selects the loopback port. The server reads the table at startup, so a
/// change applies on the next start.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_server_ranges)]
pub struct ServerConfiguration {
    /// Workers running blocking filesystem and parser operations at once, 1 to 64.
    #[schemars(range(min = 1, max = 64))]
    pub num_workers: u64,
    /// Wall-clock bound one request waits for a free worker, 1ms to 1h.
    pub worker_queue_timeout: Duration,
    /// Wall-clock span with no served request that stops the server, 1s to 1d.
    pub idle_timeout: Duration,
    /// The exact loopback port the server binds, 1024 or above. Excludes
    /// `port_range`; omitted, the server picks from `port_range` or the
    /// default serving range.
    #[schemars(range(min = 1_024))]
    pub port: Option<u16>,
    /// The loopback range the server picks its port from. Excludes `port`;
    /// omitted, the default serving range applies.
    pub port_range: Option<PortRange>,
}

impl Default for ServerConfiguration {
    fn default() -> Self {
        Self {
            num_workers: 4,
            worker_queue_timeout: Duration::from_millis(30_000),
            idle_timeout: Duration::from_millis(1_800_000),
            port: None,
            port_range: None,
        }
    }
}

/// The `server.port_range` table: the inclusive loopback range the server
/// picks the first free port from.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortRange {
    /// The lowest port the server may bind, 1024 or above.
    #[schemars(range(min = 1_024))]
    pub min: u16,
    /// The highest port the server may bind, at or above `min`.
    #[schemars(range(min = 1_024))]
    pub max: u16,
}

impl ServerConfiguration {
    /// The table's bounds in key order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "server.num_workers",
                self.num_workers,
                1,
                SERVER_NUM_WORKERS_MAX,
            ),
            (
                "server.worker_queue_timeout",
                self.worker_queue_timeout.milliseconds(),
                1,
                SERVER_QUEUE_TIMEOUT_MS_MAX,
            ),
            (
                "server.idle_timeout",
                self.idle_timeout.milliseconds(),
                SERVER_IDLE_TIMEOUT_MS_MIN,
                SERVER_IDLE_TIMEOUT_MS_MAX,
            ),
        ])
        .or_else(|| self.port_violation())
    }

    /// The port-selection contracts, checked after the numeric rows.
    fn port_violation(&self) -> Option<ConfigurationViolation> {
        match (self.port, self.port_range) {
            (Some(_), Some(_)) => Some(ConfigurationViolation::PortSelectionConflict),
            (Some(port), None) => first_out_of_range([(
                "server.port",
                u64::from(port),
                u64::from(SERVER_PORT_FLOOR),
                u64::from(u16::MAX),
            )]),
            (None, Some(range)) => range.violation(),
            (None, None) => None,
        }
    }

    /// The inclusive port range binding selects from: the pinned `port`,
    /// the configured `port_range`, or the default serving range.
    #[must_use]
    pub fn serving_ports(&self) -> std::ops::RangeInclusive<u16> {
        match (self.port, self.port_range) {
            (Some(port), _) => port..=port,
            (None, Some(range)) => range.min..=range.max,
            (None, None) => SERVER_PORT_MIN..=SERVER_PORT_MAX,
        }
    }
}

impl PortRange {
    /// The range's bounds: both ends selectable, and the range running
    /// forward.
    fn violation(self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "server.port_range.min",
                u64::from(self.min),
                u64::from(SERVER_PORT_FLOOR),
                u64::from(u16::MAX),
            ),
            (
                "server.port_range.max",
                u64::from(self.max),
                u64::from(SERVER_PORT_FLOOR),
                u64::from(u16::MAX),
            ),
        ])
        .or_else(|| {
            (self.max < self.min).then_some(ConfigurationViolation::PortRangeInverted {
                min: self.min,
                max: self.max,
            })
        })
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

/// The `[search]` table. Search fuses a lexical ranking with a semantic one:
/// `lexical` and `semantic` weigh the two against each other, `fusion_k` sets
/// how sharply a top rank counts, `pool_slots` and `busy_timeout` bound the
/// `SQLite` connections behind the lexical tier, and `text` includes
/// non-source text files in the lexical index.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_search_ranges)]
pub struct SearchConfiguration {
    /// Which non-source text files join the lexical index alongside code
    /// symbols.
    pub text: TextSearchConfiguration,
    /// The lexical ranking's share of a fused score.
    pub lexical: LexicalSearchConfiguration,
    /// The embedding model that adds semantic ranking, and the bounds its
    /// preparation runs under.
    pub semantic: SemanticSearchConfiguration,
    /// The reciprocal-rank fusion constant, 1 to 1000. A result scores as the
    /// weighted sum of `1 / (k + rank)` over the rankings that returned it, so
    /// a larger value flattens the contribution curve and lets agreement
    /// between the two rankings outweigh one ranking's top position.
    #[schemars(range(min = 1, max = 1000))]
    #[serde(default = "default_search_fusion_k")]
    pub fusion_k: u64,
    /// Pooled `SQLite` connections the lexical search index may open at
    /// once, 1 to 16.
    #[schemars(range(min = 1, max = 16))]
    #[serde(default = "default_search_pool_slots")]
    pub pool_slots: u64,
    /// Wall-clock budget one lexical search connection waits for `SQLite`'s
    /// busy lock before `SQLITE_BUSY`, 100ms to 30s.
    #[serde(default = "default_search_busy_timeout")]
    pub busy_timeout: Duration,
}

impl Default for SearchConfiguration {
    fn default() -> Self {
        Self {
            text: TextSearchConfiguration::default(),
            lexical: LexicalSearchConfiguration::default(),
            semantic: SemanticSearchConfiguration::default(),
            fusion_k: SEARCH_FUSION_K_DEFAULT,
            pool_slots: SEARCH_POOL_SLOTS_DEFAULT,
            busy_timeout: default_search_busy_timeout(),
        }
    }
}

impl SearchConfiguration {
    /// The ranking-weight pair, then the `[search.semantic]` and
    /// `[search.text]` tables' own rules, then this table's numeric bounds,
    /// in key order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        search_weights_violation(self.lexical.weight, self.semantic.weight)
            .or_else(|| self.semantic.violation())
            .or_else(|| self.text.violation())
            .or_else(|| {
                first_out_of_range([
                    (
                        "search.fusion_k",
                        self.fusion_k,
                        SEARCH_FUSION_K_MIN,
                        SEARCH_FUSION_K_MAX,
                    ),
                    (
                        "search.pool_slots",
                        self.pool_slots,
                        SEARCH_POOL_SLOTS_MIN,
                        SEARCH_POOL_SLOTS_MAX,
                    ),
                    (
                        "search.busy_timeout",
                        self.busy_timeout.milliseconds(),
                        SEARCH_BUSY_TIMEOUT_MS_MIN,
                        SEARCH_BUSY_TIMEOUT_MS_MAX,
                    ),
                ])
            })
    }
}

/// `search.pool_slots` connections accepted, at least.
pub const SEARCH_POOL_SLOTS_MIN: u64 = 1;
/// `search.pool_slots` connections accepted, at most.
pub const SEARCH_POOL_SLOTS_MAX: u64 = 16;
/// `search.pool_slots` value used when the key is absent.
const SEARCH_POOL_SLOTS_DEFAULT: u64 = 4;
/// Milliseconds `search.busy_timeout` may hold, at least.
pub const SEARCH_BUSY_TIMEOUT_MS_MIN: u64 = 100;
/// Milliseconds `search.busy_timeout` may hold, at most: thirty seconds.
pub const SEARCH_BUSY_TIMEOUT_MS_MAX: u64 = 30_000;
/// Milliseconds `search.busy_timeout` holds when the key is absent.
const SEARCH_BUSY_TIMEOUT_MS_DEFAULT: u64 = 1_000;

/// `search.fusion_k` accepted, at least.
pub const SEARCH_FUSION_K_MIN: u64 = 1;
/// `search.fusion_k` accepted, at most.
pub const SEARCH_FUSION_K_MAX: u64 = 1_000;
/// `search.fusion_k` when the key is absent: the value the reciprocal-rank
/// fusion paper uses, and the one the fusion library defaults to.
const SEARCH_FUSION_K_DEFAULT: u64 = 60;

fn default_search_fusion_k() -> u64 {
    SEARCH_FUSION_K_DEFAULT
}

fn default_search_pool_slots() -> u64 {
    SEARCH_POOL_SLOTS_DEFAULT
}

fn default_search_busy_timeout() -> Duration {
    Duration::from_millis(SEARCH_BUSY_TIMEOUT_MS_DEFAULT)
}

/// Whether the two ranking weights form a pair of shares: each finite and
/// between 0 and 1, and the two summing to 1 within
/// [`SEARCH_WEIGHT_SUM_TOLERANCE`]. The tolerance is what admits the default
/// pair, since `0.7 + 0.3` lands a fraction below 1 in binary floating point.
fn search_weights_violation(lexical: f64, semantic: f64) -> Option<ConfigurationViolation> {
    let within_unit = |weight: f64| weight.is_finite() && (0.0..=1.0).contains(&weight);
    let bounded = within_unit(lexical) && within_unit(semantic);
    let normalized = (lexical + semantic - 1.0).abs() <= SEARCH_WEIGHT_SUM_TOLERANCE;
    let valid = bounded && normalized;
    (!valid).then_some(ConfigurationViolation::SearchWeightsInvalid { lexical, semantic })
}

/// Classifies one model value against the form its declared source sets.
/// Arms are ordered by precedence: the byte bound both sources share, then
/// the declared source's own form.
fn model_violation(
    field: &'static str,
    source: SemanticSource,
    value: &str,
    bytes_max: usize,
) -> Option<ConfigurationViolation> {
    let refused = match (source, value.as_bytes()) {
        (_, []) => true,
        (_, bytes) if bytes.len() > bytes_max => true,
        (SemanticSource::Hf, _) => repository_refused(value),
        (SemanticSource::Directory, _) => path_pattern_violation(value).is_some(),
    };
    refused.then(|| ConfigurationViolation::SemanticModelInvalid {
        field,
        value: value.to_owned(),
    })
}

/// Whether `value` breaks the Hugging Face repository form: an owner and a
/// name around one `/`, carrying at most one revision after `@`.
fn repository_refused(value: &str) -> bool {
    let (repository, revision) = match value.split_once('@') {
        Some((repository, revision)) => (repository, Some(revision)),
        None => (value, None),
    };
    let mut segments = repository.split('/');
    let named = match (segments.next(), segments.next(), segments.next()) {
        (Some(owner), Some(name), None) => is_repository_word(owner) && is_repository_word(name),
        _ => false,
    };
    !(named && revision.is_none_or(is_repository_word))
}

/// Whether `word` is one repository segment or one revision: nonempty,
/// neither `.` nor `..`, and built from `A-Z a-z 0-9 . _ -`. The charset
/// leaves out `/` and `@`, so a revision carrying either is refused.
fn is_repository_word(word: &str) -> bool {
    !word.is_empty()
        && !matches!(word, "." | "..")
        && word.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

/// The `[search.lexical]` table: what the lexical ranking contributes to a
/// fused score.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LexicalSearchConfiguration {
    /// The lexical ranking's share of a fused score, 0.0 to 1.0. It and
    /// `[search.semantic].weight` must sum to 1: a fused score is the weighted
    /// average of the two rankings, and a pair summing to anything else scales
    /// every score rather than trading one ranking against the other.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[serde(default = "default_lexical_weight")]
    pub weight: f64,
}

impl Default for LexicalSearchConfiguration {
    fn default() -> Self {
        Self {
            weight: LEXICAL_WEIGHT_DEFAULT,
        }
    }
}

/// Where the semantic ranking's model weights come from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticSource {
    /// Weights come from a Hugging Face repository, cached where every
    /// other Hugging Face client on the machine caches them.
    Hf,
    /// Weights are a directory the workspace already holds. Nothing is
    /// downloaded.
    Directory,
}

/// The `[search.semantic]` table: the embedding model that ranks a query
/// against code sharing no word with it, and the bounds its preparation runs
/// under.
///
/// Preparation runs behind the answers. A search issued before the vectors are
/// in is answered lexically and carries a warning naming what is still
/// missing.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_semantic_ranges)]
pub struct SemanticSearchConfiguration {
    /// Whether semantic ranking is off. Set it when the workspace must not
    /// fetch model weights: search then answers lexically alone and raises no
    /// preparation warning.
    pub disabled: bool,
    /// The semantic ranking's share of a fused score, 0.0 to 1.0. It and
    /// `[search.lexical].weight` must sum to 1.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[serde(default = "default_semantic_weight")]
    pub weight: f64,
    /// Where the weights come from, which decides how `model` reads.
    #[serde(default = "default_semantic_source")]
    pub source: SemanticSource,
    /// Which weights: under `hf` a repository identifier such as
    /// `BAAI/bge-small-en-v1.5`, optionally carrying a revision after `@`;
    /// under `directory` a workspace-relative directory holding them. Vectors
    /// are stored per model, so changing the value embeds the workspace
    /// again.
    #[schemars(length(min = 1, max = 128))]
    #[serde(default = "default_semantic_model")]
    pub model: String,
    /// Wall-clock budget one model download has, 10s to 1h.
    #[serde(default = "default_semantic_download_timeout")]
    pub download_timeout: Duration,
    /// Attempts one model download makes before semantic ranking degrades to
    /// lexical for the life of the server, 1 to 10.
    #[schemars(range(min = 1, max = 10))]
    #[serde(default = "default_semantic_download_attempts")]
    pub download_attempts: u64,
    /// Declarations one embedding pass hands the encoder at once, 1 to 256.
    /// Attention memory grows with the square of `max_tokens`, so a raised
    /// token window wants a lowered batch.
    #[schemars(range(min = 1, max = 256))]
    #[serde(default = "default_semantic_batch_declarations")]
    pub batch_declarations: u64,
    /// Tokens the encoder reads from one declaration, 32 to 512. What follows
    /// them is truncated.
    #[schemars(range(min = 32, max = 512))]
    #[serde(default = "default_semantic_max_tokens")]
    pub max_tokens: u64,
    /// Declarations the semantic ranking returns before the two rankings are
    /// fused, 1 to 1000.
    #[schemars(range(min = 1, max = 1000))]
    #[serde(default = "default_semantic_candidates")]
    pub candidates: u64,
    /// Vectors the workspace may hold, 1000 to 1000000. Embedding stops at the
    /// bound and the search warning says so; each vector costs the model's
    /// dimension in single-precision floats.
    #[schemars(range(min = 1_000, max = 1_000_000))]
    #[serde(default = "default_semantic_max_vectors")]
    pub max_vectors: u64,
}

impl Default for SemanticSearchConfiguration {
    fn default() -> Self {
        Self {
            disabled: false,
            weight: SEMANTIC_WEIGHT_DEFAULT,
            source: SEMANTIC_SOURCE_DEFAULT,
            model: default_semantic_model(),
            download_timeout: default_semantic_download_timeout(),
            download_attempts: SEMANTIC_DOWNLOAD_ATTEMPTS_DEFAULT,
            batch_declarations: SEMANTIC_BATCH_DECLARATIONS_DEFAULT,
            max_tokens: SEMANTIC_MAX_TOKENS_DEFAULT,
            candidates: SEMANTIC_CANDIDATES_DEFAULT,
            max_vectors: SEMANTIC_MAX_VECTORS_DEFAULT,
        }
    }
}

impl SemanticSearchConfiguration {
    /// The model identifier rule, then this table's numeric bounds, in key
    /// order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        model_violation(
            "search.semantic.model",
            self.source,
            &self.model,
            SEMANTIC_MODEL_BYTES_MAX,
        )
        .or_else(|| {
            first_out_of_range([
                (
                    "search.semantic.download_timeout",
                    self.download_timeout.milliseconds(),
                    SEMANTIC_DOWNLOAD_TIMEOUT_MS_MIN,
                    SEMANTIC_DOWNLOAD_TIMEOUT_MS_MAX,
                ),
                (
                    "search.semantic.download_attempts",
                    self.download_attempts,
                    SEMANTIC_DOWNLOAD_ATTEMPTS_MIN,
                    SEMANTIC_DOWNLOAD_ATTEMPTS_MAX,
                ),
                (
                    "search.semantic.batch_declarations",
                    self.batch_declarations,
                    SEMANTIC_BATCH_DECLARATIONS_MIN,
                    SEMANTIC_BATCH_DECLARATIONS_MAX,
                ),
                (
                    "search.semantic.max_tokens",
                    self.max_tokens,
                    SEMANTIC_MAX_TOKENS_MIN,
                    SEMANTIC_MAX_TOKENS_MAX,
                ),
                (
                    "search.semantic.candidates",
                    self.candidates,
                    SEMANTIC_CANDIDATES_MIN,
                    SEMANTIC_CANDIDATES_MAX,
                ),
                (
                    "search.semantic.max_vectors",
                    self.max_vectors,
                    SEMANTIC_MAX_VECTORS_MIN,
                    SEMANTIC_MAX_VECTORS_MAX,
                ),
            ])
        })
    }
}

/// `search.lexical.weight` when the key is absent. Code's literal signal
/// carries the larger share: a caller quoting a real name is the common case,
/// and the lexical ranking is the stronger side of exactly that.
pub const LEXICAL_WEIGHT_DEFAULT: f64 = 0.7;
/// `search.semantic.weight` when the key is absent.
pub const SEMANTIC_WEIGHT_DEFAULT: f64 = 0.3;
/// How far the two ranking weights may sum from 1 and still be accepted.
pub const SEARCH_WEIGHT_SUM_TOLERANCE: f64 = 1e-9;
/// `search.semantic.source` when the key is absent: the default model is a
/// Hugging Face repository.
pub const SEMANTIC_SOURCE_DEFAULT: SemanticSource = SemanticSource::Hf;
/// `search.semantic.model` when the key is absent: 33M parameters, 384
/// dimensions, MIT, and a plain BERT encoder that runs without a C++
/// toolchain.
pub const SEMANTIC_MODEL_DEFAULT: &str = "BAAI/bge-small-en-v1.5";
/// Milliseconds `search.semantic.download_timeout` may hold, at least.
pub const SEMANTIC_DOWNLOAD_TIMEOUT_MS_MIN: u64 = 10_000;
/// Milliseconds `search.semantic.download_timeout` may hold, at most: one hour.
pub const SEMANTIC_DOWNLOAD_TIMEOUT_MS_MAX: u64 = 3_600_000;
/// Milliseconds `search.semantic.download_timeout` holds when the key is
/// absent: five minutes.
const SEMANTIC_DOWNLOAD_TIMEOUT_MS_DEFAULT: u64 = 300_000;
/// `search.semantic.download_attempts` accepted, at least.
pub const SEMANTIC_DOWNLOAD_ATTEMPTS_MIN: u64 = 1;
/// `search.semantic.download_attempts` accepted, at most.
pub const SEMANTIC_DOWNLOAD_ATTEMPTS_MAX: u64 = 10;
/// `search.semantic.download_attempts` when the key is absent.
const SEMANTIC_DOWNLOAD_ATTEMPTS_DEFAULT: u64 = 3;
/// `search.semantic.batch_declarations` accepted, at least.
pub const SEMANTIC_BATCH_DECLARATIONS_MIN: u64 = 1;
/// `search.semantic.batch_declarations` accepted, at most.
pub const SEMANTIC_BATCH_DECLARATIONS_MAX: u64 = 256;
/// `search.semantic.batch_declarations` when the key is absent.
const SEMANTIC_BATCH_DECLARATIONS_DEFAULT: u64 = 32;
/// `search.semantic.max_tokens` accepted, at least.
pub const SEMANTIC_MAX_TOKENS_MIN: u64 = 32;
/// `search.semantic.max_tokens` accepted, at most.
pub const SEMANTIC_MAX_TOKENS_MAX: u64 = 512;
/// `search.semantic.max_tokens` when the key is absent: enough for a
/// signature, a doc comment, and the head of a body.
const SEMANTIC_MAX_TOKENS_DEFAULT: u64 = 256;
/// `search.semantic.candidates` accepted, at least.
pub const SEMANTIC_CANDIDATES_MIN: u64 = 1;
/// `search.semantic.candidates` accepted, at most.
pub const SEMANTIC_CANDIDATES_MAX: u64 = 1_000;
/// `search.semantic.candidates` when the key is absent.
const SEMANTIC_CANDIDATES_DEFAULT: u64 = 200;
/// `search.semantic.max_vectors` accepted, at least.
pub const SEMANTIC_MAX_VECTORS_MIN: u64 = 1_000;
/// `search.semantic.max_vectors` accepted, at most.
pub const SEMANTIC_MAX_VECTORS_MAX: u64 = 1_000_000;
/// `search.semantic.max_vectors` when the key is absent.
const SEMANTIC_MAX_VECTORS_DEFAULT: u64 = 200_000;

fn default_lexical_weight() -> f64 {
    LEXICAL_WEIGHT_DEFAULT
}

fn default_semantic_weight() -> f64 {
    SEMANTIC_WEIGHT_DEFAULT
}

fn default_semantic_source() -> SemanticSource {
    SEMANTIC_SOURCE_DEFAULT
}

fn default_semantic_model() -> String {
    SEMANTIC_MODEL_DEFAULT.to_owned()
}

fn default_semantic_download_timeout() -> Duration {
    Duration::from_millis(SEMANTIC_DOWNLOAD_TIMEOUT_MS_DEFAULT)
}

fn default_semantic_download_attempts() -> u64 {
    SEMANTIC_DOWNLOAD_ATTEMPTS_DEFAULT
}

fn default_semantic_batch_declarations() -> u64 {
    SEMANTIC_BATCH_DECLARATIONS_DEFAULT
}

fn default_semantic_max_tokens() -> u64 {
    SEMANTIC_MAX_TOKENS_DEFAULT
}

fn default_semantic_candidates() -> u64 {
    SEMANTIC_CANDIDATES_DEFAULT
}

fn default_semantic_max_vectors() -> u64 {
    SEMANTIC_MAX_VECTORS_DEFAULT
}

/// `search.text.extensions` entries accepted, at most.
pub const TEXT_EXTENSIONS_MAX: usize = 32;
/// Bytes one `search.text.extensions` entry may hold, at most.
pub const TEXT_EXTENSION_BYTES_MAX: usize = 16;
/// Bytes one lexical chunk from a `search.text` file may hold, at least.
pub const TEXT_CHUNK_BYTES_MIN: u64 = 1 << 10;
/// Bytes one lexical chunk from a `search.text` file may hold, at most.
pub const TEXT_CHUNK_BYTES_MAX: u64 = 16 << 20;
/// Bytes one lexical chunk from a `search.text` file may hold, by default.
pub const TEXT_CHUNK_BYTES_DEFAULT: u64 = 1 << 20;

/// `search.text.extensions` included by default: prose formats with no dedicated syntax
/// provider.
const TEXT_EXTENSIONS_DEFAULT: [&str; 3] = ["md", "mdx", "txt"];

fn default_text_extensions() -> Vec<String> {
    TEXT_EXTENSIONS_DEFAULT
        .iter()
        .copied()
        .map(str::to_owned)
        .collect()
}

/// The `[search.text]` table: which non-source text files join the lexical index, as one
/// unit each, or as several size-bounded chunks when a file exceeds `max_chunk`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_text_ranges)]
pub struct TextSearchConfiguration {
    /// File extensions, without the leading dot, included as text-file lexical units:
    /// lowercase ASCII alphanumeric only (so a leading dot is already excluded), at most
    /// 16 bytes each, at most 32 entries, no duplicates.
    #[serde(default = "default_text_extensions")]
    #[schemars(length(max = 32))]
    pub extensions: Vec<String>,
    /// Bytes one lexical chunk may hold, 1kb to 16mb. A text file larger than this is
    /// indexed as several chunks of at most this size; an operator who wants a file out of
    /// the index excludes it in `[source]` or drops its extension from `extensions`.
    pub max_chunk: ByteSize,
}

impl Default for TextSearchConfiguration {
    fn default() -> Self {
        Self {
            extensions: default_text_extensions(),
            max_chunk: ByteSize::from_bytes(TEXT_CHUNK_BYTES_DEFAULT),
        }
    }
}

impl TextSearchConfiguration {
    /// The table's numeric bounds, then the extension-list rules, in key order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "search.text.extensions",
                self.extensions.len() as u64,
                0,
                TEXT_EXTENSIONS_MAX as u64,
            ),
            (
                "search.text.max_chunk",
                self.max_chunk.bytes(),
                TEXT_CHUNK_BYTES_MIN,
                TEXT_CHUNK_BYTES_MAX,
            ),
        ])
        .or_else(|| text_extensions_violation(&self.extensions))
    }
}

/// Whether `extension` matches `search.text.extensions`'s accepted spelling: nonempty,
/// lowercase ASCII alphanumeric only, at most [`TEXT_EXTENSION_BYTES_MAX`] bytes.
fn is_text_extension(extension: &str) -> bool {
    !extension.is_empty()
        && extension.len() <= TEXT_EXTENSION_BYTES_MAX
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// The first `search.text.extensions` entry breaking its charset/length contract, or the
/// first duplicate, entries in list order.
fn text_extensions_violation(extensions: &[String]) -> Option<ConfigurationViolation> {
    let mut seen = std::collections::BTreeSet::new();
    for extension in extensions {
        if !is_text_extension(extension) {
            return Some(ConfigurationViolation::TextExtensionInvalid {
                extension: extension.clone(),
            });
        }
        if !seen.insert(extension.as_str()) {
            return Some(ConfigurationViolation::TextExtensionDuplicate {
                extension: extension.clone(),
            });
        }
    }
    None
}

/// One `[[hooks]]` block: an executable Rift starts directly - no shell -
/// inside the changed tree each time a change applies. Every key is
/// required; the schema carries no defaults.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_hook_ranges)]
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
    /// Wall-clock bound before Rift kills the process, 1ms to 1h.
    pub timeout: Duration,
    /// Bytes of each output stream Rift keeps, 256b to 4kb. The full size
    /// is still reported.
    pub output_limit: ByteSize,
    /// What a passing run establishes. Each entry becomes evidence on the
    /// change the hook checked.
    #[schemars(length(max = 16))]
    pub guarantees: Vec<HookGuarantee>,
    /// Whether an identical tree and environment are expected to reproduce
    /// the result.
    pub determinism: Determinism,
}

/// How a hook runs; `command` - an executable started directly, without a
/// shell - is the only type.
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

/// One `[engines.<name>]` table: an external LSP server Rift may start.
///
/// The key is the engine's name, a lowercase word in the language-name
/// charset. Rift starts the engine on the first request for a language it
/// serves, speaks LSP to it over stdio, and reuses the running engine
/// across requests. The engine never writes: it proposes edits, and Rift
/// applies them through its own change path.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_engine_ranges)]
pub struct EngineConfiguration {
    /// The executable Rift starts inside the workspace root. An absolute
    /// path is refused; the name resolves through `PATH`.
    #[schemars(length(min = 1))]
    pub program: String,
    /// The program's literal arguments, in order. May be empty.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub arguments: Vec<String>,
    /// Environment values added on top of the environment the server
    /// inherited.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// The languages this engine serves: a language name, or `name:dialect`
    /// to pin a dialect. Each segment belongs to at most one engine across
    /// the whole `[engines]` table. A segment no syntax provider knows is
    /// accepted: an engine may serve languages the syntax tier does not.
    #[schemars(length(min = 1, max = 16))]
    pub languages: Vec<String>,
    /// Options handed to the engine at initialize. Must be a JSON object
    /// when present; the engine defines its meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<serde_json::Value>,
    /// Wall-clock bound on the engine's initialize handshake, 1s to 10m.
    #[serde(default = "default_engine_startup_timeout")]
    pub startup_timeout: Duration,
    /// Wall-clock bound on each later engine request, 1s to 10m.
    #[serde(default = "default_engine_request_timeout")]
    pub request_timeout: Duration,
    /// Bytes of the engine's standard error Rift keeps, 1kb to 8mb. The
    /// full size is still reported.
    #[serde(default = "default_engine_output_limit")]
    pub output_limit: ByteSize,
    /// How often Rift sends this engine the same request again while its
    /// answer stays unsettled - a refusal the engine invites again, or an
    /// answer it gave while still analyzing - and how the waits between
    /// those attempts grow.
    #[serde(default)]
    pub retry: RetryPolicy,
    /// How often Rift replaces this engine on its own, and over what
    /// window; the budget spent, the engine's own failure surfaces.
    #[serde(default)]
    pub restart: RestartPolicy,
}

fn default_engine_startup_timeout() -> Duration {
    Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_DEFAULT)
}

fn default_engine_request_timeout() -> Duration {
    Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_DEFAULT)
}

fn default_engine_output_limit() -> ByteSize {
    ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_DEFAULT)
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
        /// The smallest accepted value.
        min: u64,
        /// The largest accepted value.
        max: u64,
    },
    /// An `execution.allow` entry is not `name` or `name:dialect`.
    LanguageSelectorInvalid {
        /// The rejected entry.
        selector: String,
    },
    /// A `[search.semantic]` identifier is empty, too long, or breaks the form
    /// its declared source sets: `owner/name` with at most one revision after
    /// `@` under `hf`, a workspace-relative directory under `directory`.
    SemanticModelInvalid {
        /// The key's path in the file, such as `search.semantic.model`.
        field: &'static str,
        /// The rejected value.
        value: String,
    },
    /// The `[search.lexical]` and `[search.semantic]` weights are not a pair of
    /// shares: each must lie between 0.0 and 1.0, and the two must sum to 1.
    SearchWeightsInvalid {
        /// The configured lexical weight.
        lexical: f64,
        /// The configured semantic weight.
        semantic: f64,
    },
    /// A `search.text.extensions` entry is empty, uses forbidden characters, or exceeds
    /// [`TEXT_EXTENSION_BYTES_MAX`] bytes.
    TextExtensionInvalid {
        /// The rejected entry.
        extension: String,
    },
    /// Two `search.text.extensions` entries name the same extension.
    TextExtensionDuplicate {
        /// The extension both entries claim.
        extension: String,
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
    /// An `[engines.<name>]` key is not a lowercase word in the
    /// language-name charset.
    EngineNameInvalid {
        /// The rejected engine name.
        name: String,
    },
    /// An engine's `program` is empty, so there is nothing to start.
    EngineProgramEmpty {
        /// The engine missing its executable.
        engine: String,
    },
    /// An engine names its executable by absolute path, which would escape
    /// the workspace's `PATH` policy.
    EngineExecutableAbsolute {
        /// The engine naming the executable.
        engine: String,
        /// The refused executable path.
        program: String,
    },
    /// An engine argument exceeds [`ENGINE_ARGUMENT_BYTES_MAX`] bytes.
    EngineArgumentOversized {
        /// The engine declaring the argument.
        engine: String,
        /// The oversized argument's length in bytes.
        bytes: u64,
    },
    /// An engine environment key is empty, or carries `=` or a NUL byte.
    EngineEnvironmentKeyInvalid {
        /// The engine declaring the entry.
        engine: String,
        /// The rejected key.
        key: String,
    },
    /// An engine's `languages` list is empty, so no request could reach it.
    EngineLanguagesEmpty {
        /// The engine serving no language.
        engine: String,
    },
    /// An engine `languages` entry is not `name` or `name:dialect`.
    EngineLanguageInvalid {
        /// The engine declaring the entry.
        engine: String,
        /// The rejected entry.
        language: String,
    },
    /// Two engine `languages` entries claim one language identity segment,
    /// so a request for it could not pick an engine.
    EngineLanguageDuplicate {
        /// The engine claiming the segment again.
        engine: String,
        /// The language identity segment claimed twice.
        language: String,
    },
    /// An engine's `initialization_options` value is not a JSON object.
    EngineInitializationOptionsNotObject {
        /// The engine declaring the options.
        engine: String,
    },
    /// A `source.include` or `source.exclude` entry breaks the forward-slash-only path-pattern
    /// contract: it is empty, absolute, carries a backslash or control character, or a `.` or
    /// `..` segment.
    PathPatternInvalid {
        /// The key's path in the file: `source.include` or `source.exclude`.
        field: &'static str,
        /// The rejected pattern.
        pattern: String,
    },
    /// `server.port` and `server.port_range` are both set, so the file
    /// selects the port twice.
    PortSelectionConflict,
    /// `server.port_range` runs backwards: `max` sits below `min`.
    PortRangeInverted {
        /// The configured lower end.
        min: u16,
        /// The configured upper end, below `min`.
        max: u16,
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
            Self::SemanticModelInvalid { field, value } => {
                vec![("field", (*field).to_owned()), ("value", value.clone())]
            }
            Self::SearchWeightsInvalid { lexical, semantic } => vec![
                ("lexical_weight", lexical.to_string()),
                ("semantic_weight", semantic.to_string()),
            ],
            Self::TextExtensionInvalid { extension }
            | Self::TextExtensionDuplicate { extension } => {
                vec![("extension", extension.clone())]
            }
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
            Self::EngineNameInvalid { name } => vec![("name", name.clone())],
            Self::EngineProgramEmpty { engine }
            | Self::EngineLanguagesEmpty { engine }
            | Self::EngineInitializationOptionsNotObject { engine } => {
                vec![("engine", engine.clone())]
            }
            Self::EngineExecutableAbsolute { engine, program } => {
                vec![("engine", engine.clone()), ("program", program.clone())]
            }
            Self::EngineArgumentOversized { engine, bytes } => vec![
                ("engine", engine.clone()),
                ("bytes", bytes.to_string()),
                ("bytes_max", ENGINE_ARGUMENT_BYTES_MAX.to_string()),
            ],
            Self::EngineEnvironmentKeyInvalid { engine, key } => {
                vec![("engine", engine.clone()), ("key", key.clone())]
            }
            Self::EngineLanguageInvalid { engine, language }
            | Self::EngineLanguageDuplicate { engine, language } => {
                vec![("engine", engine.clone()), ("language", language.clone())]
            }
            Self::PathPatternInvalid { field, pattern } => {
                vec![("field", (*field).to_owned()), ("pattern", pattern.clone())]
            }
            Self::PortSelectionConflict => {
                vec![("fields", "server.port, server.port_range".to_owned())]
            }
            Self::PortRangeInverted { min, max } => {
                vec![("min", min.to_string()), ("max", max.to_string())]
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
pub(crate) fn first_out_of_range<const ROWS: usize>(
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
        (
            "hooks.timeout",
            hook.timeout.milliseconds(),
            1,
            HOOK_TIMEOUT_MS_MAX,
        ),
        (
            "hooks.output_limit",
            hook.output_limit.bytes(),
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

/// The first violated engine bound, engines in name order.
///
/// One claim set spans the whole table, so a language identity segment
/// repeated within one engine or across two engines is refused either way.
fn engines_violation(
    engines: &BTreeMap<String, EngineConfiguration>,
) -> Option<ConfigurationViolation> {
    if engines.len() > ENGINES_MAX {
        return out_of_range("engines", engines.len() as u64, 0, ENGINES_MAX as u64);
    }
    let mut claimed = std::collections::BTreeSet::new();
    for (name, engine) in engines {
        if !is_language_word(name) {
            return Some(ConfigurationViolation::EngineNameInvalid { name: name.clone() });
        }
        if let Some(violation) = engine_violation(name, engine) {
            return Some(violation);
        }
        for language in &engine.languages {
            if !claimed.insert(language.as_str()) {
                return Some(ConfigurationViolation::EngineLanguageDuplicate {
                    engine: name.clone(),
                    language: language.clone(),
                });
            }
        }
    }
    None
}

/// The first bound one engine breaks, rules in key order.
fn engine_violation(name: &str, engine: &EngineConfiguration) -> Option<ConfigurationViolation> {
    engine_program_violation(name, engine)
        .or_else(|| engine_arguments_violation(name, engine))
        .or_else(|| engine_environment_violation(name, engine))
        .or_else(|| engine_languages_violation(name, engine))
        .or_else(|| engine_options_violation(name, engine))
        .or_else(|| engine_bounds_violation(engine))
        .or_else(|| engine_retry_violation(&engine.retry))
        .or_else(|| engine_restart_violation(&engine.restart))
}

/// The bounds one engine's `[retry]` table carries, in key order.
fn engine_retry_violation(retry: &RetryPolicy) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "engines.retry.attempts",
            retry.attempts,
            RETRY_ATTEMPTS_MIN,
            RETRY_ATTEMPTS_MAX,
        ),
        (
            "engines.retry.delay",
            retry.delay.milliseconds(),
            RETRY_DELAY_MS_MIN,
            RETRY_DELAY_MS_MAX,
        ),
        (
            "engines.retry.delay_limit",
            retry.delay_limit.milliseconds(),
            RETRY_DELAY_LIMIT_MS_MIN,
            RETRY_DELAY_LIMIT_MS_MAX,
        ),
    ])
}

/// The bounds one engine's `[restart]` table carries, in key order.
fn engine_restart_violation(restart: &RestartPolicy) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "engines.restart.attempts",
            restart.attempts,
            RESTART_ATTEMPTS_MIN,
            RESTART_ATTEMPTS_MAX,
        ),
        (
            "engines.restart.window",
            restart.window.milliseconds(),
            RESTART_WINDOW_MS_MIN,
            RESTART_WINDOW_MS_MAX,
        ),
    ])
}

/// The rules on what the engine runs: a present, non-absolute program.
fn engine_program_violation(
    name: &str,
    engine: &EngineConfiguration,
) -> Option<ConfigurationViolation> {
    if engine.program.is_empty() {
        return Some(ConfigurationViolation::EngineProgramEmpty {
            engine: name.to_owned(),
        });
    }
    is_absolute_program(&engine.program).then(|| ConfigurationViolation::EngineExecutableAbsolute {
        engine: name.to_owned(),
        program: engine.program.clone(),
    })
}

/// The `arguments` rules: a bounded count, then bounded entry sizes.
fn engine_arguments_violation(
    name: &str,
    engine: &EngineConfiguration,
) -> Option<ConfigurationViolation> {
    first_out_of_range([(
        "engines.arguments",
        engine.arguments.len() as u64,
        0,
        ENGINE_ARGUMENTS_MAX as u64,
    )])
    .or_else(|| {
        let oversized = engine
            .arguments
            .iter()
            .find(|argument| argument.len() > ENGINE_ARGUMENT_BYTES_MAX)?;
        Some(ConfigurationViolation::EngineArgumentOversized {
            engine: name.to_owned(),
            bytes: oversized.len() as u64,
        })
    })
}

/// The `environment` rules: a bounded count, then every entry's key.
fn engine_environment_violation(
    name: &str,
    engine: &EngineConfiguration,
) -> Option<ConfigurationViolation> {
    first_out_of_range([(
        "engines.environment",
        engine.environment.len() as u64,
        0,
        ENGINE_ENVIRONMENT_ENTRIES_MAX as u64,
    )])
    .or_else(|| {
        let key = engine
            .environment
            .keys()
            .find(|key| !is_environment_key(key))?;
        Some(ConfigurationViolation::EngineEnvironmentKeyInvalid {
            engine: name.to_owned(),
            key: key.clone(),
        })
    })
}

/// The `languages` rules: at least one entry, a bounded count, and each
/// entry a language identity segment.
fn engine_languages_violation(
    name: &str,
    engine: &EngineConfiguration,
) -> Option<ConfigurationViolation> {
    if engine.languages.is_empty() {
        return Some(ConfigurationViolation::EngineLanguagesEmpty {
            engine: name.to_owned(),
        });
    }
    first_out_of_range([(
        "engines.languages",
        engine.languages.len() as u64,
        1,
        ENGINE_LANGUAGES_MAX as u64,
    )])
    .or_else(|| {
        let invalid = engine
            .languages
            .iter()
            .find(|language| !is_language_selector(language))?;
        Some(ConfigurationViolation::EngineLanguageInvalid {
            engine: name.to_owned(),
            language: invalid.clone(),
        })
    })
}

/// The `initialization_options` rule: one JSON object, when present.
fn engine_options_violation(
    name: &str,
    engine: &EngineConfiguration,
) -> Option<ConfigurationViolation> {
    let options = engine.initialization_options.as_ref()?;
    (!options.is_object()).then(
        || ConfigurationViolation::EngineInitializationOptionsNotObject {
            engine: name.to_owned(),
        },
    )
}

/// The numeric bounds one engine carries, as a table in key order.
fn engine_bounds_violation(engine: &EngineConfiguration) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "engines.startup_timeout",
            engine.startup_timeout.milliseconds(),
            ENGINE_STARTUP_TIMEOUT_MS_MIN,
            ENGINE_STARTUP_TIMEOUT_MS_MAX,
        ),
        (
            "engines.request_timeout",
            engine.request_timeout.milliseconds(),
            ENGINE_REQUEST_TIMEOUT_MS_MIN,
            ENGINE_REQUEST_TIMEOUT_MS_MAX,
        ),
        (
            "engines.output_limit",
            engine.output_limit.bytes(),
            ENGINE_OUTPUT_BYTES_MIN,
            ENGINE_OUTPUT_BYTES_MAX,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SOURCE_PATTERNS_MAX;
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
            timeout: Duration::from_millis(120_000),
            output_limit: ByteSize::from_bytes(4_096),
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
    }

    fn engine() -> EngineConfiguration {
        EngineConfiguration {
            program: "uvx".to_owned(),
            arguments: vec!["ty".to_owned(), "server".to_owned()],
            environment: BTreeMap::new(),
            languages: vec!["python".to_owned()],
            initialization_options: None,
            startup_timeout: Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_DEFAULT),
            request_timeout: Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_DEFAULT),
            output_limit: ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_DEFAULT),
            retry: RetryPolicy::default(),
            restart: RestartPolicy::default(),
        }
    }

    fn engines(entries: Vec<(&str, EngineConfiguration)>) -> WorkspaceConfiguration {
        WorkspaceConfiguration {
            engines: entries
                .into_iter()
                .map(|(name, engine)| (name.to_owned(), engine))
                .collect(),
            ..WorkspaceConfiguration::default()
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
        let size: ByteSize =
            serde_json::from_value(json!("16kb")).expect("spelling must be accepted");
        assert_eq!(serde_json::to_value(size).expect("render"), json!("16kb"));
        let duration: Duration =
            serde_json::from_value(json!("90s")).expect("spelling must be accepted");
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
        let semantic = &configuration.search.semantic;
        assert!(is_weight(
            configuration.search.lexical.weight,
            LEXICAL_WEIGHT_DEFAULT
        ));
        assert!(is_weight(semantic.weight, SEMANTIC_WEIGHT_DEFAULT));
        assert!(!semantic.disabled);
        assert_eq!(semantic.source, SemanticSource::Hf);
        assert_eq!(semantic.model, SEMANTIC_MODEL_DEFAULT);
        assert!(is_weight(
            configuration.search.lexical.weight + semantic.weight,
            1.0
        ));
        assert_eq!(semantic.download_timeout, Duration::from_millis(300_000));
        assert_eq!(semantic.download_attempts, 3);
        assert_eq!(semantic.batch_declarations, 32);
        assert_eq!(semantic.max_tokens, 256);
        assert_eq!(semantic.candidates, 200);
        assert_eq!(semantic.max_vectors, 200_000);
        assert_eq!(configuration.search.pool_slots, 4);
        assert_eq!(configuration.search.fusion_k, 60);
        assert_eq!(
            configuration.search.busy_timeout,
            Duration::from_millis(1_000)
        );
        assert!(configuration.source.include.is_empty());
        assert!(configuration.source.exclude.is_empty());
        assert!(configuration.source.respect_gitignore);
        assert!(configuration.hooks.is_empty());
        assert!(configuration.engines.is_empty());
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
            json!({ "search": { "text": { "unknown": "x" } } }),
            json!({ "source": { "unknown": "x" } }),
            json!({ "engines": { "ty": {
                "program": "uvx", "languages": ["python"], "unknown": 1,
            } } }),
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
    fn test_execution_bounds_accept_their_edges() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.execution.max_code = ByteSize::from_bytes(EXECUTION_CODE_BYTES_MAX);
        configuration.execution.max_timeout = Duration::from_millis(EXECUTION_TIMEOUT_MS_MAX);
        configuration.execution.max_output = ByteSize::from_bytes(0);
        configuration.execution.max_concurrent = EXECUTION_CONCURRENT_MAX;
        configuration.providers.history.max_revisions = HISTORY_REVISIONS_MAX;
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_server_defaults_state_four_workers_and_thirty_seconds() {
        let table = ServerConfiguration::default();
        assert_eq!(table.num_workers, 4);
        assert_eq!(table.worker_queue_timeout, Duration::from_millis(30_000));
        assert_eq!(table.idle_timeout, Duration::from_millis(1_800_000));
        assert_eq!(WorkspaceConfiguration::default().validate(), Ok(()));
    }

    #[test]
    fn test_server_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        for workers in [0, SERVER_NUM_WORKERS_MAX + 1] {
            configuration.server.num_workers = workers;
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "server.num_workers",
                        ..
                    })
                ),
                "num_workers {workers} must be refused"
            );
        }
        configuration.server.num_workers = SERVER_NUM_WORKERS_MAX;
        for timeout_ms in [0, SERVER_QUEUE_TIMEOUT_MS_MAX + 1] {
            configuration.server.worker_queue_timeout = Duration::from_millis(timeout_ms);
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "server.worker_queue_timeout",
                        ..
                    })
                ),
                "worker_queue_timeout {timeout_ms}ms must be refused"
            );
        }
        configuration.server.worker_queue_timeout =
            Duration::from_millis(SERVER_QUEUE_TIMEOUT_MS_MAX);
        for timeout_ms in [
            SERVER_IDLE_TIMEOUT_MS_MIN - 1,
            SERVER_IDLE_TIMEOUT_MS_MAX + 1,
        ] {
            configuration.server.idle_timeout = Duration::from_millis(timeout_ms);
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "server.idle_timeout",
                        ..
                    })
                ),
                "idle_timeout {timeout_ms}ms must be refused"
            );
        }
        for timeout_ms in [SERVER_IDLE_TIMEOUT_MS_MIN, SERVER_IDLE_TIMEOUT_MS_MAX] {
            configuration.server.idle_timeout = Duration::from_millis(timeout_ms);
            assert_eq!(
                configuration.validate(),
                Ok(()),
                "idle_timeout {timeout_ms}ms must be accepted"
            );
        }
    }

    #[test]
    fn test_port_selection_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.server.port = Some(SERVER_PORT_FLOOR - 1);
        assert!(
            matches!(
                configuration.validate(),
                Err(ConfigurationViolation::LimitOutOfRange {
                    field: "server.port",
                    ..
                })
            ),
            "a pinned port below the floor must be refused"
        );
        configuration.server.port = Some(SERVER_PORT_FLOOR);
        assert_eq!(configuration.validate(), Ok(()));

        configuration.server.port_range = Some(PortRange {
            min: 11_000,
            max: 12_000,
        });
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::PortSelectionConflict),
            "a pinned port and a range together must be refused"
        );

        configuration.server.port = None;
        assert_eq!(configuration.validate(), Ok(()));
        configuration.server.port_range = Some(PortRange {
            min: SERVER_PORT_FLOOR - 1,
            max: 12_000,
        });
        assert!(
            matches!(
                configuration.validate(),
                Err(ConfigurationViolation::LimitOutOfRange {
                    field: "server.port_range.min",
                    ..
                })
            ),
            "a range starting below the floor must be refused"
        );
        configuration.server.port_range = Some(PortRange {
            min: 12_000,
            max: 11_000,
        });
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::PortRangeInverted {
                min: 12_000,
                max: 11_000,
            }),
            "a backwards range must be refused"
        );
    }

    #[test]
    fn test_serving_ports_resolve_pin_range_and_default() {
        let mut table = ServerConfiguration::default();
        assert_eq!(table.serving_ports(), SERVER_PORT_MIN..=SERVER_PORT_MAX);
        table.port_range = Some(PortRange {
            min: 11_000,
            max: 12_000,
        });
        assert_eq!(table.serving_ports(), 11_000..=12_000);
        table.port = Some(11_500);
        assert_eq!(
            table.serving_ports(),
            11_500..=11_500,
            "a pinned port narrows the selection to itself"
        );
    }

    #[test]
    fn test_port_violations_carry_their_evidence() {
        let conflict = ConfigurationViolation::PortSelectionConflict;
        assert_eq!(
            conflict.evidence(),
            vec![("fields", "server.port, server.port_range".to_owned())]
        );
        let inverted = ConfigurationViolation::PortRangeInverted {
            min: 12_000,
            max: 11_000,
        };
        assert_eq!(
            inverted.evidence(),
            vec![("min", "12000".to_owned()), ("max", "11000".to_owned())]
        );
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
    fn test_language_selectors_accept_names_and_dialects() {
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

    /// The verdict `[search.semantic]` reaches on one model value under one
    /// declared source.
    fn semantic_model_verdict(
        source: SemanticSource,
        model: &str,
    ) -> Result<(), ConfigurationViolation> {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.semantic.source = source;
        configuration.search.semantic.model = model.to_owned();
        configuration.validate()
    }

    /// The refusal every rejected model value must draw.
    fn semantic_model_refusal(model: &str) -> Result<(), ConfigurationViolation> {
        Err(ConfigurationViolation::SemanticModelInvalid {
            field: "search.semantic.model",
            value: model.to_owned(),
        })
    }

    #[test]
    fn test_hf_model_names_a_repository_and_an_optional_revision() {
        for accepted in [
            "models/bge-small",
            "BAAI/bge-small-en-v1.5",
            "BAAI/bge-small-en-v1.5@a5beb1e",
        ] {
            assert_eq!(
                semantic_model_verdict(SemanticSource::Hf, accepted),
                Ok(()),
                "{accepted} must be accepted"
            );
        }
        for refused in [
            "",
            "/bge-small",
            "BAAI/",
            "BAAI/bge-small@",
            "BAAI/bge-small@a5beb1e@2c9f4d1",
            "BAAI/bge-small@a5beb1e/weights",
            "bge-small",
            "BAAI/bge-small/weights",
            "./bge-small",
            "BAAI/..",
            "spaced out",
        ] {
            assert_eq!(
                semantic_model_verdict(SemanticSource::Hf, refused),
                semantic_model_refusal(refused),
                "{refused:?} must be refused"
            );
        }
    }

    #[test]
    fn test_directory_model_names_a_workspace_relative_directory() {
        assert_eq!(
            semantic_model_verdict(SemanticSource::Directory, "vendor/bge-small"),
            Ok(())
        );
        for refused in [
            "",
            "/vendor/bge-small",
            "vendor\\bge-small",
            "vendor/../bge-small",
        ] {
            assert_eq!(
                semantic_model_verdict(SemanticSource::Directory, refused),
                semantic_model_refusal(refused),
                "{refused:?} must be refused"
            );
        }
    }

    #[test]
    fn test_semantic_model_byte_bound_holds_under_both_sources() {
        let bound = SEMANTIC_MODEL_BYTES_MAX;
        let cases = [
            (
                SemanticSource::Hf,
                format!("{}/{}", "a".repeat(63), "b".repeat(64)),
            ),
            (SemanticSource::Directory, "a".repeat(bound)),
        ];
        for (source, accepted) in cases {
            assert_eq!(accepted.len(), bound, "the case must sit on the bound");
            assert_eq!(
                semantic_model_verdict(source, &accepted),
                Ok(()),
                "{bound} bytes must be accepted"
            );
            let refused = format!("{accepted}a");
            assert_eq!(
                semantic_model_verdict(source, &refused),
                semantic_model_refusal(&refused),
                "one byte past the bound must be refused"
            );
        }
    }

    #[test]
    fn test_semantic_source_parses_both_declared_values() {
        for (spelling, declared) in [
            ("hf", SemanticSource::Hf),
            ("directory", SemanticSource::Directory),
        ] {
            let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
                "search": {
                    "semantic": { "source": spelling, "model": "vendor/bge-small" }
                }
            }))
            .expect("a declared source must parse");
            assert_eq!(configuration.search.semantic.source, declared);
            assert_eq!(configuration.validate(), Ok(()));
        }
    }

    #[test]
    fn test_semantic_source_outside_the_declared_pair_is_refused() {
        let error = serde_json::from_value::<WorkspaceConfiguration>(json!({
            "search": { "semantic": { "source": "hub" } }
        }))
        .expect_err("a source outside the declared pair must be refused");
        assert!(
            error.to_string().contains("unknown variant"),
            "the refusal must name the unknown variant: {error}"
        );
    }

    #[test]
    fn test_omitted_semantic_source_keeps_the_default() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "search": { "semantic": { "model": SEMANTIC_MODEL_DEFAULT } }
        }))
        .expect("an omitted source must keep its default");
        assert_eq!(
            configuration.search.semantic.source,
            SEMANTIC_SOURCE_DEFAULT
        );
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_search_weights_must_be_a_pair_of_shares() {
        let mut configuration = WorkspaceConfiguration::default();
        assert_eq!(
            configuration.validate(),
            Ok(()),
            "the defaults must sum to 1"
        );
        configuration.search.lexical.weight = 0.5;
        configuration.search.semantic.weight = 0.5;
        assert_eq!(configuration.validate(), Ok(()));
        configuration.search.semantic.weight = 0.6;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::SearchWeightsInvalid {
                lexical: 0.5,
                semantic: 0.6
            })
        ));
        configuration.search.lexical.weight = -0.1;
        configuration.search.semantic.weight = 1.1;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::SearchWeightsInvalid { .. })
        ));
        configuration.search.lexical.weight = f64::NAN;
        configuration.search.semantic.weight = f64::NAN;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::SearchWeightsInvalid { .. })
        ));
    }

    /// One key of `[search.semantic]`, and one way to push it out of range.
    type SemanticBoundCase = (&'static str, fn(&mut SemanticSearchConfiguration));

    /// Whether a configured weight is the expected share. Weights round-trip
    /// through TOML and JSON as written, so the two differ only by the error
    /// one decimal literal carries.
    fn is_weight(configured: f64, expected: f64) -> bool {
        (configured - expected).abs() < f64::EPSILON
    }

    #[test]
    fn test_semantic_numeric_bounds_are_enforced() {
        let cases: [SemanticBoundCase; 6] = [
            ("search.semantic.download_timeout", |semantic| {
                semantic.download_timeout =
                    Duration::from_millis(SEMANTIC_DOWNLOAD_TIMEOUT_MS_MAX + 1);
            }),
            ("search.semantic.download_attempts", |semantic| {
                semantic.download_attempts = SEMANTIC_DOWNLOAD_ATTEMPTS_MAX + 1;
            }),
            ("search.semantic.batch_declarations", |semantic| {
                semantic.batch_declarations = SEMANTIC_BATCH_DECLARATIONS_MAX + 1;
            }),
            ("search.semantic.max_tokens", |semantic| {
                semantic.max_tokens = SEMANTIC_MAX_TOKENS_MIN - 1;
            }),
            ("search.semantic.candidates", |semantic| {
                semantic.candidates = SEMANTIC_CANDIDATES_MAX + 1;
            }),
            ("search.semantic.max_vectors", |semantic| {
                semantic.max_vectors = SEMANTIC_MAX_VECTORS_MIN - 1;
            }),
        ];
        for (field, apply) in cases {
            let mut configuration = WorkspaceConfiguration::default();
            apply(&mut configuration.search.semantic);
            let violation = configuration
                .validate()
                .expect_err("the bound must refuse the value");
            assert!(
                matches!(
                    violation,
                    ConfigurationViolation::LimitOutOfRange { field: reported, .. }
                        if reported == field
                ),
                "expected {field} to be reported, got {violation:?}"
            );
        }
    }

    #[test]
    fn test_disabled_semantic_search_still_validates_its_own_keys() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.semantic.disabled = true;
        assert_eq!(configuration.validate(), Ok(()));
        configuration.search.semantic.candidates = SEMANTIC_CANDIDATES_MAX + 1;
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange { .. })
        ));
    }

    #[test]
    fn test_search_fusion_k_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        for accepted in [SEARCH_FUSION_K_MIN, SEARCH_FUSION_K_MAX] {
            configuration.search.fusion_k = accepted;
            assert_eq!(configuration.validate(), Ok(()), "{accepted} is in range");
        }
        for refused in [SEARCH_FUSION_K_MIN - 1, SEARCH_FUSION_K_MAX + 1] {
            configuration.search.fusion_k = refused;
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "search.fusion_k",
                        ..
                    })
                ),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn test_search_pool_slots_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        for slots in [SEARCH_POOL_SLOTS_MIN - 1, SEARCH_POOL_SLOTS_MAX + 1] {
            configuration.search.pool_slots = slots;
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "search.pool_slots",
                        ..
                    })
                ),
                "pool_slots {slots} must be refused"
            );
        }
        for slots in [SEARCH_POOL_SLOTS_MIN, SEARCH_POOL_SLOTS_MAX] {
            configuration.search.pool_slots = slots;
            assert_eq!(configuration.validate(), Ok(()));
        }
    }

    #[test]
    fn test_search_busy_timeout_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        for timeout_ms in [
            SEARCH_BUSY_TIMEOUT_MS_MIN - 1,
            SEARCH_BUSY_TIMEOUT_MS_MAX + 1,
        ] {
            configuration.search.busy_timeout = Duration::from_millis(timeout_ms);
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "search.busy_timeout",
                        ..
                    })
                ),
                "busy_timeout {timeout_ms}ms must be refused"
            );
        }
        for timeout_ms in [SEARCH_BUSY_TIMEOUT_MS_MIN, SEARCH_BUSY_TIMEOUT_MS_MAX] {
            configuration.search.busy_timeout = Duration::from_millis(timeout_ms);
            assert_eq!(configuration.validate(), Ok(()));
        }
    }

    #[test]
    fn test_search_text_defaults_include_markdown_and_text_extensions() {
        let configuration = WorkspaceConfiguration::default();
        assert_eq!(
            configuration.search.text.extensions,
            vec!["md".to_owned(), "mdx".to_owned(), "txt".to_owned()]
        );
        assert_eq!(
            configuration.search.text.max_chunk,
            ByteSize::from_bytes(TEXT_CHUNK_BYTES_DEFAULT)
        );
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_search_text_extension_charset_and_length_are_checked() {
        let mut configuration = WorkspaceConfiguration::default();
        for entry in ["", ".md", "MD", "md-x", "m d", "n".repeat(17).as_str()] {
            configuration.search.text.extensions = vec![entry.to_owned()];
            let violation = configuration
                .validate()
                .expect_err(&format!("{entry:?} must be refused"));
            assert_eq!(
                violation,
                ConfigurationViolation::TextExtensionInvalid {
                    extension: entry.to_owned(),
                }
            );
        }
        configuration.search.text.extensions = vec!["n".repeat(16)];
        assert_eq!(
            configuration.validate(),
            Ok(()),
            "an extension at the exact byte bound must be accepted"
        );
    }

    #[test]
    fn test_search_text_extension_duplicates_are_refused() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.extensions = vec!["md".to_owned(), "md".to_owned()];
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::TextExtensionDuplicate {
                extension: "md".to_owned(),
            })
        );
    }

    #[test]
    fn test_search_text_extensions_accept_the_cap_and_refuse_above_it() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.extensions = (0..TEXT_EXTENSIONS_MAX)
            .map(|index| format!("e{index}"))
            .collect();
        assert_eq!(configuration.validate(), Ok(()));

        configuration
            .search
            .text
            .extensions
            .push("overflow".to_owned());
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "search.text.extensions",
                ..
            })
        ));
    }

    #[test]
    fn test_search_text_max_chunk_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.search.text.max_chunk = ByteSize::from_bytes(TEXT_CHUNK_BYTES_MIN);
        assert_eq!(configuration.validate(), Ok(()));
        configuration.search.text.max_chunk = ByteSize::from_bytes(TEXT_CHUNK_BYTES_MAX);
        assert_eq!(configuration.validate(), Ok(()));

        configuration.search.text.max_chunk = ByteSize::from_bytes(TEXT_CHUNK_BYTES_MIN - 1);
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "search.text.max_chunk",
                ..
            })
        ));
        configuration.search.text.max_chunk = ByteSize::from_bytes(TEXT_CHUNK_BYTES_MAX + 1);
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "search.text.max_chunk",
                ..
            })
        ));
    }

    #[test]
    fn test_search_table_parses_text_keys_from_a_full_configuration() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "search": {
                "lexical": { "weight": 0.8 },
                "semantic": {
                    "weight": 0.2,
                    "model": "BAAI/bge-small-en-v1.5",
                    "download_timeout": "5m",
                    "max_vectors": 50000,
                },
                "text": {
                    "extensions": ["md", "rst"],
                    "max_chunk": "2mb",
                },
            }
        }))
        .expect("a full [search] table with text keys must parse");
        assert_eq!(
            configuration.search.text.extensions,
            vec!["md".to_owned(), "rst".to_owned()]
        );
        assert_eq!(
            configuration.search.text.max_chunk,
            ByteSize::from_bytes(2 << 20)
        );
        assert!(is_weight(configuration.search.lexical.weight, 0.8));
        assert!(is_weight(configuration.search.semantic.weight, 0.2));
        assert_eq!(configuration.search.semantic.max_vectors, 50_000);
        assert_eq!(
            configuration.search.semantic.download_timeout,
            Duration::from_millis(300_000)
        );
        assert_eq!(
            configuration.search.semantic.model, SEMANTIC_MODEL_DEFAULT,
            "an omitted key keeps its default while its siblings are set"
        );
        assert_eq!(configuration.validate(), Ok(()));
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
                |hook| hook.timeout = Duration::from_millis(0),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.timeout",
                            ..
                        }
                    )
                },
            ),
            (
                |hook| {
                    hook.output_limit = ByteSize::from_bytes(HOOK_OUTPUT_BYTES_MIN - 1);
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "hooks.output_limit",
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
            field: "hooks.timeout",
            value: 0,
            min: 1,
            max: HOOK_TIMEOUT_MS_MAX,
        };
        assert_eq!(
            violation.evidence(),
            vec![
                ("field", "hooks.timeout".to_owned()),
                ("value", "0".to_owned()),
                ("range", "1..=3600000".to_owned()),
            ]
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one data row per violation variant")]
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
                ConfigurationViolation::SemanticModelInvalid {
                    field: "search.semantic.model",
                    value: "spaced out".to_owned(),
                },
                vec![
                    ("field", "search.semantic.model".to_owned()),
                    ("value", "spaced out".to_owned()),
                ],
            ),
            (
                ConfigurationViolation::SearchWeightsInvalid {
                    lexical: 0.5,
                    semantic: 0.6,
                },
                vec![
                    ("lexical_weight", "0.5".to_owned()),
                    ("semantic_weight", "0.6".to_owned()),
                ],
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
            (
                ConfigurationViolation::PathPatternInvalid {
                    field: "source.include",
                    pattern: "src\\lib.rs".to_owned(),
                },
                vec![
                    ("field", "source.include".to_owned()),
                    ("pattern", "src\\lib.rs".to_owned()),
                ],
            ),
            (
                ConfigurationViolation::TextExtensionInvalid {
                    extension: ".md".to_owned(),
                },
                vec![("extension", ".md".to_owned())],
            ),
            (
                ConfigurationViolation::TextExtensionDuplicate {
                    extension: "md".to_owned(),
                },
                vec![("extension", "md".to_owned())],
            ),
            (
                ConfigurationViolation::EngineNameInvalid {
                    name: "Ty".to_owned(),
                },
                vec![("name", "Ty".to_owned())],
            ),
            (
                ConfigurationViolation::EngineProgramEmpty {
                    engine: "ty".to_owned(),
                },
                vec![("engine", "ty".to_owned())],
            ),
            (
                ConfigurationViolation::EngineLanguagesEmpty {
                    engine: "ty".to_owned(),
                },
                vec![("engine", "ty".to_owned())],
            ),
            (
                ConfigurationViolation::EngineInitializationOptionsNotObject {
                    engine: "ty".to_owned(),
                },
                vec![("engine", "ty".to_owned())],
            ),
            (
                ConfigurationViolation::EngineExecutableAbsolute {
                    engine: "ty".to_owned(),
                    program: "/usr/bin/ty".to_owned(),
                },
                vec![
                    ("engine", "ty".to_owned()),
                    ("program", "/usr/bin/ty".to_owned()),
                ],
            ),
            (
                ConfigurationViolation::EngineArgumentOversized {
                    engine: "ty".to_owned(),
                    bytes: 5_000,
                },
                vec![
                    ("engine", "ty".to_owned()),
                    ("bytes", "5000".to_owned()),
                    ("bytes_max", ENGINE_ARGUMENT_BYTES_MAX.to_string()),
                ],
            ),
            (
                ConfigurationViolation::EngineEnvironmentKeyInvalid {
                    engine: "ty".to_owned(),
                    key: "BAD=KEY".to_owned(),
                },
                vec![("engine", "ty".to_owned()), ("key", "BAD=KEY".to_owned())],
            ),
            (
                ConfigurationViolation::EngineLanguageInvalid {
                    engine: "ty".to_owned(),
                    language: "Python".to_owned(),
                },
                vec![
                    ("engine", "ty".to_owned()),
                    ("language", "Python".to_owned()),
                ],
            ),
            (
                ConfigurationViolation::EngineLanguageDuplicate {
                    engine: "b".to_owned(),
                    language: "python".to_owned(),
                },
                vec![
                    ("engine", "b".to_owned()),
                    ("language", "python".to_owned()),
                ],
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
    fn test_engine_table_defaults_apply_to_the_optional_keys() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "engines": { "ty": { "program": "uvx", "languages": ["python"] } }
        }))
        .expect("a minimal engine table must deserialize");
        let engine = &configuration.engines["ty"];
        assert!(engine.arguments.is_empty());
        assert!(engine.environment.is_empty());
        assert_eq!(engine.initialization_options, None);
        assert_eq!(
            engine.startup_timeout,
            Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_DEFAULT)
        );
        assert_eq!(
            engine.request_timeout,
            Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_DEFAULT)
        );
        assert_eq!(
            engine.output_limit,
            ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_DEFAULT)
        );
        assert_eq!(engine.retry, RetryPolicy::default());
        assert_eq!(engine.restart, RestartPolicy::default());
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_engine_retry_and_restart_tables_decode_key_by_key() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "engines": { "ty": {
                "program": "uvx",
                "languages": ["python"],
                "retry": { "attempts": 3, "delay": "1s" },
                "restart": { "attempts": 0 },
            } }
        }))
        .expect("the nested tables must deserialize");
        let engine = &configuration.engines["ty"];
        assert_eq!(engine.retry.attempts, 3);
        assert_eq!(engine.retry.delay, Duration::from_millis(1_000));
        assert_eq!(
            engine.retry.delay_limit,
            RetryPolicy::default().delay_limit,
            "an absent key inside the table keeps its own default"
        );
        assert_eq!(engine.restart.attempts, 0);
        assert_eq!(engine.restart.window, RestartPolicy::default().window);
        assert_eq!(configuration.validate(), Ok(()));
        let value = serde_json::to_value(&configuration).expect("serialize");
        assert_eq!(value["engines"]["ty"]["retry"]["delay"], json!("1s"));
        assert_eq!(value["engines"]["ty"]["restart"]["window"], json!("5m"));
    }

    #[test]
    fn test_engine_table_requires_program_and_languages() {
        for trimmed in [
            json!({ "languages": ["python"] }),
            json!({ "program": "uvx" }),
        ] {
            let table = json!({ "engines": { "ty": trimmed } });
            assert!(
                serde_json::from_value::<WorkspaceConfiguration>(table.clone()).is_err(),
                "{table} must be refused"
            );
        }
    }

    #[test]
    fn test_two_engine_table_round_trips_with_exact_wire_names() {
        let mut typescript = engine();
        typescript.program = "bunx".to_owned();
        typescript.arguments = vec![
            "typescript-language-server".to_owned(),
            "--stdio".to_owned(),
        ];
        typescript.languages = vec!["typescript".to_owned(), "typescript:tsx".to_owned()];
        typescript.initialization_options = Some(json!({ "tsserver": { "logVerbosity": "off" } }));
        let configuration = engines(vec![("ty", engine()), ("typescript", typescript)]);
        assert_eq!(configuration.validate(), Ok(()));
        let value = serde_json::to_value(&configuration).expect("serialize");
        assert_eq!(value["engines"]["ty"]["startup_timeout"], json!("30s"));
        assert_eq!(value["engines"]["ty"]["request_timeout"], json!("1m"));
        assert_eq!(value["engines"]["ty"]["output_limit"], json!("4kb"));
        assert_eq!(
            value["engines"]["typescript"]["languages"],
            json!(["typescript", "typescript:tsx"])
        );
        let round_tripped: WorkspaceConfiguration =
            serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, configuration);
    }

    /// One way to break an engine bound, and the check its refusal must
    /// pass.
    type EngineBoundCase = (
        fn(&mut EngineConfiguration),
        fn(&ConfigurationViolation) -> bool,
    );

    /// Proves each broken engine is refused with the expected violation,
    /// and that the matcher rejects an unrelated one.
    fn assert_engines_refused<const CASES: usize>(break_and_expect: [EngineBoundCase; CASES]) {
        let unrelated = ConfigurationViolation::HookIdDuplicate {
            id: "unrelated".to_owned(),
        };
        for (break_bound, expected) in break_and_expect {
            let mut broken = engine();
            break_bound(&mut broken);
            let configuration = engines(vec![("ty", broken)]);
            let violation = configuration
                .validate()
                .expect_err("the broken engine must be refused");
            assert!(expected(&violation), "unexpected violation {violation:?}");
            assert!(
                !expected(&unrelated),
                "the matcher must reject an unrelated violation"
            );
        }
    }

    #[test]
    fn test_engine_command_rules_are_enforced() {
        let break_and_expect: [EngineBoundCase; 3] = [
            (
                |engine| engine.program = String::new(),
                |violation| matches!(violation, ConfigurationViolation::EngineProgramEmpty { .. }),
            ),
            (
                |engine| engine.program = "/usr/bin/ty".to_owned(),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineExecutableAbsolute { .. }
                    )
                },
            ),
            (
                |engine| engine.program = "C:\\tools\\ty.exe".to_owned(),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineExecutableAbsolute { .. }
                    )
                },
            ),
        ];
        assert_engines_refused(break_and_expect);
    }

    #[test]
    fn test_engine_language_rules_are_enforced() {
        let break_and_expect: [EngineBoundCase; 4] = [
            (
                |engine| engine.languages = Vec::new(),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineLanguagesEmpty { .. }
                    )
                },
            ),
            (
                |engine| engine.languages = vec!["Python".to_owned()],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineLanguageInvalid { .. }
                    )
                },
            ),
            (
                |engine| engine.languages = vec!["sql:".to_owned()],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineLanguageInvalid { .. }
                    )
                },
            ),
            (
                |engine| engine.languages = vec!["python".to_owned(), "python".to_owned()],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineLanguageDuplicate { .. }
                    )
                },
            ),
        ];
        assert_engines_refused(break_and_expect);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one data row per engine bound")]
    fn test_engine_bounds_are_enforced() {
        let break_and_expect: [EngineBoundCase; 9] = [
            (
                |engine| engine.arguments = vec![String::new(); ENGINE_ARGUMENTS_MAX + 1],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.arguments",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.arguments = vec!["x".repeat(ENGINE_ARGUMENT_BYTES_MAX + 1)],
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineArgumentOversized { .. }
                    )
                },
            ),
            (
                |engine| {
                    engine.environment = (0..=ENGINE_ENVIRONMENT_ENTRIES_MAX)
                        .map(|index| (format!("KEY_{index}"), "1".to_owned()))
                        .collect();
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.environment",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| {
                    engine.environment = [("BAD=KEY".to_owned(), "1".to_owned())].into();
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::EngineEnvironmentKeyInvalid { .. }
                    )
                },
            ),
            (
                |engine| {
                    engine.languages = (0..=ENGINE_LANGUAGES_MAX)
                        .map(|index| format!("language{index}"))
                        .collect();
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.languages",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| {
                    engine.startup_timeout =
                        Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_MIN - 1);
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.startup_timeout",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| {
                    engine.request_timeout =
                        Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_MAX + 1);
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.request_timeout",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.output_limit = ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_MIN - 1),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.output_limit",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.output_limit = ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_MAX + 1),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.output_limit",
                            ..
                        }
                    )
                },
            ),
        ];
        assert_engines_refused(break_and_expect);
    }

    #[test]
    fn test_engine_retry_and_restart_bounds_are_enforced() {
        let break_and_expect: [EngineBoundCase; 6] = [
            (
                |engine| engine.retry.attempts = RETRY_ATTEMPTS_MIN - 1,
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.retry.attempts",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.retry.attempts = RETRY_ATTEMPTS_MAX + 1,
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.retry.attempts",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.retry.delay = Duration::from_millis(RETRY_DELAY_MS_MIN - 1),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.retry.delay",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| {
                    engine.retry.delay_limit = Duration::from_millis(RETRY_DELAY_LIMIT_MS_MAX + 1);
                },
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.retry.delay_limit",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.restart.attempts = RESTART_ATTEMPTS_MAX + 1,
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.restart.attempts",
                            ..
                        }
                    )
                },
            ),
            (
                |engine| engine.restart.window = Duration::from_millis(RESTART_WINDOW_MS_MIN - 1),
                |violation| {
                    matches!(
                        violation,
                        ConfigurationViolation::LimitOutOfRange {
                            field: "engines.restart.window",
                            ..
                        }
                    )
                },
            ),
        ];
        assert_engines_refused(break_and_expect);
    }

    #[test]
    fn test_engine_bounds_accept_their_edges() {
        let mut edged = engine();
        edged.arguments = vec!["x".repeat(ENGINE_ARGUMENT_BYTES_MAX); ENGINE_ARGUMENTS_MAX];
        edged.environment = (0..ENGINE_ENVIRONMENT_ENTRIES_MAX)
            .map(|index| (format!("KEY_{index}"), "1".to_owned()))
            .collect();
        edged.languages = (0..ENGINE_LANGUAGES_MAX)
            .map(|index| format!("language{index}"))
            .collect();
        edged.startup_timeout = Duration::from_millis(ENGINE_STARTUP_TIMEOUT_MS_MAX);
        edged.request_timeout = Duration::from_millis(ENGINE_REQUEST_TIMEOUT_MS_MIN);
        edged.output_limit = ByteSize::from_bytes(ENGINE_OUTPUT_BYTES_MAX);
        edged.retry = RetryPolicy {
            attempts: RETRY_ATTEMPTS_MAX,
            delay: Duration::from_millis(RETRY_DELAY_MS_MIN),
            delay_limit: Duration::from_millis(RETRY_DELAY_LIMIT_MS_MAX),
        };
        edged.restart = RestartPolicy {
            attempts: RESTART_ATTEMPTS_MIN,
            window: Duration::from_millis(RESTART_WINDOW_MS_MAX),
        };
        assert_eq!(engines(vec![("ty", edged)]).validate(), Ok(()));
    }

    #[test]
    fn test_engine_name_key_charset_is_checked() {
        for name in ["Ty", "spaced name", "", ":python", "9ty"] {
            let configuration = engines(vec![(name, engine())]);
            assert_eq!(
                configuration.validate(),
                Err(ConfigurationViolation::EngineNameInvalid {
                    name: name.to_owned(),
                }),
                "{name:?} must be refused"
            );
        }
        for name in ["ty", "typescript", "engine-2", "rust.analyzer"] {
            let configuration = engines(vec![(name, engine())]);
            assert_eq!(
                configuration.validate(),
                Ok(()),
                "{name:?} must be accepted"
            );
        }
    }

    #[test]
    fn test_cross_engine_language_duplicates_name_the_colliding_segment() {
        let mut second = engine();
        second.program = "pyright".to_owned();
        let configuration = engines(vec![("a", engine()), ("b", second)]);
        assert_eq!(
            configuration.validate(),
            Err(ConfigurationViolation::EngineLanguageDuplicate {
                engine: "b".to_owned(),
                language: "python".to_owned(),
            })
        );
    }

    #[test]
    fn test_engine_initialization_options_must_be_an_object() {
        for options in [json!([1]), json!("text"), json!(7), json!(true)] {
            let mut broken = engine();
            broken.initialization_options = Some(options.clone());
            assert_eq!(
                engines(vec![("ty", broken)]).validate(),
                Err(
                    ConfigurationViolation::EngineInitializationOptionsNotObject {
                        engine: "ty".to_owned(),
                    }
                ),
                "{options} must be refused"
            );
        }
        let mut accepted = engine();
        accepted.initialization_options = Some(json!({ "settings": {} }));
        assert_eq!(engines(vec![("ty", accepted)]).validate(), Ok(()));
    }

    #[test]
    fn test_engine_count_above_the_cap_is_refused() {
        let entries: Vec<(String, EngineConfiguration)> = (0..=ENGINES_MAX)
            .map(|index| {
                let mut numbered = engine();
                numbered.languages = vec![format!("language{index}")];
                (format!("engine{index}"), numbered)
            })
            .collect();
        let configuration = WorkspaceConfiguration {
            engines: entries.into_iter().collect(),
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "engines",
                ..
            })
        ));
    }

    #[test]
    fn test_engine_schema_bounds_and_defaults_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let engine = &schema["$defs"]["EngineConfiguration"]["properties"];
        let cases = [
            ("program min", &engine["program"]["minLength"], json!(1)),
            (
                "arguments max",
                &engine["arguments"]["maxItems"],
                json!(ENGINE_ARGUMENTS_MAX),
            ),
            ("languages min", &engine["languages"]["minItems"], json!(1)),
            (
                "languages max",
                &engine["languages"]["maxItems"],
                json!(ENGINE_LANGUAGES_MAX),
            ),
            (
                "startup timeout default",
                &engine["startup_timeout"]["default"],
                json!("30s"),
            ),
            (
                "request timeout default",
                &engine["request_timeout"]["default"],
                json!("1m"),
            ),
            (
                "output limit default",
                &engine["output_limit"]["default"],
                json!("4kb"),
            ),
        ];
        assert_schema_bounds(&cases);
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
    /// Asserts each advertised schema bound equals its enforced constant.
    fn assert_schema_bounds(cases: &[(&str, &serde_json::Value, serde_json::Value)]) {
        for (name, advertised, enforced) in cases {
            assert_eq!(
                *advertised, enforced,
                "the schema's {name} bound must equal the enforced constant"
            );
        }
    }

    #[test]
    fn test_fusion_schema_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let definitions = &schema["$defs"];
        let search = &definitions["SearchConfiguration"]["properties"];
        let lexical = &definitions["LexicalSearchConfiguration"]["properties"];
        let semantic = &definitions["SemanticSearchConfiguration"]["properties"];
        let cases = [
            (
                "fusion k min",
                &search["fusion_k"]["minimum"],
                json!(SEARCH_FUSION_K_MIN),
            ),
            (
                "fusion k max",
                &search["fusion_k"]["maximum"],
                json!(SEARCH_FUSION_K_MAX),
            ),
            ("model min", &semantic["model"]["minLength"], json!(1)),
            (
                "model max",
                &semantic["model"]["maxLength"],
                json!(SEMANTIC_MODEL_BYTES_MAX),
            ),
            (
                "semantic disabled default",
                &semantic["disabled"]["default"],
                json!(false),
            ),
            (
                "lexical weight min",
                &lexical["weight"]["minimum"],
                json!(0.0),
            ),
            (
                "lexical weight max",
                &lexical["weight"]["maximum"],
                json!(1.0),
            ),
            (
                "semantic weight min",
                &semantic["weight"]["minimum"],
                json!(0.0),
            ),
            (
                "semantic weight max",
                &semantic["weight"]["maximum"],
                json!(1.0),
            ),
            (
                "download attempts min",
                &semantic["download_attempts"]["minimum"],
                json!(SEMANTIC_DOWNLOAD_ATTEMPTS_MIN),
            ),
            (
                "download attempts max",
                &semantic["download_attempts"]["maximum"],
                json!(SEMANTIC_DOWNLOAD_ATTEMPTS_MAX),
            ),
            (
                "batch declarations min",
                &semantic["batch_declarations"]["minimum"],
                json!(SEMANTIC_BATCH_DECLARATIONS_MIN),
            ),
            (
                "batch declarations max",
                &semantic["batch_declarations"]["maximum"],
                json!(SEMANTIC_BATCH_DECLARATIONS_MAX),
            ),
            (
                "max tokens min",
                &semantic["max_tokens"]["minimum"],
                json!(SEMANTIC_MAX_TOKENS_MIN),
            ),
            (
                "max tokens max",
                &semantic["max_tokens"]["maximum"],
                json!(SEMANTIC_MAX_TOKENS_MAX),
            ),
            (
                "candidates min",
                &semantic["candidates"]["minimum"],
                json!(SEMANTIC_CANDIDATES_MIN),
            ),
            (
                "candidates max",
                &semantic["candidates"]["maximum"],
                json!(SEMANTIC_CANDIDATES_MAX),
            ),
            (
                "max vectors min",
                &semantic["max_vectors"]["minimum"],
                json!(SEMANTIC_MAX_VECTORS_MIN),
            ),
            (
                "max vectors max",
                &semantic["max_vectors"]["maximum"],
                json!(SEMANTIC_MAX_VECTORS_MAX),
            ),
        ];
        assert_schema_bounds(&cases);
    }

    #[test]
    fn test_semantic_source_schema_default_equals_the_enforced_constant() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let semantic = &schema["$defs"]["SemanticSearchConfiguration"]["properties"];
        let enforced = serde_json::to_value(SEMANTIC_SOURCE_DEFAULT)
            .expect("the default source must serialize");
        assert_schema_bounds(&[(
            "semantic source default",
            &semantic["source"]["default"],
            enforced,
        )]);
    }

    #[test]
    fn test_table_schema_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let definitions = &schema["$defs"];
        let server = &definitions["ServerConfiguration"]["properties"];
        let execution = &definitions["ExecutionConfiguration"]["properties"];
        let history = &definitions["HistoryConfiguration"]["properties"];
        let search = &definitions["SearchConfiguration"]["properties"];
        let source = &definitions["SourceConfiguration"]["properties"];
        let text = &definitions["TextSearchConfiguration"]["properties"];
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
                "num workers min",
                &server["num_workers"]["minimum"],
                json!(1),
            ),
            (
                "num workers max",
                &server["num_workers"]["maximum"],
                json!(SERVER_NUM_WORKERS_MAX),
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
            (
                "pool slots min",
                &search["pool_slots"]["minimum"],
                json!(SEARCH_POOL_SLOTS_MIN),
            ),
            (
                "pool slots max",
                &search["pool_slots"]["maximum"],
                json!(SEARCH_POOL_SLOTS_MAX),
            ),
            (
                "pool slots default",
                &search["pool_slots"]["default"],
                json!(4),
            ),
            (
                "busy timeout default",
                &search["busy_timeout"]["default"],
                json!("1s"),
            ),
            (
                "source include max",
                &source["include"]["maxItems"],
                json!(SOURCE_PATTERNS_MAX),
            ),
            (
                "source exclude max",
                &source["exclude"]["maxItems"],
                json!(SOURCE_PATTERNS_MAX),
            ),
            (
                "text extensions max",
                &text["extensions"]["maxItems"],
                json!(TEXT_EXTENSIONS_MAX),
            ),
            (
                "text extensions default",
                &text["extensions"]["default"],
                json!(["md", "mdx", "txt"]),
            ),
        ];
        assert_schema_bounds(&cases);
    }

    #[test]
    fn test_hook_schema_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let definitions = &schema["$defs"];
        let hook = &definitions["CommandHook"]["properties"];
        let guarantee = &definitions["HookGuarantee"]["properties"];
        let cases = [
            ("id min", &hook["id"]["minLength"], json!(1)),
            ("id max", &hook["id"]["maxLength"], json!(HOOK_ID_BYTES_MAX)),
            ("program min", &hook["program"]["minLength"], json!(1)),
            (
                "arguments max",
                &hook["arguments"]["maxItems"],
                json!(HOOK_ARGUMENTS_MAX),
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
        assert_schema_bounds(&cases);
    }
}
