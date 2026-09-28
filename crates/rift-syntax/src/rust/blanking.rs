//! Length-preserving blanking of the const-trait syntax the pinned grammar lacks.
//!
//! tree-sitter-rust 0.24.2 accepts no `const` in `impl_item`, `trait_item`,
//! `closure_expression`, `trait_bounds`, or `abstract_type` (`grammar.js`
//! `:493-519`, `:531-538`, `:895-906`, `:1295-1308`), so the standard library's
//! `impl [const] FnOnce(T)`, `const impl Default for String`, and
//! `const unsafe impl` parse as errors, and the declarations inside an error
//! lose their container. The provider parses a copy with those keywords
//! overwritten by spaces of the same length: every byte offset the tree
//! reports is an offset of the authored text, which the walk reads names,
//! signatures, and documentation from.

use std::borrow::Cow;
use std::ops::Range;

/// The keyword every blanked form spells.
const CONST_KEYWORD: &str = "const";

/// The source with each const-trait form blanked: `[const]`, `~const`,
/// `const` before `impl`, `trait`, `unsafe impl`, `unsafe trait`, or a
/// closure, and `const` after `impl`. Borrowed when no form occurs.
pub(super) fn blank_const_trait_keywords(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut blanked: Option<String> = None;
    let mut copied = 0;
    for (start, _) in text.match_indices(CONST_KEYWORD) {
        let Some(range) = blanked_range(bytes, start) else {
            continue;
        };
        let target = blanked.get_or_insert_with(|| String::with_capacity(text.len()));
        target.push_str(&text[copied..range.start]);
        target.extend(std::iter::repeat_n(' ', range.len()));
        copied = range.end;
    }
    match blanked {
        None => Cow::Borrowed(text),
        Some(mut target) => {
            target.push_str(&text[copied..]);
            Cow::Owned(target)
        }
    }
}

/// The bytes to blank for the `const` spelled at `start`; `None` when it is
/// no const-trait form: a `const` item, `const fn`, `const` block, or part
/// of a longer identifier. Every returned boundary sits on an ASCII byte.
fn blanked_range(bytes: &[u8], start: usize) -> Option<Range<usize>> {
    let end = start + CONST_KEYWORD.len();
    let after = bytes.get(end).copied();
    if after.is_some_and(is_identifier_byte) {
        return None;
    }
    match start.checked_sub(1).map(|before| bytes[before]) {
        Some(b'~') => return Some(start - 1..end),
        Some(b'[') if after == Some(b']') => return Some(start - 1..end + 1),
        Some(before) if is_identifier_byte(before) => return None,
        _ => {}
    }
    (precedes_blanked_form(bytes, end) || previous_word(bytes, start) == b"impl")
        .then_some(start..end)
}

/// Whether the tokens after a `const` ending at `end` are `impl`, `trait`,
/// `unsafe impl`, `unsafe trait`, or open a closure (`|`, `||`, `move`).
fn precedes_blanked_form(bytes: &[u8], end: usize) -> bool {
    let next = skip_whitespace(bytes, end);
    match word_at(bytes, next) {
        b"impl" | b"trait" | b"move" => true,
        b"unsafe" => matches!(
            word_at(bytes, skip_whitespace(bytes, next + b"unsafe".len())),
            b"impl" | b"trait"
        ),
        b"" => bytes.get(next) == Some(&b'|'),
        _ => false,
    }
}

/// The identifier ending at the last non-whitespace byte before `start`.
fn previous_word(bytes: &[u8], start: usize) -> &[u8] {
    let Some(last) = bytes[..start]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
    else {
        return b"";
    };
    let first = bytes[..=last]
        .iter()
        .rposition(|byte| !is_identifier_byte(*byte))
        .map_or(0, |separator| separator + 1);
    &bytes[first..=last]
}

/// The identifier starting at `position`; empty when none starts there.
fn word_at(bytes: &[u8], position: usize) -> &[u8] {
    let rest = bytes.get(position..).unwrap_or_default();
    let length = rest
        .iter()
        .position(|byte| !is_identifier_byte(*byte))
        .unwrap_or(rest.len());
    &rest[..length]
}

fn skip_whitespace(bytes: &[u8], position: usize) -> usize {
    bytes
        .get(position..)
        .and_then(|rest| rest.iter().position(|byte| !byte.is_ascii_whitespace()))
        .map_or(bytes.len(), |offset| position + offset)
}

/// Bytes an identifier can hold; any non-ASCII byte counts, so a `const`
/// inside a Unicode identifier is never blanked.
const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each form the grammar lacks, as the standard library spells it.
    const FORMS: [&str; 9] = [
        "const impl Default for Plain {}",
        "impl const Clone for Plain {}",
        "const unsafe impl Send for Plain {}",
        "pub const trait Measure {}",
        "pub const unsafe trait Raw {}",
        "fn call<F: [const] FnOnce() -> u8>(f: F) -> u8 { f() }",
        "fn old<F: ~const FnOnce() -> u8>(f: F) -> u8 { f() }",
        "fn closure() { let f = const || 1; }",
        "fn moved() { let f = const move |x: u8| x; }",
    ];

    #[test]
    fn test_every_form_is_blanked_with_its_byte_length_kept() {
        for form in FORMS {
            let blanked = blank_const_trait_keywords(form);
            assert_eq!(blanked.len(), form.len(), "form: {form}");
            assert!(!blanked.contains("const"), "form: {form} -> {blanked}");
            for (authored, parsed) in form.bytes().zip(blanked.bytes()) {
                assert!(authored == parsed || parsed == b' ', "form: {form}");
            }
        }
    }

    #[test]
    fn test_const_items_functions_blocks_and_identifiers_stay_as_authored() {
        let text = "const LIMIT: u8 = 1;\nconst fn f() {}\nconst unsafe fn g() {}\n\
                    fn h() { const { 1 }; let constant = [const { 0 }; 2]; }\n\
                    #[const_trait]\nstruct constimpl;\nimpl Plain { const X: u8 = 0; }\n\
                    fn é() { let éconst = 1; }\n\
                    fn bare<T: Copy + const SimdExt, F: const FnOnce()>() {}\n";
        assert!(matches!(blank_const_trait_keywords(text), Cow::Borrowed(_)));
    }

    #[test]
    fn test_only_the_form_bytes_change_around_multibyte_text() {
        let text = "/// Café.\nconst impl Default for Café {}\n";
        let blanked = blank_const_trait_keywords(text);
        assert_eq!(blanked, "/// Café.\n      impl Default for Café {}\n");
    }

    /// Files a walk of the installed `rust-src` reads at most: the 1.98 component
    /// ships about 2,900 `.rs` files under `library`.
    const RUST_SRC_FILES_MAX: usize = 20_000;

    /// The `library` folder of the active toolchain's `rust-src` component, when the
    /// toolchain carries it. `RUSTUP_AUTO_INSTALL=0` keeps rustup from installing a
    /// toolchain the workspace pins but the machine lacks.
    fn installed_rust_src() -> Option<std::path::PathBuf> {
        let output = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .ok()?;
        let sysroot = String::from_utf8(output.stdout).ok()?;
        let library = std::path::Path::new(sysroot.trim()).join("lib/rustlib/src/rust/library");
        library.is_dir().then_some(library)
    }

    /// Every `.rs` file below `root`, at most `RUST_SRC_FILES_MAX` of them.
    fn rust_files(root: std::path::PathBuf) -> Vec<std::path::PathBuf> {
        let mut pending = vec![root];
        let mut files = Vec::new();
        while let Some(directory) = pending.pop() {
            let entries = std::fs::read_dir(&directory).expect("a rust-src folder lists");
            for entry in entries {
                let path = entry.expect("a rust-src entry reads").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    files.push(path);
                }
            }
            assert!(
                files.len() <= RUST_SRC_FILES_MAX,
                "the rust-src walk stays within its bound: files={}, files_max={RUST_SRC_FILES_MAX}",
                files.len()
            );
        }
        files
    }

    /// The standard library spells the const-trait forms, so its own sources are the
    /// text the blanking must hold byte offsets over. A toolchain without the
    /// `rust-src` component carries none of it, and the test has nothing to read.
    #[test]
    fn test_every_installed_rust_src_file_keeps_its_byte_length() {
        let Some(library) = installed_rust_src() else {
            eprintln!("the active toolchain carries no rust-src component");
            return;
        };
        let files = rust_files(library);
        let mut blanked_files = 0_usize;
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let blanked = blank_const_trait_keywords(&text);
            assert_eq!(blanked.len(), text.len(), "{}", path.display());
            if matches!(blanked, Cow::Owned(_)) {
                blanked_files += 1;
            }
        }
        assert!(!files.is_empty(), "rust-src holds Rust files");
        assert!(
            blanked_files > 0,
            "the standard library spells at least one const-trait form"
        );
    }
}
