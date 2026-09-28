//! The corpus tokenizer's split, for text read outside `SQLite`.
//!
//! [`CORPUS_TOKENIZER`](crate::CORPUS_TOKENIZER) folds text to lowercase and splits it on
//! every non-alphanumeric character. Diacritics are kept, which is why the corpus declares
//! `remove_diacritics 0` rather than the tokenizer default: [`tokenize`] then splits text
//! the same way without carrying a Unicode folding table of its own.

/// Splits text the way the corpus tokenizer does.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    token_spans(text)
        .into_iter()
        .map(|(_, token)| token.to_lowercase())
        .collect()
}

/// Each token the corpus tokenizer finds in `text`, as written, with the byte offset it
/// starts at: every maximal run of alphanumeric characters, in text order.
///
/// [`tokenize`] and the body-match term finder both split through here, so the offsets
/// a body match reports sit on exactly the tokens the stored corpus indexed.
pub(crate) fn token_spans(text: &str) -> Vec<(usize, &str)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        match (character.is_alphanumeric(), start) {
            (true, None) => start = Some(offset),
            (false, Some(begin)) => {
                spans.push((begin, &text[begin..offset]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        spans.push((begin, &text[begin..]));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::{token_spans, tokenize};

    #[test]
    fn test_tokenizing_folds_case_and_splits_on_non_alphanumerics() {
        assert_eq!(tokenize("read::SearchHit"), ["read", "searchhit"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
        assert_eq!(tokenize("caf\u{e9}"), ["caf\u{e9}"]);
    }

    #[test]
    fn test_token_spans_start_where_each_token_sits_in_the_text() {
        let text = "gr\u{fc}\u{df}e \u{65e5}\u{672c}\u{8a9e}::Beta_2\r\n";
        let spans = token_spans(text);
        assert_eq!(
            spans,
            [
                (0, "gr\u{fc}\u{df}e"),
                (8, "\u{65e5}\u{672c}\u{8a9e}"),
                (19, "Beta"),
                (24, "2"),
            ]
        );
        for (offset, token) in &spans {
            assert_eq!(&text[*offset..*offset + token.len()], *token);
        }
        assert!(token_spans("").is_empty());
        assert_eq!(token_spans("tail"), [(0, "tail")]);
    }
}
