//! The page counts a `SQLite` database file's header holds, read for `sqlite.page.count`.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

/// Bytes of the header at the start of every `SQLite` database file.
const FILE_HEADER_BYTES: usize = 100;
/// Header offset of the file change counter, 4 bytes.
const HEADER_CHANGE_COUNTER: usize = 24;
/// Header offset of the database size in pages, 4 bytes.
const HEADER_PAGE_COUNT: usize = 28;
/// Header offset of the number of freelist pages, 4 bytes.
const HEADER_FREELIST_COUNT: usize = 36;
/// Header offset of the version-valid-for number, 4 bytes.
const HEADER_VALID_FOR: usize = 92;

/// The `sqlite.page.state` of the pages in use.
pub const PAGE_STATE_USED: &str = "used";
/// The `sqlite.page.state` of the pages on the freelist.
pub const PAGE_STATE_FREE: &str = "free";

/// The page counts a database file's header holds.
///
/// The `SQLite` file format places "The size of the database in pages" at offset 28 and the
/// "Number of freelist pages in the file" at offset 36, each a 4-byte big-endian integer
/// (`btree.c` file header comment, bundled `sqlite3.c`). `SQLite` itself trusts the size
/// only when it is nonzero and the change counter at offset 24 equals the
/// version-valid-for number at offset 92 (`lockBtree`), and so does this read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCounts {
    /// Every page of the file.
    pages: u32,
    /// The pages on the freelist.
    free: u32,
}

impl PageCounts {
    /// The counts the header of the database file `path` holds: one open and one 100-byte
    /// read. In WAL mode the header changes when a checkpoint writes page 1 into the file,
    /// so the counts are those of the last checkpoint; pages a commit left in the
    /// write-ahead log are not in them. A file that cannot be read, is shorter than its
    /// header, or whose header `SQLite` would not trust answers `None`.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let mut header = [0_u8; FILE_HEADER_BYTES];
        File::open(path)
            .and_then(|mut file| file.read_exact(&mut header))
            .ok()?;
        Self::from_header(&header)
    }

    /// The counts `header` holds, or `None` where `SQLite` would not trust its size.
    fn from_header(header: &[u8; FILE_HEADER_BYTES]) -> Option<Self> {
        let pages = header_integer(header, HEADER_PAGE_COUNT);
        let validated = header_integer(header, HEADER_CHANGE_COUNTER)
            == header_integer(header, HEADER_VALID_FOR);
        (pages != 0 && validated).then(|| Self {
            pages,
            free: header_integer(header, HEADER_FREELIST_COUNT),
        })
    }

    /// The pages in use: every page not on the freelist.
    #[must_use]
    pub const fn used(self) -> u32 {
        self.pages.saturating_sub(self.free)
    }

    /// The pages on the freelist.
    #[must_use]
    pub const fn free(self) -> u32 {
        self.free
    }
}

/// The 4-byte big-endian integer at `offset` of `header`; every offset passed is one of the
/// `HEADER_*` constants, each at least 4 bytes before the header's end.
fn header_integer(header: &[u8; FILE_HEADER_BYTES], offset: usize) -> u32 {
    u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::{FILE_HEADER_BYTES, PageCounts};

    /// A header reads its page counts only where `SQLite` trusts its size: a nonzero size
    /// whose change counter matches the version-valid-for number.
    #[test]
    fn a_header_reads_its_page_counts_only_where_sqlite_trusts_its_size() {
        let header = |pages: u32, free: u32, counter: u32, valid_for: u32| {
            let mut header = [0_u8; FILE_HEADER_BYTES];
            header[24..28].copy_from_slice(&counter.to_be_bytes());
            header[28..32].copy_from_slice(&pages.to_be_bytes());
            header[36..40].copy_from_slice(&free.to_be_bytes());
            header[92..96].copy_from_slice(&valid_for.to_be_bytes());
            header
        };
        let counts =
            PageCounts::from_header(&header(9, 2, 5, 5)).expect("a validated header answers");
        assert_eq!((counts.used(), counts.free()), (7, 2));
        assert_eq!(PageCounts::from_header(&header(9, 2, 5, 4)), None);
        assert_eq!(PageCounts::from_header(&header(0, 0, 5, 5)), None);
    }

    /// A file shorter than its header, and a missing file, answer no counts.
    #[test]
    fn a_short_or_missing_file_answers_no_counts() -> Result<(), std::io::Error> {
        let directory = tempfile::tempdir()?;
        let short = directory.path().join("short");
        std::fs::write(&short, [0_u8; 10])?;
        assert_eq!(PageCounts::read(&short), None);
        assert_eq!(PageCounts::read(&directory.path().join("absent")), None);
        Ok(())
    }
}
