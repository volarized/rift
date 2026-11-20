//! Compact syntax evidence for source-based name resolution.

mod heritage_edges;
mod javascript;
mod python_edges;
mod rust;
pub use heritage_edges::HeritageEdge;

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

/// One authored value access or type reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticReference {
    /// Name or member expression naming the target.
    pub target: ByteRange,
    /// The source operation on that target.
    pub role: rift_core::ReferenceRole,
}

/// Syntactic form of the callee in a captured call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticCallee {
    /// Unqualified name.
    Direct,
    /// Language-qualified path.
    Qualified,
    /// Explicit constructor invocation.
    Constructor,
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
    /// Statically named ECMAScript namespace import.
    JavaScriptEdgeNamespace {
        /// Unquoted module specifier.
        module: ByteRange,
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
    /// Statically named Python import.
    PythonImport {
        /// Authored module path.
        module: ByteRange,
        /// Imported name, absent for a module binding.
        export: Option<ByteRange>,
    },
    /// First receiver parameter of a Python method.
    PythonReceiver {
        /// Name of the enclosing class.
        class_name: ByteRange,
    },
    /// A Python binding with an authored type annotation.
    PythonAnnotation {
        /// Authored annotation expression.
        annotation: ByteRange,
    },
    /// A Python binding initialized by a named constructor call.
    PythonInstance {
        /// Authored constructor path, requiring static resolution.
        constructor: ByteRange,
    },
    /// A Python binding initialized by a named value.
    PythonAlias {
        /// Authored value path.
        value: ByteRange,
    },
    /// A Python name checked by an `isinstance` condition.
    PythonGuard {
        /// Authored class argument.
        annotation: ByteRange,
    },
    /// A Python function with an authored return annotation.
    PythonFunction {
        /// Authored return annotation.
        annotation: ByteRange,
    },
    /// Dotted Python module import without a local alias.
    PythonModule {
        /// Complete imported module path.
        module: ByteRange,
    },
    /// A Python name declared in the module scope.
    PythonGlobal,
    /// A Python name declared in an enclosing function scope.
    PythonNonlocal,
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

/// One Rust use path, including its enclosing groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticRustUse {
    /// Leaf path or wildcard range.
    pub range: ByteRange,
    /// Each group prefix followed by the leaf path.
    pub path: Vec<ByteRange>,
    /// Scope where the import applies.
    pub scope: ByteRange,
    /// Whether the path imports every visible member.
    pub glob: bool,
}

/// One Rust implementation and the type its methods belong to.
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
    /// Value accesses and type references.
    pub references: Vec<StaticReference>,
    /// Runtime function expressions and their enclosing value ranges.
    pub function_values: Vec<(ByteRange, ByteRange)>,
    /// Function expressions that establish their own receiver.
    pub function_scopes: Vec<ByteRange>,
    /// Static class initialization blocks.
    pub static_blocks: Vec<ByteRange>,
    /// Class and implementation headers.
    pub heritage_edges: Vec<HeritageEdge>,
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
    /// Rust use paths.
    pub rust_uses: Vec<StaticRustUse>,
    /// Authored Rust implementations.
    pub inherent_impls: Vec<StaticInherentImpl>,
    /// Items whose admission depends on unresolved Rust configuration.
    pub conditional_items: Vec<ByteRange>,
}

/// Calls and binding evidence retained after the syntax tree is released.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFacts(StaticFactsParts);

impl StaticFacts {
    /// Captured value accesses and type references.
    #[must_use]
    pub fn references(&self) -> &[StaticReference] {
        &self.0.references
    }
    /// Runtime function expressions and their enclosing value ranges.
    #[must_use]
    pub fn function_values(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.function_values
    }

    /// Function expressions that establish their own receiver.
    #[must_use]
    pub fn function_scopes(&self) -> &[ByteRange] {
        &self.0.function_scopes
    }

    /// Static class initialization blocks.
    #[must_use]
    pub fn static_blocks(&self) -> &[ByteRange] {
        &self.0.static_blocks
    }

    /// Captured class and implementation edges.
    #[must_use]
    pub fn heritage_edges(&self) -> &[HeritageEdge] {
        &self.0.heritage_edges
    }

    /// Restores checked facts from an original source selection.
    ///
    /// # Errors
    ///
    /// Returns an error when a collection exceeds the syntax element bound or carries invalid ranges.
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

    /// Rust use paths with their captured group components.
    #[must_use]
    pub fn rust_uses(&self) -> &[StaticRustUse] {
        &self.0.rust_uses
    }

    /// Rust implementation headers.
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
            vector_capacity(&self.0.references),
            vector_capacity(&self.0.function_values),
            vector_capacity(&self.0.function_scopes),
            vector_capacity(&self.0.static_blocks),
            vector_capacity(&self.0.heritage_edges),
            vector_capacity(&self.0.calls),
            vector_capacity(&self.0.bindings),
            vector_capacity(&self.0.reexports),
            vector_capacity(&self.0.assignments),
            vector_capacity(&self.0.unsupported_bindings),
            vector_capacity(&self.0.inherent_impls),
            vector_capacity(&self.0.rust_uses),
            vector_capacity(&self.0.conditional_items),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
        .saturating_add(
            self.0
                .rust_uses
                .iter()
                .map(|row| vector_capacity(&row.path))
                .fold(0, usize::saturating_add),
        )
    }

    pub(crate) fn validate(&self, source: &str, limits: SyntaxLimits) -> Result<(), RiftError> {
        let lengths = [
            self.references().len(),
            self.function_values().len(),
            self.function_scopes().len(),
            self.static_blocks().len(),
            self.heritage_edges().len(),
            self.calls().len(),
            self.bindings().len(),
            self.reexports().len(),
            self.assignments().len(),
            self.unsupported_bindings().len(),
            self.inherent_impls().len(),
            self.rust_uses().len(),
            self.conditional_items().len(),
        ];
        if lengths
            .into_iter()
            .any(|length| length > limits.syntax_elements_max())
        {
            return errors::syntax::facts_count_exceeded().fail();
        }
        let components = self
            .rust_uses()
            .iter()
            .try_fold(0_usize, |count, row| count.checked_add(row.path.len()));
        if components.is_none_or(|count| count > limits.syntax_elements_max()) {
            return errors::syntax::facts_count_exceeded().fail();
        }
        let valid = |range: ByteRange| {
            let start = usize::try_from(range.start).ok();
            let end = usize::try_from(range.end).ok();
            start
                .zip(end)
                .is_some_and(|(start, end)| start < end && source.get(start..end).is_some())
        };
        let edges = self.heritage_edges().iter().all(|edge| {
            valid(edge.from)
                && valid(edge.target)
                && matches!(
                    edge.kind,
                    rift_protocol::read::EdgeKind::Extends
                        | rift_protocol::read::EdgeKind::Implements
                )
        });
        let calls = self.calls().iter().all(|call| call.valid(valid));
        let references = self.references().iter().all(|reference| {
            valid(reference.target)
                && matches!(
                    reference.role,
                    rift_core::ReferenceRole::Read
                        | rift_core::ReferenceRole::Write
                        | rift_core::ReferenceRole::Type
                )
        });
        let functions = self
            .function_values()
            .iter()
            .all(|(range, value)| valid(*range) && valid(*value) && contains(*value, *range))
            && self.function_scopes().iter().copied().all(valid)
            && self.static_blocks().iter().copied().all(valid);
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
        let imports = self.rust_uses().iter().all(|row| {
            valid(row.range)
                && valid(row.scope)
                && row.scope.encloses(row.range)
                && !row.path.is_empty()
                && row.path.len() <= limits.syntax_depth_max()
                && row
                    .path
                    .iter()
                    .copied()
                    .all(|range| valid(range) && row.scope.encloses(range))
        });
        let conditional = self.conditional_items().iter().copied().all(valid);
        if !(edges
            && references
            && functions
            && calls
            && bindings
            && reexports
            && assignments
            && unsupported
            && implementations
            && imports
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
            StaticCallee::Direct
            | StaticCallee::Qualified
            | StaticCallee::Constructor
            | StaticCallee::Unsupported => true,
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
            StaticBindingKind::JavaScriptImport { module, export }
            | StaticBindingKind::PythonImport { module, export } => {
                valid(module) && export.is_none_or(&valid)
            }
            StaticBindingKind::JavaScriptRequire {
                module,
                export,
                require,
            } => valid(module) && export.is_none_or(&valid) && valid(require),
            StaticBindingKind::JavaScriptEdgeNamespace { module }
            | StaticBindingKind::PythonModule { module } => valid(module),
            StaticBindingKind::JavaScriptInstance { constructor }
            | StaticBindingKind::PythonInstance { constructor } => valid(constructor),
            StaticBindingKind::PythonReceiver { class_name } => valid(class_name),
            StaticBindingKind::PythonAnnotation { annotation }
            | StaticBindingKind::PythonFunction { annotation }
            | StaticBindingKind::PythonGuard { annotation } => valid(annotation),
            StaticBindingKind::PythonAlias { value } => valid(value),
            StaticBindingKind::RustUse { path } => valid(path),
            StaticBindingKind::RustReceiver { type_path } => valid(type_path),
            StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::PythonGlobal
            | StaticBindingKind::PythonNonlocal => true,
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
    Python(&'static python_edges::EdgePythonKinds),
    JavaScript(&'static javascript::Kinds),
    Rust(&'static rust::Kinds, rift_core::LoopBudget),
}

pub(crate) struct Capture<'source> {
    grammar: Grammar,
    source: &'source str,
    facts: StaticFacts,
    edge_heritage: &'static heritage_edges::EdgeHeritageKinds,
}

impl<'source> Capture<'source> {
    pub(crate) fn for_javascript(source: &'source str) -> Self {
        Self {
            grammar: Grammar::JavaScript(javascript::kinds()),
            edge_heritage: heritage_edges::edge_javascript_kinds(),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn for_edge_typescript(
        source: &'source str,
        dialect: crate::typescript::TypeScriptDialect,
    ) -> Self {
        Self {
            grammar: Grammar::JavaScript(javascript::edge_typescript_kinds(dialect)),
            edge_heritage: heritage_edges::edge_typescript_kinds(dialect),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn for_edge_python(source: &'source str) -> Self {
        Self {
            grammar: Grammar::Python(python_edges::edge_python_kinds()),
            source,
            facts: StaticFacts::default(),
            edge_heritage: heritage_edges::edge_python_kinds(),
        }
    }

    pub(crate) fn for_rust(source: &'source str, limits: SyntaxLimits) -> Self {
        Self {
            grammar: Grammar::Rust(
                rust::kinds(),
                rift_core::LoopBudget::new(limits.syntax_elements_max()),
            ),
            edge_heritage: heritage_edges::edge_rust_kinds(),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn record(
        &mut self,
        visited: crate::extract::Visited<'_, '_>,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        if syntax_element.is_missing() || syntax_element.is_extra() {
            return Ok(());
        }
        self.edge_heritage.record_edge(visited, &mut self.facts.0)?;
        match &mut self.grammar {
            Grammar::Python(kinds) => kinds.record_edge(visited, self.source, &mut self.facts.0),
            Grammar::JavaScript(kinds) => {
                kinds.record(syntax_element, self.source, &mut self.facts.0)
            }
            Grammar::Rust(kinds, budget) => {
                kinds.record(syntax_element, self.source, &mut self.facts.0, budget)
            }
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

fn identifier(syntax_element: Node<'_>, id: u16) -> Option<Node<'_>> {
    (syntax_element.kind_id() == id && !syntax_element.is_missing()).then_some(syntax_element)
}

fn enclosing<'tree>(syntax_element: Node<'tree>, kinds: &[u16]) -> Option<Node<'tree>> {
    let mut parent = syntax_element.parent();
    // Capture runs after the shared syntax walk has admitted each syntax element's depth.
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

fn call(
    syntax_element: Node<'_>,
    callee: Node<'_>,
    shape: StaticCallee,
) -> Result<StaticCall, RiftError> {
    Ok(StaticCall {
        range: byte_range(syntax_element)?,
        callee_range: byte_range(callee)?,
        callee: shape,
    })
}
