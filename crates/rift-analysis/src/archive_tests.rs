use super::*;
use std::io::Write;

type TestResult = Result<(), Box<dyn std::error::Error>>;

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
fn links_and_special_files_are_refused() -> TestResult {
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
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
    assert!(ArchiveLimits::new(0, 1, 1, 1, 1).is_err());
    assert!(ArchiveLimits::new(1, 1, 2, 1, 1).is_err());
    assert!(ArchiveLimits::new(1, 1, 1, 100_001, 1).is_err());
    assert!(ArchiveLimits::new(1, 1, 1, 1, 201).is_err());
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
    assert_eq!(
        read_zip_fixture(&bytes, ArchiveLimits::default()).expect_err("ZIP symlink"),
        ArchiveError::UnsupportedEntry
    );
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
fn zip_duplicate_raw_names_are_refused_before_index_collapse() -> TestResult {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for path in ["release/a", "release/b"] {
        archive.start_file(path, zip::write::SimpleFileOptions::default())?;
        archive.write_all(b"text")?;
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
