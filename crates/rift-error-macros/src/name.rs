//! Identifier text joined from `[< .. >]` segments.

/// One segment of a `[< .. >]` group: identifier text and its case modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment<'a> {
    text: &'a str,
    camel: bool,
}

impl<'a> Segment<'a> {
    /// Takes identifier text as written; a raw identifier keeps its `r#` prefix here.
    pub(crate) fn new(text: &'a str, camel: bool) -> Self {
        Self { text, camel }
    }

    fn written(self) -> String {
        let text = self.text.strip_prefix("r#").unwrap_or(self.text);
        if self.camel {
            camel(text)
        } else {
            text.to_owned()
        }
    }
}

/// Joins segments into one identifier's text.
///
/// Returns the reason the joined text cannot name an identifier: no segments, an empty
/// result, or a leading character that is neither a letter nor `_`.
pub(crate) fn join(segments: &[Segment<'_>]) -> Result<String, &'static str> {
    if segments.is_empty() {
        return Err("`[< >]` holds no identifier segment");
    }
    let joined: String = segments.iter().map(|segment| segment.written()).collect();
    match joined.chars().next() {
        None => Err("joined identifier is empty"),
        Some(first) if first == '_' || first.is_alphabetic() => Ok(joined),
        Some(_) => Err("joined identifier must start with a letter or `_`"),
    }
}

/// Converts snake case to upper camel case: `token_expired` becomes `TokenExpired`.
///
/// Underscores are removed, the character after each underscore or at the start is
/// uppercased, and a character after an uppercase one is lowercased.
fn camel(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous = '_';
    for character in text.chars() {
        if character != '_' {
            if previous == '_' {
                output.extend(character.to_uppercase());
            } else if previous.is_uppercase() {
                output.extend(character.to_lowercase());
            } else {
                output.push(character);
            }
        }
        previous = character;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{Segment, camel, join};

    #[test]
    fn camel_converts_snake_case_field_names() {
        assert_eq!(camel("token"), "Token");
        assert_eq!(camel("token_expired"), "TokenExpired");
        assert_eq!(camel("query_v2_limit"), "QueryV2Limit");
        assert_eq!(camel("_leading"), "Leading");
        assert_eq!(camel("trailing_"), "Trailing");
        assert_eq!(camel("HTTP"), "Http");
    }

    #[test]
    fn join_builds_setter_and_state_names() {
        assert_eq!(
            join(&[Segment::new("maybe_", false), Segment::new("cause", false)]),
            Ok("maybe_cause".to_owned())
        );
        assert_eq!(
            join(&[
                Segment::new("waited_for", true),
                Segment::new("State", false)
            ]),
            Ok("WaitedForState".to_owned())
        );
    }

    #[test]
    fn join_strips_raw_identifier_prefixes() {
        assert_eq!(
            join(&[Segment::new("maybe_", false), Segment::new("r#type", false)]),
            Ok("maybe_type".to_owned())
        );
        assert_eq!(
            join(&[Segment::new("r#type", true), Segment::new("State", false)]),
            Ok("TypeState".to_owned())
        );
    }

    #[test]
    fn join_rejects_text_that_names_no_identifier() {
        assert_eq!(join(&[]), Err("`[< >]` holds no identifier segment"));
        assert_eq!(
            join(&[Segment::new("_", true)]),
            Err("joined identifier is empty")
        );
        assert_eq!(
            join(&[Segment::new("_1", true)]),
            Err("joined identifier must start with a letter or `_`")
        );
    }
}
