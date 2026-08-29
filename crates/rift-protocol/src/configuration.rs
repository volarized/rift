//! Model of the workspace configuration file `rift.toml`.
//!
//! Every type here is a contract: serde attributes define exactly what the
//! file may say, and the exported `rift.schema.json` derives from these
//! definitions. The numeric bounds the types advertise are enforced by
//! [`WorkspaceConfiguration::validate`]; a file that breaks one is refused
//! whole as `configuration_invalid`.

use std::collections::BTreeMap;

use crate::lock::{SERVER_PORT_FLOOR, SERVER_PORT_MAX, SERVER_PORT_MIN};
use crate::read::{CoverageScope, Language, PathPattern, ProjectPath};
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
/// Milliseconds the server serves after the last request completes before
/// it stops, at least: one second.
pub const SERVER_IDLE_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds the server serves after the last request completes before
/// it stops, at most: one day.
pub const SERVER_IDLE_TIMEOUT_MS_MAX: u64 = 86_400_000;
/// Milliseconds a request waits for the workspace and its language services to
/// prove they are ready to answer, at least: one second.
pub const SERVER_READINESS_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds a request waits for the workspace and its language services to
/// prove they are ready to answer, at most: one hour.
pub const SERVER_READINESS_TIMEOUT_MS_MAX: u64 = 3_600_000;
/// Milliseconds [`ServerConfiguration::readiness_timeout`] defaults to.
const SERVER_READINESS_TIMEOUT_MS_DEFAULT: u64 = 30_000;
/// Records the log store keeps, at least.
pub const LOGS_RETENTION_RECORDS_MIN: u64 = 100;
/// Records the log store keeps, at most.
pub const LOGS_RETENTION_RECORDS_MAX: u64 = 1_000_000;
/// Records the log store keeps by default.
const LOGS_RETENTION_RECORDS_DEFAULT: u64 = 50_000;
/// Records one `rift://logs` read returns, at most.
pub const LOGS_PAGE_RECORDS_MAX: u64 = 5_000;
/// Records one `rift://logs` read returns by default.
const LOGS_PAGE_RECORDS_DEFAULT: u64 = 500;
/// Bytes `logs.capture` may hold, at most.
pub const LOGS_CAPTURE_BYTES_MAX: usize = 512;
/// The filter the log store captures under by default: the same targets the
/// stderr diagnostics carry.
const LOGS_CAPTURE_DEFAULT: &str = "rift=info,rift_mcp=info,rift_server=info";

/// Bytes one submitted execution block may hold, at most.
pub const EXECUTION_CODE_BYTES_MAX: u64 = 32 << 10;
/// Milliseconds one evaluation may run, at most: one day.
pub const EXECUTION_TIMEOUT_MS_MAX: u64 = 86_400_000;
/// Bytes one captured execution stream may keep, at most.
pub const EXECUTION_OUTPUT_BYTES_MAX: u64 = 16 << 10;
/// Evaluations running concurrently across the workspace, at most.
pub const EXECUTION_CONCURRENT_MAX: u64 = 64;
/// Revisions the history provider may walk from the current head, at most.
pub const HISTORY_REVISIONS_MAX: u64 = 100_000;
/// Bytes `search.semantic.model` may hold, at most.
pub const SEMANTIC_MODEL_BYTES_MAX: usize = 128;
/// Configured hooks one workspace may declare, at most.
pub const HOOKS_MAX: usize = 32;
/// Bytes one hook's `id` may hold, at most.
pub const HOOK_ID_BYTES_MAX: usize = 64;
/// Literal arguments one configured command may hold, at most.
pub const COMMAND_ARGUMENTS_MAX: usize = 64;
/// Bytes one configured command argument may hold, at most.
pub const COMMAND_ARGUMENT_BYTES_MAX: usize = 4_096;
/// Path patterns one language, hook, or text-search table may hold, at most.
pub const CONFIGURATION_PATTERNS_MAX: usize = 64;
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
/// Configured exact languages one workspace may declare, at most.
pub const LANGUAGES_MAX: usize = 64;
/// Named LSP processes one workspace may declare, at most.
pub const LSP_CONFIGURATIONS_MAX: usize = 16;
/// Entries one LSP process's `environment` may hold, at most.
pub const LSP_ENVIRONMENT_ENTRIES_MAX: usize = 64;
/// Milliseconds one LSP process may take to initialize, at least: one second.
pub const LSP_STARTUP_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one LSP process may take to initialize, at most: ten minutes.
pub const LSP_STARTUP_TIMEOUT_MS_MAX: u64 = 600_000;
/// Milliseconds `lsp.startup_timeout` holds when the key is absent.
const LSP_STARTUP_TIMEOUT_MS_DEFAULT: u64 = 30_000;
/// Milliseconds one LSP request may run, at least: one second.
pub const LSP_REQUEST_TIMEOUT_MS_MIN: u64 = 1_000;
/// Milliseconds one LSP request may run, at most: ten minutes.
pub const LSP_REQUEST_TIMEOUT_MS_MAX: u64 = 600_000;
/// Milliseconds `lsp.request_timeout` holds when the key is absent.
const LSP_REQUEST_TIMEOUT_MS_DEFAULT: u64 = 60_000;
/// Bytes of each LSP process's standard error Rift keeps, at least.
pub const LSP_OUTPUT_BYTES_MIN: u64 = 1_024;
/// Bytes of each LSP process's standard error Rift keeps, at most.
pub const LSP_OUTPUT_BYTES_MAX: u64 = 8 << 20;
/// Bytes `lsp.output_limit` holds when the key is absent.
const LSP_OUTPUT_BYTES_DEFAULT: u64 = 4_096;

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
#[schemars(transform = crate::schema::declare_workspace_contract)]
pub struct WorkspaceConfiguration {
    /// The server's own blocking-work bounds: worker count and queue wait.
    pub server: ServerConfiguration,
    /// Bounds and switches for the built-in providers.
    pub providers: ProvidersConfiguration,
    /// Enablement and limits for caller-provided code.
    pub execution: ExecutionConfiguration,
    /// Search ranking, visible text chunking, and storage bounds.
    pub search: SearchConfiguration,
    /// Which files below the workspace root the index and reads consider visible.
    pub source: SourceConfiguration,
    /// The server's own log records: how many the workspace database keeps,
    /// how many one read returns, and which targets are captured.
    pub logs: LogsConfiguration,
    /// Hooks run in the changed tree, in list order, each time a change
    /// applies.
    #[schemars(length(max = 32))]
    pub hooks: Vec<CommandHook>,
    /// Exact language entries keyed by their `name` or `name:dialect` identity segment.
    pub languages: BTreeMap<String, LanguageConfiguration>,
    /// Shared LSP processes keyed by name. Language entries select them explicitly.
    pub lsp: BTreeMap<String, LspConfiguration>,
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

    /// Resolves one language entry's named or inline LSP process.
    #[must_use]
    pub fn resolve_language_lsp(&self, identity: &str) -> Option<ResolvedLspConfiguration<'_>> {
        match self.languages.get(identity)?.lsp.as_ref()? {
            LanguageLspConfiguration::Named(name) => {
                self.lsp
                    .get(name)
                    .map(|configuration| ResolvedLspConfiguration {
                        name: Some(name),
                        configuration,
                    })
            }
            LanguageLspConfiguration::Inline(configuration) => Some(ResolvedLspConfiguration {
                name: None,
                configuration,
            }),
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
            .or_else(|| self.logs.violation())
            .or_else(|| hooks_violation(&self.hooks))
            .or_else(|| languages_violation(&self.languages, &self.lsp))
            .or_else(|| lsp_configurations_violation(&self.lsp))
    }
}

/// One language entry's resolved LSP process.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedLspConfiguration<'configuration> {
    /// Shared process name, or absent for an inline process.
    pub name: Option<&'configuration str>,
    /// Process configuration selected by the language entry.
    pub configuration: &'configuration LspConfiguration,
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
    /// Wall-clock span after the last served request completes that stops
    /// the server while no request remains active, 1s to 1d.
    pub idle_timeout: Duration,
    /// Wall-clock bound one request waits for the workspace's index and
    /// its language services to prove they are ready to answer, 1s to 1h.
    /// One deadline: index validation starts it, and once a request
    /// resolves the languages it needs, waiting on those services spends
    /// what remains of it.
    pub readiness_timeout: Duration,
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
            readiness_timeout: Duration::from_millis(SERVER_READINESS_TIMEOUT_MS_DEFAULT),
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
            (
                "server.readiness_timeout",
                self.readiness_timeout.milliseconds(),
                SERVER_READINESS_TIMEOUT_MS_MIN,
                SERVER_READINESS_TIMEOUT_MS_MAX,
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

/// The `[logs]` table. The server records its own diagnostics in the workspace
/// database, where `rift://logs` reads them back, and this table bounds how
/// many records the store keeps, how many one read returns, and which targets
/// are captured at all. The server reads the table at startup, so a change
/// applies on the next start.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfiguration {
    /// Records the store keeps before the oldest are dropped, 100 to 1000000.
    #[schemars(range(min = 100, max = 1_000_000))]
    pub retention_records: u64,
    /// Records one `rift://logs` read returns at most, 1 to 5000.
    #[schemars(range(min = 1, max = 5_000))]
    pub page_records: u64,
    /// Which targets are recorded, in the `RUST_LOG` spelling `tracing` takes:
    /// comma-separated `target=level` pairs. A target this filter excludes
    /// never reaches the store, whatever the stderr diagnostics carry.
    #[schemars(length(max = 512))]
    pub capture: String,
}

impl Default for LogsConfiguration {
    fn default() -> Self {
        Self {
            retention_records: LOGS_RETENTION_RECORDS_DEFAULT,
            page_records: LOGS_PAGE_RECORDS_DEFAULT,
            capture: LOGS_CAPTURE_DEFAULT.to_owned(),
        }
    }
}

impl LogsConfiguration {
    /// The table's bounds in key order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "logs.retention_records",
                self.retention_records,
                LOGS_RETENTION_RECORDS_MIN,
                LOGS_RETENTION_RECORDS_MAX,
            ),
            (
                "logs.page_records",
                self.page_records,
                1,
                LOGS_PAGE_RECORDS_MAX,
            ),
        ])
        .or_else(|| self.capture_violation())
    }

    /// The capture filter's own bound, checked after the numeric rows.
    fn capture_violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([(
            "logs.capture",
            self.capture.len() as u64,
            0,
            LOGS_CAPTURE_BYTES_MAX as u64,
        )])
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

/// The `[execution]` table. Exact language entries enable execution; this
/// table owns the workspace-wide bounds.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_execution_ranges)]
pub struct ExecutionConfiguration {
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
            max_code: ByteSize::from_bytes(16 << 10),
            max_timeout: Duration::from_millis(30_000),
            max_output: ByteSize::from_bytes(8 << 10),
            max_concurrent: 2,
        }
    }
}

impl ExecutionConfiguration {
    /// The table's bounds in key order.
    fn violation(&self) -> Option<ConfigurationViolation> {
        let limits = [
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
        first_out_of_range(limits)
    }
}

/// The `[search]` table. Search fuses a lexical ranking with a semantic one:
/// `lexical` and `semantic` weigh the two against each other, `fusion_k` sets
/// how sharply a top rank counts, `pool_slots` and `busy_timeout` bound the
/// shared `SQLite` connections behind search and logs, and `text` bounds
/// lexical chunks derived from visible text files.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_search_ranges)]
pub struct SearchConfiguration {
    /// Chunking policy for visible text files in lexical search.
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
    /// Pooled `SQLite` connections the workspace database may open at once,
    /// 1 to 16. Search reads and stored logs share this pool.
    #[schemars(range(min = 1, max = 16))]
    #[serde(default = "default_search_pool_slots")]
    pub pool_slots: u64,
    /// Wall-clock budget one connection waits for a database lock held by
    /// another process before `SQLITE_BUSY`, 100ms to 30s.
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
const SEARCH_BUSY_TIMEOUT_MS_DEFAULT: u64 = 5_000;

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
    /// `minishlab/potion-retrieval-32M`, optionally carrying a revision after
    /// `@`; under `directory` a workspace-relative directory holding them.
    /// Vectors are stored per model, so changing the value embeds the
    /// workspace again.
    ///
    /// The model's own `config.json` decides how it is read. A `model_type` of
    /// `model2vec` is a static model, which embeds a declaration by averaging
    /// one row per token; anything else is read as a BERT checkpoint, which
    /// runs a forward pass per batch and costs minutes rather than seconds
    /// over a whole workspace.
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
    /// Declarations one file may contribute to the semantic ranking before
    /// the rest of its declarations are dropped, 1 to 64. Without the bound
    /// one file whose declarations all rank well fills `candidates` on its
    /// own, and no other file reaches the candidate list however well it
    /// would have ranked.
    #[schemars(range(min = 1, max = 64))]
    #[serde(default = "default_semantic_candidates_per_file")]
    pub candidates_per_file: u64,
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
            candidates_per_file: SEMANTIC_CANDIDATES_PER_FILE_DEFAULT,
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
                    "search.semantic.candidates_per_file",
                    self.candidates_per_file,
                    SEMANTIC_CANDIDATES_PER_FILE_MIN,
                    SEMANTIC_CANDIDATES_PER_FILE_MAX,
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
/// `search.semantic.model` when the key is absent: 32M parameters, 512
/// dimensions, MIT, and a static model that carries no transformer at all.
///
/// A workspace is embedded once before its first search can rank
/// semantically, so what the default costs is the wait a new workspace pays.
/// A 12-layer encoder spends a forward pass on every declaration and takes
/// minutes over a workspace of this size on a laptop CPU; this model gathers
/// one row per token and averages them, which is the same answer shape for
/// arithmetic a machine finishes in seconds. It ranks less sharply than a
/// transformer, and the lexical tier carries the larger share of a fused
/// score anyway.
pub const SEMANTIC_MODEL_DEFAULT: &str = "minishlab/potion-retrieval-32M";
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
/// `search.semantic.candidates_per_file` accepted, at least.
pub const SEMANTIC_CANDIDATES_PER_FILE_MIN: u64 = 1;
/// `search.semantic.candidates_per_file` accepted, at most.
pub const SEMANTIC_CANDIDATES_PER_FILE_MAX: u64 = 64;
/// `search.semantic.candidates_per_file` when the key is absent.
const SEMANTIC_CANDIDATES_PER_FILE_DEFAULT: u64 = 3;
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

fn default_semantic_candidates_per_file() -> u64 {
    SEMANTIC_CANDIDATES_PER_FILE_DEFAULT
}

fn default_semantic_max_vectors() -> u64 {
    SEMANTIC_MAX_VECTORS_DEFAULT
}
/// Bytes one lexical chunk from a `search.text` file may hold, at least.
pub const TEXT_CHUNK_BYTES_MIN: u64 = 1 << 10;
/// Bytes one lexical chunk from a `search.text` file may hold, at most.
pub const TEXT_CHUNK_BYTES_MAX: u64 = 16 << 20;
/// Bytes one lexical chunk from a `search.text` file may hold, by default.
pub const TEXT_CHUNK_BYTES_DEFAULT: u64 = 1 << 20;

/// The `[search.text]` table. `max_chunk` bounds lexical units derived from
/// visible text files.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_text_ranges)]
pub struct TextSearchConfiguration {
    /// Project-relative path patterns included as plain text when no language claims them.
    #[schemars(length(max = 64))]
    pub include: Vec<PathPattern>,
    /// Bytes one lexical chunk may hold, 1kb to 16mb. Larger files are indexed as
    /// several chunks of at most this size.
    pub max_chunk: ByteSize,
}

impl Default for TextSearchConfiguration {
    fn default() -> Self {
        Self {
            include: ["**/*.md", "**/*.mdx", "**/*.txt"]
                .map(|pattern| PathPattern(pattern.to_owned()))
                .into(),
            max_chunk: ByteSize::from_bytes(TEXT_CHUNK_BYTES_DEFAULT),
        }
    }
}

impl TextSearchConfiguration {
    /// Table numeric bounds.
    fn violation(&self) -> Option<ConfigurationViolation> {
        first_out_of_range([
            (
                "search.text.include",
                self.include.len() as u64,
                0,
                CONFIGURATION_PATTERNS_MAX as u64,
            ),
            (
                "search.text.max_chunk",
                self.max_chunk.bytes(),
                TEXT_CHUNK_BYTES_MIN,
                TEXT_CHUNK_BYTES_MAX,
            ),
        ])
        .or_else(|| path_patterns_violation("search.text.include", &self.include))
    }
}

/// One executable command as a program string or a program followed by arguments.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CommandInput {
    /// One executable name with no arguments. Whitespace is refused.
    Program(String),
    /// Executable first, followed by its literal arguments.
    ProgramAndArguments(Vec<String>),
}

impl JsonSchema for CommandInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CommandInput".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "oneOf": [
                {
                    "type": "string",
                    "minLength": 1,
                    "pattern": "^\\S+$"
                },
                {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 65,
                    "prefixItems": [{"type": "string", "minLength": 1}],
                    "items": {"type": "string", "maxLength": 4096}
                }
            ]
        })
    }
}

impl CommandInput {
    /// The configured executable.
    #[must_use]
    pub fn program(&self) -> &str {
        match self {
            Self::Program(program) => program,
            Self::ProgramAndArguments(command) => command.first().map_or("", String::as_str),
        }
    }

    /// The configured literal arguments.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        match self {
            Self::Program(_) => &[],
            Self::ProgramAndArguments(command) => command.get(1..).unwrap_or_default(),
        }
    }

    fn violation(&self, field: &'static str) -> Option<ConfigurationViolation> {
        let program = self.program();
        if program.is_empty() {
            return Some(ConfigurationViolation::CommandProgramEmpty { field });
        }
        if matches!(self, Self::Program(program) if program.chars().any(char::is_whitespace)) {
            return Some(ConfigurationViolation::CommandProgramWhitespace {
                field,
                program: program.to_owned(),
            });
        }
        if is_absolute_program(program) {
            return Some(ConfigurationViolation::CommandProgramAbsolute {
                field,
                program: program.to_owned(),
            });
        }
        if program.split('/').any(is_dot_path_segment) {
            return Some(ConfigurationViolation::CommandProgramDotSegment {
                field,
                program: program.to_owned(),
            });
        }
        if self.arguments().len() > COMMAND_ARGUMENTS_MAX {
            return out_of_range(
                field,
                self.arguments().len() as u64,
                0,
                COMMAND_ARGUMENTS_MAX as u64,
            );
        }
        self.arguments()
            .iter()
            .find(|argument| argument.len() > COMMAND_ARGUMENT_BYTES_MAX)
            .map(
                |argument| ConfigurationViolation::CommandArgumentOversized {
                    field,
                    bytes: argument.len() as u64,
                },
            )
    }
}

/// One `[[hooks]]` command the server runs inside the changed tree.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_hook_contract)]
pub struct CommandHook {
    /// Label for this hook's results, unique within the list.
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    /// What the hook is: a formatter, test suite, linter, build, or another command.
    pub kind: HookKind,
    /// Executable and literal arguments. Rift starts it directly without a shell.
    pub command: CommandInput,
    /// Whether the server appends changed project paths after the configured command, in byte order.
    #[serde(default)]
    pub changed_paths: ChangedPaths,
    /// Source files the server permits the hook to change.
    pub writes: HookWrites,
    /// Directory the process starts in, relative to the changed tree's root. Empty selects the
    /// root. Absolute paths and `.` or `..` segments are refused.
    #[serde(default)]
    pub working_directory: ProjectPath,
    /// Environment values added to the environment the server inherited.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Wall-clock bound before the server kills the process, 1ms to 1h.
    #[serde(default = "default_hook_timeout")]
    pub timeout: Duration,
    /// Bytes of each output stream the server keeps, 256b to 4kb. The full size is still reported.
    #[serde(default = "default_hook_output_limit")]
    pub output_limit: ByteSize,
    /// Severity the server reports when the hook does not pass.
    pub failure_severity: HookFailureSeverity,
    /// What a passing validation establishes. Transform hooks cannot declare guarantees.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub guarantees: Vec<HookGuarantee>,
    /// Whether an identical tree and environment are expected to reproduce the result.
    pub determinism: Determinism,
    /// Project-relative path patterns that select this hook. Empty selects every change.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub include: Vec<PathPattern>,
    /// Project-relative path patterns removed from hook selection.
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub exclude: Vec<PathPattern>,
}

fn default_hook_timeout() -> Duration {
    Duration::from_millis(120_000)
}

fn default_hook_output_limit() -> ByteSize {
    ByteSize::from_bytes(4_096)
}

/// What a hook is, as workspace configuration presents it.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// A source formatter.
    Format,
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
    Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChangedPaths {
    /// The command runs exactly as configured.
    #[default]
    None,
    /// The changed project paths follow the configured command, in byte
    /// order, for a tool that takes files.
    Append,
}

/// Source writes the server may retain from one hook.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HookWrites {
    /// A validation hook that may not change source files.
    None,
    /// A transform hook that may change only paths changed before hooks ran.
    ChangedPaths,
    /// A transform hook that may change any source file in the workspace.
    Workspace,
}

impl HookWrites {
    /// Returns whether hook changes source files.
    #[must_use]
    pub const fn is_transform(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Returns whether hook only validates source files.
    #[must_use]
    pub const fn is_validation(self) -> bool {
        matches!(self, Self::None)
    }
}

/// Severity the server reports when a hook does not pass.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureSeverity {
    /// The hook result reports a warning.
    Warning,
    /// The hook result reports an error.
    Error,
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

/// One external LSP process Rift may start.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_lsp_ranges)]
pub struct LspConfiguration {
    /// Executable and literal arguments. Rift starts it directly without a shell.
    pub command: CommandInput,
    /// Environment values added on top of the environment the server
    /// inherited.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Options handed to the process at initialize. Must be a JSON object
    /// when present; the LSP server defines its meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<serde_json::Value>,
    /// Wall-clock bound on the process's initialize handshake, 1s to 10m.
    #[serde(default = "default_lsp_startup_timeout")]
    pub startup_timeout: Duration,
    /// Wall-clock bound on each later LSP request, 1s to 10m.
    #[serde(default = "default_lsp_request_timeout")]
    pub request_timeout: Duration,
    /// Bytes of the process's standard error Rift keeps, 1kb to 8mb. The
    /// full size is still reported.
    #[serde(default = "default_lsp_output_limit")]
    pub output_limit: ByteSize,
    /// How often Rift sends this process the same request again while its
    /// answer stays unsettled - a refusal the process invites again, or an
    /// answer it gave while still analyzing - and how the waits between
    /// those attempts grow.
    #[serde(default)]
    pub retry: RetryPolicy,
    /// How often Rift replaces this process on its own, and over what
    /// window; the budget spent, the process's own failure surfaces.
    #[serde(default)]
    pub restart: RestartPolicy,
}

fn default_lsp_startup_timeout() -> Duration {
    Duration::from_millis(LSP_STARTUP_TIMEOUT_MS_DEFAULT)
}

fn default_lsp_request_timeout() -> Duration {
    Duration::from_millis(LSP_REQUEST_TIMEOUT_MS_DEFAULT)
}

fn default_lsp_output_limit() -> ByteSize {
    ByteSize::from_bytes(LSP_OUTPUT_BYTES_DEFAULT)
}

/// How one exact language selects an LSP process.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LanguageLspConfiguration {
    /// Name of one shared top-level `[lsp.<name>]` process.
    Named(String),
    /// Process used only by this exact language entry.
    Inline(LspConfiguration),
}

/// One exact `[languages.<identity>]` entry.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageConfiguration {
    /// Whether syntax, LSP service, and execution are enabled for matched paths.
    pub enabled: bool,
    /// Replacement path patterns. Absence keeps shipped patterns; an empty list matches none.
    #[schemars(length(max = 64))]
    pub include: Option<Vec<PathPattern>>,
    /// Path patterns removed from this language's effective matches.
    #[schemars(length(max = 64))]
    pub exclude: Vec<PathPattern>,
    /// Whether caller-provided code may execute under this exact language.
    pub execution: bool,
    /// Inline LSP process or name of one shared process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp: Option<LanguageLspConfiguration>,
}

impl Default for LanguageConfiguration {
    fn default() -> Self {
        Self {
            enabled: true,
            include: None,
            exclude: Vec::new(),
            execution: false,
            lsp: None,
        }
    }
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
    /// A language table key is not `name` or `name:dialect`.
    LanguageIdentityInvalid {
        /// The rejected key.
        language: String,
    },
    /// A language names an LSP process absent from the top-level table.
    LanguageLspUnknown {
        /// Language entry selecting the process.
        language: String,
        /// Missing LSP process name.
        lsp: String,
    },
    /// Two language entries carry the same exact include pattern.
    LanguageIncludeDuplicate {
        /// Repeated pattern.
        pattern: String,
        /// First language entry carrying it.
        first: String,
        /// Later language entry carrying it.
        second: String,
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
    /// Transform hook follows validation hook.
    HookTransformAfterValidation {
        /// Transform hook appearing too late.
        transform: String,
        /// Earlier validation hook.
        validation: String,
    },
    /// Transform hook declares guarantees reserved for validation hooks.
    HookTransformGuarantees {
        /// Transform hook declaring guarantees.
        id: String,
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
    /// A command carries no executable.
    CommandProgramEmpty {
        /// Configuration key carrying the command.
        field: &'static str,
    },
    /// A string command carries whitespace.
    CommandProgramWhitespace {
        /// Configuration key carrying the command.
        field: &'static str,
        /// Rejected executable.
        program: String,
    },
    /// A command names its executable by absolute path.
    CommandProgramAbsolute {
        /// Configuration key carrying the command.
        field: &'static str,
        /// Rejected executable.
        program: String,
    },
    /// A command program path carries a `.` or `..` segment.
    CommandProgramDotSegment {
        /// Configuration key carrying the command.
        field: &'static str,
        /// Rejected executable.
        program: String,
    },
    /// One command argument exceeds 4096 bytes.
    CommandArgumentOversized {
        /// Configuration key carrying the command.
        field: &'static str,
        /// Rejected argument size.
        bytes: u64,
    },
    /// A hook's `working_directory` is not a project-relative path: it is
    /// absolute, or carries a `.` or `..` segment.
    HookWorkingDirectoryInvalid {
        /// The hook declaring the directory.
        id: String,
        /// The rejected directory.
        working_directory: String,
    },
    /// A hook environment key is empty, or carries `=` or a NUL byte.
    HookEnvironmentKeyInvalid {
        /// The hook declaring the entry.
        id: String,
        /// The rejected key.
        key: String,
    },
    /// An `[lsp.<name>]` key is not a lowercase word.
    LspNameInvalid {
        /// The rejected LSP process name.
        name: String,
    },
    /// An LSP environment key is empty, or carries `=` or a NUL byte.
    LspEnvironmentKeyInvalid {
        /// The LSP process declaring the entry.
        lsp: String,
        /// The rejected key.
        key: String,
    },
    /// An LSP process's `initialization_options` value is not a JSON object.
    LspInitializationOptionsNotObject {
        /// The LSP process declaring the options.
        lsp: String,
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
    /// A `logs.capture` value is not a tracing filter directive.
    LogCaptureInvalid {
        /// The rejected filter.
        capture: String,
        /// The filter parser's account of the failure.
        detail: String,
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
            Self::LanguageIdentityInvalid { language } => {
                vec![("language", language.clone())]
            }
            Self::LanguageLspUnknown { language, lsp } => {
                vec![("language", language.clone()), ("lsp", lsp.clone())]
            }
            Self::LanguageIncludeDuplicate {
                pattern,
                first,
                second,
            } => vec![
                ("pattern", pattern.clone()),
                ("first", first.clone()),
                ("second", second.clone()),
            ],
            Self::SemanticModelInvalid { field, value } => {
                vec![("field", (*field).to_owned()), ("value", value.clone())]
            }
            Self::SearchWeightsInvalid { lexical, semantic } => vec![
                ("lexical_weight", lexical.to_string()),
                ("semantic_weight", semantic.to_string()),
            ],
            Self::HookTransformAfterValidation {
                transform,
                validation,
            } => vec![
                ("transform", transform.clone()),
                ("validation", validation.clone()),
            ],
            Self::HookTransformGuarantees { id } => vec![
                ("id", id.clone()),
                (
                    "rule",
                    "guarantees belong only to validation hooks".to_owned(),
                ),
            ],
            Self::HookIdDuplicate { id } | Self::HookIdInvalid { id } => {
                vec![("id", id.clone())]
            }
            Self::CommandProgramEmpty { field } => vec![("field", (*field).to_owned())],
            Self::CommandProgramWhitespace { field, program }
            | Self::CommandProgramAbsolute { field, program }
            | Self::CommandProgramDotSegment { field, program } => {
                vec![("field", (*field).to_owned()), ("program", program.clone())]
            }
            Self::CommandArgumentOversized { field, bytes } => vec![
                ("field", (*field).to_owned()),
                ("bytes", bytes.to_string()),
                ("bytes_max", COMMAND_ARGUMENT_BYTES_MAX.to_string()),
            ],
            Self::HookWorkingDirectoryInvalid {
                id,
                working_directory,
            } => vec![
                ("id", id.clone()),
                ("working_directory", working_directory.clone()),
            ],
            Self::HookEnvironmentKeyInvalid { id, key } => {
                vec![("id", id.clone()), ("key", key.clone())]
            }
            Self::LspNameInvalid { name } => vec![("name", name.clone())],
            Self::LspEnvironmentKeyInvalid { lsp, key } => {
                vec![("lsp", lsp.clone()), ("key", key.clone())]
            }
            Self::LspInitializationOptionsNotObject { lsp } => {
                vec![("lsp", lsp.clone())]
            }
            Self::PathPatternInvalid { field, pattern } => {
                vec![("field", (*field).to_owned()), ("pattern", pattern.clone())]
            }
            Self::LogCaptureInvalid { capture, detail } => vec![
                ("field", "logs.capture".to_owned()),
                ("capture", capture.clone()),
                ("detail", detail.clone()),
            ],
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

/// Returns first hook violation in list order.
fn hooks_violation(hooks: &[CommandHook]) -> Option<ConfigurationViolation> {
    if hooks.len() > HOOKS_MAX {
        return out_of_range("hooks", hooks.len() as u64, 0, HOOKS_MAX as u64);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut first_validation = None;
    for hook in hooks {
        if let Some(violation) = hook_violation(hook) {
            return Some(violation);
        }
        if !seen.insert(hook.id.as_str()) {
            return Some(ConfigurationViolation::HookIdDuplicate {
                id: hook.id.clone(),
            });
        }
        if hook.writes.is_validation() {
            first_validation.get_or_insert(hook.id.as_str());
        } else if let Some(validation) = first_validation {
            return Some(ConfigurationViolation::HookTransformAfterValidation {
                transform: hook.id.clone(),
                validation: validation.to_owned(),
            });
        }
    }
    None
}

/// Returns first violation one hook carries.
fn hook_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    identity_violation(hook)
        .or_else(|| command_violation(hook))
        .or_else(|| working_directory_violation(hook))
        .or_else(|| environment_violation(hook))
        .or_else(|| {
            (hook.writes.is_transform() && !hook.guarantees.is_empty()).then(|| {
                ConfigurationViolation::HookTransformGuarantees {
                    id: hook.id.clone(),
                }
            })
        })
        .or_else(|| guarantee_violation(hook))
        .or_else(|| hook_bounds_violation(hook))
}

/// The `id` rule: the label every result of this hook carries.
fn identity_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    (!is_hook_id(&hook.id)).then(|| ConfigurationViolation::HookIdInvalid {
        id: hook.id.clone(),
    })
}

/// The rules on what the hook runs: a present, non-absolute,
/// dot-segment-free program.
fn command_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    hook.command.violation("hooks.command")
}

/// The rule on `working_directory`: empty selects the workspace root;
/// otherwise it must be relative, with no `.` or `..` segment.
fn working_directory_violation(hook: &CommandHook) -> Option<ConfigurationViolation> {
    (!is_project_relative_path(&hook.working_directory.0)).then(|| {
        ConfigurationViolation::HookWorkingDirectoryInvalid {
            id: hook.id.clone(),
            working_directory: hook.working_directory.0.clone(),
        }
    })
}

/// Whether `value` is a valid project-relative path: empty names the
/// workspace root; a non-empty value must not be absolute (by
/// [`is_absolute_program`]'s rule, which also refuses any backslash) and
/// must carry no `.` or `..` segment.
fn is_project_relative_path(value: &str) -> bool {
    value.is_empty() || (!is_absolute_program(value) && !value.split('/').any(is_dot_path_segment))
}

/// Whether `segment` is `.` or `..`: a path segment that resolves outside
/// the location it names.
fn is_dot_path_segment(segment: &str) -> bool {
    matches!(segment, "." | "..")
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
        (
            "hooks.include",
            hook.include.len() as u64,
            0,
            CONFIGURATION_PATTERNS_MAX as u64,
        ),
        (
            "hooks.exclude",
            hook.exclude.len() as u64,
            0,
            CONFIGURATION_PATTERNS_MAX as u64,
        ),
    ];
    first_out_of_range(limits)
        .or_else(|| path_patterns_violation("hooks.include", &hook.include))
        .or_else(|| path_patterns_violation("hooks.exclude", &hook.exclude))
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

fn path_patterns_violation(
    field: &'static str,
    patterns: &[PathPattern],
) -> Option<ConfigurationViolation> {
    patterns.iter().find_map(|pattern| {
        path_pattern_violation(&pattern.0).map(|_| ConfigurationViolation::PathPatternInvalid {
            field,
            pattern: pattern.0.clone(),
        })
    })
}

fn languages_violation(
    languages: &BTreeMap<String, LanguageConfiguration>,
    lsp: &BTreeMap<String, LspConfiguration>,
) -> Option<ConfigurationViolation> {
    if languages.len() > LANGUAGES_MAX {
        return out_of_range("languages", languages.len() as u64, 0, LANGUAGES_MAX as u64);
    }
    let mut includes = BTreeMap::<&str, &str>::new();
    for (identity, language) in languages {
        if Language::from_identity_segment(identity).is_err() {
            return Some(ConfigurationViolation::LanguageIdentityInvalid {
                language: identity.clone(),
            });
        }
        if let Some(violation) = language_violation(identity, language, lsp) {
            return Some(violation);
        }
        if let Some(patterns) = &language.include {
            for pattern in patterns {
                if let Some(first) = includes.insert(&pattern.0, identity) {
                    return Some(ConfigurationViolation::LanguageIncludeDuplicate {
                        pattern: pattern.0.clone(),
                        first: first.to_owned(),
                        second: identity.clone(),
                    });
                }
            }
        }
    }
    None
}

fn language_violation(
    identity: &str,
    language: &LanguageConfiguration,
    lsp: &BTreeMap<String, LspConfiguration>,
) -> Option<ConfigurationViolation> {
    let include_len = language.include.as_ref().map_or(0, Vec::len);
    first_out_of_range([
        (
            "languages.include",
            include_len as u64,
            0,
            CONFIGURATION_PATTERNS_MAX as u64,
        ),
        (
            "languages.exclude",
            language.exclude.len() as u64,
            0,
            CONFIGURATION_PATTERNS_MAX as u64,
        ),
    ])
    .or_else(|| {
        language
            .include
            .as_deref()
            .and_then(|patterns| path_patterns_violation("languages.include", patterns))
    })
    .or_else(|| path_patterns_violation("languages.exclude", &language.exclude))
    .or_else(|| match &language.lsp {
        Some(LanguageLspConfiguration::Named(name)) if !lsp.contains_key(name) => {
            Some(ConfigurationViolation::LanguageLspUnknown {
                language: identity.to_owned(),
                lsp: name.clone(),
            })
        }
        Some(LanguageLspConfiguration::Inline(configuration)) => {
            lsp_violation("languages.lsp", identity, configuration)
        }
        _ => None,
    })
}

fn lsp_configurations_violation(
    configurations: &BTreeMap<String, LspConfiguration>,
) -> Option<ConfigurationViolation> {
    if configurations.len() > LSP_CONFIGURATIONS_MAX {
        return out_of_range(
            "lsp",
            configurations.len() as u64,
            0,
            LSP_CONFIGURATIONS_MAX as u64,
        );
    }
    configurations.iter().find_map(|(name, configuration)| {
        let parsed = Language::from_identity_segment(name);
        if parsed.as_ref().is_err() || parsed.is_ok_and(|language| language.dialect.is_some()) {
            return Some(ConfigurationViolation::LspNameInvalid { name: name.clone() });
        }
        lsp_violation("lsp", name, configuration)
    })
}

fn lsp_violation(
    field: &'static str,
    name: &str,
    lsp: &LspConfiguration,
) -> Option<ConfigurationViolation> {
    let command_field = if field == "languages.lsp" {
        "languages.lsp.command"
    } else {
        "lsp.command"
    };
    let environment_field = if field == "languages.lsp" {
        "languages.lsp.environment"
    } else {
        "lsp.environment"
    };
    lsp.command
        .violation(command_field)
        .or_else(|| {
            first_out_of_range([(
                environment_field,
                lsp.environment.len() as u64,
                0,
                LSP_ENVIRONMENT_ENTRIES_MAX as u64,
            )])
        })
        .or_else(|| {
            let key = lsp
                .environment
                .keys()
                .find(|key| !is_environment_key(key))?;
            Some(ConfigurationViolation::LspEnvironmentKeyInvalid {
                lsp: name.to_owned(),
                key: key.clone(),
            })
        })
        .or_else(|| {
            lsp.initialization_options
                .as_ref()
                .is_some_and(|options| !options.is_object())
                .then(
                    || ConfigurationViolation::LspInitializationOptionsNotObject {
                        lsp: name.to_owned(),
                    },
                )
        })
        .or_else(|| lsp_bounds_violation(lsp))
        .or_else(|| lsp_retry_violation(&lsp.retry))
        .or_else(|| lsp_restart_violation(&lsp.restart))
}

fn lsp_retry_violation(retry: &RetryPolicy) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "lsp.retry.attempts",
            retry.attempts,
            RETRY_ATTEMPTS_MIN,
            RETRY_ATTEMPTS_MAX,
        ),
        (
            "lsp.retry.delay",
            retry.delay.milliseconds(),
            RETRY_DELAY_MS_MIN,
            RETRY_DELAY_MS_MAX,
        ),
        (
            "lsp.retry.delay_limit",
            retry.delay_limit.milliseconds(),
            RETRY_DELAY_LIMIT_MS_MIN,
            RETRY_DELAY_LIMIT_MS_MAX,
        ),
    ])
}

fn lsp_restart_violation(restart: &RestartPolicy) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "lsp.restart.attempts",
            restart.attempts,
            RESTART_ATTEMPTS_MIN,
            RESTART_ATTEMPTS_MAX,
        ),
        (
            "lsp.restart.window",
            restart.window.milliseconds(),
            RESTART_WINDOW_MS_MIN,
            RESTART_WINDOW_MS_MAX,
        ),
    ])
}

fn lsp_bounds_violation(lsp: &LspConfiguration) -> Option<ConfigurationViolation> {
    first_out_of_range([
        (
            "lsp.startup_timeout",
            lsp.startup_timeout.milliseconds(),
            LSP_STARTUP_TIMEOUT_MS_MIN,
            LSP_STARTUP_TIMEOUT_MS_MAX,
        ),
        (
            "lsp.request_timeout",
            lsp.request_timeout.milliseconds(),
            LSP_REQUEST_TIMEOUT_MS_MIN,
            LSP_REQUEST_TIMEOUT_MS_MAX,
        ),
        (
            "lsp.output_limit",
            lsp.output_limit.bytes(),
            LSP_OUTPUT_BYTES_MIN,
            LSP_OUTPUT_BYTES_MAX,
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
            id: "tests".to_owned(),
            kind: HookKind::Test,
            command: CommandInput::ProgramAndArguments(vec!["cargo".to_owned(), "test".to_owned()]),
            changed_paths: ChangedPaths::None,
            writes: HookWrites::None,
            working_directory: ProjectPath(String::new()),
            environment: BTreeMap::new(),
            timeout: Duration::from_millis(120_000),
            output_limit: ByteSize::from_bytes(4_096),
            failure_severity: HookFailureSeverity::Error,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    fn lsp() -> LspConfiguration {
        LspConfiguration {
            command: CommandInput::ProgramAndArguments(vec![
                "uvx".to_owned(),
                "ty".to_owned(),
                "server".to_owned(),
            ]),
            environment: BTreeMap::new(),
            initialization_options: None,
            startup_timeout: Duration::from_millis(LSP_STARTUP_TIMEOUT_MS_DEFAULT),
            request_timeout: Duration::from_millis(LSP_REQUEST_TIMEOUT_MS_DEFAULT),
            output_limit: ByteSize::from_bytes(LSP_OUTPUT_BYTES_DEFAULT),
            retry: RetryPolicy::default(),
            restart: RestartPolicy::default(),
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
        assert_eq!(semantic.candidates_per_file, 3);
        assert_eq!(semantic.max_vectors, 200_000);
        assert_eq!(configuration.search.pool_slots, 4);
        assert_eq!(configuration.search.fusion_k, 60);
        assert_eq!(
            configuration.search.busy_timeout,
            Duration::from_millis(SEARCH_BUSY_TIMEOUT_MS_DEFAULT)
        );
        assert!(configuration.source.include.is_empty());
        assert!(configuration.source.exclude.is_empty());
        assert!(configuration.source.respect_gitignore);
        assert!(configuration.hooks.is_empty());
        assert!(configuration.languages.is_empty());
        assert!(configuration.lsp.is_empty());
        assert_eq!(
            configuration.search.text.include,
            vec![
                PathPattern("**/*.md".to_owned()),
                PathPattern("**/*.mdx".to_owned()),
                PathPattern("**/*.txt".to_owned()),
            ]
        );
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
            json!({ "lsp": { "ty": {
                "command": "ty", "unknown": 1,
            } } }),
            json!({ "engines": {} }),
            json!({ "execution": { "allow": ["python"] } }),
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
        for missing in [
            "id",
            "kind",
            "command",
            "writes",
            "failure_severity",
            "determinism",
        ] {
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
        assert_eq!(table.readiness_timeout, Duration::from_millis(30_000));
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
        configuration.server.idle_timeout = Duration::from_millis(SERVER_IDLE_TIMEOUT_MS_MAX);
        for timeout_ms in [
            SERVER_READINESS_TIMEOUT_MS_MIN - 1,
            SERVER_READINESS_TIMEOUT_MS_MAX + 1,
        ] {
            configuration.server.readiness_timeout = Duration::from_millis(timeout_ms);
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "server.readiness_timeout",
                        ..
                    })
                ),
                "readiness_timeout {timeout_ms}ms must be refused"
            );
        }
        for timeout_ms in [
            SERVER_READINESS_TIMEOUT_MS_MIN,
            SERVER_READINESS_TIMEOUT_MS_MAX,
        ] {
            configuration.server.readiness_timeout = Duration::from_millis(timeout_ms);
            assert_eq!(
                configuration.validate(),
                Ok(()),
                "readiness_timeout {timeout_ms}ms must be accepted"
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
        let cases: [SemanticBoundCase; 7] = [
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
            ("search.semantic.candidates_per_file", |semantic| {
                semantic.candidates_per_file = SEMANTIC_CANDIDATES_PER_FILE_MAX + 1;
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
    fn test_semantic_candidates_per_file_parses_and_keeps_its_default() {
        let configured: WorkspaceConfiguration = serde_json::from_value(json!({
            "search": { "semantic": { "candidates_per_file": 8 } }
        }))
        .expect("the key must parse");
        assert_eq!(configured.search.semantic.candidates_per_file, 8);
        assert_eq!(
            configured.search.semantic.candidates,
            SEMANTIC_CANDIDATES_DEFAULT
        );
        assert_eq!(configured.validate(), Ok(()));
        let omitted: WorkspaceConfiguration = serde_json::from_value(json!({
            "search": { "semantic": { "candidates": 100 } }
        }))
        .expect("an omitted key must keep its default");
        assert_eq!(
            omitted.search.semantic.candidates_per_file,
            SEMANTIC_CANDIDATES_PER_FILE_DEFAULT
        );
    }

    #[test]
    fn test_semantic_candidates_per_file_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration::default();
        for accepted in [
            SEMANTIC_CANDIDATES_PER_FILE_MIN,
            SEMANTIC_CANDIDATES_PER_FILE_MAX,
        ] {
            configuration.search.semantic.candidates_per_file = accepted;
            assert_eq!(configuration.validate(), Ok(()), "{accepted} is in range");
        }
        for refused in [
            SEMANTIC_CANDIDATES_PER_FILE_MIN - 1,
            SEMANTIC_CANDIDATES_PER_FILE_MAX + 1,
        ] {
            configuration.search.semantic.candidates_per_file = refused;
            assert!(
                matches!(
                    configuration.validate(),
                    Err(ConfigurationViolation::LimitOutOfRange {
                        field: "search.semantic.candidates_per_file",
                        ..
                    })
                ),
                "candidates_per_file {refused} must be refused"
            );
        }
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
    fn test_search_text_rejects_unknown_keys_and_schema_has_declared_keys() {
        let unknown = json!({ "search": { "text": { "extensions": ["md"] } } });
        assert!(
            serde_json::from_value::<WorkspaceConfiguration>(unknown).is_err(),
            "unknown search.text key must be refused"
        );

        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let properties = &schema["$defs"]["TextSearchConfiguration"]["properties"];
        assert_eq!(properties.as_object().expect("properties").len(), 2);
        assert!(properties.get("include").is_some());
        assert!(properties.get("max_chunk").is_some());
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
    fn test_search_table_parses_text_chunk_from_full_configuration() {
        let configuration: WorkspaceConfiguration = serde_json::from_value(json!({
            "search": {
                "lexical": { "weight": 0.8 },
                "semantic": {
                    "weight": 0.2,
                    "download_timeout": "5m",
                    "max_vectors": 50000
                },
                "text": { "max_chunk": "2mb" }
            }
        }))
        .expect("full search table must parse");
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
        assert_eq!(configuration.validate(), Ok(()));
    }

    #[test]
    fn test_hook_round_trips_through_json_with_exact_wire_names() {
        let value = serde_json::to_value(hook()).expect("serialize");
        assert_eq!(value["command"], json!(["cargo", "test"]));
        assert_eq!(value["changed_paths"], json!("none"));
        assert_eq!(value["writes"], json!("none"));
        assert_eq!(value["failure_severity"], json!("error"));
        assert_eq!(value["determinism"], json!("deterministic"));
        let round_tripped: CommandHook = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, hook());

        let format: HookKind = serde_json::from_value(json!("format")).expect("format kind");
        assert_eq!(format, HookKind::Format);
        let workspace: HookWrites =
            serde_json::from_value(json!("workspace")).expect("workspace writes");
        assert_eq!(workspace, HookWrites::Workspace);
        let warning: HookFailureSeverity =
            serde_json::from_value(json!("warning")).expect("warning severity");
        assert_eq!(warning, HookFailureSeverity::Warning);
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
        ];
        assert_schema_bounds(&cases);
    }

    #[test]
    fn test_semantic_schema_bounds_equal_the_enforced_constants() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let semantic = &schema["$defs"]["SemanticSearchConfiguration"]["properties"];
        let cases = [
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
                "candidates per file min",
                &semantic["candidates_per_file"]["minimum"],
                json!(SEMANTIC_CANDIDATES_PER_FILE_MIN),
            ),
            (
                "candidates per file max",
                &semantic["candidates_per_file"]["maximum"],
                json!(SEMANTIC_CANDIDATES_PER_FILE_MAX),
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
        let cases = [
            (
                "hooks max",
                &schema["properties"]["hooks"]["maxItems"],
                json!(HOOKS_MAX),
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
                json!(Duration::from_millis(SEARCH_BUSY_TIMEOUT_MS_DEFAULT)),
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
        let command = &definitions["CommandInput"]["oneOf"];
        let cases = [
            ("id min", &hook["id"]["minLength"], json!(1)),
            ("id max", &hook["id"]["maxLength"], json!(HOOK_ID_BYTES_MAX)),
            ("program min", &command[0]["minLength"], json!(1)),
            (
                "command max",
                &command[1]["maxItems"],
                json!(COMMAND_ARGUMENTS_MAX + 1),
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

    #[test]
    fn test_command_input_accepts_both_forms_and_enforces_bounds() {
        let program: CommandInput = serde_json::from_value(json!("cargo")).expect("program");
        assert_eq!(program.program(), "cargo");
        assert!(program.arguments().is_empty());

        let command: CommandInput =
            serde_json::from_value(json!(["cargo", "test"])).expect("command");
        assert_eq!(command.program(), "cargo");
        assert_eq!(command.arguments(), &["test"]);

        for refused in [
            CommandInput::Program(String::new()),
            CommandInput::Program("cargo test".to_owned()),
            CommandInput::ProgramAndArguments(Vec::new()),
        ] {
            assert!(refused.violation("command").is_some());
        }
        let too_many = CommandInput::ProgramAndArguments(
            std::iter::once("tool".to_owned())
                .chain((0..=COMMAND_ARGUMENTS_MAX).map(|_| "x".to_owned()))
                .collect(),
        );
        assert!(matches!(
            too_many.violation("command"),
            Some(ConfigurationViolation::LimitOutOfRange { .. })
        ));
        let oversized = CommandInput::ProgramAndArguments(vec![
            "tool".to_owned(),
            "x".repeat(COMMAND_ARGUMENT_BYTES_MAX + 1),
        ]);
        assert!(matches!(
            oversized.violation("command"),
            Some(ConfigurationViolation::CommandArgumentOversized { .. })
        ));
    }

    #[test]
    fn test_language_entries_validate_identity_lsp_and_exact_include_duplicates() {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.lsp.insert("ty".to_owned(), lsp());
        configuration.languages.insert(
            "python".to_owned(),
            LanguageConfiguration {
                lsp: Some(LanguageLspConfiguration::Named("ty".to_owned())),
                include: Some(vec![PathPattern("**/*.py".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        assert_eq!(configuration.validate(), Ok(()));
        let resolved = configuration
            .resolve_language_lsp("python")
            .expect("named LSP must resolve");
        assert_eq!(resolved.name, Some("ty"));

        configuration.languages.insert(
            "python:stub".to_owned(),
            LanguageConfiguration {
                include: Some(vec![PathPattern("**/*.py".to_owned())]),
                ..LanguageConfiguration::default()
            },
        );
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LanguageIncludeDuplicate { .. })
        ));

        configuration.languages.remove("python:stub");
        configuration
            .languages
            .get_mut("python")
            .expect("language")
            .lsp = Some(LanguageLspConfiguration::Named("missing".to_owned()));
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LanguageLspUnknown { .. })
        ));

        configuration.languages.clear();
        configuration
            .languages
            .insert("Python".to_owned(), LanguageConfiguration::default());
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LanguageIdentityInvalid { .. })
        ));
    }

    #[test]
    fn test_language_and_hook_defaults_and_legacy_keys() {
        let language: LanguageConfiguration = serde_json::from_value(json!({})).expect("language");
        assert!(language.enabled);
        assert!(language.include.is_none());
        assert!(language.exclude.is_empty());
        assert!(!language.execution);
        assert!(language.lsp.is_none());

        let value = json!({
            "id": "tests",
            "kind": "test",
            "command": ["cargo", "test"],
            "writes": "none",
            "failure_severity": "error",
            "determinism": "deterministic"
        });
        let hook: CommandHook = serde_json::from_value(value).expect("hook defaults");
        assert_eq!(hook.changed_paths, ChangedPaths::None);
        assert_eq!(hook.working_directory, ProjectPath::default());
        assert!(hook.environment.is_empty());
        assert_eq!(hook.timeout, Duration::from_millis(120_000));
        assert_eq!(hook.output_limit, ByteSize::from_bytes(4_096));
        assert!(hook.guarantees.is_empty());

        for legacy in [
            json!({"type": "command", "id": "x", "kind": "test", "command": "cargo", "writes": "none", "failure_severity": "error", "determinism": "deterministic"}),
            json!({"id": "x", "kind": "test", "program": "cargo", "arguments": [], "writes": "none", "failure_severity": "error", "determinism": "deterministic"}),
        ] {
            assert!(serde_json::from_value::<CommandHook>(legacy).is_err());
        }
    }

    #[test]
    fn test_lsp_defaults_validation_and_legacy_keys() {
        let parsed: LspConfiguration =
            serde_json::from_value(json!({"command": "rust-analyzer"})).expect("LSP defaults");
        assert_eq!(parsed.startup_timeout, Duration::from_millis(30_000));
        assert_eq!(parsed.request_timeout, Duration::from_millis(60_000));
        assert_eq!(parsed.output_limit, ByteSize::from_bytes(4_096));

        for legacy in [
            json!({"program": "rust-analyzer"}),
            json!({"command": "rust-analyzer", "arguments": []}),
            json!({"command": "rust-analyzer", "languages": ["rust"]}),
        ] {
            assert!(serde_json::from_value::<LspConfiguration>(legacy).is_err());
        }

        let mut configuration = WorkspaceConfiguration::default();
        configuration.lsp.insert("Rust".to_owned(), lsp());
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LspNameInvalid { .. })
        ));
        configuration.lsp.clear();
        let mut invalid = lsp();
        invalid
            .environment
            .insert("BAD=KEY".to_owned(), "x".to_owned());
        configuration.lsp.insert("rust".to_owned(), invalid);
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LspEnvironmentKeyInvalid { .. })
        ));
        configuration
            .lsp
            .get_mut("rust")
            .expect("LSP")
            .environment
            .clear();
        configuration
            .lsp
            .get_mut("rust")
            .expect("LSP")
            .initialization_options = Some(json!([]));
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LspInitializationOptionsNotObject { .. })
        ));
    }

    #[test]
    fn test_lsp_and_pattern_collection_bounds_are_enforced() {
        let mut configuration = WorkspaceConfiguration {
            lsp: (0..=LSP_CONFIGURATIONS_MAX)
                .map(|index| (format!("lsp{index}"), lsp()))
                .collect(),
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange { field: "lsp", .. })
        ));

        configuration = WorkspaceConfiguration::default();
        configuration.languages = (0..=LANGUAGES_MAX)
            .map(|index| (format!("language{index}"), LanguageConfiguration::default()))
            .collect();
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "languages",
                ..
            })
        ));

        configuration = WorkspaceConfiguration::default();
        configuration.search.text.include = vec![PathPattern("../outside".to_owned())];
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::PathPatternInvalid {
                field: "search.text.include",
                ..
            })
        ));

        let mut hook = hook();
        hook.include = vec![PathPattern("../outside".to_owned())];
        configuration = WorkspaceConfiguration {
            hooks: vec![hook],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::PathPatternInvalid {
                field: "hooks.include",
                ..
            })
        ));
    }

    #[test]
    fn test_lsp_numeric_bounds_are_enforced() {
        type BreakBound = fn(&mut LspConfiguration);

        let cases: [(BreakBound, &'static str); 5] = [
            (
                |lsp| lsp.startup_timeout = Duration::from_millis(LSP_STARTUP_TIMEOUT_MS_MIN - 1),
                "lsp.startup_timeout",
            ),
            (
                |lsp| lsp.request_timeout = Duration::from_millis(LSP_REQUEST_TIMEOUT_MS_MAX + 1),
                "lsp.request_timeout",
            ),
            (
                |lsp| lsp.output_limit = ByteSize::from_bytes(LSP_OUTPUT_BYTES_MIN - 1),
                "lsp.output_limit",
            ),
            (
                |lsp| lsp.retry.attempts = RETRY_ATTEMPTS_MAX + 1,
                "lsp.retry.attempts",
            ),
            (
                |lsp| lsp.restart.attempts = RESTART_ATTEMPTS_MAX + 1,
                "lsp.restart.attempts",
            ),
        ];
        for (break_bound, field) in cases {
            let mut invalid = lsp();
            break_bound(&mut invalid);
            let configuration = WorkspaceConfiguration {
                lsp: BTreeMap::from([("server".to_owned(), invalid)]),
                ..WorkspaceConfiguration::default()
            };
            assert!(matches!(
                configuration.validate(),
                Err(ConfigurationViolation::LimitOutOfRange { field: actual, .. }) if actual == field
            ));
        }
    }

    #[test]
    fn test_lsp_and_command_schemas_state_collection_bounds() {
        let schema =
            serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
        let command = &schema["$defs"]["CommandInput"]["oneOf"];
        assert_eq!(command[1]["minItems"], json!(1));
        assert_eq!(command[1]["maxItems"], json!(COMMAND_ARGUMENTS_MAX + 1));
        assert_eq!(
            command[1]["items"]["maxLength"],
            json!(COMMAND_ARGUMENT_BYTES_MAX)
        );
        assert_eq!(schema["properties"]["hooks"]["maxItems"], json!(HOOKS_MAX));
        assert_eq!(
            schema["properties"]["languages"]["maxProperties"],
            json!(LANGUAGES_MAX)
        );
        assert_eq!(
            schema["properties"]["lsp"]["maxProperties"],
            json!(LSP_CONFIGURATIONS_MAX)
        );
        assert_eq!(
            schema["$defs"]["CommandHook"]["properties"]["environment"]["maxProperties"],
            json!(HOOK_ENVIRONMENT_ENTRIES_MAX)
        );
        assert_eq!(
            schema["$defs"]["LspConfiguration"]["properties"]["environment"]["maxProperties"],
            json!(LSP_ENVIRONMENT_ENTRIES_MAX)
        );
        assert_eq!(
            schema["$defs"]["LanguageConfiguration"]["properties"]["exclude"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
        assert_eq!(
            schema["$defs"]["LanguageConfiguration"]["properties"]["include"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
        assert_eq!(
            schema["$defs"]["CommandHook"]["properties"]["include"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
        assert_eq!(
            schema["$defs"]["TextSearchConfiguration"]["properties"]["include"]["maxItems"],
            json!(CONFIGURATION_PATTERNS_MAX)
        );
    }

    #[test]
    fn test_hook_contract_validation_covers_command_order_and_bounds() {
        let mut invalid = hook();
        invalid.command = CommandInput::Program(String::new());
        let configuration = WorkspaceConfiguration {
            hooks: vec![invalid],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::CommandProgramEmpty { .. })
        ));

        let mut invalid = hook();
        invalid.working_directory = ProjectPath("../outside".to_owned());
        let configuration = WorkspaceConfiguration {
            hooks: vec![invalid],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::HookWorkingDirectoryInvalid { .. })
        ));

        let mut transform = hook();
        transform.id = "format".to_owned();
        transform.writes = HookWrites::ChangedPaths;
        let validation = hook();
        let accepted = WorkspaceConfiguration {
            hooks: vec![transform.clone(), validation.clone()],
            ..WorkspaceConfiguration::default()
        };
        assert_eq!(accepted.validate(), Ok(()));
        let refused = WorkspaceConfiguration {
            hooks: vec![validation, transform],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            refused.validate(),
            Err(ConfigurationViolation::HookTransformAfterValidation { .. })
        ));

        let mut invalid = hook();
        invalid.output_limit = ByteSize::from_bytes(HOOK_OUTPUT_BYTES_MIN - 1);
        let configuration = WorkspaceConfiguration {
            hooks: vec![invalid],
            ..WorkspaceConfiguration::default()
        };
        assert!(matches!(
            configuration.validate(),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "hooks.output_limit",
                ..
            })
        ));
    }

    #[test]
    fn test_every_configuration_violation_carries_evidence() {
        let text = || "x".to_owned();
        let violations = [
            ConfigurationViolation::LimitOutOfRange {
                field: "x",
                value: 2,
                min: 0,
                max: 1,
            },
            ConfigurationViolation::LanguageIdentityInvalid { language: text() },
            ConfigurationViolation::LanguageLspUnknown {
                language: text(),
                lsp: text(),
            },
            ConfigurationViolation::LanguageIncludeDuplicate {
                pattern: text(),
                first: text(),
                second: text(),
            },
            ConfigurationViolation::SemanticModelInvalid {
                field: "x",
                value: text(),
            },
            ConfigurationViolation::SearchWeightsInvalid {
                lexical: 0.2,
                semantic: 0.2,
            },
            ConfigurationViolation::HookTransformAfterValidation {
                transform: text(),
                validation: text(),
            },
            ConfigurationViolation::HookTransformGuarantees { id: text() },
            ConfigurationViolation::HookIdDuplicate { id: text() },
            ConfigurationViolation::HookIdInvalid { id: text() },
            ConfigurationViolation::CommandProgramEmpty { field: "x" },
            ConfigurationViolation::CommandProgramWhitespace {
                field: "x",
                program: text(),
            },
            ConfigurationViolation::CommandProgramAbsolute {
                field: "x",
                program: text(),
            },
            ConfigurationViolation::CommandProgramDotSegment {
                field: "x",
                program: text(),
            },
            ConfigurationViolation::CommandArgumentOversized {
                field: "x",
                bytes: 4_097,
            },
            ConfigurationViolation::HookWorkingDirectoryInvalid {
                id: text(),
                working_directory: text(),
            },
            ConfigurationViolation::HookEnvironmentKeyInvalid {
                id: text(),
                key: text(),
            },
            ConfigurationViolation::LspNameInvalid { name: text() },
            ConfigurationViolation::LspEnvironmentKeyInvalid {
                lsp: text(),
                key: text(),
            },
            ConfigurationViolation::LspInitializationOptionsNotObject { lsp: text() },
            ConfigurationViolation::PathPatternInvalid {
                field: "x",
                pattern: text(),
            },
            ConfigurationViolation::LogCaptureInvalid {
                capture: text(),
                detail: text(),
            },
            ConfigurationViolation::PortSelectionConflict,
            ConfigurationViolation::PortRangeInverted { min: 2, max: 1 },
        ];
        for violation in violations {
            assert!(!violation.evidence().is_empty(), "{violation:?}");
        }
    }
}
