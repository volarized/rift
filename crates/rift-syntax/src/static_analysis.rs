//! Compact syntax evidence for source-based name resolution.

mod javascript;
mod rust;

use rift_error::{RiftError, errors};
use tree_sitter::{Language, Node};

use crate::extract::byte_range;
use crate::{ByteRange, SyntaxLimits};

/// One authored call expression and its supported callee shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticCall {
    /// Complete invocation range.
    pub range: ByteRange,
    /// Range of the complete authored callee.
    pub callee_range: ByteRange,
    /// Form of the callee.
    pub callee: StaticCallee,
}

/// Syntactic form of the callee in a captured call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticCallee {
    /// Unqualified name.
    Direct,
    /// Language-qualified path.
    Qualified,
    /// Named member on a simple receiver.
    Member {
        /// Receiver expression.
        receiver: ByteRange,
        /// Member name.
        member: ByteRange,
    },
    /// Computed or otherwise unsupported callee.
    Unsupported,
}

/// One name binding and the source interval in which it takes precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticBinding {
    /// Authored local name.
    pub name: ByteRange,
    /// Scope containing possible uses of the name.
    pub scope: ByteRange,
    /// First position where the binding can supply a target.
    pub available_from: u64,
    /// Evidence that may establish a target.
    pub kind: StaticBindingKind,
}

/// Syntactic evidence held by one name binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticBindingKind {
    /// The name range belongs to a local declaration.
    Local,
    /// The binding prevents lookup in an outer scope.
    Shadow,
    /// Statically named ESM import.
    JavaScriptImport {
        /// Unquoted module specifier.
        module: ByteRange,
        /// Imported name, or absent for the default export.
        export: Option<ByteRange>,
    },
    /// Literal `CommonJS` import whose mode and `require` binding need verification.
    JavaScriptRequire {
        /// Authored identifier whose binding must be checked.
        require: ByteRange,
        /// Unquoted module specifier.
        module: ByteRange,
        /// Named property, or absent for the module value.
        export: Option<ByteRange>,
    },
    /// A local variable initialized directly by a constructor.
    JavaScriptInstance {
        /// Constructor name.
        constructor: ByteRange,
    },
    /// Explicit Rust use path.
    RustUse {
        /// Imported path before a local alias.
        path: ByteRange,
    },
    /// Local Rust binding with an authored receiver type.
    RustReceiver {
        /// Authored type or constructor path.
        type_path: ByteRange,
    },
}

/// One named export forwarded directly from another module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticReexport {
    /// Name visible to consumers of this module.
    pub name: ByteRange,
    /// Unquoted module specifier.
    pub module: ByteRange,
    /// Name visible in the source module.
    pub export: ByteRange,
}

/// One inherent Rust implementation and the type its methods belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticInherentImpl {
    /// Authored implementation range.
    pub range: ByteRange,
    /// Type path on the implementation header.
    pub type_path: ByteRange,
}

/// Collections retained by static syntax capture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFactsParts {
    /// Authored calls.
    pub calls: Vec<StaticCall>,
    /// Lexical bindings.
    pub bindings: Vec<StaticBinding>,
    /// Explicit re-exports.
    pub reexports: Vec<StaticReexport>,
    /// Assignment targets.
    pub assignments: Vec<ByteRange>,
    /// Scopes with unsupported binding shapes.
    pub unsupported_bindings: Vec<ByteRange>,
    /// Authored Rust inherent implementations.
    pub inherent_impls: Vec<StaticInherentImpl>,
    /// Items whose admission depends on unresolved Rust configuration.
    pub conditional_items: Vec<ByteRange>,
}

/// Calls and binding evidence retained after the syntax tree is released.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFacts(StaticFactsParts);

impl StaticFacts {
    /// Restores checked facts from an original source selection.
    ///
    /// # Errors
    ///
    /// Returns an error when a collection exceeds the node bound or carries invalid ranges.
    pub fn from_parts(
        source: &str,
        limits: SyntaxLimits,
        parts: StaticFactsParts,
    ) -> Result<Self, RiftError> {
        let facts = Self(parts);
        facts.validate(source, limits)?;
        Ok(facts)
    }

    /// Captured calls, in source order.
    #[must_use]
    pub fn calls(&self) -> &[StaticCall] {
        &self.0.calls
    }

    /// Captured lexical bindings.
    #[must_use]
    pub fn bindings(&self) -> &[StaticBinding] {
        &self.0.bindings
    }

    /// Explicit re-exports from another module.
    #[must_use]
    pub fn reexports(&self) -> &[StaticReexport] {
        &self.0.reexports
    }

    /// Authored assignment targets that can invalidate simple bindings.
    #[must_use]
    pub fn assignments(&self) -> &[ByteRange] {
        &self.0.assignments
    }

    /// Scopes where an unsupported binding can affect lookup.
    #[must_use]
    pub fn unsupported_bindings(&self) -> &[ByteRange] {
        &self.0.unsupported_bindings
    }

    /// Rust inherent implementation headers.
    #[must_use]
    pub fn inherent_impls(&self) -> &[StaticInherentImpl] {
        &self.0.inherent_impls
    }

    /// Authored Rust items requiring configuration evidence.
    #[must_use]
    pub fn conditional_items(&self) -> &[ByteRange] {
        &self.0.conditional_items
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        [
            vector_capacity(&self.0.calls),
            vector_capacity(&self.0.bindings),
            vector_capacity(&self.0.reexports),
            vector_capacity(&self.0.assignments),
            vector_capacity(&self.0.unsupported_bindings),
            vector_capacity(&self.0.inherent_impls),
            vector_capacity(&self.0.conditional_items),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    pub(crate) fn validate(&self, source: &str, limits: SyntaxLimits) -> Result<(), RiftError> {
        let lengths = [
            self.calls().len(),
            self.bindings().len(),
            self.reexports().len(),
            self.assignments().len(),
            self.unsupported_bindings().len(),
            self.inherent_impls().len(),
            self.conditional_items().len(),
        ];
        if lengths
            .into_iter()
            .any(|length| length > limits.syntax_nodes_max())
        {
            return errors::syntax::facts_count_exceeded().fail();
        }
        let valid = |range: ByteRange| {
            let start = usize::try_from(range.start).ok();
            let end = usize::try_from(range.end).ok();
            start
                .zip(end)
                .is_some_and(|(start, end)| start < end && source.get(start..end).is_some())
        };
        let calls = self.calls().iter().all(|call| call.valid(valid));
        let bindings = self.bindings().iter().all(|binding| binding.valid(valid));
        let reexports = self
            .reexports()
            .iter()
            .all(|row| valid(row.name) && valid(row.module) && valid(row.export));
        let assignments = self.assignments().iter().copied().all(valid);
        let unsupported = self.unsupported_bindings().iter().copied().all(valid);
        let implementations = self.inherent_impls().iter().all(|row| {
            valid(row.range) && valid(row.type_path) && contains(row.range, row.type_path)
        });
        let conditional = self.conditional_items().iter().copied().all(valid);
        if !(calls
            && bindings
            && reexports
            && assignments
            && unsupported
            && implementations
            && conditional)
        {
            return errors::syntax::facts_range_invalid().fail();
        }
        Ok(())
    }
}

impl StaticCall {
    fn valid(&self, valid: impl Fn(ByteRange) -> bool) -> bool {
        let inside = |range| valid(range) && contains(self.callee_range, range);
        let member = match self.callee {
            StaticCallee::Member { receiver, member } => inside(receiver) && inside(member),
            StaticCallee::Direct | StaticCallee::Qualified | StaticCallee::Unsupported => true,
        };
        valid(self.range)
            && inside(self.callee_range)
            && contains(self.range, self.callee_range)
            && member
    }
}

impl StaticBinding {
    fn valid(&self, valid: impl Fn(ByteRange) -> bool) -> bool {
        let evidence = match self.kind {
            StaticBindingKind::JavaScriptImport { module, export } => {
                valid(module) && export.is_none_or(&valid)
            }
            StaticBindingKind::JavaScriptRequire {
                module,
                export,
                require,
            } => valid(module) && export.is_none_or(&valid) && valid(require),
            StaticBindingKind::JavaScriptInstance { constructor } => valid(constructor),
            StaticBindingKind::RustUse { path } => valid(path),
            StaticBindingKind::RustReceiver { type_path } => valid(type_path),
            StaticBindingKind::Local | StaticBindingKind::Shadow => true,
        };
        valid(self.name)
            && valid(self.scope)
            && contains(self.scope, self.name)
            && self.scope.start <= self.available_from
            && self.available_from <= self.scope.end
            && evidence
    }
}

fn contains(outer: ByteRange, inner: ByteRange) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn vector_capacity<T>(items: &Vec<T>) -> usize {
    items.capacity().saturating_mul(std::mem::size_of::<T>())
}

enum Grammar {
    JavaScript(&'static javascript::Kinds),
    Rust(&'static rust::Kinds),
}

pub(crate) struct Capture<'source> {
    grammar: Grammar,
    source: &'source str,
    facts: StaticFacts,
}

impl<'source> Capture<'source> {
    pub(crate) fn for_javascript(source: &'source str) -> Self {
        Self {
            grammar: Grammar::JavaScript(javascript::kinds()),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn for_rust(source: &'source str) -> Self {
        Self {
            grammar: Grammar::Rust(rust::kinds()),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn record(&mut self, node: Node<'_>) -> Result<(), RiftError> {
        if node.is_missing() {
            return Ok(());
        }
        match self.grammar {
            Grammar::JavaScript(kinds) => kinds.record(node, self.source, &mut self.facts.0),
            Grammar::Rust(kinds) => kinds.record(node, self.source, &mut self.facts.0),
        }
    }

    pub(crate) fn finish(self) -> StaticFacts {
        self.facts
    }
}

fn kind(grammar: &Language, name: &str) -> u16 {
    let id = grammar.id_for_node_kind(name, true);
    assert!(
        id != 0,
        "the pinned static-analysis grammar must define kind {name}"
    );
    id
}

fn field(grammar: &Language, name: &str) -> u16 {
    grammar
        .field_id_for_name(name)
        .unwrap_or_else(|| panic!("the pinned static-analysis grammar must define field {name}"))
        .get()
}

fn identifier(node: Node<'_>, id: u16) -> Option<Node<'_>> {
    (node.kind_id() == id && !node.is_missing()).then_some(node)
}

fn enclosing<'tree>(node: Node<'tree>, kinds: &[u16]) -> Option<Node<'tree>> {
    let mut parent = node.parent();
    // Capture runs after the shared syntax walk has admitted each node's depth.
    while let Some(candidate) = parent {
        if kinds.contains(&candidate.kind_id()) {
            return Some(candidate);
        }
        parent = candidate.parent();
    }
    None
}

fn binding(
    name: Node<'_>,
    scope: Node<'_>,
    available_from: u64,
    kind: StaticBindingKind,
) -> Result<StaticBinding, RiftError> {
    Ok(StaticBinding {
        name: byte_range(name)?,
        scope: byte_range(scope)?,
        available_from,
        kind,
    })
}

fn call(node: Node<'_>, callee: Node<'_>, shape: StaticCallee) -> Result<StaticCall, RiftError> {
    Ok(StaticCall {
        range: byte_range(node)?,
        callee_range: byte_range(callee)?,
        callee: shape,
    })
}
