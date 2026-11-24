use super::*;
use std::io::Write;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn verified_archive_digest_is_retained_for_both_algorithms() -> TestResult {
    let members = [(
        "release/src/lib.rs",
        b"pub fn beacon() {}\n".as_slice(),
        tar::EntryType::Regular,
    )];
    for (format, bytes) in [
        (ArchiveFormat::TarGzip, tar_bytes(&members)?),
        (ArchiveFormat::Zip, zip_members_bytes(&members)?),
    ] {
        let expected_digest = FileDigest::of(&bytes);
        for expected in [
            ArchiveDigest::Sha256(Sha256::digest(&bytes).into()),
            ArchiveDigest::Sha512(Sha512::digest(&bytes).into()),
        ] {
            let files = read_archive(
                &bytes,
                format,
                &expected,
                Some("release"),
                ArchiveLimits::default(),
            )?;
            assert_eq!(files.digest(), expected_digest);
            assert_eq!(
                files.files()[&ProjectPath::new("src/lib.rs")?],
                members[0].1
            );
        }
        for expected in [
            ArchiveDigest::Sha256([0; 32]),
            ArchiveDigest::Sha512([0; 64]),
        ] {
            assert!(matches!(
                read_archive(
                    &bytes,
                    format,
                    &expected,
                    Some("release"),
                    ArchiveLimits::default()
                ),
                Err(ArchiveError::DigestMismatch)
            ));
        }
    }
    Ok(())
}

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
    zip_members_bytes_with_options(entries, zip::write::SimpleFileOptions::default())
}

fn zip_members_bytes_with_options(
    entries: &[(&str, &[u8], tar::EntryType)],
    options: zip::write::SimpleFileOptions,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for &(path, content, kind) in entries {
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
        options.add_extra_field(0x7075, extra, true)?;
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
            ArchiveError::unsafe_path(name)
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
        ArchiveBound::Members.exceeded(3, 2, None)
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

// Issue #1036: the last member is kept, as tar extraction keeps it.
#[test]
fn repeated_files_with_conflicting_bytes_or_lengths_keep_the_last() -> TestResult {
    let regular = tar::EntryType::Regular;
    for second in [b"tesT".as_slice(), b"texts", b""] {
        let entries = [
            ("release/a", b"text".as_slice(), regular),
            ("release/./a", second, regular),
        ];
        let mut raw = zip_members_bytes(&[
            ("release/a", b"text".as_slice(), regular),
            ("release/b", second, regular),
        ])?;
        replace_zip_name(&mut raw, b"release/b", b"release/a");
        for (bytes, format) in [
            (tar_bytes(&entries)?, ArchiveFormat::TarGzip),
            (zip_members_bytes(&entries)?, ArchiveFormat::Zip),
            (raw, ArchiveFormat::Zip),
        ] {
            for limits in both_modes(ArchiveLimits::default()) {
                let files = read_format(&bytes, format, limits)?;
                assert_eq!(retained(&files), [("a", second)], "{format:?}");
                assert_eq!(replaced(&files), ["a"], "{format:?}");
                assert_eq!(files.members().len(), 1);
                assert!(files.omissions().is_empty());
                assert_eq!(files.digest(), FileDigest::of(&bytes));
            }
        }
    }
    let entries = [
        ("release/a", b"one".as_slice(), regular),
        ("release/b", b"same", regular),
        ("release/a", b"two", regular),
        ("release/b", b"same", regular),
        ("release/a", b"one", regular),
    ];
    let files = read(&tar_bytes(&entries)?, ArchiveLimits::default())?;
    assert_eq!(retained(&files), [("a", b"one".as_slice()), ("b", b"same")]);
    assert_eq!(replaced(&files), ["a"], "equal bytes replace nothing");
    Ok(())
}

#[test]
fn repeated_files_with_different_bytes_keep_every_refusal() -> TestResult {
    let regular = tar::EntryType::Regular;
    let file = ("release/a", b"one".as_slice(), regular);
    let other = ("release/a", b"two".as_slice(), regular);
    for third in [
        ("release/a", b"".as_slice(), tar::EntryType::Symlink),
        ("release/a", b"", tar::EntryType::Link),
        ("release/a/", b"", tar::EntryType::Directory),
        ("release/a/b", b"", regular),
    ] {
        for entries in [
            [file, other, third],
            [file, third, other],
            [third, file, other],
        ] {
            let bytes = tar_bytes(&entries)?;
            for limits in both_modes(ArchiveLimits::default()) {
                assert_eq!(
                    read(&bytes, limits).expect_err("member type or parent collision"),
                    ArchiveError::DuplicatePath,
                    "{entries:?}"
                );
            }
        }
    }
    let name = format!("release/{DECOMPOSED}");
    let repeated = format!("release/./{DECOMPOSED}");
    let entries = [
        (name.as_str(), b"one".as_slice(), regular),
        (&repeated, b"three", regular),
    ];
    for (archive, format, bytes) in member_archives(&entries)? {
        let files = read_format(&bytes, format, ArchiveLimits::default())?;
        assert_eq!(replaced(&files), [DECOMPOSED], "{archive}");
        assert_eq!(files.omissions().len(), 1, "{archive}");
        assert_eq!(files.omissions()[0].size(), 5, "{archive}");
    }
    Ok(())
}

#[test]
fn replaced_files_consume_member_and_expanded_byte_bounds() -> TestResult {
    let regular = tar::EntryType::Regular;
    let entries = [
        ("release/a", b"one".as_slice(), regular),
        ("release/./a", b"three", regular),
    ];
    let bytes = zip_members_bytes(&entries)?;
    let limits =
        |expanded, member, members| ArchiveLimits::new(bytes.len(), expanded, member, members, 200);
    assert_eq!(read_zip_fixture(&bytes, limits(8, 5, 2)?)?.files().len(), 1);
    assert_eq!(
        read_zip_fixture(&bytes, limits(7, 5, 2)?).expect_err("decompressed bytes"),
        ArchiveBound::ExpandedBytes.exceeded(8, 7, Some(fixture_path("release/a")))
    );
    assert_eq!(
        read_zip_fixture(&bytes, limits(8, 4, 2)?).expect_err("member bytes"),
        ArchiveBound::MemberBytes.exceeded(5, 4, Some(fixture_path("release/a")))
    );
    assert_eq!(
        read_zip_fixture(&bytes, limits(8, 5, 1)?).expect_err("member count"),
        ArchiveBound::Members.exceeded(2, 1, None)
    );
    let tar = tar_bytes(&entries)?;
    let limits = ArchiveLimits::new(tar.len(), 8192, 1024, 1, 200)?;
    assert_eq!(
        read(&tar, limits).expect_err("tar member count"),
        ArchiveBound::Members.exceeded(2, 1, Some(fixture_path("release/a")))
    );
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
            ArchiveBound::Members.exceeded(2, 1, None)
        );
        let expanded_limits = ArchiveLimits::new(bytes.len(), 7, 4, 2, 200)?;
        assert_eq!(
            read_zip_fixture(bytes, expanded_limits).expect_err("repeated ZIP decompressed bytes"),
            ArchiveBound::ExpandedBytes.exceeded(8, 7, Some(fixture_path("release/a")))
        );
        let exact_limits = ArchiveLimits::new(bytes.len(), 8, 4, 2, 200)?;
        assert_eq!(read_zip_fixture(bytes, exact_limits)?.files().len(), 1);
    }
    let limits = ArchiveLimits::new(tar.len(), 8192, 1024, 1, 200)?;
    assert_eq!(
        read(&tar, limits).expect_err("repeated tar member count"),
        ArchiveBound::Members.exceeded(2, 1, Some(fixture_path("release/a")))
    );
    let limits = ArchiveLimits::new(tar.len(), 2047, 1024, 2, 200)?;
    assert_eq!(
        read(&tar, limits).expect_err("repeated tar decompressed container bytes"),
        ArchiveBound::ExpandedBytes.exceeded(2048, 2047, None)
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

fn fixture_path(path: &str) -> ProjectPath {
    ProjectPath::new(path).expect("fixture member path")
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
fn archive_error_messages_name_no_path_but_a_bounded_or_unsafe_member() {
    let errors = [
        ArchiveError::InvalidLimits,
        ArchiveError::CompressedLimit,
        ArchiveError::DigestMismatch,
        ArchiveError::InvalidArchive,
        ArchiveBound::ExpandedBytes.exceeded(2, 1, None),
        ArchiveBound::Members.exceeded(2, 1, None),
        ArchiveError::WorkLimit,
        ArchiveError::DuplicatePath,
        ArchiveError::UnsupportedEntry,
        ArchiveError::RootMismatch,
    ];

    for error in errors {
        let message = error.to_string();
        assert!(!message.is_empty());
        assert!(!message.contains("release/"), "{message}");
    }
    let bounded = ArchiveBound::MemberBytes
        .exceeded(2, 1, Some(fixture_path("release/a")))
        .to_string();
    assert_eq!(bounded.matches("release/a").count(), 1, "{bounded}");
    let named = ArchiveError::unsafe_path("release/../a\n").to_string();
    assert_eq!(
        named, "archive path is invalid: \"release/../a\\n\"",
        "control characters are escaped"
    );
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
            ArchiveError::unsafe_path(path),
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
fn normalized_duplicates_keep_the_last_and_file_parent_collisions_are_refused() -> TestResult {
    for (first, second) in [("release/a", "./release/a"), ("release/a", "release//a")] {
        let bytes = tar_bytes(&[
            (first, b"one", tar::EntryType::Regular),
            (second, b"two", tar::EntryType::Regular),
        ])?;
        let files = read(&bytes, ArchiveLimits::default())?;
        assert_eq!(retained(&files), [("a", b"two".as_slice())]);
        assert_eq!(replaced(&files), ["a"]);
    }
    for (first, second) in [("release/a", "release/a/b"), ("release/a/b", "release/a")] {
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
        ArchiveError::unsafe_path("./")
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
        ArchiveBound::Members.exceeded(3, 2, Some(fixture_path("release/link2")))
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
        ArchiveError::unsafe_path("../release")
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
        ArchiveError::unsafe_path("release/docs")
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
        ArchiveBound::MemberBytes.exceeded(4, 3, Some(fixture_path("release/a")))
    );
    let two = tar_bytes(&[
        ("release/a", b"1234", tar::EntryType::Regular),
        ("release/b", b"1234", tar::EntryType::Regular),
    ])?;
    assert_eq!(
        read(&two, ArchiveLimits::new(two.len(), 16_384, 4, 1, 200)?)
            .expect_err("archive must be refused"),
        ArchiveBound::Members.exceeded(2, 1, Some(fixture_path("release/b")))
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
        ArchiveBound::ExpandedBytes.exceeded(
            expanded.len() as u64,
            expanded.len() as u64 - 1,
            None
        )
    );
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::new(bytes.len(), expanded.len(), 4, 1, 1)?
        )
        .expect_err("archive must be refused"),
        ArchiveBound::ExpansionRatio.exceeded(bytes.len() as u64 + 1, bytes.len() as u64, None)
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
    assert_eq!(
        read(
            &bytes,
            ArchiveLimits::from_configuration(&low.configuration().package.archive)?
        )
        .expect_err("configured member byte bound"),
        ArchiveBound::MemberBytes.exceeded(5, 4, Some(fixture_path("release/guide.md")))
    );
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
            ArchiveError::unsafe_path(path)
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
        ArchiveBound::Members.exceeded(0xfffe, 1, None)
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
        ArchiveBound::Members.exceeded(100_001, 100_000, None)
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
        ArchiveBound::ExtensionBytes.exceeded(
            TAR_EXTENSION_BYTES_MAX + 1,
            TAR_EXTENSION_BYTES_MAX,
            Some(fixture_path("@LongLink"))
        )
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
    let bytes = gzip(&tar)?;
    let exceeded =
        |maximum| ArchiveBound::ExtensionBytes.exceeded(extension_bytes as u64, maximum, None);
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("default extension byte bound"),
        exceeded(TAR_EXTENSION_BYTES_MAX)
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
        exceeded(extension_bytes as u64 - 1)
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
        ArchiveBound::MemberBytes.exceeded(
            extension_bytes as u64,
            extension_bytes as u64 - 1,
            None
        )
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
fn zip_conflicting_raw_names_keep_the_last_before_index_collapse() -> TestResult {
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
    let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
    assert_eq!(retained(&files), [("a", b"release/b".as_slice())]);
    assert_eq!(replaced(&files), ["a"]);
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

#[test]
fn complete_member_inventory_preserves_original_paths_and_implicit_parents() -> TestResult {
    let entries = [
        ("./release/", b"".as_slice(), tar::EntryType::Directory),
        (
            "./release//src/package/__init__.py",
            b"text",
            tar::EntryType::Regular,
        ),
        ("release/empty/", b"", tar::EntryType::Directory),
        ("release/hook.py", b"", tar::EntryType::Symlink),
    ];
    for (bytes, format, past_bound) in [
        (
            tar_bytes(&entries)?,
            ArchiveFormat::TarGzip,
            Some(fixture_path("release/hook.py")),
        ),
        (zip_members_bytes(&entries)?, ArchiveFormat::Zip, None),
    ] {
        let digest = ArchiveDigest::Sha256(Sha256::digest(&bytes).into());
        let exact = ArchiveLimits::new(bytes.len(), 16_384, 16, 4, 200)?;
        let archive = read_archive(&bytes, format, &digest, Some("release"), exact)?;
        let members = archive.members();
        assert_eq!(members.len(), 4);
        assert_eq!(members.get("release"), Some(&ArchiveMemberKind::Directory));
        assert_eq!(
            members.get("release/empty"),
            Some(&ArchiveMemberKind::Directory)
        );
        assert_eq!(
            members.get("release/hook.py"),
            Some(&ArchiveMemberKind::Link)
        );
        assert_eq!(
            members.get("release/src/package/__init__.py"),
            Some(&ArchiveMemberKind::File)
        );
        assert!(!members.contains_key("release/src"));
        assert!(
            members
                .range("release/src/".to_owned()..)
                .next()
                .is_some_and(|(path, _)| path.starts_with("release/src/"))
        );
        assert!(!members.contains_key("release/setup.py"));
        assert_eq!(
            archive.files()[&ProjectPath::new("src/package/__init__.py")?],
            b"text"
        );
        assert_eq!(archive.skipped_links(), &[ProjectPath::new("hook.py")?]);
        assert_eq!(
            read_archive(
                &bytes,
                format,
                &digest,
                Some("release"),
                ArchiveLimits::new(bytes.len(), 16_384, 16, 3, 200)?
            )
            .expect_err("complete inventory spends every admitted member"),
            ArchiveBound::Members.exceeded(4, 3, past_bound)
        );
    }
    Ok(())
}

#[test]
fn complete_member_inventory_refuses_file_and_link_parents_in_both_orders() -> TestResult {
    for kind in [tar::EntryType::Regular, tar::EntryType::Symlink] {
        for parent_first in [true, false] {
            let parent = ("release/parent", b"".as_slice(), kind);
            let child = (
                "release/parent/child.py",
                b"text".as_slice(),
                tar::EntryType::Regular,
            );
            let entries = if parent_first {
                [parent, child]
            } else {
                [child, parent]
            };
            for (bytes, format) in [
                (tar_bytes(&entries)?, ArchiveFormat::TarGzip),
                (zip_members_bytes(&entries)?, ArchiveFormat::Zip),
            ] {
                assert_eq!(
                    read_archive(
                        &bytes,
                        format,
                        &ArchiveDigest::Sha256(Sha256::digest(&bytes).into()),
                        Some("release"),
                        ArchiveLimits::default()
                    )
                    .expect_err("file or link cannot establish a directory parent"),
                    ArchiveError::DuplicatePath
                );
            }
        }
    }
    Ok(())
}

#[test]
fn ordinary_zip_members_preserve_exact_bytes_for_supported_compression() -> TestResult {
    let source = "pub fn beacon() { /* λ */ }\r\n".as_bytes();
    for method in [
        zip::CompressionMethod::Stored,
        zip::CompressionMethod::Deflated,
        zip::CompressionMethod::Zstd,
    ] {
        let bytes = zip_members_bytes_with_options(
            &[("release/src/café.rs", source, tar::EntryType::Regular)],
            zip::write::SimpleFileOptions::default().compression_method(method),
        )?;
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes))?;
        assert_eq!(archive.by_index(0)?.compression(), method);
        let files = read_zip_fixture(&bytes, ArchiveLimits::default())?;
        assert_eq!(files.files().len(), 1, "{method:?}");
        assert_eq!(files.files()[&ProjectPath::new("src/café.rs")?], source);
        assert!(files.skipped_links().is_empty(), "{method:?}");
        assert_eq!(files.digest(), FileDigest::of(&bytes));
    }
    Ok(())
}

/// One Zstandard frame of `2^(10 + window_exponent)` window bytes around one last raw block.
///
/// The descriptor byte sets neither single segment nor a content size, so the window
/// descriptor alone names the window the decoder allocates.
fn zstd_frame(window_exponent: u8, content: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, window_exponent << 3];
    let block = (u32::try_from(content.len())? << 3) | 1;
    frame.extend_from_slice(&block.to_le_bytes()[..3]);
    frame.extend_from_slice(content);
    Ok(frame)
}

/// Stores `frame` under each name, then marks every member Zstandard-compressed with the
/// CRC-32 and size of `content` in both its local and central headers.
fn zstd_frame_zip(
    names: &[&str],
    frame: &[u8],
    content: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let entries = names
        .iter()
        .map(|name| (*name, frame, tar::EntryType::Regular))
        .collect::<Vec<_>>();
    let mut bytes = zip_members_bytes_with_options(
        &entries,
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.clone()))?;
    let crc = rawzip::crc32(content).to_le_bytes();
    let size = u32::try_from(content.len())?.to_le_bytes();
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index)?;
        let local = usize::try_from(entry.header_start())?;
        let central = usize::try_from(entry.central_header_start())?;
        for (method, crc_offset, size_offset) in [
            (local + 8, local + 14, local + 22),
            (central + 10, central + 16, central + 24),
        ] {
            bytes[method..method + 2].copy_from_slice(&93_u16.to_le_bytes());
            bytes[crc_offset..crc_offset + 4].copy_from_slice(&crc);
            bytes[size_offset..size_offset + 4].copy_from_slice(&size);
        }
    }
    Ok(bytes)
}

/// Both ZIP readers: one member, and two members whose names repeat after the patch.
fn zstd_frame_zips(
    frame: &[u8],
    content: &[u8],
) -> Result<[Vec<u8>; 2], Box<dyn std::error::Error>> {
    let single = zstd_frame_zip(&["release/a"], frame, content)?;
    let mut repeated = zstd_frame_zip(&["release/a", "release/b"], frame, content)?;
    replace_zip_name(&mut repeated, b"release/b", b"release/a");
    Ok([single, repeated])
}

#[test]
fn zstd_frames_within_the_member_bound_decode_in_both_zip_readers() -> TestResult {
    let limits = ArchiveLimits::new(64 * 1024, 64 * 1024, 4096, 16, 200)?;
    // A 4 KiB window fits the 4 KiB member bound exactly.
    for bytes in zstd_frame_zips(&zstd_frame(2, b"text")?, b"text")? {
        let files = read_zip_fixture(&bytes, limits)?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&ProjectPath::new("a")?], b"text");
    }
    for bytes in zstd_frame_zips(&zstd_frame(2, b"text")?, b"txet")? {
        assert_eq!(
            read_zip_fixture(&bytes, limits).expect_err("Zstandard member CRC-32 mismatch"),
            ArchiveError::InvalidArchive
        );
    }
    Ok(())
}

#[test]
fn zstd_window_over_the_member_bound_is_refused_before_decoding() -> TestResult {
    let limits = ArchiveLimits::new(64 * 1024, 64 * 1024, 4096, 16, 200)?;
    // A 1 MiB window sits under libzstd's default 128 MiB limit and over the 4 KiB bound.
    for bytes in zstd_frame_zips(&zstd_frame(10, b"text")?, b"text")? {
        assert_eq!(
            read_zip_fixture(&bytes, limits).expect_err("Zstandard window over the member bound"),
            ArchiveBound::WindowBytes.exceeded(1 << 20, 4096, Some(fixture_path("release/a")))
        );
    }
    // The default 64 MiB member bound refuses a 1 GiB window the same way.
    for bytes in zstd_frame_zips(&zstd_frame(20, b"text")?, b"text")? {
        assert_eq!(
            read_zip_fixture(&bytes, ArchiveLimits::default())
                .expect_err("Zstandard window over the default member bound"),
            ArchiveBound::WindowBytes.exceeded(1 << 30, 64 << 20, Some(fixture_path("release/a")))
        );
    }
    Ok(())
}

/// A refused window is the one its frame header asks for: a window descriptor with a
/// mantissa, a single segment's content size in each width, and the header of a later frame
/// that arrives one byte at a time.
#[test]
fn a_refused_zstd_window_reports_the_bytes_its_frame_asks_for() -> TestResult {
    let limits = ArchiveLimits::new(64 * 1024, 64 * 1024, 1024, 16, 200)?;
    let magic = [0x28, 0xb5, 0x2f, 0xfd];
    let headers: [(&[u8], u64); 5] = [
        // Exponent 1 and mantissa 5: 2 KiB and five eighths of it.
        (&[0x00, (1 << 3) | 5], 2048 + 1280),
        // A one-byte dictionary identifier before a two-byte content size.
        (&[0x61, 0, 0x44, 0x0e], 0x0e44 + 256),
        (&[0xa0, 0x39, 0x30, 0, 0], 12_345),
        (&[0xe0, 0x01, 0, 0, 0, 1, 0, 0, 0], (1 << 32) + 1),
        // The widest header: a four-byte dictionary identifier and an eight-byte size.
        (&[0xe3, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0], 4096),
    ];
    for (header, expected) in headers {
        for first in [Vec::new(), zstd_frame(0, b"text")?] {
            for capacity in [1, 3, 8192] {
                let mut frames = first.clone();
                frames.extend_from_slice(&magic);
                frames.extend_from_slice(header);
                frames.extend_from_slice(&[0; 8]);
                let reader = BufReader::with_capacity(capacity, frames.as_slice());
                let mut member = ZstdMember::new(reader, limits, 1024)?;
                member
                    .read_to_end(&mut Vec::new())
                    .expect_err("window over 1 KiB");
                assert_eq!(
                    member.refusal(ArchiveError::InvalidArchive, "release/a"),
                    ArchiveBound::WindowBytes.exceeded(
                        expected,
                        1024,
                        Some(fixture_path("release/a"))
                    ),
                    "{header:?} after {} bytes, {capacity} at a time",
                    first.len()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn every_bound_names_its_quantities_and_member() {
    for (bound, expected) in [
        (ArchiveBound::Members, "member count"),
        (ArchiveBound::MemberBytes, "member byte"),
        (ArchiveBound::ExtensionBytes, "extended header byte"),
        (ArchiveBound::WindowBytes, "Zstandard window byte"),
        (ArchiveBound::ExpandedBytes, "decompressed byte"),
        (ArchiveBound::ExpansionRatio, "expansion ratio byte"),
    ] {
        let ArchiveError::BoundExceeded(exceeded) =
            bound.exceeded(5, 4, Some(fixture_path("release/a")))
        else {
            panic!("a bound refusal carries its quantities");
        };
        assert_eq!(exceeded.bound(), bound);
        assert_eq!(exceeded.observed(), 5);
        assert_eq!(exceeded.maximum(), 4);
        assert_eq!(exceeded.member(), Some(&fixture_path("release/a")));
        assert_eq!(
            ArchiveError::BoundExceeded(exceeded).to_string(),
            format!(
                "archive exceeds its {expected} bound: observed 5, maximum 4, member \"release/a\""
            )
        );
        assert_eq!(
            bound.exceeded(5, 4, None).to_string(),
            format!("archive exceeds its {expected} bound: observed 5, maximum 4")
        );
    }
}

const OVERSIZED_BYTES: usize = 5000;
const SOURCE: &[u8] = b"VALUE = 1\n";

fn oversized() -> Vec<u8> {
    (0..OVERSIZED_BYTES)
        .map(|index| u8::try_from(index * 31 % 251).expect("remainder fits a byte"))
        .collect()
}

fn skipping(bytes: &[u8]) -> Result<ArchiveLimits, ArchiveError> {
    Ok(ArchiveLimits::new(bytes.len(), 64 * 1024, 1024, 8, 200)?
        .with_oversized_members(OversizedMembers::Skip))
}

fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(bytes)?;
    gzip.finish()
}

/// Fixture name, container, and archive bytes.
type NamedArchive = (String, ArchiveFormat, Vec<u8>);

/// Every container and ZIP reader holding one source file and one oversized binary file.
fn oversized_archives() -> Result<Vec<NamedArchive>, Box<dyn std::error::Error>> {
    let binary = oversized();
    let entries = [
        ("release/__init__.py", SOURCE, tar::EntryType::Regular),
        (
            "release/native.so",
            binary.as_slice(),
            tar::EntryType::Regular,
        ),
        ("release/after.py", SOURCE, tar::EntryType::Regular),
    ];
    let mut archives = vec![(
        "tar.gz".to_owned(),
        ArchiveFormat::TarGzip,
        tar_bytes(&entries)?,
    )];
    for method in [
        zip::CompressionMethod::Stored,
        zip::CompressionMethod::Deflated,
        zip::CompressionMethod::Zstd,
    ] {
        // The writer's Zstandard frames ask for a 2 MiB window whatever their length, which no
        // retained member may under these bounds, so only the oversized member is compressed.
        let zip = |names: &[&str]| -> zip::result::ZipResult<Vec<u8>> {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for name in names {
                let native = *name == "release/native.so";
                let method = if native {
                    method
                } else {
                    zip::CompressionMethod::Stored
                };
                writer.start_file(
                    *name,
                    zip::write::SimpleFileOptions::default().compression_method(method),
                )?;
                writer.write_all(if native { &binary } else { SOURCE })?;
            }
            Ok(writer.finish()?.into_inner())
        };
        let names = [
            "release/__init__.py",
            "release/native.so",
            "release/after.py",
            "release/after.pz",
        ];
        archives.push((
            format!("zip {method:?}"),
            ArchiveFormat::Zip,
            zip(&names[..3])?,
        ));
        let mut repeated = zip(&names)?;
        replace_zip_name(&mut repeated, b"release/after.pz", b"release/after.py");
        archives.push((
            format!("repeated zip {method:?}"),
            ArchiveFormat::Zip,
            repeated,
        ));
    }
    Ok(archives)
}

fn read_format(
    bytes: &[u8],
    format: ArchiveFormat,
    limits: ArchiveLimits,
) -> Result<ArchiveFiles, ArchiveError> {
    let digest = ArchiveDigest::Sha256(Sha256::digest(bytes).into());
    read_archive(bytes, format, &digest, Some("release"), limits)
}

#[test]
fn one_oversized_file_refuses_by_default_and_is_omitted_on_request() -> TestResult {
    for (name, format, bytes) in oversized_archives()? {
        let skip = skipping(&bytes)?;
        let refuse = skip.with_oversized_members(OversizedMembers::Refuse);
        assert_eq!(
            refuse,
            ArchiveLimits::new(bytes.len(), 64 * 1024, 1024, 8, 200)?
        );
        assert_eq!(
            read_format(&bytes, format, refuse).expect_err(&name),
            ArchiveBound::MemberBytes.exceeded(5000, 1024, Some(fixture_path("release/native.so"))),
            "{name}"
        );

        let files = read_format(&bytes, format, skip)?;
        assert_eq!(
            files
                .files()
                .iter()
                .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
                .collect::<Vec<_>>(),
            [("__init__.py", SOURCE), ("after.py", SOURCE)],
            "{name}"
        );
        let [omission] = files.omissions() else {
            panic!("{name}: one omission, got {:?}", files.omissions());
        };
        assert_eq!(omission.path().as_str(), "native.so", "{name}");
        assert_eq!(omission.size(), 5000, "{name}");
        assert_eq!(
            files.members().get("release/native.so"),
            Some(&ArchiveMemberKind::File),
            "{name}"
        );
        assert!(files.skipped_links().is_empty(), "{name}");

        // A retained Zstandard member needs the writer's 2 MiB window under the member bound.
        let wide = ArchiveLimits::new(bytes.len(), 4 << 20, 4 << 20, 8, 200)?
            .with_oversized_members(OversizedMembers::Skip);
        let files = read_format(&bytes, format, wide)?;
        assert!(files.omissions().is_empty(), "{name}");
        assert_eq!(files.files().len(), 3, "{name}");
        assert_eq!(
            files.files()[&ProjectPath::new("native.so")?],
            oversized(),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn a_wheel_without_one_root_omits_its_native_library() -> TestResult {
    let binary = oversized();
    let bytes = zip_members_bytes(&[
        ("beacon/__init__.py", SOURCE, tar::EntryType::Regular),
        (
            "beacon/_native.so",
            binary.as_slice(),
            tar::EntryType::Regular,
        ),
        (
            "beacon-1.0.dist-info/METADATA",
            b"Name: beacon\n",
            tar::EntryType::Regular,
        ),
    ])?;
    let digest = ArchiveDigest::Sha256(Sha256::digest(&bytes).into());
    let files = read_archive(&bytes, ArchiveFormat::Zip, &digest, None, skipping(&bytes)?)?;
    assert_eq!(files.files().len(), 2);
    assert_eq!(files.omissions().len(), 1);
    assert_eq!(files.omissions()[0].path().as_str(), "beacon/_native.so");
    Ok(())
}

#[test]
fn plain_tar_members_are_omitted_from_a_stream() -> TestResult {
    let binary = oversized();
    let bytes = tar_bytes(&[
        ("release/a.py", SOURCE, tar::EntryType::Regular),
        (
            "release/native.so",
            binary.as_slice(),
            tar::EntryType::Regular,
        ),
    ])?;
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut tar)?;
    let limits = skipping(&bytes)?;
    check_tar_headers(tar.as_slice(), limits)?;
    let mut output = ArchiveOutput::new(Some("release"), limits, limits.expanded(tar.len()));
    read_tar_members(tar.as_slice(), &mut output)?;
    let files = output.into_files(FileDigest::of(&tar));
    assert_eq!(files.files().len(), 1);
    assert_eq!(files.omissions()[0].path().as_str(), "native.so");
    assert_eq!(files.omissions()[0].size(), 5000);
    Ok(())
}

#[test]
fn omitted_files_consume_member_and_expanded_byte_bounds() -> TestResult {
    for (name, format, bytes) in oversized_archives()? {
        let count = skipping(&bytes)?;
        let count = ArchiveLimits {
            members: 2,
            ..count
        };
        let error = read_format(&bytes, format, count).expect_err(&name);
        let ArchiveError::BoundExceeded(exceeded) = error else {
            panic!("{name}: {error:?}");
        };
        assert_eq!(exceeded.bound(), ArchiveBound::Members, "{name}");
        assert_eq!(exceeded.maximum(), 2, "{name}");
        if format == ArchiveFormat::TarGzip {
            assert_eq!(exceeded.member(), Some(&fixture_path("release/after.py")));
        }

        let expanded = ArchiveLimits {
            expanded_bytes: 5000,
            ..skipping(&bytes)?
        };
        let expected = if format == ArchiveFormat::TarGzip {
            ArchiveBound::ExpandedBytes.exceeded(5001, 5000, None)
        } else {
            ArchiveBound::ExpandedBytes.exceeded(
                5010,
                5000,
                Some(fixture_path("release/native.so")),
            )
        };
        assert_eq!(
            read_format(&bytes, format, expanded).expect_err(&name),
            expected,
            "{name}"
        );
        let after = ArchiveLimits {
            expanded_bytes: 5019,
            ..skipping(&bytes)?
        };
        if format == ArchiveFormat::Zip {
            assert_eq!(
                read_format(&bytes, format, after).expect_err(&name),
                ArchiveBound::ExpandedBytes.exceeded(
                    5020,
                    5019,
                    Some(fixture_path("release/after.py"))
                ),
                "{name}: an omitted file's bytes stay charged"
            );
        }
    }
    Ok(())
}

#[test]
fn omitted_zip_files_with_false_sizes_or_bytes_are_refused() -> TestResult {
    for (name, format, bytes) in oversized_archives()? {
        if format != ArchiveFormat::Zip {
            continue;
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.clone()))?;
        let entry = archive.by_index_raw(1)?;
        assert_eq!(entry.name()?, "release/native.so");
        let local = usize::try_from(entry.header_start())?;
        let central = usize::try_from(entry.central_header_start())?;
        drop(entry);
        for size in [2000_u32, 4999, 5001, 9000] {
            let mut bytes = bytes.clone();
            for offset in [local + 22, central + 24] {
                bytes[offset..offset + 4].copy_from_slice(&size.to_le_bytes());
            }
            assert_eq!(
                read_format(&bytes, format, skipping(&bytes)?).expect_err(&name),
                ArchiveError::InvalidArchive,
                "{name}: declared size {size}"
            );
        }
        let mut bytes = bytes.clone();
        for offset in [local + 14, central + 16] {
            bytes[offset] ^= 1;
        }
        assert_eq!(
            read_format(&bytes, format, skipping(&bytes)?).expect_err(&name),
            ArchiveError::InvalidArchive,
            "{name}: CRC-32"
        );
    }
    Ok(())
}

#[test]
fn omitted_tar_files_with_false_sizes_or_streams_are_refused() -> TestResult {
    let binary = oversized();
    let bytes = tar_bytes(&[(
        "release/native.so",
        binary.as_slice(),
        tar::EntryType::Regular,
    )])?;
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut tar)?;
    for length in [512 + 4096, 512 + 4999] {
        let short = gzip(&tar[..length])?;
        assert_eq!(
            read(&short, skipping(&short)?).expect_err("file shorter than its header"),
            ArchiveError::InvalidArchive
        );
    }
    assert_eq!(
        read(&bytes[..bytes.len() - 4], skipping(&bytes)?).expect_err("truncated gzip"),
        ArchiveError::InvalidArchive
    );
    let mut corrupt = bytes.clone();
    let trailer = corrupt.len() - 8;
    corrupt[trailer] ^= 1;
    assert_eq!(
        read(&corrupt, skipping(&corrupt)?).expect_err("gzip CRC-32"),
        ArchiveError::InvalidArchive
    );

    for (records, accepted) in [
        (["5000", "5000"], true),
        (["4999", "5000"], false),
        (["10000", "5000"], false),
        (["5000", "10000"], false),
    ] {
        let mut builder = tar::Builder::new(Vec::new());
        builder.append_pax_extensions(records.map(|size| ("size", size.as_bytes())))?;
        let mut header = tar::Header::new_ustar();
        header.set_path("release/native.so")?;
        header.set_size(5000);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, binary.as_slice())?;
        let bytes = gzip(&builder.into_inner()?)?;
        let result = read(&bytes, skipping(&bytes)?);
        if accepted {
            assert_eq!(result?.omissions().len(), 1);
        } else {
            assert_eq!(
                result.expect_err("PAX size differs from its header"),
                ArchiveError::InvalidArchive,
                "{records:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn oversized_links_directories_and_extended_headers_are_refused_in_both_modes() -> TestResult {
    let content = oversized();
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Directory,
        tar::EntryType::GNULongName,
        tar::EntryType::XHeader,
    ] {
        let bytes = tar_bytes(&[
            ("release/a", content.as_slice(), kind),
            ("release/b", SOURCE, tar::EntryType::Regular),
        ])?;
        let skip = skipping(&bytes)?;
        for limits in [skip, skip.with_oversized_members(OversizedMembers::Refuse)] {
            assert_eq!(
                read(&bytes, limits).expect_err("only regular files are omitted"),
                ArchiveBound::MemberBytes.exceeded(5000, 1024, Some(fixture_path("release/a"))),
                "{kind:?}"
            );
        }
    }
    let bytes = tar_bytes(&[("release/../a", content.as_slice(), tar::EntryType::Regular)])?;
    assert_eq!(
        read(&bytes, skipping(&bytes)?).expect_err("an omitted path is still checked"),
        ArchiveError::unsafe_path("release/../a")
    );
    Ok(())
}

#[test]
fn bound_refusals_resolve_extended_member_paths() -> TestResult {
    let content = oversized();
    let path = format!("release/{}/native.so", "x".repeat(120));
    let mut gnu = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5000);
    header.set_mode(0o644);
    gnu.append_data(&mut header, &path, content.as_slice())?;
    let mut pax = tar::Builder::new(Vec::new());
    pax.append_pax_extensions([("path", path.as_bytes())])?;
    let mut header = tar::Header::new_ustar();
    header.set_path("release/short")?;
    header.set_size(5000);
    header.set_mode(0o644);
    header.set_cksum();
    pax.append(&header, content.as_slice())?;
    for tar in [gnu.into_inner()?, pax.into_inner()?] {
        let bytes = gzip(&tar)?;
        let skip = skipping(&bytes)?;
        assert_eq!(
            read(
                &bytes,
                skip.with_oversized_members(OversizedMembers::Refuse)
            )
            .expect_err("member byte bound"),
            ArchiveBound::MemberBytes.exceeded(5000, 1024, Some(fixture_path(&path)))
        );
        let files = read(&bytes, skip)?;
        assert_eq!(
            files.omissions()[0].path().as_str(),
            path.strip_prefix("release/").ok_or("fixture root")?
        );
    }
    Ok(())
}

#[test]
fn repeated_omitted_files_compare_bytes_and_keep_the_last() -> TestResult {
    let binary = oversized();
    let mut changed = binary.clone();
    changed[2500] ^= 1;
    let longer = [binary.as_slice(), b"x"].concat();
    for (first, second, equal) in [
        (binary.as_slice(), binary.as_slice(), true),
        (&binary, &changed, false),
        (&binary, &longer, false),
        (&binary, SOURCE, false),
        (SOURCE, &binary, false),
    ] {
        let entries = [
            ("release/a", first, tar::EntryType::Regular),
            ("release/./a", second, tar::EntryType::Regular),
        ];
        let mut raw = zip_members_bytes(&[
            ("release/a", first, tar::EntryType::Regular),
            ("release/b", second, tar::EntryType::Regular),
        ])?;
        replace_zip_name(&mut raw, b"release/b", b"release/a");
        for (bytes, format) in [
            (tar_bytes(&entries)?, ArchiveFormat::TarGzip),
            (zip_members_bytes(&entries)?, ArchiveFormat::Zip),
            (raw, ArchiveFormat::Zip),
        ] {
            let files = read_format(&bytes, format, skipping(&bytes)?)?;
            if second == SOURCE {
                assert_eq!(retained(&files), [("a", SOURCE)]);
                assert!(files.omissions().is_empty());
            } else {
                assert!(files.files().is_empty());
                assert_eq!(files.omissions().len(), 1);
                assert_eq!(files.omissions()[0].path().as_str(), "a");
                assert_eq!(files.omissions()[0].size(), second.len() as u64);
            }
            let expected: &[&str] = if equal { &[] } else { &["a"] };
            assert_eq!(replaced(&files), expected);
        }
    }
    Ok(())
}

#[test]
fn omitted_zstd_windows_reach_the_recommended_width_and_no_further() -> TestResult {
    let binary = oversized();
    // Exponents 13 and 14 ask for 8 MiB and 16 MiB over the 1 KiB member bound.
    for bytes in zstd_frame_zips(&zstd_frame(13, &binary)?, &binary)? {
        let files = read_zip_fixture(&bytes, skipping(&bytes)?)?;
        assert_eq!(files.omissions().len(), 1);
        assert_eq!(files.omissions()[0].size(), 5000);
    }
    for bytes in zstd_frame_zips(&zstd_frame(14, &binary)?, &binary)? {
        assert_eq!(
            read_zip_fixture(&bytes, skipping(&bytes)?).expect_err("window over 8 MiB"),
            ArchiveBound::WindowBytes.exceeded(16 << 20, 8 << 20, Some(fixture_path("release/a")))
        );
    }
    Ok(())
}

/// One raw tar header of `flag` with its content, padded to whole blocks.
fn raw_member(name: &str, flag: u8, content: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut header = tar::Header::new_ustar();
    header
        .as_mut_bytes()
        .get_mut(..name.len())
        .ok_or("fixture path exceeds raw header")?
        .copy_from_slice(name.as_bytes());
    header.set_size(u64::try_from(content.len())?);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::new(flag));
    header.set_cksum();
    let mut bytes = header.as_bytes().to_vec();
    bytes.extend_from_slice(content);
    bytes.resize(bytes.len().next_multiple_of(512), 0);
    Ok(bytes)
}

/// One PAX record, whose length counts its own digits.
fn pax_record(key: &str, value: &str) -> String {
    let rest = format!(" {key}={value}\n");
    let mut length = rest.len() + 1;
    while length != rest.len() + length.to_string().len() {
        length = rest.len() + length.to_string().len();
    }
    format!("{length}{rest}")
}

fn global_header(records: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    raw_member("pax_global_header", b'g', records.as_bytes())
}

/// The limits of both oversized member modes over the same bounds.
fn both_modes(limits: ArchiveLimits) -> [ArchiveLimits; 2] {
    [
        limits,
        limits.with_oversized_members(OversizedMembers::Skip),
    ]
}

// Issue #1033.
#[test]
fn pax_global_headers_are_metadata_under_no_root() -> TestResult {
    let comment = pax_record("comment", "0123456789abcdef");
    let source = raw_member("release/a.py", b'0', SOURCE)?;
    let other = raw_member("release/b.py", b'0', SOURCE)?;
    let global = global_header(&comment)?;
    for (tar, files) in [
        ([global.as_slice(), &source].concat(), 1),
        ([&source, global.as_slice(), &other].concat(), 2),
        ([&source, global.as_slice()].concat(), 1),
        ([global.as_slice(), &global, &source].concat(), 1),
    ] {
        let bytes = gzip(&tar)?;
        for limits in both_modes(ArchiveLimits::default()) {
            let read = read(&bytes, limits)?;
            assert_eq!(read.files().len(), files);
            assert_eq!(read.files()[&fixture_path("a.py")], SOURCE);
            assert_eq!(read.members().len(), files);
            assert!(read.skipped_links().is_empty());
            assert!(read.omissions().is_empty());
        }
    }
    let local = raw_member(
        "PaxHeaders/b",
        b'x',
        pax_record("path", "release/c.py").as_bytes(),
    )?;
    let bytes = gzip(&[global.as_slice(), &local, &other].concat())?;
    for limits in both_modes(ArchiveLimits::default()) {
        let files = read(&bytes, limits)?;
        assert_eq!(files.files().len(), 1);
        assert_eq!(files.files()[&fixture_path("c.py")], SOURCE);
    }
    Ok(())
}

#[test]
fn pax_global_headers_consume_member_and_extension_byte_bounds() -> TestResult {
    let comment = pax_record("comment", &"x".repeat(600));
    let size = comment.len();
    let bytes = gzip(
        &[
            global_header(&comment)?,
            raw_member("release/a.py", b'0', SOURCE)?,
        ]
        .concat(),
    )?;
    let named = || Some(fixture_path("pax_global_header"));
    for limits in both_modes(ArchiveLimits::new(
        bytes.len(),
        8192,
        size,
        2,
        u32::MAX as usize,
    )?) {
        assert_eq!(read(&bytes, limits)?.files().len(), 1);
        assert_eq!(
            read(&bytes, limits.with_extension_size(size)?)?
                .files()
                .len(),
            1
        );
        assert_eq!(
            read(&bytes, limits.with_extension_size(size - 1)?).expect_err("extension bytes"),
            ArchiveBound::ExtensionBytes.exceeded(size as u64, size as u64 - 1, named())
        );
    }
    for limits in both_modes(ArchiveLimits::new(
        bytes.len(),
        8192,
        size,
        1,
        u32::MAX as usize,
    )?) {
        assert_eq!(
            read(&bytes, limits).expect_err("member count"),
            ArchiveBound::Members.exceeded(2, 1, Some(fixture_path("release/a.py")))
        );
    }
    for limits in both_modes(ArchiveLimits::new(
        bytes.len(),
        8192,
        size - 1,
        2,
        u32::MAX as usize,
    )?) {
        assert_eq!(
            read(&bytes, limits).expect_err("member bytes"),
            ArchiveBound::MemberBytes.exceeded(size as u64, size as u64 - 1, named())
        );
    }
    Ok(())
}

#[test]
fn pax_global_headers_that_change_members_are_refused() -> TestResult {
    let source = raw_member("release/a.py", b'0', SOURCE)?;
    for (key, value) in [
        ("path", "release/b.py"),
        ("path", "../b.py"),
        ("linkpath", "/etc/passwd"),
        ("size", "4"),
        ("GNU.sparse.map", "0,4"),
        ("SCHILY.filetype", "sparse"),
    ] {
        let records = [pax_record("comment", "x"), pax_record(key, value)].concat();
        let bytes = gzip(&[global_header(&records)?, source.clone()].concat())?;
        for limits in both_modes(ArchiveLimits::default()) {
            assert_eq!(
                read(&bytes, limits).expect_err("global record changes members"),
                ArchiveError::UnsupportedEntry,
                "{key}={value}"
            );
        }
    }
    let bytes = gzip(&[global_header("malformed\n")?, source.clone()].concat())?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("malformed global record"),
        ArchiveError::InvalidArchive
    );
    // The tar library would give the local records to the global header.
    let comment = global_header(&pax_record("comment", "x"))?;
    let path = pax_record("path", "release/b.py");
    let local = raw_member("PaxHeaders/a", b'x', path.as_bytes())?;
    let long_name = raw_member("././@LongLink", b'L', b"release/b.py\0")?;
    for extension in [&local, &long_name] {
        let bytes = gzip(&[extension.as_slice(), &comment, &source].concat())?;
        for limits in both_modes(ArchiveLimits::default()) {
            assert_eq!(
                read(&bytes, limits).expect_err("global header between a member and its records"),
                ArchiveError::UnsupportedEntry
            );
        }
    }
    let outside = raw_member("release/../a.py", b'0', SOURCE)?;
    let bytes = gzip(&[comment.as_slice(), &outside].concat())?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("traversal after a global header"),
        ArchiveError::unsafe_path("release/../a.py")
    );
    let outside = raw_member("other/a.py", b'0', SOURCE)?;
    let bytes = gzip(&[comment.as_slice(), &outside].concat())?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("another root after a global header"),
        ArchiveError::RootMismatch
    );
    Ok(())
}

// Issue #1034.
#[test]
fn nul_padding_after_the_gzip_stream_is_accepted_within_the_compressed_bound() -> TestResult {
    let plain = tar_bytes(&[("release/a.py", SOURCE, tar::EntryType::Regular)])?;
    let empty = gzip(b"")?;
    for stream in [
        plain.clone(),
        [plain.as_slice(), &empty].concat(),
        [empty.as_slice(), &plain, &empty, &empty].concat(),
    ] {
        for padding in [0, 1, 512, 10_240] {
            let bytes = [stream.as_slice(), &vec![0; padding]].concat();
            let exact = ArchiveLimits::new(bytes.len(), 8192, 1024, 2, 200)?;
            for limits in both_modes(exact) {
                let files = read(&bytes, limits)?;
                assert_eq!(files.files()[&fixture_path("a.py")], SOURCE);
                assert_eq!(files.digest(), FileDigest::of(&bytes));
            }
            for limits in both_modes(ArchiveLimits::new(bytes.len() - 1, 8192, 1024, 2, 200)?) {
                assert_eq!(
                    read(&bytes, limits).expect_err("compressed byte bound"),
                    ArchiveError::CompressedLimit
                );
            }
        }
    }
    Ok(())
}

#[test]
fn trailing_gzip_data_other_than_nul_padding_is_refused() -> TestResult {
    let plain = tar_bytes(&[("release/a.py", SOURCE, tar::EntryType::Regular)])?;
    let other = tar_bytes(&[("release/b.py", SOURCE, tar::EntryType::Regular)])?;
    let cut = |bytes: usize| plain[..plain.len() - bytes].to_vec();
    let mut wrong_length = plain.clone();
    let length = wrong_length.len() - 4;
    wrong_length[length] ^= 1;
    for (name, bytes) in [
        ("text", [plain.as_slice(), b"x"].concat()),
        (
            "text after padding",
            [plain.as_slice(), &[0; 512], b"x"].concat(),
        ),
        (
            "text in padding",
            [plain.as_slice(), &[0; 8], b"x", &[0; 8]].concat(),
        ),
        (
            "member after padding",
            [plain.as_slice(), &[0; 512], &other].concat(),
        ),
        (
            "part of a member header",
            [plain.as_slice(), &other[..4]].concat(),
        ),
        (
            "part of a member header, padded",
            [plain.as_slice(), &other[..2], &[0; 8]].concat(),
        ),
        ("padding alone", vec![0; 512]),
        ("cut length", cut(1)),
        (
            "cut checksum, padded",
            [cut(5).as_slice(), &[0; 512]].concat(),
        ),
        (
            "wrong length, padded",
            [wrong_length.as_slice(), &[0; 512]].concat(),
        ),
    ] {
        for limits in both_modes(ArchiveLimits::default()) {
            assert_eq!(
                read(&bytes, limits).expect_err(name),
                ArchiveError::InvalidArchive,
                "{name}"
            );
        }
    }
    Ok(())
}

#[test]
fn nul_padding_does_not_raise_the_expansion_ratio_bound() -> TestResult {
    let plain = tar_bytes(&[("release/a.py", SOURCE, tar::EntryType::Regular)])?;
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(plain.as_slice()).read_to_end(&mut decoded)?;
    let accepted = decoded.len().div_ceil(plain.len());
    let refused = accepted - 1;
    let maximum = plain.len() * refused;
    for padding in [0, 512, 10_240] {
        let bytes = [plain.as_slice(), &vec![0; padding]].concat();
        for limits in both_modes(ArchiveLimits::new(bytes.len(), 65_536, 1024, 2, refused)?) {
            assert_eq!(
                read(&bytes, limits).expect_err("expansion ratio of the gzip stream"),
                ArchiveBound::ExpansionRatio.exceeded(maximum as u64 + 1, maximum as u64, None),
                "padding={padding}"
            );
        }
        for limits in both_modes(ArchiveLimits::new(bytes.len(), 65_536, 1024, 2, accepted)?) {
            assert_eq!(read(&bytes, limits)?.files().len(), 1);
        }
        // The byte bound decides where it is the narrower one.
        let narrow = decoded.len() - 1;
        for limits in both_modes(ArchiveLimits::new(bytes.len(), narrow, 1024, 2, 200)?) {
            assert_eq!(
                read(&bytes, limits).expect_err("decompressed bytes"),
                ArchiveBound::ExpandedBytes.exceeded(narrow as u64 + 1, narrow as u64, None)
            );
        }
    }
    Ok(())
}

/// `é` as `e` and a combining acute accent, the Unicode NFD form.
const DECOMPOSED: &str = "e\u{301}";
/// The Unicode NFC form of [`DECOMPOSED`].
const COMPOSED: &str = "\u{e9}";

/// One archive of the same members in every container and ZIP reader.
///
/// The repeated ZIP reader runs when a raw name repeats, so its archive holds `release/r` twice.
fn member_archives(
    entries: &[(&str, &[u8], tar::EntryType)],
) -> Result<Vec<NamedArchive>, Box<dyn std::error::Error>> {
    let regular = tar::EntryType::Regular;
    let mut repeated = entries.to_vec();
    repeated.push(("release/r", b"", regular));
    let mut raw_entries = repeated.clone();
    raw_entries.push(("release/s", b"", regular));
    let mut raw = zip_members_bytes(&raw_entries)?;
    replace_zip_name(&mut raw, b"release/s", b"release/r");
    Ok(vec![
        (
            "tar".to_owned(),
            ArchiveFormat::TarGzip,
            tar_bytes(&repeated)?,
        ),
        (
            "zip".to_owned(),
            ArchiveFormat::Zip,
            zip_members_bytes(&repeated)?,
        ),
        ("repeated zip".to_owned(), ArchiveFormat::Zip, raw),
    ])
}

fn retained(files: &ArchiveFiles) -> Vec<(&str, &[u8])> {
    files
        .files()
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect()
}

fn replaced(files: &ArchiveFiles) -> Vec<&str> {
    files.replaced().iter().map(ArchivePath::as_str).collect()
}

fn omitted(files: &ArchiveFiles) -> Vec<(&str, ArchiveMemberKind, ArchiveOmissionReason, u64)> {
    files
        .omissions()
        .iter()
        .map(|omission| {
            (
                omission.path().as_str(),
                omission.kind(),
                omission.reason(),
                omission.size(),
            )
        })
        .collect()
}

// Issue #1035.
#[test]
fn members_outside_unicode_nfc_are_omitted_by_path() -> TestResult {
    let regular = tar::EntryType::Regular;
    let directory = format!("release/{DECOMPOSED}/");
    let nested = format!("release/{DECOMPOSED}/b.py");
    let link = format!("release/{DECOMPOSED}.lnk");
    let file = format!("release/{DECOMPOSED}.txt");
    let entries = [
        ("release/a.py", SOURCE, regular),
        (file.as_str(), b"text".as_slice(), regular),
        (&directory, b"", tar::EntryType::Directory),
        (&nested, SOURCE, regular),
        (&link, b"", tar::EntryType::Symlink),
        ("release/z.py", SOURCE, regular),
    ];
    for (name, format, bytes) in member_archives(&entries)? {
        for limits in both_modes(ArchiveLimits::default()) {
            let files = read_format(&bytes, format, limits)?;
            assert_eq!(
                retained(&files),
                [("a.py", SOURCE), ("r", b"".as_slice()), ("z.py", SOURCE)],
                "{name}"
            );
            let reason = ArchiveOmissionReason::NonCanonicalUnicode;
            assert_eq!(
                omitted(&files),
                [
                    (DECOMPOSED, ArchiveMemberKind::Directory, reason, 0),
                    (
                        link.strip_prefix("release/").ok_or("fixture root")?,
                        ArchiveMemberKind::Link,
                        reason,
                        0
                    ),
                    (
                        file.strip_prefix("release/").ok_or("fixture root")?,
                        ArchiveMemberKind::File,
                        reason,
                        4
                    ),
                    (
                        nested.strip_prefix("release/").ok_or("fixture root")?,
                        ArchiveMemberKind::File,
                        reason,
                        SOURCE.len() as u64
                    ),
                ],
                "{name}"
            );
            assert_eq!(
                files.members().keys().collect::<Vec<_>>(),
                ["release/a.py", "release/r", "release/z.py"],
                "{name}"
            );
            assert!(files.skipped_links().is_empty(), "{name}");
        }
    }
    Ok(())
}

#[test]
fn names_outside_unicode_nfc_keep_every_path_refusal() -> TestResult {
    let long = format!("release/{}", DECOMPOSED.repeat(PROJECT_PATH_BYTES_MAX / 3));
    assert!(long.len() > PROJECT_PATH_BYTES_MAX);
    assert!(ProjectPath::new(long.nfc().collect::<String>()).is_ok());
    for (root, name) in [
        (Some("release"), format!("release/../{DECOMPOSED}")),
        (Some("release"), format!("release/{DECOMPOSED}/../../a")),
        (Some("release"), format!("/release/{DECOMPOSED}")),
        (Some("release"), format!("release\\{DECOMPOSED}")),
        (Some("release"), format!("release/{DECOMPOSED}\n")),
        (Some("release"), format!("C:/release/{DECOMPOSED}")),
        (Some("release"), format!("release/.rift/{DECOMPOSED}")),
        (None, format!(".rift/{DECOMPOSED}")),
        // The NFC form of U+212A is `K`, a drive letter.
        (None, "\u{212a}:/a".to_owned()),
    ] {
        for kind in [
            tar::EntryType::Regular,
            tar::EntryType::Symlink,
            tar::EntryType::Directory,
        ] {
            let entries = [
                ("release/a.py", SOURCE, tar::EntryType::Regular),
                (name.as_str(), b"".as_slice(), kind),
            ];
            let mut archives = vec![(ArchiveFormat::TarGzip, tar_bytes(&entries)?, name.clone())];
            if kind == tar::EntryType::Regular {
                archives.push((
                    ArchiveFormat::Zip,
                    zip_members_bytes(&entries)?,
                    name.clone(),
                ));
            }
            for (format, bytes, member) in archives {
                let digest = ArchiveDigest::Sha256(Sha256::digest(&bytes).into());
                assert_eq!(
                    read_archive(&bytes, format, &digest, root, ArchiveLimits::default())
                        .expect_err("unsafe name outside NFC"),
                    ArchiveError::unsafe_path(&member),
                    "{format:?} {kind:?}"
                );
            }
        }
    }
    let bytes = zip_members_bytes(&[(&long, b"", tar::EntryType::Regular)])?;
    let error = read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("long name");
    let ArchiveError::UnsafePath(path) = &error else {
        panic!("unsafe path, got {error:?}");
    };
    assert_eq!(
        path.name(),
        &long[..long.floor_char_boundary(PROJECT_PATH_BYTES_MAX)]
    );
    assert_eq!(path.name().len() + path.bytes_dropped(), long.len());
    assert!(path.bytes_dropped() > 0);
    assert!(
        error
            .to_string()
            .ends_with(&format!(" and {} more bytes", path.bytes_dropped())),
        "{error}"
    );
    let other = format!("other/{DECOMPOSED}");
    let bytes = tar_bytes(&[(&other, b"", tar::EntryType::Regular)])?;
    assert_eq!(
        read(&bytes, ArchiveLimits::default()).expect_err("another root"),
        ArchiveError::RootMismatch
    );
    Ok(())
}

#[test]
fn an_unsafe_tar_name_that_is_not_utf8_is_named() -> TestResult {
    let mut tar = raw_member("release/a.py", b'0', SOURCE)?;
    let mut header = tar::Header::new_ustar();
    header.as_mut_bytes()[..10].copy_from_slice(b"release/\xffb");
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.extend_from_slice(header.as_bytes());
    let bytes = gzip(&tar)?;
    for limits in both_modes(ArchiveLimits::default()) {
        assert_eq!(
            read(&bytes, limits).expect_err("name is not UTF-8"),
            ArchiveError::unsafe_path("release/\u{fffd}b")
        );
    }
    Ok(())
}

#[test]
fn names_equal_in_unicode_nfc_stay_separate_members() -> TestResult {
    let regular = tar::EntryType::Regular;
    let composed = format!("release/{COMPOSED}");
    let decomposed = format!("release/{DECOMPOSED}");
    for (first, second) in [(&composed, &decomposed), (&decomposed, &composed)] {
        let entries = [
            (first.as_str(), first.as_bytes(), regular),
            (second.as_str(), second.as_bytes(), regular),
        ];
        for (name, format, bytes) in member_archives(&entries)? {
            let files = read_format(&bytes, format, ArchiveLimits::default())?;
            assert_eq!(
                retained(&files),
                [("r", b"".as_slice()), (COMPOSED, composed.as_bytes())],
                "{name}"
            );
            assert_eq!(files.omissions().len(), 1, "{name}");
            assert_eq!(files.omissions()[0].path().as_str(), DECOMPOSED, "{name}");
        }
    }
    let entries = [
        ("release/e\u{302}\u{323}", b"one".as_slice(), regular),
        ("release/e\u{323}\u{302}", b"two", regular),
    ];
    for (name, format, bytes) in member_archives(&entries)? {
        let files = read_format(&bytes, format, ArchiveLimits::default())?;
        assert_eq!(files.omissions().len(), 2, "{name}");
    }
    Ok(())
}

#[test]
fn members_outside_unicode_nfc_keep_collision_refusals() -> TestResult {
    let regular = tar::EntryType::Regular;
    let file = format!("release/{DECOMPOSED}");
    let directory = format!("release/{DECOMPOSED}/");
    let child = format!("release/{DECOMPOSED}/a");
    let directory_kind = tar::EntryType::Directory;
    for entries in [
        [(&file, regular), (&child, regular)],
        [(&child, regular), (&file, regular)],
        [(&directory, directory_kind), (&file, regular)],
        [(&file, tar::EntryType::Symlink), (&file, regular)],
        [(&directory, directory_kind), (&directory, directory_kind)],
    ] {
        let bytes = tar_bytes(&entries.map(|(path, kind)| (path.as_str(), b"".as_slice(), kind)))?;
        assert_eq!(
            read(&bytes, ArchiveLimits::default()).expect_err("collision outside NFC"),
            ArchiveError::DuplicatePath,
            "{entries:?}"
        );
    }
    Ok(())
}

#[test]
fn members_outside_unicode_nfc_consume_every_bound() -> TestResult {
    let regular = tar::EntryType::Regular;
    let name = format!("release/{DECOMPOSED}.so");
    let binary = oversized();
    for (archive, format, bytes) in member_archives(&[(&name, &binary, regular)])? {
        let skip = skipping(&bytes)?;
        assert_eq!(
            read_format(
                &bytes,
                format,
                skip.with_oversized_members(OversizedMembers::Refuse)
            )
            .expect_err("member bytes"),
            ArchiveBound::MemberBytes.exceeded(5000, 1024, None),
            "{archive}"
        );
        let files = read_format(&bytes, format, skip)?;
        assert_eq!(
            omitted(&files),
            [(
                name.strip_prefix("release/").ok_or("fixture root")?,
                ArchiveMemberKind::File,
                ArchiveOmissionReason::NonCanonicalUnicode,
                5000
            )],
            "{archive}"
        );
        let members = ArchiveLimits::new(bytes.len(), 65_536, 8192, 1, 200)?;
        assert!(
            matches!(
                read_format(&bytes, format, members),
                Err(ArchiveError::BoundExceeded(exceeded))
                    if exceeded.bound() == ArchiveBound::Members
            ),
            "{archive}"
        );
        if format == ArchiveFormat::Zip {
            let expanded = ArchiveLimits::new(bytes.len(), 4999, 4999, 4, 200)?
                .with_oversized_members(OversizedMembers::Skip);
            assert_eq!(
                read_format(&bytes, format, expanded).expect_err("decompressed bytes"),
                ArchiveBound::ExpandedBytes.exceeded(5000, 4999, None),
                "{archive}"
            );
        }
    }
    let bytes = tar_bytes(&[(&name, &binary, regular)])?;
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut tar)?;
    let short = gzip(&tar[..512 + 4999])?;
    for limits in both_modes(ArchiveLimits::default()) {
        assert_eq!(
            read(&short, limits).expect_err("file shorter than its header"),
            ArchiveError::InvalidArchive
        );
    }
    let mut bytes = zip_bytes(&name, b"text")?;
    let start = zip::ZipArchive::new(Cursor::new(bytes.as_slice()))?
        .by_index(0)?
        .data_start()
        .ok_or("ZIP data offset unavailable")?;
    bytes[usize::try_from(start)?] ^= 1;
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("CRC-32"),
        ArchiveError::InvalidArchive
    );
    Ok(())
}
