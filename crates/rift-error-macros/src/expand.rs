//! Token walk that replaces `[< .. >]` groups with joined identifiers.

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

use crate::name::{Segment, join};

/// A malformed `[< .. >]` group and the span the diagnostic points at.
pub(crate) struct Failure {
    span: Span,
    message: String,
}

impl Failure {
    fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }

    /// Renders the failure as `::core::compile_error!` at the failing span.
    pub(crate) fn into_compile_error(self) -> TokenStream {
        let span = self.span;
        let punct = |character, spacing| {
            let mut punct = Punct::new(character, spacing);
            punct.set_span(span);
            TokenTree::Punct(punct)
        };
        let mut message = Literal::string(&self.message);
        message.set_span(span);
        let mut arguments = Group::new(
            Delimiter::Brace,
            TokenStream::from(TokenTree::Literal(message)),
        );
        arguments.set_span(span);
        TokenStream::from_iter([
            punct(':', Spacing::Joint),
            punct(':', Spacing::Alone),
            TokenTree::Ident(Ident::new("core", span)),
            punct(':', Spacing::Joint),
            punct(':', Spacing::Alone),
            TokenTree::Ident(Ident::new("compile_error", span)),
            punct('!', Spacing::Alone),
            TokenTree::Group(arguments),
        ])
    }
}

/// Copies `input`, replacing each `[< .. >]` group, at any depth, with one identifier.
pub(crate) fn expand(input: TokenStream) -> Result<TokenStream, Failure> {
    let mut output = TokenStream::new();
    for tree in input {
        let replaced = match tree {
            TokenTree::Group(group) => {
                if let Some(ident) = joined(&group)? {
                    TokenTree::Ident(ident)
                } else {
                    let mut nested = Group::new(group.delimiter(), expand(group.stream())?);
                    nested.set_span(group.span());
                    TokenTree::Group(nested)
                }
            }
            other => other,
        };
        output.extend([replaced]);
    }
    Ok(output)
}

/// Returns the identifier a `[< .. >]` group names, or `None` for any other group.
///
/// The identifier carries the bracket group's span, so it resolves with the hygiene of
/// the macro that wrote the group.
fn joined(group: &Group) -> Result<Option<Ident>, Failure> {
    if group.delimiter() != Delimiter::Bracket {
        return Ok(None);
    }
    let tokens = flatten(group.stream());
    let Some((TokenTree::Punct(open), rest)) = tokens.split_first() else {
        return Ok(None);
    };
    if open.as_char() != '<' {
        return Ok(None);
    }
    let Some((TokenTree::Punct(close), inner)) = rest.split_last() else {
        return Err(Failure::new(group.span(), "expected `>` before `]`"));
    };
    if close.as_char() != '>' {
        return Err(Failure::new(close.span(), "expected `>` before `]`"));
    }
    let names = names(inner)?;
    let segments: Vec<Segment<'_>> = names
        .iter()
        .map(|(text, camel)| Segment::new(text, *camel))
        .collect();
    let joined = join(&segments).map_err(|message| Failure::new(group.span(), message))?;
    Ok(Some(Ident::new(&joined, group.span())))
}

/// Reads `ident` and `ident:camel` segments; any other token is a failure.
fn names(tokens: &[TokenTree]) -> Result<Vec<(String, bool)>, Failure> {
    let mut names = Vec::with_capacity(tokens.len());
    let mut tokens = tokens.iter().peekable();
    while let Some(tree) = tokens.next() {
        let TokenTree::Ident(ident) = tree else {
            return Err(Failure::new(
                tree.span(),
                "expected an identifier segment inside `[< >]`",
            ));
        };
        let colon = tokens
            .next_if(|next| matches!(next, TokenTree::Punct(punct) if punct.as_char() == ':'));
        let camel = match colon {
            None => false,
            Some(colon) => match tokens.next() {
                Some(TokenTree::Ident(modifier)) if modifier.to_string() == "camel" => true,
                Some(other) => {
                    return Err(Failure::new(
                        other.span(),
                        "unknown segment modifier; expected `camel`",
                    ));
                }
                None => return Err(Failure::new(colon.span(), "expected `camel` after `:`")),
            },
        };
        names.push((ident.to_string(), camel));
    }
    Ok(names)
}

/// Unwraps invisible groups, which carry `macro_rules!` fragments other than `ident` and `tt`.
fn flatten(stream: TokenStream) -> Vec<TokenTree> {
    let mut tokens = Vec::new();
    for tree in stream {
        match tree {
            TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
                tokens.extend(flatten(group.stream()));
            }
            other => tokens.push(other),
        }
    }
    tokens
}
