//! The corpus tokenizer's split, for text read outside `SQLite`.
//!
//! [`CORPUS_TOKENIZER`](crate::CORPUS_TOKENIZER) folds text to lowercase and splits it on
//! every non-alphanumeric character. Diacritics are kept, which is why the corpus declares
//! `remove_diacritics 0` rather than the tokenizer default: [`tokenize`] then splits text
//! the same way without carrying a Unicode folding table of its own.

/// Splits text the way the corpus tokenizer does.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn test_tokenizing_folds_case_and_splits_on_non_alphanumerics() {
        assert_eq!(tokenize("read::SearchHit"), ["read", "searchhit"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
        assert_eq!(tokenize("caf\u{e9}"), ["caf\u{e9}"]);
    }
}
