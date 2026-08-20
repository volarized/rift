/// Returns whether value is one bounded canonical lowercase ASCII name.
///
/// First byte must be lowercase ASCII. Remaining bytes may also be digits or
/// punctuation explicitly owned by caller's domain.
#[must_use]
pub fn is_canonical_ascii_name(value: &str, bytes_max: usize, punctuation: &[u8]) -> bool {
    is_within_bound(value, bytes_max)
        && starts_with_lowercase(value)
        && tail_uses_owned_alphabet(value, punctuation)
}

fn is_within_bound(value: &str, bytes_max: usize) -> bool {
    !value.is_empty() && value.len() <= bytes_max
}

fn starts_with_lowercase(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}

fn tail_uses_owned_alphabet(value: &str, punctuation: &[u8]) -> bool {
    value
        .bytes()
        .skip(1)
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || punctuation.contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::is_canonical_ascii_name;

    #[test]
    fn test_canonical_ascii_name_enforces_bound_and_owned_punctuation() {
        assert!(is_canonical_ascii_name("rift-core", 9, b"-"));
        assert!(!is_canonical_ascii_name("", 9, b"-"));
        assert!(!is_canonical_ascii_name("Rift", 9, b"-"));
        assert!(!is_canonical_ascii_name("rift_core", 9, b"-"));
        assert!(!is_canonical_ascii_name("rift-core", 8, b"-"));
    }
}
