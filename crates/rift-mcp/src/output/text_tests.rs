use serde::ser::Error as _;

use super::{
    INDENT_UNIT, OUTPUT_TEXT_BYTES_MAX, OutputOverflow, TextError, TextWriter, quoted, visible,
};

/// A limit far above every fixture in these tests.
const LIMIT: usize = 1 << 20;

#[test]
fn the_indent_unit_is_one_tab() {
    assert_eq!(INDENT_UNIT, "\t");
}

#[test]
fn raw_lines_are_indented_by_levels_of_the_unit_and_end_with_a_line_feed() {
    let mut writer = TextWriter::new(LIMIT);
    writer.raw_line(0, "top").expect("writes");
    writer.raw_line(2, "deep").expect("writes");
    writer.blank_line().expect("writes");
    writer.raw_line(2, "").expect("writes");
    let unit = INDENT_UNIT;
    assert_eq!(
        writer.finish().expect("finishes"),
        format!("top\n{unit}{unit}deep\n\n\n")
    );
}

#[test]
fn raw_lines_write_one_line_per_segment_and_recover_the_text() {
    let text = "a\n\n  b\t\r\n\n";
    let mut writer = TextWriter::new(LIMIT);
    writer.raw_lines(1, text).expect("writes");
    let output = writer.finish().expect("finishes");
    assert_eq!(output, "\ta\n\n\t  b\t\r\n\n\n");
    let recovered: Vec<&str> = output
        .strip_suffix('\n')
        .expect("ends with a line feed")
        .split('\n')
        .map(|line| line.strip_prefix(INDENT_UNIT).unwrap_or(line))
        .collect();
    assert_eq!(recovered.join("\n"), text);
    let mut empty = TextWriter::new(LIMIT);
    empty.raw_lines(1, "").expect("writes");
    assert_eq!(empty.finish().expect("finishes"), "\n");
}

#[test]
fn raw_lines_obey_the_output_limit_exactly() {
    let line = "\t\tabc\n";
    let mut exact = TextWriter::new(line.len());
    exact.raw_line(2, "abc").expect("exact fit");
    assert_eq!(exact.finish().expect("finishes"), line);
    let mut tight = TextWriter::new(line.len() - 1);
    let failure = tight.raw_line(2, "abc").expect_err("one byte over");
    assert_eq!(
        failure,
        TextError::Overflow(OutputOverflow {
            limit: line.len() - 1
        })
    );
    assert_eq!(tight.finish(), Err(failure));
    let mut after = TextWriter::new(6);
    after.raw_line(0, "ab").expect("fits");
    let refused = after.raw_line(0, "abc").expect_err("over the limit");
    assert_eq!(after.blank_line(), Err(refused));
}

#[test]
fn every_limit_below_the_output_size_overflows_and_the_exact_size_fits() {
    let write = |writer: &mut TextWriter| {
        writer.raw_line(0, "head")?;
        writer.raw_lines(2, "a\n\nb")?;
        writer.blank_line()
    };
    let mut full = TextWriter::new(LIMIT);
    write(&mut full).expect("fixture renders");
    let expected = full.finish().expect("fixture finishes");
    let mut exact = TextWriter::new(expected.len());
    write(&mut exact).expect("exact size fits");
    assert_eq!(exact.finish().as_ref(), Ok(&expected));
    for limit in 0..expected.len() {
        let mut writer = TextWriter::new(limit);
        let overflow = TextError::Overflow(OutputOverflow { limit });
        assert_eq!(write(&mut writer), Err(overflow.clone()), "limit {limit}");
        assert_eq!(writer.finish(), Err(overflow), "no text at limit {limit}");
    }
}

#[test]
fn the_limit_counts_bytes_not_characters() {
    let mut exact = TextWriter::new("日本\n".len());
    exact.raw_line(0, "日本").expect("fits");
    assert_eq!(exact.finish().as_deref(), Ok("日本\n"));
    let mut over = TextWriter::new("日本\n".len() - 1);
    assert!(matches!(
        over.raw_line(0, "日本"),
        Err(TextError::Overflow(_))
    ));
}

#[test]
fn a_blank_line_costs_one_byte_under_the_limit() {
    let mut exact = TextWriter::new(1);
    exact.blank_line().expect("exact fit");
    assert_eq!(
        exact.blank_line(),
        Err(TextError::Overflow(OutputOverflow { limit: 1 }))
    );
}

#[test]
fn a_zero_limit_admits_only_an_empty_answer() {
    let writer = TextWriter::new(0);
    assert_eq!(writer.finish().as_deref(), Ok(""));
    let mut writer = TextWriter::new(0);
    assert!(writer.raw_line(0, "x").is_err());
}

#[test]
fn the_production_limit_is_sixteen_mebibytes() {
    assert_eq!(OUTPUT_TEXT_BYTES_MAX, 16 * 1024 * 1024);
}

#[test]
fn error_text_names_the_cause() {
    assert_eq!(
        TextError::Overflow(OutputOverflow { limit: 8 }).to_string(),
        "output exceeds the limit of 8 bytes"
    );
    assert_eq!(
        TextError::Unsupported("bytes").to_string(),
        "unsupported shape in text: bytes"
    );
    assert_eq!(TextError::Custom("boom".to_owned()).to_string(), "boom");
    assert_eq!(
        OutputOverflow { limit: 8 }.to_string(),
        "output exceeds the limit of 8 bytes"
    );
}

#[test]
fn a_serialize_failure_is_a_custom_error() {
    assert_eq!(
        TextError::custom("boom"),
        TextError::Custom("boom".to_owned())
    );
}

#[test]
fn visible_escapes_control_characters_and_keeps_everything_else() {
    assert_eq!(visible("plain \\ \" é"), "plain \\ \" é");
    assert_eq!(visible("a\nb\rc\td"), "a\\nb\\rc\\td");
    assert_eq!(
        visible("\u{0}\u{1b}\u{7f}\u{85}"),
        "\\u{0}\\u{1b}\\u{7f}\\u{85}"
    );
    assert!(matches!(visible("x"), std::borrow::Cow::Borrowed(_)));
    assert_eq!(visible(""), "");
}

/// The delimiters of a fact line: the fact separator and the detail separator.
const DELIMITERS: [&str; 2] = [" · ", ": "];

#[test]
fn quoted_keeps_a_value_without_a_delimiter_bare() {
    for bare in [
        "plain",
        "",
        "a·b",
        "a:b",
        "path:12",
        "back \\ slash \" quote",
        "a ·b",
        "a :b",
    ] {
        assert!(
            matches!(quoted(bare, &DELIMITERS), std::borrow::Cow::Borrowed(text) if text == bare),
            "{bare:?}"
        );
    }
}

#[test]
fn quoted_wraps_a_value_holding_a_delimiter_and_escapes_quote_and_backslash() {
    assert_eq!(quoted("a · b", &DELIMITERS), "\"a · b\"");
    assert_eq!(quoted("key: value", &DELIMITERS), "\"key: value\"");
    assert_eq!(quoted(" · ", &DELIMITERS), "\" · \"");
    assert_eq!(quoted(": ", &DELIMITERS), "\": \"");
    assert_eq!(
        quoted("say \"hi\": C:\\dir", &DELIMITERS),
        "\"say \\\"hi\\\": C:\\\\dir\""
    );
    assert_eq!(quoted("日本 · 語", &DELIMITERS), "\"日本 · 語\"");
}

#[test]
fn a_quoted_value_unescapes_to_the_original_text() {
    let unquote = |text: &str| -> String {
        let inner = text
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("quoted");
        let mut out = String::new();
        let mut escaped = false;
        for character in inner.chars() {
            if escaped || character != '\\' {
                out.push(character);
                escaped = false;
            } else {
                escaped = true;
            }
        }
        assert!(!escaped, "a lone backslash ends {text:?}");
        out
    };
    for original in ["a · b", "a: b", "\\ · \"", "x\\: \"\"", "\" · \\"] {
        let text = quoted(original, &DELIMITERS);
        assert_eq!(unquote(&text), original, "{text}");
    }
}

#[test]
fn quoted_with_no_delimiters_keeps_every_value_bare() {
    assert_eq!(quoted("a · b: c", &[]), "a · b: c");
}

#[test]
fn a_quoted_value_with_a_control_character_stays_on_one_line_after_visible() {
    let text = quoted("a · b\nc", &DELIMITERS);
    assert_eq!(visible(&text), "\"a · b\\nc\"");
}
