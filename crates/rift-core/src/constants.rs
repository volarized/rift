//! Cross-crate domain limits and canonical vocabulary.

/// Maximum ASCII bytes in one source-resolver identity.
pub const SOURCE_RESOLVER_ID_BYTES_MAX: usize = 128;
/// Maximum bytes in one canonical source-unit URI.
pub const SOURCE_UNIT_ID_BYTES_MAX: usize = 8_192;
/// Canonical prefix for resolver-owned source-unit identities.
pub const SOURCE_UNIT_URI_PREFIX: &str = "rift://source/";
/// Canonical source-unit address separator.
pub const SOURCE_UNIT_SEPARATOR: char = '/';
/// Encoded width of one source-unit address separator.
pub const SOURCE_UNIT_SEPARATOR_BYTES: usize = 1;
/// Punctuation accepted after first source-resolver byte.
pub const SOURCE_RESOLVER_PUNCTUATION: &[u8] = b"_.-";
/// Marker introducing one percent-encoded byte.
pub const PERCENT_ESCAPE_MARKER: u8 = b'%';
/// Encoded width of one percent-encoded byte.
pub const PERCENT_ESCAPE_BYTES: usize = 3;
/// Offset of high hexadecimal digit in one percent escape.
pub const PERCENT_ESCAPE_HIGH_OFFSET: usize = 1;
/// Offset of low hexadecimal digit in one percent escape.
pub const PERCENT_ESCAPE_LOW_OFFSET: usize = 2;
/// Bits represented by one hexadecimal digit.
pub const HEX_NIBBLE_BITS: u32 = 4;
/// Numeric value of first hexadecimal letter.
pub const HEX_LETTER_VALUE_OFFSET: u8 = 10;
/// URI punctuation safe without percent encoding in source-unit keys.
pub const SOURCE_UNIT_SAFE_PUNCTUATION: &[u8] = b"/!$&'()*+,;=:@-._~";
/// Maximum UTF-8 bytes in one project path.
pub const PROJECT_PATH_BYTES_MAX: usize = 1_000;
/// Maximum UTF-8 bytes in one source-catalog path.
pub const SOURCE_PATH_BYTES_MAX: usize = 4_096;
/// Rift-owned workspace state directory.
pub const RIFT_STATE_DIRECTORY: &str = ".rift";
/// Prefix of every path below Rift-owned workspace state.
pub const RIFT_STATE_DIRECTORY_PREFIX: &str = ".rift/";
