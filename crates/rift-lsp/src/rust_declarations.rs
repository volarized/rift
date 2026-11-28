//! Converts a negotiated Rust semantic-token answer into physical declaration ranges.

use lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensClientCapabilities,
    SemanticTokensClientCapabilitiesRequests, SemanticTokensFullOptions, SemanticTokensLegend,
    TokenFormat,
};
use rift_core::{FileDigest, ProjectPath, SourceRange};

use crate::capabilities::PositionEncoding;
use crate::position::LineIndex;

const MACRO: SemanticTokenModifier = SemanticTokenModifier::new("macro");
const ATTRIBUTE: SemanticTokenModifier = SemanticTokenModifier::new("attribute");
const TOKENS_MAX: usize = 262_144;

/// Which source-local declaration kind Rust reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustDeclarationKind {
    /// A local variable declared by an authored pattern.
    Local,
    /// A formal parameter.
    Parameter,
}

/// One authored source-local declaration token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustDeclarationToken {
    /// Physical UTF-8 byte range.
    pub range: SourceRange,
    /// Kind granted by the engine.
    pub kind: RustDeclarationKind,
}

/// One macro-tagged declaration token, retained separately from ordinary Rust bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustMacroDeclarationToken {
    /// Physical UTF-8 byte range in the opened Rust unit.
    pub range: SourceRange,
    /// Whether the server identified a function name rather than a parameter.
    pub function: bool,
}

/// Declaration observations for the exact source opened in one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustDeclarationTokens {
    path: ProjectPath,
    digest: FileDigest,
    tokens: Vec<RustDeclarationToken>,
    macro_tokens: Vec<RustMacroDeclarationToken>,
}

impl RustDeclarationTokens {
    /// The project-relative path used in the request.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// Digest of the opened bytes used for range conversion.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Observations in source order.
    #[must_use]
    pub fn tokens(&self) -> &[RustDeclarationToken] {
        &self.tokens
    }

    /// Macro-tagged functions and parameters in source order, pending separate syntax proof.
    #[must_use]
    pub fn macro_tokens(&self) -> &[RustMacroDeclarationToken] {
        &self.macro_tokens
    }
}

pub(crate) fn offered() -> SemanticTokensClientCapabilities {
    SemanticTokensClientCapabilities {
        requests: SemanticTokensClientCapabilitiesRequests {
            full: Some(SemanticTokensFullOptions::Bool(true)),
            ..SemanticTokensClientCapabilitiesRequests::default()
        },
        token_types: vec![
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::FUNCTION,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            MACRO,
            ATTRIBUTE,
        ],
        formats: vec![TokenFormat::RELATIVE],
        overlapping_token_support: Some(false),
        multiline_token_support: Some(false),
        ..SemanticTokensClientCapabilities::default()
    }
}

#[derive(Debug)]
pub(crate) struct RustTokenLegend<'legend> {
    types: &'legend [SemanticTokenType],
    declarations: u32,
    macro_expansion: u32,
    attribute: u32,
    documentation: u32,
    all_modifiers: u32,
}

impl<'legend> RustTokenLegend<'legend> {
    pub(crate) fn new(legend: &'legend SemanticTokensLegend) -> Option<Self> {
        let bits = u32::try_from(legend.token_modifiers.len()).ok()?;
        let all_modifiers = u32::MAX.checked_shr(u32::BITS.checked_sub(bits)?)?;
        let mut declarations = 0;
        let mut macro_expansion = 0;
        let mut attribute = 0;
        let mut documentation = 0;
        for (index, modifier) in legend.token_modifiers.iter().enumerate() {
            let bit = 1_u32.checked_shl(u32::try_from(index).ok()?)?;
            if *modifier == MACRO {
                macro_expansion |= bit;
            } else if *modifier == ATTRIBUTE {
                attribute |= bit;
            } else if *modifier == SemanticTokenModifier::DOCUMENTATION {
                documentation |= bit;
            } else if *modifier == SemanticTokenModifier::DECLARATION
                || *modifier == SemanticTokenModifier::DEFINITION
            {
                declarations |= bit;
            }
        }
        (declarations != 0 && macro_expansion != 0).then_some(Self {
            types: &legend.token_types,
            declarations,
            macro_expansion,
            attribute,
            documentation,
            all_modifiers,
        })
    }

    fn line_end_placeholder(
        &self,
        token_type: &SemanticTokenType,
        modifiers: u32,
    ) -> Option<LineEndPlaceholder> {
        match (token_type.as_str(), modifiers) {
            ("string" | "comment", 0) => Some(LineEndPlaceholder::BlankLine),
            ("string", mask)
                if mask.is_power_of_two()
                    && (mask == self.macro_expansion || mask == self.attribute) =>
            {
                Some(LineEndPlaceholder::TaggedLineEnd)
            }
            ("comment", mask) if mask.is_power_of_two() && mask == self.documentation => {
                Some(LineEndPlaceholder::TaggedLineEnd)
            }
            _ => None,
        }
    }

    pub(crate) fn convert(
        &self,
        path: ProjectPath,
        digest: FileDigest,
        source: &str,
        encoding: PositionEncoding,
        data: &[SemanticToken],
    ) -> Option<RustDeclarationTokens> {
        if data.len() > TOKENS_MAX {
            return None;
        }
        let index = LineIndex::new(source);
        let mut lines = TokenLines::default();
        let mut tokens = Vec::new();
        let mut macro_tokens = Vec::new();
        for token in data {
            let token_type = self.types.get(usize::try_from(token.token_type).ok()?)?;
            let modifiers = token.token_modifiers_bitset;
            if modifiers & !self.all_modifiers != 0 {
                return None;
            }
            let placeholder = if token.length == 0 {
                self.line_end_placeholder(token_type, modifiers)
            } else {
                None
            };
            let TokenSpan::Range(range) = lines.advance(&index, encoding, token, placeholder)?
            else {
                continue;
            };
            if modifiers & self.declarations == 0 {
                continue;
            }
            if modifiers & self.macro_expansion != 0 {
                if modifiers & self.attribute == 0 {
                    let function = token_type == &SemanticTokenType::FUNCTION;
                    if function || token_type == &SemanticTokenType::PARAMETER {
                        macro_tokens.push(RustMacroDeclarationToken { range, function });
                    }
                }
                continue;
            }
            let kind = match token_type.as_str() {
                "variable" => RustDeclarationKind::Local,
                "parameter" => RustDeclarationKind::Parameter,
                _ => continue,
            };
            tokens.push(RustDeclarationToken { range, kind });
        }
        Some(RustDeclarationTokens {
            path,
            digest,
            tokens,
            macro_tokens,
        })
    }
}

enum TokenSpan {
    Range(SourceRange),
    Placeholder,
}

enum LineEndPlaceholder {
    BlankLine,
    TaggedLineEnd,
}

#[derive(Default)]
struct TokenLines {
    line: u32,
    start_character: u32,
    end_character: u32,
    end_byte: usize,
}

impl TokenLines {
    fn advance(
        &mut self,
        index: &LineIndex<'_>,
        encoding: PositionEncoding,
        token: &SemanticToken,
        placeholder: Option<LineEndPlaceholder>,
    ) -> Option<TokenSpan> {
        // Token starts are delta-encoded from the previous start, while the index
        // converts from the previous end so every source character is scanned once.
        let column = if token.delta_line == 0 {
            self.start_character.checked_add(token.delta_start)?
        } else {
            self.line = self.line.checked_add(token.delta_line)?;
            self.end_byte = index.line_start(self.line).ok()?;
            self.end_character = 0;
            token.delta_start
        };
        if let Some(placeholder) = placeholder {
            // Empty line fragments retain their exact position in the token deltas.
            if matches!(placeholder, LineEndPlaceholder::BlankLine) && column != 0 {
                return None;
            }
            self.end_byte = index.ordered_line_end(
                encoding,
                self.line,
                (self.end_character, self.end_byte),
                column,
            )?;
            self.start_character = column;
            self.end_character = column;
            return Some(TokenSpan::Placeholder);
        }
        let end = column.checked_add(token.length)?;
        let (start_byte, end_byte) = index.ordered_range(
            encoding,
            self.line,
            (self.end_character, self.end_byte),
            (column, end),
        )?;
        self.start_character = column;
        self.end_character = end;
        self.end_byte = end_byte;
        SourceRange::new(
            u64::try_from(start_byte).ok()?,
            u64::try_from(end_byte).ok()?,
        )
        .ok()
        .map(TokenSpan::Range)
    }
}
