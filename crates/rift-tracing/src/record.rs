//! One log record, the bounds every record keeps, and the query one read carries.

use std::time::Duration;

/// Most records one append may carry. A drain task holding more splits.
pub const LOG_BATCH_RECORDS_MAX: usize = 4_096;
/// Maximum UTF-8 bytes kept for one record's message. A longer message is
/// truncated at a character boundary rather than refused: a log record exists
/// to be read back, and half of it read back beats none.
pub const LOG_MESSAGE_BYTES_MAX: usize = 8_192;
/// Maximum UTF-8 bytes kept for one record's rendered fields. The capture writes them
/// through `serde_json` within this bound: a member is kept whole or left out, and the
/// count of members left out follows as `fields_left_out`.
pub const LOG_FIELDS_BYTES_MAX: usize = 8_192;
/// Maximum UTF-8 bytes kept for one record's level, target, component, or
/// operation. These are short by construction; the bound stops a caller's
/// runaway value from filling the file.
pub const LOG_LABEL_BYTES_MAX: usize = 256;
/// Most records one read may return, whatever page size a caller asks for.
pub const LOG_PAGE_RECORDS_MAX: usize = 5_000;
/// The levels a read accepts, in the spelling the store holds.
pub const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// The spelling every stored row's `kind` column holds: the store writes log records
/// alone, and a read returns rows of this kind alone.
pub(crate) const LOG_KIND: &str = "log";

/// One record on its way into the store: when it happened, how severe it was,
/// where it came from, and what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    pub(crate) recorded_at_ms: i64,
    pub(crate) level: String,
    pub(crate) target: String,
    pub(crate) component: String,
    pub(crate) operation: String,
    pub(crate) message: String,
    pub(crate) fields: String,
}

impl LogRecord {
    /// Builds one record, bounding every field it carries.
    ///
    /// Every string is truncated to its maximum at a character boundary, so a
    /// record built from an unbounded message still costs a bounded row.
    #[must_use]
    pub fn new(
        recorded_at_ms: i64,
        level: &str,
        target: &str,
        component: &str,
        operation: &str,
        message: &str,
        fields: &str,
    ) -> Self {
        Self {
            recorded_at_ms,
            level: bounded(&level.to_lowercase(), LOG_LABEL_BYTES_MAX),
            target: bounded(target, LOG_LABEL_BYTES_MAX),
            component: bounded(component, LOG_LABEL_BYTES_MAX),
            operation: bounded(operation, LOG_LABEL_BYTES_MAX),
            message: bounded(message, LOG_MESSAGE_BYTES_MAX),
            fields: bounded(fields, LOG_FIELDS_BYTES_MAX),
        }
    }

    /// Milliseconds since the Unix epoch at which the record was emitted.
    #[must_use]
    pub const fn recorded_at_ms(&self) -> i64 {
        self.recorded_at_ms
    }

    /// The record's severity in lower case, whatever case the caller spelled
    /// it: a level read and a stored row have to match on one spelling.
    #[must_use]
    pub fn level(&self) -> &str {
        &self.level
    }

    /// The emitting module path.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The `component` field the emitting span or event carried, empty when it
    /// carried none.
    #[must_use]
    pub fn component(&self) -> &str {
        &self.component
    }

    /// The `operation` field the emitting span or event carried, empty when it
    /// carried none.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// The record's message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The record's remaining fields, rendered as a JSON object.
    ///
    /// An event emitted inside a span carries, after its own fields, the member `root_span`
    /// for the outermost span around it and `nearest_span` for the span it was emitted in,
    /// when that span is not the outermost. Each is an object holding the span's `name` and
    /// its `fields`: `component`, `operation`, then the span's other fields as it recorded
    /// them, cut at 1 KiB with the count of the members left out as `fields_left_out`. A
    /// span close record carries the span's own fields, `span`, and `elapsed_ms`, then
    /// `root_span` when the span closed inside another. Members keep the order they were
    /// recorded in. An own member that does not fit [`LOG_FIELDS_BYTES_MAX`] whole beside
    /// the span members is left out, and the count follows as `fields_left_out`.
    #[must_use]
    pub fn fields(&self) -> &str {
        &self.fields
    }
}

/// One record read back, carrying the identity the store filed it under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLogRecord {
    pub(crate) identity: i64,
    pub(crate) record: LogRecord,
}

impl StoredLogRecord {
    /// The store's own ascending identity for this record. Two records from one
    /// run compare by it, whatever clock wrote their timestamps.
    #[must_use]
    pub const fn identity(&self) -> i64 {
        self.identity
    }

    /// The record itself.
    #[must_use]
    pub const fn record(&self) -> &LogRecord {
        &self.record
    }
}

/// Which records one read returns, and how many.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogQuery {
    pub(crate) level: Option<String>,
    pub(crate) component: Option<String>,
    pub(crate) after: Option<i64>,
    pub(crate) since_ms: Option<i64>,
    pub(crate) until_ms: Option<i64>,
    pub(crate) limit: usize,
    /// The tracing clock's reading, in milliseconds since the Unix epoch, that every age
    /// bound of this query counts back from: taken by the first age bound, so a second one
    /// and every page of a follow read share it.
    pub(crate) clock_ms: Option<i64>,
}

impl LogQuery {
    /// A read of the newest `limit` log records, bounded by [`LOG_PAGE_RECORDS_MAX`].
    #[must_use]
    pub fn newest(limit: usize) -> Self {
        Self {
            level: None,
            component: None,
            after: None,
            since_ms: None,
            until_ms: None,
            limit: limit.min(LOG_PAGE_RECORDS_MAX),
            clock_ms: None,
        }
    }

    /// Restricts the read to one severity, matched in lower case.
    #[must_use]
    pub fn at_level(mut self, level: &str) -> Self {
        self.level = Some(bounded(&level.to_lowercase(), LOG_LABEL_BYTES_MAX));
        self
    }

    /// Restricts the read to one component.
    #[must_use]
    pub fn for_component(mut self, component: &str) -> Self {
        self.component = Some(bounded(component, LOG_LABEL_BYTES_MAX));
        self
    }

    /// Restricts the read to records the store filed after `identity`.
    #[must_use]
    pub fn after(mut self, identity: i64) -> Self {
        self.after = Some(identity);
        self
    }

    /// Restricts the read to records recorded at or after `recorded_at_ms`.
    #[must_use]
    pub fn since_ms(mut self, recorded_at_ms: i64) -> Self {
        self.since_ms = Some(recorded_at_ms);
        self
    }

    /// Restricts the read to records recorded before `recorded_at_ms`.
    ///
    /// With [`Self::since_ms`] it selects one window: `recorded_at >= since` and
    /// `recorded_at < until`, in identity order.
    #[must_use]
    pub const fn until_ms(mut self, recorded_at_ms: i64) -> Self {
        self.until_ms = Some(recorded_at_ms);
        self
    }

    /// Restricts the read to records recorded at or after the cutoff `age` before the
    /// tracing clock's reading.
    ///
    /// The cutoff is resolved here, once, so every page a follow read takes with
    /// [`Self::after`] selects the same records. An age past the epoch cuts off nothing.
    #[must_use]
    pub fn since_age(mut self, age: Duration) -> Self {
        self.since_ms = Some(self.cutoff_ms(age));
        self
    }

    /// Restricts the read to records recorded before the cutoff `age` before the tracing
    /// clock's reading; with [`Self::since_age`], both cutoffs count back from one reading.
    #[must_use]
    pub fn until_age(mut self, age: Duration) -> Self {
        self.until_ms = Some(self.cutoff_ms(age));
        self
    }

    /// The instant `age` before the tracing clock's reading, in milliseconds since the Unix
    /// epoch; the clock is read on the query's first cutoff and kept for the rest.
    fn cutoff_ms(&mut self, age: Duration) -> i64 {
        let clock_ms = *self.clock_ms.get_or_insert_with(crate::capture::now_ms);
        clock_ms.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX))
    }

    /// The severity this read is restricted to, when it is.
    #[must_use]
    pub fn level(&self) -> Option<&str> {
        self.level.as_deref()
    }

    /// The component this read is restricted to, when it is.
    #[must_use]
    pub fn component(&self) -> Option<&str> {
        self.component.as_deref()
    }

    /// The record count this read returns at most.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// `value` truncated to at most `maximum` UTF-8 bytes, cut at a character
/// boundary.
pub(crate) fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_LABEL_BYTES_MAX, LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord,
        bounded,
    };

    #[test]
    fn a_long_message_is_bounded_at_a_character_boundary() {
        let long = "é".repeat(LOG_MESSAGE_BYTES_MAX);

        let record = LogRecord::new(1, "info", "rift", "index", "index.reconcile", &long, "{}");

        assert!(record.message().len() <= LOG_MESSAGE_BYTES_MAX);
        assert!(record.message().chars().all(|character| character == 'é'));
    }

    #[test]
    fn a_long_label_is_bounded() {
        let long = "c".repeat(LOG_LABEL_BYTES_MAX * 2);

        let record = LogRecord::new(1, "info", &long, &long, &long, "message", "{}");

        assert_eq!(record.target().len(), LOG_LABEL_BYTES_MAX);
        assert_eq!(record.component().len(), LOG_LABEL_BYTES_MAX);
    }

    #[test]
    fn a_page_larger_than_the_maximum_is_bounded() {
        assert_eq!(
            LogQuery::newest(LOG_PAGE_RECORDS_MAX * 2).limit(),
            LOG_PAGE_RECORDS_MAX
        );
    }

    #[test]
    fn a_value_within_the_bound_is_unchanged() {
        assert_eq!(bounded("short", 64), "short");
    }

    #[test]
    fn a_bound_inside_a_multibyte_character_moves_to_its_start() {
        assert_eq!(bounded("éé", 3), "é");
    }
}
