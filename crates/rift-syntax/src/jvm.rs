//! Java and Kotlin declarations and source occurrences.

mod declarations;
mod grammar;
mod occurrences;

use rift_error::RiftError;
use rift_protocol::read::{Language, SyntaxKind};
use tree_sitter::Language as Grammar;

use crate::{
    LanguageDefinition, ShippedLanguage, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

use declarations::{DeclarationKind, Rules};
use grammar::{Kinds, NodeKind};

pub use occurrences::{JvmOccurrence, JvmOccurrenceKind, JvmOccurrences};

/// A Java or Kotlin source grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JvmLanguage {
    /// Java sources.
    Java,
    /// Kotlin sources and scripts.
    Kotlin,
}

impl JvmLanguage {
    /// The grammar that claims `extension`, without its leading dot.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        [Self::Java, Self::Kotlin]
            .into_iter()
            .find(|language| language.matches_extension(extension))
    }

    /// The grammar that claims a parser filename, including either path separator.
    #[must_use]
    pub fn from_filename(filename: &str) -> Option<Self> {
        let filename = filename.rsplit(['/', '\\']).next()?;
        let extension = std::path::Path::new(filename).extension()?.to_str()?;
        Self::from_extension(extension)
    }

    pub(crate) fn grammar(self) -> &'static Grammar {
        Kinds::for_language(self).grammar()
    }

    /// Collects exact source positions for one category of authored occurrences.
    ///
    /// # Errors
    ///
    /// Returns a syntax refusal when the source cannot be parsed within the syntax limits.
    pub fn occurrences(
        self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
        kind: JvmOccurrenceKind,
        maximum: usize,
    ) -> Result<JvmOccurrences, RiftError> {
        occurrences::collect(&Rules::new(self), source, limits, kind, maximum)
    }
}

impl LanguageDefinition for JvmLanguage {
    fn shipped(&self) -> ShippedLanguage {
        match self {
            Self::Java => ShippedLanguage::Java,
            Self::Kotlin => ShippedLanguage::Kotlin,
        }
    }

    fn extensions(&self) -> &'static [&'static str] {
        match self {
            Self::Java => &["java"],
            Self::Kotlin => &["kt", "kts"],
        }
    }

    fn syntax_provider(&self) -> Box<dyn SyntaxProvider> {
        Box::new(JvmSyntaxProvider {
            jvm: *self,
            language: self.shipped().language(),
        })
    }
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    DeclarationKind::restore(name)
}

#[derive(Debug)]
struct JvmSyntaxProvider {
    jvm: JvmLanguage,
    language: Language,
}

impl SyntaxProvider for JvmSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        crate::parse::document(
            source,
            limits,
            &self.language,
            self.jvm.grammar(),
            &Rules::new(self.jvm),
            &[],
        )
    }

    fn syntax_element_kinds(&self, kind: &str) -> Vec<SyntaxKind> {
        use NodeKind as Node;
        use SyntaxKind as Syntax;
        match Kinds::for_language(self.jvm).classify(kind) {
            Some(
                Node::Package
                | Node::Class
                | Node::Interface
                | Node::AnnotationType
                | Node::Record
                | Node::Enum
                | Node::EnumConstant
                | Node::Module
                | Node::Method
                | Node::Function
                | Node::Constructor
                | Node::PrimaryConstructor
                | Node::Object
                | Node::Companion
                | Node::TypeAlias
                | Node::Field
                | Node::Property
                | Node::Variable
                | Node::TypeParameter,
            ) => {
                vec![Syntax::Declaration]
            }
            Some(Node::Parameter | Node::ClassParameter | Node::SpreadParameter) => {
                vec![Syntax::Declaration, Syntax::Parameter]
            }
            Some(Node::ClassBody | Node::EnumBody | Node::Body) => vec![Syntax::Body],
            Some(Node::Block) => vec![Syntax::Block],
            Some(Node::Import) => vec![Syntax::Import],
            Some(Node::Type | Node::GenericType) => vec![Syntax::TypeExpression],
            Some(Node::Identifier | Node::QualifiedName) => vec![Syntax::Identifier],
            Some(Node::TypeIdentifier) => vec![Syntax::TypeExpression, Syntax::Identifier],
            Some(Node::BlockComment | Node::LineComment) => vec![Syntax::Comment],
            Some(Node::Annotation) => vec![Syntax::Annotation],
            Some(Node::Arguments) => vec![Syntax::Argument],
            Some(Node::PatternVariable) => vec![Syntax::Pattern],
            Some(Node::Call | Node::ConstructorCall | Node::MemberAccess | Node::InstanceOf) => {
                vec![Syntax::Expression]
            }
            _ => Vec::new(),
        }
    }
}
