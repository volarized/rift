//! Declaration names, scopes, and documentation for Java and Kotlin.

use rift_error::RiftError;
use rift_protocol::read::{Documentation, DocumentationFormat, SymbolKind};
use strum::{IntoStaticStr, VariantArray};
use tree_sitter::Node;

use crate::ByteRange;
use crate::extract::{Declaration, GrammarRules, Visited, byte_range};

use super::JvmLanguage;
use super::grammar::{Field, Keyword, Kinds, NodeKind};

#[derive(Clone, Copy, Debug, IntoStaticStr, PartialEq, Eq, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub(super) enum DeclarationKind {
    Annotation,
    Class,
    Constructor,
    Enum,
    Field,
    Function,
    Interface,
    Method,
    Module,
    Object,
    Parameter,
    Property,
    Record,
    TypeAlias,
    TypeParameter,
    Variable,
    Variant,
}

impl DeclarationKind {
    pub(super) fn restore(name: &str) -> Option<&'static str> {
        Self::VARIANTS
            .iter()
            .copied()
            .map(<&str>::from)
            .find(|kind| *kind == name)
    }

    fn opens_scope(self) -> bool {
        matches!(
            self,
            Self::Annotation
                | Self::Class
                | Self::Enum
                | Self::Interface
                | Self::Module
                | Self::Object
                | Self::Record
                | Self::Variant
        ) || self.callable()
    }

    fn callable(self) -> bool {
        matches!(self, Self::Constructor | Self::Function | Self::Method)
    }

    fn has_api_visibility(self) -> bool {
        !matches!(self, Self::Parameter | Self::TypeParameter | Self::Variable)
    }

    fn kinds(self) -> Vec<SymbolKind> {
        use SymbolKind as Kind;
        match self {
            Self::Annotation => vec![Kind::Type, Kind::Annotation, Kind::MemberContainer],
            Self::Class | Self::Record => {
                vec![Kind::Type, Kind::Constructible, Kind::MemberContainer]
            }
            Self::Constructor => vec![Kind::Callable, Kind::Constructor],
            Self::Enum => vec![Kind::Type, Kind::Enumeration, Kind::MemberContainer],
            Self::Field | Self::Variable => vec![Kind::Value],
            Self::Function | Self::Method => vec![Kind::Callable],
            Self::Interface => vec![Kind::Type, Kind::Implementable, Kind::MemberContainer],
            Self::Module => vec![Kind::Module, Kind::Namespace],
            Self::Object => vec![Kind::Value, Kind::MemberContainer],
            Self::Parameter => vec![Kind::Value, Kind::Parameter],
            Self::Property => vec![Kind::Value, Kind::Property],
            Self::TypeAlias => vec![Kind::Type, Kind::Alias],
            Self::TypeParameter => vec![Kind::Type, Kind::TypeParameter],
            Self::Variant => vec![Kind::Value, Kind::Variant],
        }
    }
}

#[derive(Clone, Copy)]
enum Name<'tree> {
    Authored(Node<'tree>),
    Companion(Node<'tree>),
}

impl<'tree> Name<'tree> {
    fn node(self) -> Node<'tree> {
        match self {
            Self::Authored(node) | Self::Companion(node) => node,
        }
    }

    fn text(self, source: &str) -> Option<&str> {
        match self {
            Self::Authored(node) => {
                let text = source.get(node.byte_range())?;
                Some(
                    text.strip_prefix('`')
                        .and_then(|text| text.strip_suffix('`'))
                        .unwrap_or(text),
                )
            }
            Self::Companion(_) => Some("Companion"),
        }
    }
}

pub(super) struct Rules {
    language: JvmLanguage,
    pub(super) kinds: &'static Kinds,
}

impl Rules {
    pub(super) fn new(language: JvmLanguage) -> Self {
        Self {
            language,
            kinds: Kinds::for_language(language),
        }
    }

    fn name<'tree>(&self, node: Node<'tree>) -> Option<Name<'tree>> {
        use NodeKind as Kind;
        let kind = self.kinds.kind(node)?;
        let named = |field| self.kinds.field(node, field).map(Name::Authored);
        let identifier = |kind| self.kinds.child(node, kind).map(Name::Authored);
        match kind {
            Kind::Class
            | Kind::Interface
            | Kind::AnnotationType
            | Kind::Record
            | Kind::Enum
            | Kind::Module
            | Kind::Method
            | Kind::Function
            | Kind::Object
            | Kind::InstanceOf
            | Kind::LocalBinding => named(Field::Name),
            Kind::Identifier => Some(Name::Authored(node)),
            Kind::PatternVariable | Kind::Catch => identifier(Kind::Identifier),
            Kind::EnumConstant | Kind::Variable | Kind::Parameter | Kind::ClassParameter => {
                named(Field::Name).or_else(|| identifier(Kind::Identifier))
            }
            Kind::TypeParameter => {
                identifier(Kind::TypeIdentifier).or_else(|| identifier(Kind::Identifier))
            }
            Kind::Constructor | Kind::PrimaryConstructor => named(Field::Name).or_else(|| {
                self.kinds
                    .token(node, Keyword::Constructor)
                    .map(Name::Authored)
            }),
            Kind::TypeAlias => named(Field::Type),
            Kind::Accessor => self
                .kinds
                .token(node, Keyword::Get)
                .or_else(|| self.kinds.token(node, Keyword::Set))
                .map(Name::Authored),
            Kind::Companion => named(Field::Name).or_else(|| {
                self.kinds
                    .token(node, Keyword::Companion)
                    .map(Name::Companion)
            }),
            _ => None,
        }
    }

    /// The source element naming this declaration.
    pub(super) fn name_node<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        self.name(node).map(Name::node)
    }

    pub(super) fn declaration_kind(
        &self,
        node: Node<'_>,
        parent: Option<Node<'_>>,
        grandparent: Option<Node<'_>>,
    ) -> Option<DeclarationKind> {
        use DeclarationKind as Declaration;
        use NodeKind as Kind;
        let parent_kind = parent.and_then(|node| self.kinds.kind(node));
        let grandparent_kind = grandparent.and_then(|node| self.kinds.kind(node));
        let kind = match self.kinds.kind(node)? {
            Kind::AnnotationType => Declaration::Annotation,
            Kind::Class => self.class_kind(node),
            Kind::Record => Declaration::Record,
            Kind::Interface => Declaration::Interface,
            Kind::Enum => Declaration::Enum,
            Kind::EnumConstant => Declaration::Variant,
            Kind::Module => Declaration::Module,
            Kind::Method | Kind::Accessor => Declaration::Method,
            Kind::Function if parent_kind.is_some_and(Self::is_class_body) => Declaration::Method,
            Kind::Function => Declaration::Function,
            Kind::Constructor | Kind::PrimaryConstructor => Declaration::Constructor,
            Kind::Object | Kind::Companion => Declaration::Object,
            Kind::TypeAlias => Declaration::TypeAlias,
            Kind::Variable if parent_kind == Some(Kind::Field) => Declaration::Field,
            Kind::Variable if parent_kind == Some(Kind::SpreadParameter) => Declaration::Parameter,
            Kind::Variable
                if parent_kind == Some(Kind::Property)
                    && grandparent_kind.is_some_and(Self::is_api_scope) =>
            {
                Declaration::Property
            }
            Kind::Variable | Kind::InstanceOf | Kind::PatternVariable | Kind::LocalBinding => {
                Declaration::Variable
            }
            Kind::Identifier
                if matches!(parent_kind, Some(Kind::Accessor | Kind::LambdaParameters))
                    || (parent_kind == Some(Kind::Lambda)
                        && parent
                            .and_then(|lambda| self.kinds.field(lambda, Field::Parameters))
                            == Some(node)) =>
            {
                Declaration::Parameter
            }
            Kind::TypeParameter => Declaration::TypeParameter,
            Kind::ClassParameter if self.is_property_parameter(node) => Declaration::Property,
            Kind::Parameter
                if parent_kind == Some(Kind::Parameters)
                    && grandparent_kind == Some(Kind::Record) =>
            {
                Declaration::Property
            }
            Kind::Parameter | Kind::ClassParameter | Kind::Catch => Declaration::Parameter,
            _ => return None,
        };
        Some(kind)
    }

    fn class_kind(&self, node: Node<'_>) -> DeclarationKind {
        if self.kinds.token(node, Keyword::Interface).is_some() {
            DeclarationKind::Interface
        } else if self.has_modifier(node, Keyword::Enum) {
            DeclarationKind::Enum
        } else if self.has_modifier(node, Keyword::Annotation) {
            DeclarationKind::Annotation
        } else {
            DeclarationKind::Class
        }
    }

    fn is_property_parameter(&self, node: Node<'_>) -> bool {
        self.kinds.token(node, Keyword::Val).is_some()
            || self.kinds.token(node, Keyword::Var).is_some()
    }

    fn property_with_accessor<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        if self.kinds.kind(node) != Some(NodeKind::Property) {
            return None;
        }
        self.kinds.child(node, NodeKind::Accessor)?;
        self.kinds.child(node, NodeKind::Variable)
    }

    fn is_class_body(kind: NodeKind) -> bool {
        matches!(kind, NodeKind::ClassBody | NodeKind::EnumBody)
    }

    fn is_api_scope(kind: NodeKind) -> bool {
        Self::is_class_body(kind) || kind == NodeKind::Root
    }

    fn modifier(&self, node: Node<'_>) -> Option<Keyword> {
        if self.kinds.kind(node) == Some(NodeKind::Modifier) {
            self.kinds.keyword(node.child(0)?)
        } else {
            self.kinds.keyword(node)
        }
    }

    fn has_modifier(&self, node: Node<'_>, keyword: Keyword) -> bool {
        self.kinds
            .child(node, NodeKind::Modifiers)
            .is_some_and(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .any(|child| self.modifier(child) == Some(keyword))
            })
    }

    fn carrier<'walk, 'tree>(&self, visited: Visited<'walk, 'tree>) -> Visited<'walk, 'tree> {
        let Some(parent) = visited.parent() else {
            return visited;
        };
        if self.kinds.kind(visited.syntax_element()) == Some(NodeKind::Variable)
            && matches!(
                self.kinds.kind(parent.syntax_element()),
                Some(NodeKind::Field | NodeKind::Property)
            )
        {
            parent
        } else {
            visited
        }
    }

    fn body<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        if matches!(
            self.kinds.kind(node),
            Some(NodeKind::Catch | NodeKind::LocalBinding)
        ) {
            return None;
        }
        self.kinds.field(node, Field::Body).or_else(|| {
            [
                NodeKind::ClassBody,
                NodeKind::EnumBody,
                NodeKind::Body,
                NodeKind::Block,
            ]
            .into_iter()
            .find_map(|kind| self.kinds.child(node, kind))
        })
    }

    fn documentation<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
        source: &str,
    ) -> Option<Node<'tree>> {
        let carrier = self.carrier(visited);
        let node = carrier.syntax_element();
        if node != visited.syntax_element()
            && self.kinds.child(node, NodeKind::Variable) != Some(visited.syntax_element())
        {
            return None;
        }
        let previous = carrier.previous_sibling()?.syntax_element();
        let docs = source.get(previous.byte_range())?;
        (self.kinds.kind(previous) == Some(NodeKind::BlockComment)
            && docs
                .strip_prefix("/**")
                .is_some_and(|text| text.ends_with("*/"))
            && source
                .get(previous.end_byte()..node.start_byte())?
                .trim()
                .is_empty())
        .then_some(previous)
    }

    fn documentation_text(&self, comment: Node<'_>, source: &str) -> Option<Documentation> {
        let text = source
            .get(comment.byte_range())?
            .strip_prefix("/**")?
            .strip_suffix("*/")?;
        let format = match self.language {
            JvmLanguage::Java => DocumentationFormat::Plain,
            JvmLanguage::Kotlin => DocumentationFormat::Markdown,
        };
        let text = text
            .lines()
            .map(|line| {
                let line = line.trim_start();
                line.strip_prefix('*')
                    .map_or(line, |line| line.strip_prefix(' ').unwrap_or(line))
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        Some(Documentation { format, text })
    }

    fn public(
        &self,
        kind: DeclarationKind,
        visibility: Option<Keyword>,
        parent: Option<NodeKind>,
        owner: Option<NodeKind>,
    ) -> bool {
        if !kind.has_api_visibility()
            || matches!(
                owner,
                Some(NodeKind::AnonymousClass | NodeKind::ConstructorCall)
            )
        {
            return false;
        }
        if matches!(visibility, Some(Keyword::Private | Keyword::Internal)) {
            return false;
        }
        match self.language {
            JvmLanguage::Java => {
                matches!(visibility, Some(Keyword::Public | Keyword::Protected))
                    || (parent.is_some_and(Self::is_class_body)
                        && matches!(owner, Some(NodeKind::Interface | NodeKind::AnnotationType)))
                    || matches!(kind, DeclarationKind::Property | DeclarationKind::Variant)
            }
            JvmLanguage::Kotlin => {
                parent.is_some_and(Self::is_api_scope)
                    || matches!(
                        kind,
                        DeclarationKind::Constructor | DeclarationKind::Property
                    )
            }
        }
    }
}

impl GrammarRules for Rules {
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let node = visited.syntax_element();
        let parent = visited.parent();
        let grandparent = parent.and_then(Visited::parent);
        let Some((kind, name)) = self
            .declaration_kind(
                node,
                parent.map(Visited::syntax_element),
                grandparent.map(Visited::syntax_element),
            )
            .zip(self.name(node).and_then(|name| name.text(source)))
        else {
            return Ok(None);
        };
        if name.is_empty() || node.is_missing() {
            return Ok(None);
        }
        let carrier = self.carrier(visited);
        let property = (self.kinds.kind(node) == Some(NodeKind::Accessor))
            .then(|| visited.parent())
            .flatten();
        let scope = property.unwrap_or(carrier).parent();
        let parent_kind = scope.and_then(|scope| self.kinds.kind(scope.syntax_element()));
        let owner_kind = scope
            .and_then(Visited::parent)
            .and_then(|scope| self.kinds.kind(scope.syntax_element()));
        let mut kinds = kind.kinds();
        let mut visibility = None;
        if let Some(modifiers) = self
            .kinds
            .child(carrier.syntax_element(), NodeKind::Modifiers)
        {
            for modifier in modifiers
                .children(&mut modifiers.walk())
                .filter_map(|child| self.modifier(child))
            {
                match modifier {
                    Keyword::Public | Keyword::Protected | Keyword::Private | Keyword::Internal => {
                        visibility = Some(modifier);
                    }
                    Keyword::Abstract => kinds.push(SymbolKind::Abstract),
                    Keyword::Static => kinds.push(SymbolKind::Static),
                    Keyword::Open => kinds.push(SymbolKind::Extensible),
                    _ => {}
                }
            }
        }
        if parent_kind.is_some_and(Self::is_class_body)
            || matches!(
                kind,
                DeclarationKind::Field
                    | DeclarationKind::Method
                    | DeclarationKind::Constructor
                    | DeclarationKind::Variant
            )
            || (kind == DeclarationKind::Property && parent_kind != Some(NodeKind::Root))
        {
            kinds.push(SymbolKind::Member);
        }
        let property_visible = property.is_none_or(|property| {
            let node = property.syntax_element();
            !self.has_modifier(node, Keyword::Private)
                && !self.has_modifier(node, Keyword::Internal)
        });
        if property_visible && self.public(kind, visibility, parent_kind, owner_kind) {
            kinds.push(SymbolKind::Public);
        }
        if kind == DeclarationKind::Property
            && self
                .property_with_accessor(carrier.syntax_element())
                .is_some()
        {
            kinds.push(SymbolKind::MemberContainer);
        }
        if (self.language == JvmLanguage::Kotlin
            && self
                .kinds
                .token(carrier.syntax_element(), Keyword::Var)
                .is_some())
            || (self.language == JvmLanguage::Java
                && matches!(kind, DeclarationKind::Field | DeclarationKind::Variable)
                && !self.has_modifier(carrier.syntax_element(), Keyword::Final))
        {
            kinds.push(SymbolKind::Mutable);
        }
        let documentation = self.documentation(visited, source);
        let docs = documentation.and_then(|comment| self.documentation_text(comment, source));
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind: kind.into(),
            kinds,
            visibility: visibility.map(|modifier| <&str>::from(modifier).to_owned()),
            body_range: self.body(node).map(byte_range).transpose()?,
            documentation: docs.into_iter().collect(),
            documentation_ranges: documentation
                .map(byte_range)
                .transpose()?
                .into_iter()
                .collect(),
        }))
    }

    fn declaration_qualification(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        qualification: &str,
    ) -> Option<String> {
        let property = self.property_with_accessor(visited.parent()?.syntax_element())?;
        if property != visited.syntax_element() {
            return None;
        }
        let property_name = self.name(property)?.text(source)?;
        let container = qualification.strip_suffix(property_name)?;
        Some(container.strip_suffix('.').unwrap_or(container).to_owned())
    }

    fn container_name(&self, node: Node<'_>, source: &str) -> Option<String> {
        if let Some(property) = self.property_with_accessor(node) {
            return self.name(property)?.text(source).map(str::to_owned);
        }
        if self.kinds.kind(node) == Some(NodeKind::Root) {
            let header = self.kinds.child(node, NodeKind::Package)?;
            let name = self
                .kinds
                .child(header, NodeKind::QualifiedName)
                .or_else(|| self.kinds.child(header, NodeKind::Identifier))?;
            let mut package = String::new();
            let mut pending = vec![name];
            let mut cursor = name.walk();
            while let Some(element) = pending.pop() {
                match self.kinds.kind(element) {
                    Some(NodeKind::Identifier) => {
                        if !package.is_empty() {
                            package.push('.');
                        }
                        package.push_str(Name::Authored(element).text(source)?);
                    }
                    Some(NodeKind::QualifiedName) => {
                        crate::extract::push_named_children(
                            &mut pending,
                            &mut cursor,
                            element,
                            |child| child,
                        );
                    }
                    _ => {}
                }
            }
            return Some(package);
        }
        self.declaration_kind(node, None, None).filter(|kind| {
            kind.opens_scope() && self.kinds.kind(node) != Some(NodeKind::PrimaryConstructor)
        })?;
        self.name(node)?.text(source).map(str::to_owned)
    }

    fn declaration_start(&self, visited: Visited<'_, '_>, source: &str) -> usize {
        self.documentation(visited, source).map_or_else(
            || visited.syntax_element().start_byte(),
            |comment| comment.start_byte(),
        )
    }

    fn declaration_syntax_element<'tree>(&self, visited: Visited<'_, 'tree>) -> Node<'tree> {
        let carrier = self.carrier(visited).syntax_element();
        let node = visited.syntax_element();
        if carrier != node && self.kinds.child(carrier, NodeKind::Variable) == Some(node) {
            carrier
        } else {
            node
        }
    }

    fn renders_signature(&self, visited: Visited<'_, '_>) -> bool {
        self.declaration_kind(visited.syntax_element(), None, None)
            .is_some_and(DeclarationKind::callable)
    }

    fn name_range(&self, node: Node<'_>) -> Result<Option<ByteRange>, RiftError> {
        self.name_node(node).map(byte_range).transpose()
    }

    fn qualification_separator(&self) -> &'static str {
        "."
    }
}
