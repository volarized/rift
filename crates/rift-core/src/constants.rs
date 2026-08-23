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
/// Workspace configuration file, read from the workspace root.
pub const WORKSPACE_CONFIGURATION_FILE: &str = "rift.toml";
/// Workspace database file, below [`RIFT_STATE_DIRECTORY`]: one `SQLite`
/// database at `.rift/db` holds every store Rift persists for the
/// workspace.
pub const WORKSPACE_DATABASE_FILE_NAME: &str = "db";
/// Maximum ASCII bytes in one provider-composition stage name.
pub const STAGE_NAME_BYTES_MAX: usize = 64;
/// Punctuation accepted in provider-composition stage names.
pub const STAGE_NAME_PUNCTUATION: &[u8] = b"-";
/// Default maximum Rust files indexed from one workspace.
pub const WORKSPACE_FILES_MAX_DEFAULT: usize = 20_000;
/// Shared default maximum bytes accepted from one Rust source.
pub const RUST_SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;
/// Default maximum aggregate Rust source bytes indexed from one workspace.
pub const WORKSPACE_BYTES_MAX_DEFAULT: usize = 128 * 1_024 * 1_024;
/// Default maximum directory depth scanned from one workspace.
pub const WORKSPACE_DIRECTORY_DEPTH_MAX_DEFAULT: usize = 64;
/// Default maximum results returned by one read query.
pub const READ_RESULTS_MAX_DEFAULT: usize = 1_000;
/// Directories excluded from current-workspace indexing.
pub const WORKSPACE_IGNORED_DIRECTORIES: &[&str] = &[".git", ".rift", "target"];
/// Default maximum results returned by one search call.
pub const SEARCH_RESULTS_DEFAULT: usize = 20;
/// Maximum files one search's `paths.force_include` may pull into the index on demand.
pub const FORCE_INCLUDE_FILES_MAX: usize = 256;
/// Canonical identity of the built-in Rust read provider.
pub const RUST_READ_PROVIDER_ID: &str = "rift.rust.read";
/// Hexadecimal characters in one SHA-256 digest.
pub const SHA256_HEX_LENGTH: usize = 64;
/// Hexadecimal characters kept from a SHA-256 digest on the wire. Full-length
/// hashing stays internal to identity computation; a wire `Digest` truncates
/// to this width at the boundary.
pub const DIGEST_WIRE_CHARS: usize = 8;
/// Base32 characters kept from a SHA-256 digest minting an opaque `chg_`-style
/// identity.
pub const OPAQUE_ID_DIGEST_CHARS: usize = 26;
/// GitHub API endpoint naming the latest official Rift release.
pub const RELEASE_API_URL: &str = "https://api.github.com/repos/volarized/rift/releases/latest";
/// Base URL below which official Rift release assets are downloaded.
pub const RELEASE_DOWNLOAD_BASE_URL: &str = "https://github.com/volarized/rift/releases/download";
/// Maximum bytes in one release-metadata response.
pub const RELEASE_METADATA_BYTES_MAX: u64 = 1_024 * 1_024;
/// Maximum bytes in one release checksum manifest.
pub const CHECKSUM_MANIFEST_BYTES_MAX: u64 = 16 * 1_024;
/// Maximum bytes in one release archive download.
pub const RELEASE_ARCHIVE_BYTES_MAX: u64 = 128 * 1_024 * 1_024;
/// Maximum bytes in one released Rift binary.
pub const RELEASE_BINARY_BYTES_MAX: u64 = 128 * 1_024 * 1_024;
/// Maximum bytes in one release document member.
pub const RELEASE_DOCUMENT_BYTES_MAX: u64 = 4 * 1_024 * 1_024;
/// Exact file count inside one official release archive.
pub const RELEASE_ARCHIVE_MEMBER_COUNT: usize = 3;
/// Maximum entries in one release checksum manifest.
pub const CHECKSUM_ENTRY_COUNT_MAX: usize = 16;
/// Wall-clock limit for one release download request.
pub const RELEASE_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);
/// Maximum redirect hops followed during one release download.
pub const REDIRECT_HOPS_MAX: usize = 5;
/// Sole URL scheme accepted for release downloads and redirects.
pub const HTTPS_SCHEME: &str = "https";
