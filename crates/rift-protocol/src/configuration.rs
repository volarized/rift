//! Model of the workspace configuration file `rift.toml`.
//!
//! Every type here is a contract: serde attributes define exactly what the
//! file may say, and the exported `rift.schema.json` derives from these
//! definitions.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// The spelling a [`ByteSize`] value must match.
const BYTE_SIZE_PATTERN: &str = "^(?:0|[1-9][0-9]*)(?:B|KiB|MiB|GiB|TiB)$";
/// The spelling a [`Duration`] value must match.
const DURATION_PATTERN: &str = "^(?:0|[1-9][0-9]*)(?:ms|s|m|h|d)$";

/// A byte size: an integer magnitude with a required binary-unit suffix
/// `B`, `KiB`, `MiB`, `GiB`, or `TiB`.
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

    /// Parses the file spelling: digits, then one of `B`, `KiB`, `MiB`,
    /// `GiB`, or `TiB`.
    ///
    /// # Errors
    ///
    /// Returns [`UnitParseError`] when the magnitude or suffix breaks the
    /// documented form, or the product overflows.
    pub fn parse(text: &str) -> Result<Self, UnitParseError> {
        let (magnitude, unit) = split_magnitude(text, ByteSize::EXPECTED)?;
        let scale: u64 = match unit {
            "B" => 1,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            "TiB" => 1 << 40,
            _ => return Err(UnitParseError::new(text, ByteSize::EXPECTED)),
        };
        let bytes = magnitude
            .checked_mul(scale)
            .ok_or_else(|| UnitParseError::new(text, ByteSize::EXPECTED))?;
        Ok(Self(bytes))
    }

    /// The documented form, named in every parse failure.
    const EXPECTED: &'static str =
        "an integer magnitude followed by B, KiB, MiB, GiB, or TiB, such as `16KiB`";
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
            (1_u64 << 40, "TiB"),
            (1 << 30, "GiB"),
            (1 << 20, "MiB"),
            (1 << 10, "KiB"),
        ];
        for (scale, suffix) in units {
            if size.0 != 0 && size.0.is_multiple_of(scale) {
                return format!("{}{suffix}", size.0 / scale);
            }
        }
        format!("{}B", size.0)
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
            "description": "A byte size: an integer magnitude with a required \
                            binary-unit suffix `B`, `KiB`, `MiB`, `GiB`, or `TiB`."
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_byte_size_parses_every_documented_unit() {
        let cases = [
            ("0B", 0),
            ("1B", 1),
            ("16KiB", 16 << 10),
            ("3MiB", 3 << 20),
            ("2GiB", 2 << 30),
            ("1TiB", 1 << 40),
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
            "", "16", "KiB", "016KiB", "16kib", "16 KiB", "-1B", "1.5KiB",
        ] {
            assert!(ByteSize::parse(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn test_byte_size_refuses_overflow() {
        assert!(ByteSize::parse("99999999999TiB").is_err());
        assert!(ByteSize::parse("999999999999999999999B").is_err());
    }

    #[test]
    fn test_byte_size_renders_largest_exact_unit() {
        let cases = [
            (0, "0B"),
            (1, "1B"),
            (16 << 10, "16KiB"),
            ((16 << 10) + 1, "16385B"),
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
        let error = ByteSize::parse("16kb").expect_err("lowercase unit must be refused");
        let message = error.to_string();
        assert!(
            message.contains("16kb") && message.contains("16KiB"),
            "the failure must show the value and the documented form: {message}"
        );
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
}
