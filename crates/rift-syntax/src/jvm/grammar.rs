//! Named syntax elements, fields, and tokens from the pinned JVM grammars.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use strum::{IntoStaticStr, VariantArray};
use tree_sitter::{Language, Node};

use super::JvmLanguage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeKind {
    Root,
    Package,
    Import,
    QualifiedName,
    Identifier,
    TypeIdentifier,
    BlockComment,
    LineComment,
    Modifiers,
    Modifier,
    Annotation,
    Class,
    Interface,
    AnnotationType,
    Record,
    Enum,
    EnumConstant,
    Module,
    ClassBody,
    EnumBody,
    Method,
    Function,
    Constructor,
    PrimaryConstructor,
    Object,
    Companion,
    AnonymousClass,
    TypeAlias,
    Field,
    Variable,
    Property,
    Accessor,
    Catch,
    VariableList,
    Parameter,
    ClassParameter,
    SpreadParameter,
    Lambda,
    LambdaParameters,
    LocalBinding,
    TypeParameter,
    InstanceOf,
    PatternVariable,
    Parameters,
    Type,
    GenericType,
    Body,
    Block,
    Call,
    ConstructorCall,
    ConstructorDelegation,
    MemberAccess,
    Arguments,
}

const COMMON: &[(NodeKind, &str)] = &[
    (NodeKind::Identifier, "identifier"),
    (NodeKind::BlockComment, "block_comment"),
    (NodeKind::LineComment, "line_comment"),
    (NodeKind::Modifiers, "modifiers"),
    (NodeKind::Annotation, "annotation"),
    (NodeKind::Class, "class_declaration"),
    (NodeKind::ClassBody, "class_body"),
    (NodeKind::TypeParameter, "type_parameter"),
    (NodeKind::Block, "block"),
];

const JAVA: &[(NodeKind, &str)] = &[
    (NodeKind::Root, "program"),
    (NodeKind::Package, "package_declaration"),
    (NodeKind::Import, "import_declaration"),
    (NodeKind::QualifiedName, "scoped_identifier"),
    (NodeKind::TypeIdentifier, "type_identifier"),
    (NodeKind::Annotation, "marker_annotation"),
    (NodeKind::Interface, "interface_declaration"),
    (NodeKind::AnnotationType, "annotation_type_declaration"),
    (NodeKind::Record, "record_declaration"),
    (NodeKind::Enum, "enum_declaration"),
    (NodeKind::EnumConstant, "enum_constant"),
    (NodeKind::Module, "module_declaration"),
    (NodeKind::ClassBody, "interface_body"),
    (NodeKind::ClassBody, "annotation_type_body"),
    (NodeKind::EnumBody, "enum_body"),
    (NodeKind::ClassBody, "enum_body_declarations"),
    (NodeKind::Method, "method_declaration"),
    (NodeKind::Method, "annotation_type_element_declaration"),
    (NodeKind::Constructor, "constructor_declaration"),
    (NodeKind::Constructor, "compact_constructor_declaration"),
    (NodeKind::VariableList, "local_variable_declaration"),
    (NodeKind::Field, "field_declaration"),
    (NodeKind::Field, "constant_declaration"),
    (NodeKind::Variable, "variable_declarator"),
    (NodeKind::Parameter, "formal_parameter"),
    (NodeKind::Parameter, "catch_formal_parameter"),
    (NodeKind::SpreadParameter, "spread_parameter"),
    (NodeKind::Lambda, "lambda_expression"),
    (NodeKind::LambdaParameters, "inferred_parameters"),
    (NodeKind::LocalBinding, "enhanced_for_statement"),
    (NodeKind::LocalBinding, "resource"),
    (NodeKind::Parameters, "formal_parameters"),
    (NodeKind::InstanceOf, "instanceof_expression"),
    (NodeKind::PatternVariable, "type_pattern"),
    (NodeKind::PatternVariable, "record_pattern_component"),
    (NodeKind::Type, "scoped_type_identifier"),
    (NodeKind::GenericType, "generic_type"),
    (NodeKind::Body, "constructor_body"),
    (NodeKind::Call, "method_invocation"),
    (NodeKind::ConstructorCall, "object_creation_expression"),
    (
        NodeKind::ConstructorDelegation,
        "explicit_constructor_invocation",
    ),
    (NodeKind::MemberAccess, "field_access"),
    (NodeKind::Arguments, "argument_list"),
];

const KOTLIN: &[(NodeKind, &str)] = &[
    (NodeKind::Root, "source_file"),
    (NodeKind::Package, "package_header"),
    (NodeKind::Import, "import"),
    (NodeKind::QualifiedName, "qualified_identifier"),
    (NodeKind::Modifier, "visibility_modifier"),
    (NodeKind::Modifier, "inheritance_modifier"),
    (NodeKind::Modifier, "class_modifier"),
    (NodeKind::EnumBody, "enum_class_body"),
    (NodeKind::EnumConstant, "enum_entry"),
    (NodeKind::Function, "function_declaration"),
    (NodeKind::Constructor, "secondary_constructor"),
    (NodeKind::PrimaryConstructor, "primary_constructor"),
    (NodeKind::Object, "object_declaration"),
    (NodeKind::Companion, "companion_object"),
    (NodeKind::AnonymousClass, "object_literal"),
    (NodeKind::TypeAlias, "type_alias"),
    (NodeKind::Property, "property_declaration"),
    (NodeKind::Accessor, "getter"),
    (NodeKind::Accessor, "setter"),
    (NodeKind::Catch, "catch_block"),
    (NodeKind::VariableList, "multi_variable_declaration"),
    (NodeKind::Variable, "variable_declaration"),
    (NodeKind::Parameter, "parameter"),
    (NodeKind::ClassParameter, "class_parameter"),
    (NodeKind::Parameters, "class_parameters"),
    (NodeKind::Parameters, "function_value_parameters"),
    (NodeKind::Type, "user_type"),
    (NodeKind::Body, "function_body"),
    (NodeKind::Call, "call_expression"),
    (NodeKind::ConstructorCall, "constructor_invocation"),
    (
        NodeKind::ConstructorDelegation,
        "constructor_delegation_call",
    ),
    (NodeKind::MemberAccess, "navigation_expression"),
    (NodeKind::Arguments, "value_arguments"),
];

#[derive(Clone, Copy, Debug, IntoStaticStr, PartialEq, Eq, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub(super) enum Keyword {
    Abstract,
    Annotation,
    Companion,
    Constructor,
    Enum,
    Final,
    Get,
    Internal,
    Interface,
    Object,
    Open,
    Private,
    Protected,
    Public,
    Sealed,
    Set,
    Static,
    Super,
    This,
    Val,
    Var,
}

impl Keyword {
    fn available_in(self, language: JvmLanguage) -> bool {
        match self {
            Self::Static => language == JvmLanguage::Java,
            Self::Annotation
            | Self::Companion
            | Self::Constructor
            | Self::Get
            | Self::Internal
            | Self::Object
            | Self::Open
            | Self::Set
            | Self::Val
            | Self::Var => language == JvmLanguage::Kotlin,
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum Field {
    Body,
    Constructor,
    #[strum(serialize = "field")]
    Member,
    Name,
    Parameters,
    Type,
}

struct Fields {
    body: Option<NonZeroU16>,
    constructor: Option<NonZeroU16>,
    field: Option<NonZeroU16>,
    name: NonZeroU16,
    parameters: Option<NonZeroU16>,
    type_field: NonZeroU16,
}

impl Fields {
    fn resolve(grammar: &Language, language: JvmLanguage) -> Self {
        let field = |name: Field| grammar.field_id_for_name(<&str>::from(name));
        let required = |name: Field| {
            field(name).unwrap_or_else(|| panic!("the {language:?} grammar defines field {name:?}"))
        };
        let java_field = |name| (language == JvmLanguage::Java).then(|| required(name));
        Self {
            body: java_field(Field::Body),
            constructor: java_field(Field::Constructor),
            field: java_field(Field::Member),
            name: required(Field::Name),
            parameters: java_field(Field::Parameters),
            type_field: required(Field::Type),
        }
    }
}

pub(super) struct Kinds {
    grammar: Language,
    nodes: Vec<Option<NodeKind>>,
    keywords: Vec<Option<Keyword>>,
    fields: Fields,
}

impl Kinds {
    fn resolve(language: JvmLanguage) -> Self {
        let (grammar, named): (Language, &[(NodeKind, &str)]) = match language {
            JvmLanguage::Java => (tree_sitter_java::LANGUAGE.into(), JAVA),
            JvmLanguage::Kotlin => (tree_sitter_kotlin_ng::LANGUAGE.into(), KOTLIN),
        };
        let mut nodes = vec![None; grammar.node_kind_count()];
        for &(kind, name) in COMMON.iter().chain(named) {
            let id = crate::parse::kind(&grammar, name);
            assert!(
                nodes[usize::from(id)].replace(kind).is_none(),
                "each {language:?} grammar kind has one classification: kind={name}",
            );
        }
        let mut keywords = vec![None; grammar.node_kind_count()];
        for &keyword in Keyword::VARIANTS {
            if keyword.available_in(language) {
                let name = <&str>::from(keyword);
                let id = grammar.id_for_node_kind(
                    name,
                    language == JvmLanguage::Java
                        && matches!(keyword, Keyword::This | Keyword::Super),
                );
                assert_ne!(id, 0, "the {language:?} grammar defines keyword {name}");
                keywords[usize::from(id)] = Some(keyword);
            }
        }
        Self {
            fields: Fields::resolve(&grammar, language),
            grammar,
            nodes,
            keywords,
        }
    }

    pub(super) fn for_language(language: JvmLanguage) -> &'static Self {
        static JAVA: OnceLock<Kinds> = OnceLock::new();
        static KOTLIN: OnceLock<Kinds> = OnceLock::new();
        match language {
            JvmLanguage::Java => JAVA.get_or_init(|| Self::resolve(language)),
            JvmLanguage::Kotlin => KOTLIN.get_or_init(|| Self::resolve(language)),
        }
    }

    pub(super) const fn grammar(&self) -> &Language {
        &self.grammar
    }

    pub(super) fn classify(&self, name: &str) -> Option<NodeKind> {
        self.kind_at(self.grammar.id_for_node_kind(name, true))
    }

    pub(super) fn kind(&self, node: Node<'_>) -> Option<NodeKind> {
        self.kind_at(node.kind_id())
    }

    fn kind_at(&self, id: u16) -> Option<NodeKind> {
        self.nodes.get(usize::from(id)).copied().flatten()
    }

    pub(super) fn keyword(&self, node: Node<'_>) -> Option<Keyword> {
        self.keywords
            .get(usize::from(node.kind_id()))
            .copied()
            .flatten()
    }

    pub(super) fn field<'tree>(&self, node: Node<'tree>, field: Field) -> Option<Node<'tree>> {
        let id = match field {
            Field::Body => self.fields.body?,
            Field::Constructor => self.fields.constructor?,
            Field::Member => self.fields.field?,
            Field::Name => self.fields.name,
            Field::Parameters => self.fields.parameters?,
            Field::Type => self.fields.type_field,
        };
        node.child_by_field_id(id.get())
            .filter(|child| !child.is_missing())
    }

    pub(super) fn child<'tree>(&self, node: Node<'tree>, kind: NodeKind) -> Option<Node<'tree>> {
        node.named_children(&mut node.walk())
            .find(|child| self.kind(*child) == Some(kind) && !child.is_missing())
    }

    pub(super) fn token<'tree>(&self, node: Node<'tree>, keyword: Keyword) -> Option<Node<'tree>> {
        node.children(&mut node.walk())
            .find(|child| self.keyword(*child) == Some(keyword) && !child.is_missing())
    }
}
