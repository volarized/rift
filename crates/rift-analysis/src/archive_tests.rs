use super::*;
use std::io::Write;

type TestResult = Result<(), Box<dyn std::error::Error>>;

// Issue #600: compare every repeated member before retaining one normalized file.
#[test]
fn identical_tar_members_retain_one_file_after_normalization() -> TestResult {
    for content in [b"pub fn value() {}\r\n".as_slice(), b""] {
        for second in ["release/src/lib.rs", "release/./src/lib.rs"] {
            let bytes = tar_bytes(&[
                ("release/./src/lib.rs", content, tar::EntryType::Regular),
                (second, content, tar::EntryType::Regular),
            ])?;
            let files = read(&bytes, ArchiveLimits::default())?;
            assert_eq!(files.files().len(), 1);
            assert_eq!(files.files()[&ProjectPath::new("src/lib.rs")?], content);
            assert_eq!(files.digest(), FileDigest::of(&bytes));
        }
    }
    Ok(())
}

#[test]
fn identical_zip_members_retain_one_file_after_normalization() -> TestResult {
    for content in [b"pub fn value() {}\r\n".as_slice(), b""] {
        let regular = tar::EntryType::Regular;
        let bytes = zip_members_bytes(&[
            ("release/./src/lib.rs", content, regular),
            ("release/src/lib.rs", content, regular),
        ])?;
        let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&ProjectPath::new("src/lib.rs")?], content);
        assert_eq!(files.digest(), FileDigest::of(&bytes));
    }
    Ok(())
}

fn zip_members_bytes(
    entries: &[(&str, &[u8], tar::EntryType)],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for &(path, content, kind) in entries {
        let options = zip::write::SimpleFileOptions::default();
        match kind {
            tar::EntryType::Directory => archive.add_directory(path, options)?,
            tar::EntryType::Symlink => archive.add_symlink(path, "target", options)?,
            _ => {
                archive.start_file(path, options)?;
                archive.write_all(content)?;
            }
        }
    }
    Ok(archive.finish()?.into_inner())
}

fn replace_zip_name(bytes: &mut [u8], before: &[u8], after: &[u8]) {
    assert_eq!(
        before.len(),
        after.len(),
        "fixture names must preserve header lengths"
    );
    for start in 0..=bytes.len().saturating_sub(before.len()) {
        if bytes.get(start..start + before.len()) == Some(before) {
            bytes[start..start + before.len()].copy_from_slice(after);
        }
    }
}

#[test]
fn raw_zip_duplicates_preserve_utf8_and_cp437_names() -> TestResult {
    let regular = tar::EntryType::Regular;
    let mut utf8 = zip_members_bytes(&[
        ("release/é", b"text", regular),
        ("release/à", b"text", regular),
    ])?;
    replace_zip_name(&mut utf8, "release/à".as_bytes(), "release/é".as_bytes());
    let mut cp437 = zip_members_bytes(&[
        ("release/a", b"text", regular),
        ("release/b", b"text", regular),
    ])?;
    replace_zip_name(&mut cp437, b"release/a", b"release/\x82");
    replace_zip_name(&mut cp437, b"release/b", b"release/\x82");
    for bytes in [utf8, cp437] {
        let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&ProjectPath::new("é")?], b"text");
    }
    Ok(())
}

fn unicode_name_zip(name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for path in ["release/a", "release/b"] {
        let mut extra = vec![1];
        // FileOptions validates extras against an empty filename before start_file.
        extra.extend_from_slice(&0_u32.to_le_bytes());
        extra.extend_from_slice(name.as_bytes());
        let mut options = zip::write::FullFileOptions::default();
        options.add_extra_data(0x7075, extra, true)?;
        archive.start_file(path, options)?;
        archive.write_all(b"text")?;
    }
    let mut bytes = archive.finish()?.into_inner();
    let archive =
        rawzip::ZipArchive::from_slice(bytes.as_slice()).map_err(|error| error.to_string())?;
    let mut entries = archive.entries();
    let mut corrections = Vec::new();
    while let Some(entry) = entries.next_entry()? {
        let path = entry.file_path();
        let offset =
            usize::try_from(entry.central_directory_offset())? + 46 + path.as_ref().len() + 5;
        corrections.push((offset, rawzip::crc32(path.as_ref()).to_le_bytes()));
    }
    for (offset, crc) in corrections {
        bytes[offset..offset + 4].copy_from_slice(&crc);
    }
    Ok(bytes)
}

#[test]
fn repeated_zip_unicode_extra_names_share_path_validation() -> TestResult {
    let bytes = unicode_name_zip("release/é")?;
    let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
    assert_eq!(files.files().len(), 1);
    assert_eq!(files.files()[&ProjectPath::new("é")?], b"text");
    for name in ["release/../a", "/release/a", "release\\a"] {
        let bytes = unicode_name_zip(name)?;
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("unsafe Unicode name"),
            ArchiveError::UnsafePath
        );
    }
    let bytes = unicode_name_zip("other/a")?;
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("Unicode root mismatch"),
        ArchiveError::RootMismatch
    );
    let bytes = unicode_name_zip("release/a/")?;
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default())
            .expect_err("directory with nonzero bytes"),
        ArchiveError::UnsupportedEntry
    );
    Ok(())
}

#[test]
fn raw_zip_member_types_match_existing_decoder() -> TestResult {
    let original = zip_members_bytes(&[
        ("release/a", b"text", tar::EntryType::Regular),
        ("release/b", b"text", tar::EntryType::Regular),
    ])?;
    let mut archive = zip::ZipArchive::new(Cursor::new(&original))?;
    let central = usize::try_from(archive.by_index_raw(1)?.central_header_start())?;
    for (creator, mode, expected) in [
        (0, 0o120_777_u32, ArchiveError::DuplicatePath),
        (10, 0o120_777_u32, ArchiveError::DuplicatePath),
        (3, 0o160_644_u32, ArchiveError::DuplicatePath),
        (3, 0o030_644_u32, ArchiveError::UnsupportedEntry),
    ] {
        let mut bytes = original.clone();
        bytes[central + 5] = creator;
        bytes[central + 38..central + 42].copy_from_slice(&(mode << 16).to_le_bytes());
        if expected == ArchiveError::DuplicatePath {
            let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
            assert_eq!(files.files().len(), 1);
            assert_eq!(files.skipped_links(), &[ProjectPath::new("b")?]);
        } else {
            assert_eq!(
                read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("unknown Unix type"),
                expected
            );
        }
        replace_zip_name(&mut bytes, b"release/b", b"release/a");
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("repeated ZIP member type"),
            expected
        );
    }
    Ok(())
}

#[test]
fn raw_zip_duplicates_validate_members_before_index_collapse() -> TestResult {
    let original = zip_members_bytes(&[
        ("release/a", b"text", tar::EntryType::Regular),
        ("release/b", b"text", tar::EntryType::Regular),
    ])?;
    let mut archive = zip::ZipArchive::new(Cursor::new(&original))?;
    let entry = archive.by_index_raw(0)?;
    let central = usize::try_from(entry.central_header_start())?;
    let data = usize::try_from(entry.data_start().ok_or("missing data offset")?)?;
    drop(entry);
    for (offset, replacement, error) in [
        (central + 16, vec![0, 0, 0, 0], ArchiveError::InvalidArchive),
        (
            central + 24,
            5_u32.to_le_bytes().to_vec(),
            ArchiveError::InvalidArchive,
        ),
        (central + 10, vec![99, 0], ArchiveError::InvalidArchive),
        (central + 8, vec![1, 0], ArchiveError::UnsupportedEntry),
        (
            central + 38,
            (0o020_644_u32 << 16).to_le_bytes().to_vec(),
            ArchiveError::UnsupportedEntry,
        ),
        (data, vec![255], ArchiveError::InvalidArchive),
    ] {
        let mut bytes = original.clone();
        bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
        replace_zip_name(&mut bytes, b"release/b", b"release/a");
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("invalid earlier ZIP member"),
            error
        );
    }
    Ok(())
}

#[test]
fn zip_stored_member_declared_size_must_match_payload() -> TestResult {
    let original = zip_bytes("release/a", b"text")?;
    let files = read_zip_fixture(&original, ArchiveLimits::default())?;
    assert_eq!(files.files()[&ProjectPath::new("a")?], b"text");
    let mut archive = zip::ZipArchive::new(Cursor::new(&original))?;
    let central = usize::try_from(archive.by_index_raw(0)?.central_header_start())?;
    for declared_size in [3_u32, 5] {
        let mut bytes = original.clone();
        bytes[central + 24..central + 28].copy_from_slice(&declared_size.to_le_bytes());
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("stored ZIP payload length differs from declared size"),
            ArchiveError::InvalidArchive
        );
    }
    Ok(())
}

#[test]
fn repeated_zip_footer_member_counts_must_agree() -> TestResult {
    let mut bytes = zip_members_bytes(&[
        ("release/a", b"text", tar::EntryType::Regular),
        ("release/b", b"text", tar::EntryType::Regular),
    ])?;
    replace_zip_name(&mut bytes, b"release/b", b"release/a");
    let footer = bytes.len() - 22;
    bytes[footer + 10..footer + 12].copy_from_slice(&3_u16.to_le_bytes());
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("unequal ZIP member counts"),
        ArchiveError::InvalidArchive
    );
    Ok(())
}

#[test]
fn undeclared_repeated_zip_members_are_counted_and_refused() -> TestResult {
    let mut bytes = zip_members_bytes(&[
        ("release/a", b"text", tar::EntryType::Regular),
        ("release/b", b"text", tar::EntryType::Regular),
        ("release/c", b"text", tar::EntryType::Regular),
    ])?;
    replace_zip_name(&mut bytes, b"release/b", b"release/a");
    replace_zip_name(&mut bytes, b"release/c", b"release/a");
    let footer = bytes.len() - 22;
    bytes[footer + 8..footer + 12].copy_from_slice(&[2, 0, 2, 0]);
    let limits = ArchiveLimits::new(bytes.len(), 8192, 1024, 2, 200)?;
    assert_eq!(
        read_zip_fixture(&bytes, limits).expect_err("undeclared ZIP member exceeds bound"),
        ArchiveError::MemberLimit
    );
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("undeclared ZIP member"),
        ArchiveError::InvalidArchive
    );
    Ok(())
}

#[test]
fn raw_zip_unsupported_compression_precedes_skipped_links() -> TestResult {
    for kind in [tar::EntryType::Regular, tar::EntryType::Symlink] {
        let mut bytes = zip_members_bytes(&[
            ("release/a", b"text", kind),
            ("release/b", b"text", tar::EntryType::Regular),
        ])?;
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes))?;
        let central = usize::try_from(archive.by_index_raw(0)?.central_header_start())?;
        bytes[central + 10..central + 12].copy_from_slice(&98_u16.to_le_bytes());
        replace_zip_name(&mut bytes, b"release/b", b"release/a");
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("unsupported ZIP compression"),
            ArchiveError::InvalidArchive
        );
    }
    Ok(())
}

#[test]
fn repeated_zip_payload_reads_consume_work_bound() -> TestResult {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file(
        "release/a",
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    writer.write_all(&[b'x'; 4096])?;
    let original = writer.finish()?.into_inner();
    let mut archive = zip::ZipArchive::new(Cursor::new(&original))?;
    let central = usize::try_from(archive.by_index_raw(0)?.central_header_start())?;
    let footer = original.len() - 22;
    let record = &original[central..footer];
    let mut bytes = original[..central].to_vec();
    for _ in 0..10 {
        bytes.extend_from_slice(record);
    }
    bytes.extend_from_slice(&original[footer..]);
    let footer = bytes.len() - 22;
    bytes[footer + 8..footer + 12].copy_from_slice(&[10, 0, 10, 0]);
    bytes[footer + 12..footer + 16]
        .copy_from_slice(&u32::try_from(record.len() * 10)?.to_le_bytes());
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default())
            .expect_err("repeated compressed payload work"),
        ArchiveError::WorkLimit
    );
    Ok(())
}

#[test]
fn raw_zip_duplicates_support_zip64_and_prepended_data() -> TestResult {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for path in ["release/a", "release/b"] {
        writer.start_file(
            path,
            zip::write::SimpleFileOptions::default().large_file(true),
        )?;
        writer.write_all(b"text")?;
    }
    let mut bytes = writer.finish()?.into_inner();
    replace_zip_name(&mut bytes, b"release/b", b"release/a");
    let mut zip64 = bytes.clone();
    let footer_position = zip64.len() - 22;
    let mut footer = zip64.split_off(footer_position);
    let directory_size = u32::from_le_bytes(footer[12..16].try_into()?);
    let directory_offset = u32::from_le_bytes(footer[16..20].try_into()?);
    zip64.extend_from_slice(b"PK\x06\x06");
    zip64.extend_from_slice(&44_u64.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&[0; 8]);
    zip64.extend_from_slice(&2_u64.to_le_bytes());
    zip64.extend_from_slice(&2_u64.to_le_bytes());
    zip64.extend_from_slice(&u64::from(directory_size).to_le_bytes());
    zip64.extend_from_slice(&u64::from(directory_offset).to_le_bytes());
    zip64.extend_from_slice(b"PK\x06\x07");
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&u64::try_from(footer_position)?.to_le_bytes());
    zip64.extend_from_slice(&1_u32.to_le_bytes());
    footer[8..20].fill(0xff);
    zip64.extend_from_slice(&footer);
    for (archive, prefix) in [
        (&bytes, b"".as_slice()),
        (&bytes, b"#!/bin/sh\n"),
        (&zip64, b""),
        (&zip64, b"#!/bin/sh\n"),
    ] {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(archive);
        let mut reference = zip::ZipArchive::new(Cursor::new(&bytes))?;
        assert_eq!(reference.by_index(0)?.size(), 4);
        let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&ProjectPath::new("a")?], b"text");
        assert_eq!(files.digest(), FileDigest::of(&bytes));
    }
    Ok(())
}

#[test]
fn raw_zip_duplicates_compare_every_member_and_keep_exact_bytes() -> TestResult {
    for content in [b"exact\r\n".as_slice(), b""] {
        let mut bytes = zip_members_bytes(&[
            ("release/a", content, tar::EntryType::Regular),
            ("release/b", content, tar::EntryType::Regular),
        ])?;
        replace_zip_name(&mut bytes, b"release/b", b"release/a");
        let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&ProjectPath::new("a")?], content);
        assert_eq!(files.digest(), FileDigest::of(&bytes));
    }
    Ok(())
}

#[test]
fn repeated_files_with_conflicting_bytes_or_lengths_are_refused() -> TestResult {
    for second in [b"tesT".as_slice(), b"texts", b""] {
        let entries = [
            ("release/a", b"text".as_slice(), tar::EntryType::Regular),
            ("release/./a", second, tar::EntryType::Regular),
        ];
        let tar = tar_bytes(&entries)?;
        let zip = zip_members_bytes(&entries)?;
        assert_eq!(
            read(&tar, ArchiveLimits::default()).expect_err("conflicting tar bytes"),
            ArchiveError::DuplicatePath
        );
        assert_eq!(
            read_zip_fixture(&zip, ArchiveLimits::default()).expect_err("conflicting ZIP bytes"),
            ArchiveError::DuplicatePath
        );
        let mut raw = zip_members_bytes(&[
            ("release/a", b"text".as_slice(), tar::EntryType::Regular),
            ("release/b", second, tar::EntryType::Regular),
        ])?;
        replace_zip_name(&mut raw, b"release/b", b"release/a");
        assert_eq!(
            read_zip_fixture(&raw, ArchiveLimits::default())
                .expect_err("conflicting raw ZIP bytes"),
            ArchiveError::DuplicatePath
        );
    }
    Ok(())
}

#[test]
fn repeated_directories_and_member_type_collisions_are_refused() -> TestResult {
    let regular = tar::EntryType::Regular;
    let directory = tar::EntryType::Directory;
    let link = tar::EntryType::Symlink;
    for (first, second) in [
        (directory, directory),
        (directory, regular),
        (regular, directory),
        (link, regular),
        (regular, link),
        (link, link),
    ] {
        let first_path = if first == directory {
            "release/a/"
        } else {
            "release/a"
        };
        let second_path = if second == directory {
            "release/./a/"
        } else {
            "release/./a"
        };
        let entries = [
            (first_path, b"".as_slice(), first),
            (second_path, b"".as_slice(), second),
        ];
        let tar = tar_bytes(&entries)?;
        let zip = zip_members_bytes(&entries)?;
        assert_eq!(
            read(&tar, ArchiveLimits::default()).expect_err("tar member type collision"),
            ArchiveError::DuplicatePath
        );
        assert_eq!(
            read_zip_fixture(&zip, ArchiveLimits::default())
                .expect_err("ZIP member type collision"),
            ArchiveError::DuplicatePath
        );
    }
    Ok(())
}

#[test]
fn repeated_zip_names_preserve_parent_and_link_collisions() -> TestResult {
    for (first, second) in [("release/a", "release/a/b"), ("release/a/b", "release/a")] {
        let bytes = zip_members_bytes(&[
            (first, b"".as_slice(), tar::EntryType::Regular),
            (second, b"", tar::EntryType::Regular),
        ])?;
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("ZIP file parent collision"),
            ArchiveError::DuplicatePath
        );
    }
    for (first, second) in [
        (tar::EntryType::Regular, tar::EntryType::Symlink),
        (tar::EntryType::Symlink, tar::EntryType::Regular),
    ] {
        let mut bytes = zip_members_bytes(&[
            ("release/a", b"".as_slice(), first),
            ("release/b", b"", second),
        ])?;
        replace_zip_name(&mut bytes, b"release/b", b"release/a");
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("raw ZIP link collision"),
            ArchiveError::DuplicatePath
        );
    }
    Ok(())
}

#[test]
fn repeated_members_consume_member_and_expanded_byte_bounds() -> TestResult {
    let entries = [
        ("release/a", b"text".as_slice(), tar::EntryType::Regular),
        ("release/./a", b"text".as_slice(), tar::EntryType::Regular),
    ];
    let tar = tar_bytes(&entries)?;
    let zip = zip_members_bytes(&entries)?;
    let mut raw = zip_members_bytes(&[
        ("release/a", b"text".as_slice(), tar::EntryType::Regular),
        ("release/b", b"text".as_slice(), tar::EntryType::Regular),
    ])?;
    replace_zip_name(&mut raw, b"release/b", b"release/a");
    for bytes in [&zip, &raw] {
        let member_limits = ArchiveLimits::new(bytes.len(), 8192, 1024, 1, 200)?;
        assert_eq!(
            read_zip_fixture(bytes, member_limits).expect_err("repeated ZIP member count"),
            ArchiveError::MemberLimit
        );
        let expanded_limits = ArchiveLimits::new(bytes.len(), 7, 4, 2, 200)?;
        assert_eq!(
            read_zip_fixture(bytes, expanded_limits).expect_err("repeated ZIP decompressed bytes"),
            ArchiveError::ExpandedLimit
        );
        let exact_limits = ArchiveLimits::new(bytes.len(), 8, 4, 2, 200)?;
        assert_eq!(read_zip_fixture(bytes, exact_limits)?.files().len(), 1);
    }
    let limits = ArchiveLimits::new(tar.len(), 8192, 1024, 1, 200)?;
    assert_eq!(
        read(&tar, limits).expect_err("repeated tar member count"),
        ArchiveError::MemberLimit
    );
    let limits = ArchiveLimits::new(tar.len(), 2047, 1024, 2, 200)?;
    assert_eq!(
        read(&tar, limits).expect_err("repeated tar decompressed container bytes"),
        ArchiveError::ExpandedLimit
    );
    Ok(())
}

fn tar_bytes(
    entries: &[(&str, &[u8], tar::EntryType)],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut builder = tar::Builder::new(Vec::new());
    for &(path, bytes, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len())?);
        header.set_entry_type(kind);
        header.set_mode(0o644);
        // Raw header bytes allow hostile paths the safe archive writer refuses.
        if path.len() >= 100 {
            return Err("fixture path exceeds raw header".into());
        }
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder.append(&header, bytes)?;
    }
    let tar = builder.into_inner()?;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar)?;
    Ok(gzip.finish()?)
}

fn read(bytes: &[u8], limits: ArchiveLimits) -> Result<ArchiveFiles, ArchiveError> {
    read_archive(
        bytes,
        ArchiveFormat::TarGzip,
        &ArchiveDigest::Sha256(Sha256::digest(bytes).into()),
        Some("release"),
        limits,
    )
}

#[test]
fn archive_error_messages_are_generic() {
    let errors = [
        ArchiveError::InvalidLimits,
        ArchiveError::CompressedLimit,
        ArchiveError::DigestMismatch,
        ArchiveError::InvalidArchive,
        ArchiveError::ExpandedLimit,
        ArchiveError::MemberLimit,
        ArchiveError::WorkLimit,
        ArchiveError::UnsafePath,
        ArchiveError::DuplicatePath,
        ArchiveError::UnsupportedEntry,
        ArchiveError::RootMismatch,
    ];

    for error in errors {
        let message = error.to_string();
        assert!(!message.is_empty());
        assert!(!message.contains("release/"), "{message}");
    }
}

#[test]
fn verified_tar_and_zip_produce_identical_files() -> TestResult {
    let tar = tar_bytes(&[
        ("release/", b"", tar::EntryType::Directory),
        (
            "release/docs/guide.md",
            b"# Guide\nExact bytes.\n",
            tar::EntryType::Regular,
        ),
    ])?;
    let files = read(&tar, ArchiveLimits::default())?;
    assert_eq!(files.digest(), FileDigest::of(&tar));
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(
        "release/docs/guide.md",
        zip::write::SimpleFileOptions::default(),
    )?;
    zip.write_all(b"# Guide\nExact bytes.\n")?;
    let zip = zip.finish()?.into_inner();
    let zip = read_archive(
        &zip,
        ArchiveFormat::Zip,
        &ArchiveDigest::Sha512(Sha512::digest(&zip).into()),
        Some("release"),
        ArchiveLimits::default(),
    )?;
    assert_eq!(files.files(), zip.files());
    assert_eq!(
        files
            .files()
            .get(&ProjectPath::new("docs/guide.md")?)
            .map(Vec::as_slice),
        Some(b"# Guide\nExact bytes.\n".as_slice())
    );
    Ok(())
}

#[test]
fn digest_is_verified_before_container_decode() {
    assert_eq!(
        read_archive(
            b"not a container",
            ArchiveFormat::TarGzip,
            &ArchiveDigest::Sha256([0; 32]),
            None,
            ArchiveLimits::default()
        )
        .expect_err("archive must be refused"),
        ArchiveError::DigestMismatch
    );
    assert_eq!(
        read(b"not a container", ArchiveLimits::default()).expect_err("archive must be refused"),
        ArchiveError::InvalidArchive
    );
}

#[test]
fn unsafe_and_cross_root_paths_are_refused() -> TestResult {
    for path in [
        "/release/a",
        "release/../a",
        "release/a\\b",
        "C:/release/a",
        "release/a\n",
    ] {
        let bytes = tar_bytes(&[(path, b"text", tar::EntryType::Regular)])?;
        assert_eq!(
            read(&bytes, ArchiveLimits::default()).expect_err("archive must be refused"),
            ArchiveError::UnsafePath,
            "path={path:?}"
        );
    }
    let bytes = tar_bytes(&[("release-other/a", b"text", tar::EntryType::Regular)])?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("archive must be refused"),
        ArchiveError::RootMismatch
    );
    Ok(())
}

#[test]
fn normalized_duplicates_and_file_parent_collisions_are_refused() -> TestResult {
    for (first, second) in [
        ("release/a", "./release/a"),
        ("release/a", "release//a"),
        ("release/a", "release/a/b"),
        ("release/a/b", "release/a"),
    ] {
        let bytes = tar_bytes(&[
            (first, b"one", tar::EntryType::Regular),
            (second, b"two", tar::EntryType::Regular),
        ])?;
        assert_eq!(
            read(&bytes, ArchiveLimits::default()).expect_err("archive must be refused"),
            ArchiveError::DuplicatePath
        );
    }
    Ok(())
}

#[test]
fn devices_fifos_and_sparse_files_are_refused() -> TestResult {
    for kind in [
        tar::EntryType::Fifo,
        tar::EntryType::Char,
        tar::EntryType::Block,
        tar::EntryType::GNUSparse,
    ] {
        let bytes = tar_bytes(&[("release/a", b"", kind)])?;
        assert_eq!(
            read(&bytes, ArchiveLimits::default()).expect_err("archive must be refused"),
            ArchiveError::UnsupportedEntry
        );
    }
    Ok(())
}

#[test]
fn tar_symlink_and_hard_link_entries_are_skipped_and_named() -> TestResult {
    let bytes = tar_bytes(&[
        ("release/a", b"text", tar::EntryType::Regular),
        ("release/link.txt", b"", tar::EntryType::Symlink),
        ("release/hard.txt", b"", tar::EntryType::Link),
    ])?;
    let files = read(&bytes, ArchiveLimits::default())?;
    assert_eq!(files.files().len(), 1);
    assert_eq!(
        files
            .files()
            .get(&ProjectPath::new("a")?)
            .map(Vec::as_slice),
        Some(b"text".as_slice())
    );
    assert_eq!(
        files.skipped_links(),
        &[ProjectPath::new("link.txt")?, ProjectPath::new("hard.txt")?]
    );
    Ok(())
}

#[test]
fn link_naming_the_archive_root_is_refused() -> TestResult {
    let bytes = tar_bytes(&[("./", b"", tar::EntryType::Symlink)])?;
    assert_eq!(
        read_archive(
            &bytes,
            ArchiveFormat::TarGzip,
            &ArchiveDigest::Sha256(Sha256::digest(&bytes).into()),
            None,
            ArchiveLimits::default(),
        )
        .expect_err("a link cannot name the archive root"),
        ArchiveError::UnsafePath
    );
    Ok(())
}

#[test]
fn zip_symlink_entry_is_skipped_and_named() -> TestResult {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file("release/a", zip::write::SimpleFileOptions::default())?;
    archive.write_all(b"text")?;
    archive.add_symlink(
        "release/link.txt",
        "a",
        zip::write::SimpleFileOptions::default(),
    )?;
    let bytes = archive.finish()?.into_inner();
    let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
    assert_eq!(files.files().len(), 1);
    assert_eq!(
        files
            .files()
            .get(&ProjectPath::new("a")?)
            .map(Vec::as_slice),
        Some(b"text".as_slice())
    );
    assert_eq!(files.skipped_links(), &[ProjectPath::new("link.txt")?]);
    Ok(())
}

#[test]
fn skipped_link_count_is_bounded_by_the_member_limit() -> TestResult {
    let exact = tar_bytes(&[
        ("release/a", b"1234", tar::EntryType::Regular),
        ("release/link", b"", tar::EntryType::Symlink),
    ])?;
    let files = read(&exact, ArchiveLimits::new(exact.len(), 16_384, 4, 2, 200)?)?;
    assert_eq!(files.skipped_links(), &[ProjectPath::new("link")?]);

    let over = tar_bytes(&[
        ("release/a", b"1234", tar::EntryType::Regular),
        ("release/link", b"", tar::EntryType::Symlink),
        ("release/link2", b"", tar::EntryType::Symlink),
    ])?;
    assert_eq!(
        read(&over, ArchiveLimits::new(over.len(), 16_384, 4, 2, 200)?)
            .expect_err("archive must be refused"),
        ArchiveError::MemberLimit
    );
    Ok(())
}

#[test]
fn sparse_pax_metadata_and_invalid_release_roots_are_refused() -> TestResult {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_pax_extensions([("GNU.sparse.map", b"0,4".as_slice())])?;
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, "release/a", b"text".as_slice())?;
    let tar = builder.into_inner()?;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar)?;
    let sparse = gzip.finish()?;

    assert_eq!(
        read(&sparse, ArchiveLimits::default()).expect_err("sparse PAX metadata is refused"),
        ArchiveError::UnsupportedEntry
    );

    let regular = tar_bytes(&[("release/a", b"text", tar::EntryType::Regular)])?;
    let digest = ArchiveDigest::Sha256(Sha256::digest(&regular).into());
    assert_eq!(
        read_archive(
            &regular,
            ArchiveFormat::TarGzip,
            &digest,
            Some("../release"),
            ArchiveLimits::default(),
        )
        .expect_err("release root must be one safe path component"),
        ArchiveError::UnsafePath
    );
    Ok(())
}

#[test]
fn schily_sparse_metadata_and_nested_root_are_refused() -> TestResult {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_pax_extensions([("SCHILY.filetype", b"sparse".as_slice())])?;
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, "release/a", b"text".as_slice())?;
    let tar = builder.into_inner()?;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar)?;
    let sparse = gzip.finish()?;
    assert_eq!(
        read(&sparse, ArchiveLimits::default()).expect_err("SCHILY sparse metadata is refused"),
        ArchiveError::UnsupportedEntry
    );

    let regular = tar_bytes(&[("release/a", b"text", tar::EntryType::Regular)])?;
    let digest = ArchiveDigest::Sha256(Sha256::digest(&regular).into());
    assert_eq!(
        read_archive(
            &regular,
            ArchiveFormat::TarGzip,
            &digest,
            Some("release/docs"),
            ArchiveLimits::default(),
        )
        .expect_err("nested root is refused"),
        ArchiveError::UnsafePath
    );
    Ok(())
}

#[test]
fn archive_without_root_keeps_archive_paths() -> TestResult {
    let bytes = tar_bytes(&[("release/a", b"text", tar::EntryType::Regular)])?;
    let files = read_archive(
        &bytes,
        ArchiveFormat::TarGzip,
        &ArchiveDigest::Sha256(Sha256::digest(&bytes).into()),
        None,
        ArchiveLimits::default(),
    )?;

    assert_eq!(
        files
            .files()
            .get(&ProjectPath::new("release/a")?)
            .map(Vec::as_slice),
        Some(b"text".as_slice())
    );
    Ok(())
}

#[test]
fn member_and_compressed_bounds_accept_exact_and_refuse_one_over() -> TestResult {
    let bytes = tar_bytes(&[("release/a", b"1234", tar::EntryType::Regular)])?;
    let exact = ArchiveLimits::new(bytes.len(), 16_384, 4, 1, 200)?;
    assert_eq!(read(&bytes, exact)?.files().len(), 1);
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::new(bytes.len() - 1, 16_384, 4, 1, 200)?
        )
        .expect_err("archive must be refused"),
        ArchiveError::CompressedLimit
    );
    assert_eq!(
        read(&bytes, ArchiveLimits::new(bytes.len(), 16_384, 3, 1, 200)?)
            .expect_err("archive must be refused"),
        ArchiveError::MemberLimit
    );
    let two = tar_bytes(&[
        ("release/a", b"1234", tar::EntryType::Regular),
        ("release/b", b"1234", tar::EntryType::Regular),
    ])?;
    assert_eq!(
        read(&two, ArchiveLimits::new(two.len(), 16_384, 4, 1, 200)?)
            .expect_err("archive must be refused"),
        ArchiveError::MemberLimit
    );
    Ok(())
}

#[test]
fn decompressed_bytes_ratio_and_truncated_payloads_are_refused() -> TestResult {
    let bytes = tar_bytes(&[("release/a", b"1234", tar::EntryType::Regular)])?;
    let mut expanded = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut expanded)?;
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::new(bytes.len(), expanded.len(), 4, 1, 200)?
        )?
        .files()
        .len(),
        1
    );
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::new(bytes.len(), expanded.len() - 1, 4, 1, 200)?
        )
        .expect_err("archive must be refused"),
        ArchiveError::ExpandedLimit
    );
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::new(bytes.len(), expanded.len(), 4, 1, 1)?
        )
        .expect_err("archive must be refused"),
        ArchiveError::ExpandedLimit
    );
    assert_eq!(
        read(&bytes[..bytes.len() - 4], ArchiveLimits::default())
            .expect_err("archive must be refused"),
        ArchiveError::InvalidArchive
    );
    Ok(())
}

#[test]
fn archive_limits_reject_zero_and_excessive_values() {
    use rift_protocol::configuration::{ARCHIVE_BYTES_MAX, ARCHIVE_MEMBERS_MAX};

    for limits in [
        (0, 1, 1, 1, 1),
        (1, 0, 1, 1, 1),
        (1, 1, 0, 1, 1),
        (1, 1, 1, 0, 1),
        (1, 1, 1, 1, 0),
        (1, 1, 2, 1, 1),
        (
            1,
            1,
            1,
            usize::try_from(ARCHIVE_MEMBERS_MAX).expect("supported archive count") + 1,
            1,
        ),
    ] {
        assert_eq!(
            ArchiveLimits::new(limits.0, limits.1, limits.2, limits.3, limits.4),
            Err(ArchiveError::InvalidLimits)
        );
    }
    if let Ok(excessive) = usize::try_from(ARCHIVE_BYTES_MAX + 1) {
        assert_eq!(
            ArchiveLimits::new(excessive, 1, 1, 1, 1),
            Err(ArchiveError::InvalidLimits)
        );
        assert_eq!(
            ArchiveLimits::new(1, excessive, 1, 1, 1),
            Err(ArchiveError::InvalidLimits)
        );
        assert_eq!(
            ArchiveLimits::new(1, excessive, excessive, 1, 1),
            Err(ArchiveError::InvalidLimits)
        );
    }
    if let Ok(excessive) = usize::try_from(u64::from(u32::MAX) + 1) {
        assert_eq!(
            ArchiveLimits::new(1, 1, 1, 1, excessive),
            Err(ArchiveError::InvalidLimits)
        );
    }
}

#[test]
fn archive_limits_above_defaults_are_accepted() -> TestResult {
    use rift_protocol::configuration::{
        ARCHIVE_BYTES_MAX, ARCHIVE_EXPANSION_RATIO_MAX, ARCHIVE_MEMBERS_MAX, ArchiveConfiguration,
    };

    assert_eq!(
        ArchiveLimits::from_configuration(&ArchiveConfiguration::default())?,
        ArchiveLimits::default()
    );
    let limits = ArchiveLimits::new(65 << 20, 513 << 20, 65 << 20, 100_001, 201)?;
    assert_eq!(limits.expanded_bytes, 513 << 20);
    assert_eq!(limits.member_bytes, 65 << 20);
    assert_eq!(limits.members, 100_001);
    assert_eq!(limits.expansion_ratio, 201);
    if let Ok(bytes) = usize::try_from(ARCHIVE_BYTES_MAX) {
        let maximum = ArchiveLimits::new(
            bytes,
            bytes,
            bytes,
            usize::try_from(ARCHIVE_MEMBERS_MAX)?,
            usize::try_from(ARCHIVE_EXPANSION_RATIO_MAX)?,
        )?;
        assert_eq!(maximum.compressed_bytes_max(), bytes);
        assert_eq!(maximum.expanded_bytes, bytes);
    }
    Ok(())
}

#[test]
fn archive_configuration_and_environment_reach_acquisition() -> TestResult {
    use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
    use rift_protocol::configuration::WorkspaceConfiguration;

    let document = r#"
        [package.archive]
        compressed_size = "65mb"
        expanded_size = "513mb"
        member_size = "65mb"
        members = 100001
        expansion_ratio = 201
    "#;
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(document),
        &ConfigurationEnvironment::default(),
    )?;
    assert_eq!(accepted.configuration().validate(), Ok(()));
    let configured = ArchiveLimits::from_configuration(&accepted.configuration().package.archive)?;
    assert_eq!(
        configured,
        ArchiveLimits::new(65 << 20, 513 << 20, 65 << 20, 100_001, 201)?
    );

    let environment = ConfigurationEnvironment::from_variables([
        ("RIFT_PACKAGE_ARCHIVE_COMPRESSED_SIZE", "66mb"),
        ("RIFT_PACKAGE_ARCHIVE_EXPANDED_SIZE", "514mb"),
        ("RIFT_PACKAGE_ARCHIVE_MEMBER_SIZE", "66mb"),
        ("RIFT_PACKAGE_ARCHIVE_MEMBERS", "100002"),
        ("RIFT_PACKAGE_ARCHIVE_EXPANSION_RATIO", "202"),
    ]);
    let accepted = accept_configuration::<WorkspaceConfiguration>(Some(document), &environment)?;
    assert_eq!(accepted.configuration().validate(), Ok(()));
    let overridden = ArchiveLimits::from_configuration(&accepted.configuration().package.archive)?;
    assert_eq!(
        overridden,
        ArchiveLimits::new(66 << 20, 514 << 20, 66 << 20, 100_002, 202)?
    );
    assert_eq!(accepted.variables().len(), 5);

    let bytes = tar_bytes(&[("release/guide.md", b"guide", tar::EntryType::Regular)])?;
    let low = accept_configuration::<WorkspaceConfiguration>(
        Some("[package.archive]\nmember_size = '4b'\n"),
        &ConfigurationEnvironment::default(),
    )?;
    assert!(matches!(
        read(
            &bytes,
            ArchiveLimits::from_configuration(&low.configuration().package.archive)?
        ),
        Err(ArchiveError::MemberLimit)
    ));
    let raised = accept_configuration::<WorkspaceConfiguration>(
        Some("[package.archive]\nmember_size = '4b'\n"),
        &ConfigurationEnvironment::from_variables([("RIFT_PACKAGE_ARCHIVE_MEMBER_SIZE", "5b")]),
    )?;
    let files = read(
        &bytes,
        ArchiveLimits::from_configuration(&raised.configuration().package.archive)?,
    )?;
    assert_eq!(files.files().len(), 1);
    Ok(())
}

fn zip_bytes(path: &str, content: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file(
        path,
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    archive.write_all(content)?;
    Ok(archive.finish()?.into_inner())
}

fn read_zip_fixture(bytes: &[u8], limits: ArchiveLimits) -> Result<ArchiveFiles, ArchiveError> {
    read_archive(
        bytes,
        ArchiveFormat::Zip,
        &ArchiveDigest::Sha256(Sha256::digest(bytes).into()),
        Some("release"),
        limits,
    )
}

#[test]
fn zip_paths_modes_crc_and_declared_member_count_are_checked() -> TestResult {
    for path in ["/release/a", "release/../a", "release/a\\b"] {
        let bytes = zip_bytes(path, b"text")?;
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("unsafe ZIP path"),
            ArchiveError::UnsafePath
        );
    }
    let mut bytes = zip_bytes("release/a", b"text")?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice()))?;
    let entry = archive.by_index(0)?;
    let data_start = usize::try_from(entry.data_start().ok_or("ZIP data offset unavailable")?)?;
    let header_start = usize::try_from(entry.central_header_start())?;
    drop(entry);
    drop(archive);
    bytes[data_start] ^= 1;
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("CRC mismatch"),
        ArchiveError::InvalidArchive
    );
    bytes[data_start] ^= 1;
    // Set Unix origin and symlink mode in the central-directory metadata.
    bytes[header_start + 5] = 3;
    bytes[header_start + 38..header_start + 42]
        .copy_from_slice(&(0o120_777_u32 << 16).to_le_bytes());
    let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
    assert!(files.files().is_empty());
    assert_eq!(files.skipped_links(), &[ProjectPath::new("a")?]);
    let mut bytes = zip_bytes("release/a", b"text")?;
    let footer = bytes.len() - 22;
    bytes[footer + 8..footer + 12].copy_from_slice(&[0xfe, 0xff, 0xfe, 0xff]);
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::new(bytes.len(), 8192, 1024, 1, 200)?)
            .expect_err("declared count must be checked before allocation"),
        ArchiveError::MemberLimit
    );
    Ok(())
}

#[test]
fn zip_encrypted_entry_is_refused_as_unsupported() -> TestResult {
    let mut bytes = zip_bytes("release/a", b"text")?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice()))?;
    let header_start = usize::try_from(archive.by_index(0)?.central_header_start())?;
    drop(archive);
    // Set the encrypted bit in the central-directory general-purpose flags.
    bytes[header_start + 8] |= 1;
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("encrypted ZIP member"),
        ArchiveError::UnsupportedEntry
    );
    Ok(())
}

#[test]
fn zip64_declared_member_allocation_is_bounded() -> TestResult {
    let mut bytes = zip_bytes("release/a", b"text")?;
    let footer_position = bytes.len() - 22;
    let mut footer = bytes.split_off(footer_position);
    let directory_size = u32::from_le_bytes(footer[12..16].try_into()?);
    let directory_offset = u32::from_le_bytes(footer[16..20].try_into()?);
    bytes.extend_from_slice(b"PK\x06\x06");
    bytes.extend_from_slice(&44_u64.to_le_bytes());
    bytes.extend_from_slice(&45_u16.to_le_bytes());
    bytes.extend_from_slice(&45_u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&100_001_u64.to_le_bytes());
    bytes.extend_from_slice(&100_001_u64.to_le_bytes());
    bytes.extend_from_slice(&u64::from(directory_size).to_le_bytes());
    bytes.extend_from_slice(&u64::from(directory_offset).to_le_bytes());
    bytes.extend_from_slice(b"PK\x06\x07");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&u64::try_from(footer_position)?.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    footer[8..20].fill(0xff);
    bytes.extend_from_slice(&footer);
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default())
            .expect_err("ZIP64 member bound before allocation"),
        ArchiveError::MemberLimit
    );
    Ok(())
}

#[test]
fn tar_extended_headers_are_bounded_before_library_allocation() -> TestResult {
    let long_name = vec![b'x'; usize::try_from(TAR_EXTENSION_BYTES_MAX)? + 1];
    let bytes = tar_bytes(&[
        ("././@LongLink", &long_name, tar::EntryType::GNULongName),
        ("release/a", b"text", tar::EntryType::Regular),
    ])?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("extended header size"),
        ArchiveError::MemberLimit
    );
    Ok(())
}

#[test]
fn configured_tar_extension_bytes_accept_exact_and_refuse_one_over_before_allocation() -> TestResult
{
    use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
    use rift_protocol::configuration::{ARCHIVE_EXTENSION_BYTES_MAX, WorkspaceConfiguration};

    let mut builder = tar::Builder::new(Vec::new());
    let comment = vec![b'x'; usize::try_from(TAR_EXTENSION_BYTES_MAX)? + 1];
    builder.append_pax_extensions([("comment", comment.as_slice())])?;
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, "release/a", b"text".as_slice())?;
    let tar = builder.into_inner()?;
    let mut raw = tar::Archive::new(tar.as_slice());
    let extension_bytes = usize::try_from(
        raw.entries()?
            .raw(true)
            .next()
            .ok_or("extended header")??
            .size(),
    )?;
    assert!(extension_bytes > usize::try_from(TAR_EXTENSION_BYTES_MAX)?);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar)?;
    let bytes = gzip.finish()?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("default extension byte bound"),
        ArchiveError::MemberLimit
    );

    let document = format!(
        "[package.archive]\nextension_size = '{}b'\n",
        extension_bytes - 1
    );
    let configured = accept_configuration::<WorkspaceConfiguration>(
        Some(&document),
        &ConfigurationEnvironment::default(),
    )?;
    let limits = ArchiveLimits::from_configuration(&configured.configuration().package.archive)?;
    assert_eq!(
        read(&bytes, limits).expect_err("one byte past extension bound"),
        ArchiveError::MemberLimit
    );
    let environment = ConfigurationEnvironment::from_variables([(
        "RIFT_PACKAGE_ARCHIVE_EXTENSION_SIZE",
        format!("{extension_bytes}b"),
    )]);
    let configured = accept_configuration::<WorkspaceConfiguration>(Some(&document), &environment)?;
    let limits = ArchiveLimits::from_configuration(&configured.configuration().package.archive)?;
    assert_eq!(
        read(&bytes, limits)?
            .files()
            .get(&ProjectPath::new("a")?)
            .map(Vec::as_slice),
        Some(b"text".as_slice())
    );
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::default().with_extension_size(extension_bytes)?
        )?
        .files()
        .len(),
        1
    );

    let member_bound = ArchiveLimits::new(bytes.len(), 1 << 20, extension_bytes - 1, 10, 200)?
        .with_extension_size(extension_bytes)?;
    assert_eq!(
        read(&bytes, member_bound).expect_err("extension still passes member byte bound"),
        ArchiveError::MemberLimit
    );
    for value in [1, usize::try_from(ARCHIVE_EXTENSION_BYTES_MAX)?] {
        assert!(ArchiveLimits::default().with_extension_size(value).is_ok());
    }
    for value in [0, usize::try_from(ARCHIVE_EXTENSION_BYTES_MAX)? + 1] {
        assert_eq!(
            ArchiveLimits::default().with_extension_size(value),
            Err(ArchiveError::InvalidLimits)
        );
    }
    let invalid = ConfigurationEnvironment::from_variables([(
        "RIFT_PACKAGE_ARCHIVE_EXTENSION_SIZE",
        "67108865b",
    )]);
    let configured = accept_configuration::<WorkspaceConfiguration>(None, &invalid)?;
    assert!(matches!(
        configured.configuration().validate(),
        Err(
            rift_protocol::configuration::ConfigurationViolation::LimitOutOfRange {
                field: "package.archive.extension_size",
                ..
            }
        )
    ));
    Ok(())
}

#[test]
fn zip_conflicting_raw_names_are_refused_before_index_collapse() -> TestResult {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for path in ["release/a", "release/b"] {
        archive.start_file(path, zip::write::SimpleFileOptions::default())?;
        archive.write_all(path.as_bytes())?;
    }
    let mut bytes = archive.finish()?.into_inner();
    // Change both occurrences of the second name; safe writers refuse duplicate names.
    for offset in 0..bytes.len().saturating_sub(8) {
        if &bytes[offset..offset + 9] == b"release/b" {
            bytes[offset + 8] = b'a';
        }
    }
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("duplicate raw name"),
        ArchiveError::DuplicatePath
    );
    Ok(())
}

#[test]
fn zip_file_bytes_that_resemble_a_footer_remain_bytes() -> TestResult {
    let mut content = [0_u8; 22];
    content[..4].copy_from_slice(b"PK\x05\x06");
    content[8..12].fill(0xff);
    let bytes = zip_bytes("release/a", &content)?;
    let files = read_zip_fixture(&bytes, ArchiveLimits::new(bytes.len(), 8192, 1024, 1, 200)?)?;
    assert_eq!(files.files()[&ProjectPath::new("a")?], content);
    Ok(())
}

#[test]
fn zip_metadata_retry_work_is_bounded() -> TestResult {
    // Each invalid candidate forces the dependency to retry footer discovery.
    let mut bytes = vec![0; 64];
    for _ in 0..5000 {
        let mut footer = [0_u8; 22];
        footer[..4].copy_from_slice(b"PK\x05\x06");
        footer[8..12].copy_from_slice(&[1, 0, 1, 0]);
        bytes.extend_from_slice(&footer);
    }
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::new(bytes.len(), 8192, 1024, 1, 200)?)
            .expect_err("footer retry work must be bounded"),
        ArchiveError::WorkLimit
    );
    Ok(())
}
