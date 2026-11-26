//! Compact syntax evidence for source-based name resolution.

mod heritage_edges;
mod javascript;
mod python_edges;
mod rust;
pub use heritage_edges::HeritageEdge;

use rift_core::ReferenceRole;
use rift_error::{RiftError, errors};
use tree_sitter::{Language, Node};

use crate::extract::{Visited, byte_range};
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

/// Source evidence for an initializer generated from a decorated Python class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticPythonInitializer {
    /// The complete authored class declaration.
    pub class: ByteRange,
    /// The decorator expression naming the initializer generator.
    pub decorator: ByteRange,
    /// The single authored base class, when present.
    pub base: Option<ByteRange>,
    /// The annotated fields the class writes.
    pub fields: Vec<StaticPythonInitializerField>,
}

/// The authored name and annotation of one initializer field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticPythonInitializerField {
    /// The field's identifier.
    pub name: ByteRange,
    /// The field's type annotation.
    pub annotation: ByteRange,
}

/// One authored value access or type reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticReference {
    /// Name or member expression naming the target.
    pub target: ByteRange,
    /// The source operation on that target.
    pub role: rift_core::ReferenceRole,
}

/// One written Rust path in a value expression or a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticRustValueRead {
    /// Complete written path, including its qualifiers.
    pub range: ByteRange,
    /// Whether the path appears in a pattern.
    pub pattern: bool,
}

/// One written Rust macro invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticRustMacroCall {
    /// Complete authored invocation.
    pub range: ByteRange,
    /// Written macro name.
    pub name: ByteRange,
}

/// A Rust macro declaration's name, item and textual scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticRustMacroDefinition {
    /// Written name of the declaration.
    pub name: ByteRange,
    /// Complete declaration. It becomes visible after this range.
    pub item: ByteRange,
    /// Source region that inherits the declaration.
    pub scope: ByteRange,
}

/// One named field in a Rust struct literal or pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticRustFieldSite {
    /// Written path of the struct or variant owning the field.
    pub owner: ByteRange,
    /// Written field label, including a shorthand name.
    pub field: ByteRange,
    /// Complete struct expression for a write. Patterns read the field.
    pub literal: Option<ByteRange>,
}

/// One member expression with its original receiver and property spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticMember {
    /// Complete member expression.
    pub range: ByteRange,
    /// Receiver expression.
    pub receiver: ByteRange,
    /// Literal property name; computed properties remain unknown.
    pub member: Option<ByteRange>,
}

/// Authored names of two callable members assigned the same function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticMemberAlias {
    /// Name of the member on the outer assignment.
    pub name: ByteRange,
    /// Name of the member whose assignment holds the function.
    pub target: ByteRange,
}

/// A plain assignment that copies an authored `this` member to the same named member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticMemberCopy {
    /// Complete assignment expression.
    pub assignment: ByteRange,
    /// Member on the left of the assignment.
    pub target: ByteRange,
    /// Complete member expression on the right.
    pub value: ByteRange,
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

/// One member of an ECMAScript object literal, type literal or class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticBodyMember {
    /// Complete member.
    pub member: ByteRange,
    /// Literal or class holding the member.
    pub body: ByteRange,
    /// Syntax element the literal or class is the whole value or the whole written type of,
    /// once parentheses, assertions and the annotation are read through.
    pub holder: ByteRange,
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
    /// A type a JavaScript documentation comment imports from another module; it names
    /// that module's export where a type is written and is no value.
    JavaScriptTypeImport {
        /// Unquoted module specifier.
        module: ByteRange,
        /// Imported name.
        export: ByteRange,
    },
    /// An unchanged local variable initialized directly with a class expression.
    JavaScriptClassValue {
        /// Exact physical class body, excluding the initializer's heritage header.
        body: ByteRange,
    },
    /// A local variable initialized directly by a constructor.
    JavaScriptInstance {
        /// Constructor name.
        constructor: ByteRange,
    },
    /// A lexical `const` initialized directly by a constructor.
    JavaScriptConstInstance {
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
    /// A Python name assigned the `None` literal.
    PythonNone,
    /// A Python `for` target, in a loop or a comprehension, bound to each element of the
    /// iterated value or to one part of it.
    PythonElement {
        /// Authored iterated expression.
        iterable: ByteRange,
        /// Place of the name in the target, absent for a target of one name.
        position: Option<PythonPosition>,
    },
    /// A Python assignment target bound to one part of the assigned value.
    PythonUnpacked {
        /// Authored value: a name, a member or a call.
        value: ByteRange,
        /// Place of the target among the targets.
        position: PythonPosition,
    },
    /// A Python name assigned an empty list literal.
    PythonEmptyList,
    /// A write to a Python member slot, separate from lexical bindings.
    PythonMemberWrite {
        /// Authored receiver of the assignment target.
        receiver: ByteRange,
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
        /// Single identifier decorator, absent for other decorator forms.
        decorator: Option<ByteRange>,
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
    /// Authored Rust crate binding.
    RustExternCrate {
        /// Authored crate name, or `self` for the current crate.
        crate_name: ByteRange,
    },
    /// Local Rust binding with an authored receiver type.
    RustReceiver {
        /// Authored type or constructor path.
        type_path: ByteRange,
    },
    /// The name of a type item bound inside a Rust block.
    RustBlockType,
    /// Rust type parameter whose declared bounds remain in its lexical scope.
    RustTypeParameter,
    /// Local Rust value whose initializer can establish its declared return owner.
    RustValue {
        /// Authored initializer expression.
        initializer: ByteRange,
    },
}

/// Place of one name in a Python target that unpacks a value without a starred name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythonPosition {
    /// Zero-based place of the name.
    pub index: u32,
    /// Number of parts the target unpacks.
    pub count: u32,
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
    /// Authored visibility on the use declaration; absent for a private import.
    pub visibility: Option<ByteRange>,
}

/// One Rust implementation and the type its methods belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticInherentImpl {
    /// Authored implementation range.
    pub range: ByteRange,
    /// Type path on the implementation header.
    pub type_path: ByteRange,
}

/// One Rust configuration attribute and the item whose presence it controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticConditionalItem {
    /// Item or enclosing body controlled by the attribute.
    pub range: ByteRange,
    /// Authored configuration attribute, including its delimiters.
    pub attribute: ByteRange,
}

/// One simple parameter of function-like syntax, by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticParameter {
    /// The function, method, signature or function type that declares the parameter.
    pub function: ByteRange,
    /// Name of the declaration that function belongs to, when it has one.
    pub owner: Option<ByteRange>,
    /// Zero-based position in the parameter list.
    pub index: u32,
    /// The parameter's name.
    pub name: ByteRange,
}

/// One named type a function's declared return type writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticDeclaredReturn {
    /// Name of the declaration the function belongs to.
    pub owner: ByteRange,
    /// The function or signature that writes the return type.
    pub function: ByteRange,
    /// The member of an inline object type this entry types; absent for the whole type.
    pub member: Option<ByteRange>,
    /// The named type written for the return value or for that member.
    pub annotation: ByteRange,
}

/// The written place that types a function value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticFunctionContext {
    /// The function initializes the declaration with this name.
    Initializer(ByteRange),
    /// The function is an argument of a call.
    Argument {
        /// Complete invocation range.
        call: ByteRange,
        /// Zero-based argument position.
        index: u32,
    },
    /// The function is returned by this enclosing function.
    Returned(ByteRange),
}

/// An unannotated function parameter in an object literal assigned to a typed member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticMemberCallback {
    /// The parameter that takes its type from the assignment.
    pub parameter: ByteRange,
    /// The assigned property of the asserted receiver.
    pub property: ByteRange,
    /// The function-valued member of the assigned object literal.
    pub callback: ByteRange,
    /// The named type asserted on the receiver.
    pub asserted: ByteRange,
    /// The second named type when the receiver is asserted to an intersection.
    pub intersection: Option<ByteRange>,
}

/// A direct `CommonJS` default write whose wrapper uses were checked in the same file.
/// A literal default has no authored local initializer in this witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticCommonJsExport {
    /// The complete default assignment.
    pub assignment: ByteRange,
    /// The surrounding assignment also updates the original `exports` binding.
    pub synchronized: bool,
    /// Binding names that the exports use and direct module declarations initialize.
    pub initialized: Vec<ByteRange>,
    /// The physical right-hand side of a checked direct object literal assignment.
    pub literal: Option<ByteRange>,
}

/// A direct type annotation use with its written generic binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticRustGenericReference {
    /// Original use token.
    pub range: ByteRange,
    /// Original generic declaration token.
    pub binding: ByteRange,
    /// The role the token has in the type annotation.
    pub role: ReferenceRole,
}

/// Collections retained by static syntax capture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFactsParts {
    /// Whether ECMAScript syntax requires module interpretation without an explicit mode.
    pub javascript_module_syntax: bool,
    /// Assignments to `module.exports` or a computed key of `module` that run while the file
    /// does, in no function called later, and whether no condition encloses them.
    pub commonjs_writes: Vec<(ByteRange, bool)>,
    /// Source a `return` can skip: from the first one of the file, or of an immediately
    /// called function, to that function's end.
    pub skippable: Vec<ByteRange>,
    /// Whether an ECMAScript import or export statement stands directly in the source file.
    pub javascript_module_statement: bool,
    /// The single direct default write in a file with closed `CommonJS` wrapper uses.
    pub commonjs_export: Option<StaticCommonJsExport>,
    /// Value accesses and type references.
    pub references: Vec<StaticReference>,
    /// Receiver and property spans for member expressions.
    pub members: Vec<StaticMember>,
    /// Callable member aliases established from assignment chains.
    pub member_aliases: Vec<StaticMemberAlias>,
    /// Declaration names and their explicit named type annotations.
    pub declared_types: Vec<(ByteRange, ByteRange)>,
    /// Type declaration names whose own shape is a signature: a call signature or
    /// function type (`false`), or a construct signature or constructor type (`true`).
    pub type_signatures: Vec<(ByteRange, bool)>,
    /// Simple parameters of functions, signatures and function types.
    pub parameters: Vec<StaticParameter>,
    /// Declaration names whose written type is a function type, with that type.
    pub function_types: Vec<(ByteRange, ByteRange)>,
    /// Function-like syntax and the named type its return annotation writes.
    pub return_types: Vec<(ByteRange, ByteRange)>,
    /// Function values and the written place that types them.
    pub function_contexts: Vec<(ByteRange, StaticFunctionContext)>,
    /// Parameters typed by an object literal's assignment to a member.
    pub member_callbacks: Vec<StaticMemberCallback>,
    /// Names of type and property declarations with their explicit object type bodies.
    pub member_type_shapes: Vec<(ByteRange, ByteRange)>,
    /// Declared return types by the name of the declaration the function belongs to.
    pub declared_returns: Vec<StaticDeclaredReturn>,
    /// Names bound from a call: the bound name, the key it is destructured by (absent when
    /// the name takes the whole result) and the call.
    pub destructured: Vec<(ByteRange, Option<ByteRange>, ByteRange)>,
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
    /// Name and member targets of assignments whose value is a function written in place.
    pub function_writes: Vec<ByteRange>,
    /// Plain assignments copying a member from `this`.
    pub member_copies: Vec<StaticMemberCopy>,
    /// Declaration names whose written type is an array, with the named element type.
    pub element_types: Vec<(ByteRange, ByteRange)>,
    /// Declarations whose function writes an array return type, with the named element
    /// type: the owning declaration's name and the element type.
    pub element_returns: Vec<(ByteRange, ByteRange)>,
    /// Functions written as members of an object literal, with that literal: `this` inside
    /// them is the literal.
    pub object_members: Vec<(ByteRange, ByteRange)>,
    /// Members written in an ECMAScript object literal, type literal or class.
    pub body_members: Vec<StaticBodyMember>,
    /// What each `return` of a function returns, by the name of the declaration the
    /// function belongs to: the object literal it returns, or none for any other value.
    pub returned_objects: Vec<(ByteRange, Option<ByteRange>)>,
    /// Declared names whose initializer is an object literal that ends in a spread of a
    /// name, with that spread name: `const o = { a: 1, ...base }` gives `(o, base)`.
    pub spread_values: Vec<(ByteRange, ByteRange)>,
    /// Scopes with unsupported binding shapes.
    pub unsupported_bindings: Vec<ByteRange>,
    /// Rust use paths.
    pub rust_uses: Vec<StaticRustUse>,
    /// Rust value paths and named pattern terminals.
    pub rust_value_reads: Vec<StaticRustValueRead>,
    /// Rust value paths written directly as call arguments.
    pub rust_argument_value_reads: Vec<ByteRange>,
    /// Direct uses of generic parameters in type annotations.
    pub rust_generic_references: Vec<StaticRustGenericReference>,
    /// Written crate-root tokens in paths.
    pub rust_crate_paths: Vec<ByteRange>,
    /// Written crate-root tokens in restricted visibility.
    pub rust_crate_visibility: Vec<ByteRange>,
    /// Written relative paths and visibility qualifiers naming the immediate parent.
    pub rust_parent_module_references: Vec<ByteRange>,
    /// Hoisted block-local value names with their scopes.
    pub rust_local_values: Vec<(ByteRange, ByteRange)>,
    /// Block-local type and module items with their lexical blocks.
    pub rust_local_types: Vec<(ByteRange, ByteRange)>,
    /// Rust field labels in struct literals and patterns.
    pub rust_field_sites: Vec<StaticRustFieldSite>,
    /// Rust macro calls with a written crate-root qualifier.
    pub rust_root_macro_calls: Vec<StaticRustMacroCall>,
    /// Rust macro calls with a single unqualified name.
    pub rust_textual_macro_calls: Vec<StaticRustMacroCall>,
    /// Rust macro declarations with their textual scopes.
    pub rust_macro_definitions: Vec<StaticRustMacroDefinition>,
    /// Items and scopes where expansion or module loading may introduce macros.
    pub rust_macro_barriers: Vec<(ByteRange, ByteRange)>,
    /// Authored Rust implementations.
    pub inherent_impls: Vec<StaticInherentImpl>,
    /// Items whose admission depends on Rust configuration.
    pub conditional_items: Vec<StaticConditionalItem>,
    /// Rust path calls and the range of their first argument, which selects the
    /// implementation of a `Trait::method(value, ..)` call.
    pub receiver_arguments: Vec<(ByteRange, ByteRange)>,
    /// Python loop statements, whose bodies run again after their later statements.
    pub loops: Vec<ByteRange>,
    /// Python lambda, comprehension and generator expression scopes, whose bodies can run
    /// after later statements of the enclosing scope.
    pub expression_scopes: Vec<ByteRange>,
    /// Python scopes and the parts of each that the enclosing scope evaluates. Includes
    /// function defaults evaluated outside the type parameter scope.
    pub evaluated_outside: Vec<(ByteRange, ByteRange)>,
    /// Python names deleted in a function, with the function and the block that deletes them.
    pub python_deletions: Vec<PythonDeletion>,
    /// Plain Python reads paired with their earlier binding name in the same function.
    pub python_local_reads: Vec<(ByteRange, ByteRange)>,
    /// Plain reads in a nested Python function paired with an earlier outer local name.
    pub python_captured_local_reads: Vec<(ByteRange, ByteRange)>,
    /// Reads of a Python method receiver paired with its parameter name.
    pub python_receiver_reads: Vec<(ByteRange, ByteRange)>,
    /// Reads of Python variadic parameters paired with their declared names.
    pub python_variadic_reads: Vec<(ByteRange, ByteRange)>,
    /// Authored class fields for a possible generated Python initializer.
    pub python_initializers: Vec<StaticPythonInitializer>,
    /// The call and keyword name ranges of Python keyword arguments.
    pub python_keyword_arguments: Vec<(ByteRange, ByteRange)>,
    /// Python names checked for a non-`None` value and the bodies the checks admit.
    pub non_none_guards: Vec<(ByteRange, ByteRange)>,
    /// Rust calls with a closure argument and the range of every argument in order, which
    /// relate the closure's parameters to the callee's declared parameters.
    pub closure_calls: Vec<(ByteRange, Vec<ByteRange>)>,
}

/// Calls and binding evidence retained after the syntax tree is released.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFacts(StaticFactsParts);

/// A Python `del` target and the block in its function that executes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythonDeletion {
    /// The deleted name.
    pub name: ByteRange,
    /// The function that contains the statement.
    pub function: ByteRange,
    /// The block that contains the statement.
    pub block: ByteRange,
}

impl StaticFacts {
    /// Whether captured ECMAScript syntax requires module interpretation without an explicit mode.
    #[must_use]
    pub fn has_javascript_module_syntax(&self) -> bool {
        self.0.javascript_module_syntax
    }

    /// Whether an import or export statement stands directly in the ECMAScript source file,
    /// outside every ambient module and namespace.
    #[must_use]
    pub fn has_javascript_module_statement(&self) -> bool {
        self.0.javascript_module_statement
    }

    /// Assignments to `module.exports` or a computed key of `module` that run while the file
    /// does, in no function called later, and whether no condition encloses them.
    #[must_use]
    pub fn commonjs_writes(&self) -> &[(ByteRange, bool)] {
        &self.0.commonjs_writes
    }

    /// Source a `return` of the file or of an immediately called function can skip.
    #[must_use]
    pub fn skippable(&self) -> &[ByteRange] {
        &self.0.skippable
    }

    /// The direct default write, if the `CommonJS` wrapper remains closed.
    #[must_use]
    pub const fn commonjs_export(&self) -> Option<&StaticCommonJsExport> {
        self.0.commonjs_export.as_ref()
    }

    /// Captured value accesses and type references.
    #[must_use]
    pub fn references(&self) -> &[StaticReference] {
        &self.0.references
    }

    /// The boundary between established Rust reference rows and bare local reads.
    ///
    /// The Rust producer appends its plain reads. Earlier Rust reads name field paths
    /// and preserve the dotted expression's range. The supplied source must be the
    /// exact bytes these facts were captured from.
    #[must_use]
    pub fn rust_local_reference_start(&self, source: &str) -> usize {
        self.references().partition_point(|reference| {
            reference.role != rift_core::ReferenceRole::Read
                || !usize::try_from(reference.target.start)
                    .ok()
                    .zip(usize::try_from(reference.target.end).ok())
                    .and_then(|(start, end)| source.get(start..end))
                    .is_some_and(crate::rust::local_bindings::plain_name)
        })
    }

    /// Rust value paths written directly as call arguments.
    #[must_use]
    pub fn rust_argument_value_reads(&self) -> &[ByteRange] {
        &self.0.rust_argument_value_reads
    }

    /// Direct uses of generic parameters in type annotations.
    #[must_use]
    pub fn rust_generic_references(&self) -> &[StaticRustGenericReference] {
        &self.0.rust_generic_references
    }

    /// Written crate-root tokens in paths.
    #[must_use]
    pub fn rust_crate_paths(&self) -> &[ByteRange] {
        &self.0.rust_crate_paths
    }

    /// Written crate-root tokens in restricted visibility.
    #[must_use]
    pub fn rust_crate_visibility(&self) -> &[ByteRange] {
        &self.0.rust_crate_visibility
    }

    /// Written relative paths and visibility qualifiers naming the immediate parent.
    #[must_use]
    pub fn rust_parent_module_references(&self) -> &[ByteRange] {
        &self.0.rust_parent_module_references
    }

    pub(crate) fn set_rust_generic_references(
        &mut self,
        mut references: Vec<StaticRustGenericReference>,
    ) {
        references.sort_by_key(|reference| reference.range);
        self.0.rust_generic_references = references;
    }

    pub(crate) fn add_rust_local_references(&mut self, references: Vec<StaticReference>) {
        self.0.references.extend(references);
    }

    /// Captured member expressions with their original receiver and property spans.
    #[must_use]
    pub fn members(&self) -> &[StaticMember] {
        &self.0.members
    }

    /// Callable member aliases established from assignment chains.
    #[must_use]
    pub fn member_aliases(&self) -> &[StaticMemberAlias] {
        &self.0.member_aliases
    }

    pub(crate) fn set_member_aliases(&mut self, aliases: Vec<StaticMemberAlias>) {
        self.0.member_aliases = aliases;
    }
    /// Declaration names and their explicit named type annotations.
    #[must_use]
    pub fn declared_types(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.declared_types
    }

    /// Type declarations whose own shape is a call (`false`) or construct (`true`) signature.
    #[must_use]
    pub fn type_signatures(&self) -> &[(ByteRange, bool)] {
        &self.0.type_signatures
    }

    /// Simple parameters of functions, signatures and function types, by position.
    #[must_use]
    pub fn parameters(&self) -> &[StaticParameter] {
        &self.0.parameters
    }

    /// Declaration names whose written type is a function type, with that type.
    #[must_use]
    pub fn function_types(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.function_types
    }

    /// Function-like syntax and the named type its return annotation writes.
    #[must_use]
    pub fn return_types(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.return_types
    }

    /// Function values and the written place that types them.
    #[must_use]
    pub fn function_contexts(&self) -> &[(ByteRange, StaticFunctionContext)] {
        &self.0.function_contexts
    }

    /// Parameters typed by an object literal's assignment to a member.
    #[must_use]
    pub fn member_callbacks(&self) -> &[StaticMemberCallback] {
        &self.0.member_callbacks
    }

    /// Names of type and property declarations with their explicit object type bodies.
    #[must_use]
    pub fn member_type_shapes(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.member_type_shapes
    }

    /// Declared return types by the name of the declaration the function belongs to.
    #[must_use]
    pub fn declared_returns(&self) -> &[StaticDeclaredReturn] {
        &self.0.declared_returns
    }

    /// Names bound from a call: the bound name, the key it is destructured by (absent when
    /// the name takes the whole result) and the call.
    #[must_use]
    pub fn destructured(&self) -> &[(ByteRange, Option<ByteRange>, ByteRange)] {
        &self.0.destructured
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

    /// Name and member targets of assignments whose value is a function written in place.
    #[must_use]
    pub fn function_writes(&self) -> &[ByteRange] {
        &self.0.function_writes
    }

    /// Authored same-member copies in order of the assignment target.
    #[must_use]
    pub fn member_copies(&self) -> &[StaticMemberCopy] {
        &self.0.member_copies
    }

    /// Declaration names written with an array type, and the named element type.
    #[must_use]
    pub fn element_types(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.element_types
    }

    /// Declarations whose function returns a written array type, and its element type.
    #[must_use]
    pub fn element_returns(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.element_returns
    }

    /// Functions written as members of an object literal, and the literal.
    #[must_use]
    pub fn object_members(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.object_members
    }

    /// Members written in an ECMAScript object literal, type literal or class.
    #[must_use]
    pub fn body_members(&self) -> &[StaticBodyMember] {
        &self.0.body_members
    }

    /// What each `return` of a function returns: the object literal, or none.
    #[must_use]
    pub fn returned_objects(&self) -> &[(ByteRange, Option<ByteRange>)] {
        &self.0.returned_objects
    }

    /// Names initialized by an object literal ending in a spread of a name, with that name.
    #[must_use]
    pub fn spread_values(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.spread_values
    }

    /// Scopes where an unsupported binding can affect lookup.
    #[must_use]
    pub fn unsupported_bindings(&self) -> &[ByteRange] {
        &self.0.unsupported_bindings
    }

    /// Rust path calls with the range of their first argument.
    #[must_use]
    pub fn receiver_arguments(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.receiver_arguments
    }

    /// Rust calls with a closure argument, with every argument range in order.
    #[must_use]
    pub fn closure_calls(&self) -> &[(ByteRange, Vec<ByteRange>)] {
        &self.0.closure_calls
    }

    /// Rust use paths with their captured group components.
    #[must_use]
    pub fn rust_uses(&self) -> &[StaticRustUse] {
        &self.0.rust_uses
    }

    /// Rust paths written as values or named patterns.
    #[must_use]
    pub fn rust_value_reads(&self) -> &[StaticRustValueRead] {
        &self.0.rust_value_reads
    }

    /// Named Rust fields in literals and patterns.
    #[must_use]
    pub fn rust_field_sites(&self) -> &[StaticRustFieldSite] {
        &self.0.rust_field_sites
    }

    /// Authored Rust macro calls with a crate-root qualifier.
    #[must_use]
    pub fn rust_root_macro_calls(&self) -> &[StaticRustMacroCall] {
        &self.0.rust_root_macro_calls
    }

    /// Authored Rust macro calls with a bare name.
    #[must_use]
    pub fn rust_textual_macro_calls(&self) -> &[StaticRustMacroCall] {
        &self.0.rust_textual_macro_calls
    }

    /// Macro declarations with their textual scopes.
    #[must_use]
    pub fn rust_macro_definitions(&self) -> &[StaticRustMacroDefinition] {
        &self.0.rust_macro_definitions
    }

    /// Items and scopes with potentially new macro bindings.
    #[must_use]
    pub fn rust_macro_barriers(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.rust_macro_barriers
    }

    /// Hoisted block-local value names and their scopes.
    #[must_use]
    pub fn rust_local_values(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.rust_local_values
    }

    /// Block-local type and module item ranges with their scopes.
    #[must_use]
    pub fn rust_local_types(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.rust_local_types
    }

    /// Rust implementation headers.
    #[must_use]
    pub fn inherent_impls(&self) -> &[StaticInherentImpl] {
        &self.0.inherent_impls
    }

    /// Authored Rust items requiring configuration evidence.
    #[must_use]
    pub fn conditional_items(&self) -> &[StaticConditionalItem] {
        &self.0.conditional_items
    }

    /// Python loop statements.
    #[must_use]
    pub fn loops(&self) -> &[ByteRange] {
        &self.0.loops
    }

    /// Python lambda, comprehension and generator expression scopes.
    #[must_use]
    pub fn expression_scopes(&self) -> &[ByteRange] {
        &self.0.expression_scopes
    }

    /// Python scopes and the ranges evaluated in their enclosing scopes.
    #[must_use]
    pub fn evaluated_outside(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.evaluated_outside
    }

    /// Python `del` targets and their function and block.
    #[must_use]
    pub fn python_deletions(&self) -> &[PythonDeletion] {
        &self.0.python_deletions
    }

    /// Plain Python reads and the direct local names their bindings could select.
    #[must_use]
    pub fn python_local_reads(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.python_local_reads
    }

    /// Plain Python reads and their candidate binding in the enclosing function.
    #[must_use]
    pub fn python_captured_local_reads(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.python_captured_local_reads
    }

    /// Reads of a Python method receiver and the available parameter name.
    #[must_use]
    pub fn python_receiver_reads(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.python_receiver_reads
    }

    /// Python variadic parameter reads and the names their bindings select.
    #[must_use]
    pub fn python_variadic_reads(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.python_variadic_reads
    }

    /// The authored fields an initializer may expose as keyword arguments.
    #[must_use]
    pub fn python_initializers(&self) -> &[StaticPythonInitializer] {
        &self.0.python_initializers
    }

    /// The call and name ranges for Python keyword arguments.
    #[must_use]
    pub fn python_keyword_arguments(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.python_keyword_arguments
    }

    pub(crate) fn retain_python_local_reads(&mut self, keep: impl Fn(ByteRange) -> bool) {
        self.0.python_local_reads.retain(|&(_, name)| keep(name));
        self.0.python_receiver_reads.retain(|&(_, name)| keep(name));
        self.0.python_variadic_reads.retain(|&(_, name)| keep(name));
    }

    /// Python names checked for a non-`None` value and the bodies the checks admit.
    #[must_use]
    pub fn non_none_guards(&self) -> &[(ByteRange, ByteRange)] {
        &self.0.non_none_guards
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        [
            self.commonjs_export()
                .map_or(0, |export| vector_capacity(&export.initialized)),
            vector_capacity(&self.0.references),
            vector_capacity(&self.0.members),
            vector_capacity(&self.0.member_aliases),
            vector_capacity(&self.0.declared_types),
            vector_capacity(&self.0.type_signatures),
            vector_capacity(&self.0.parameters),
            vector_capacity(&self.0.function_types),
            vector_capacity(&self.0.return_types),
            vector_capacity(&self.0.function_contexts),
            vector_capacity(&self.0.member_callbacks),
            vector_capacity(&self.0.member_type_shapes),
            vector_capacity(&self.0.declared_returns),
            vector_capacity(&self.0.destructured),
            vector_capacity(&self.0.function_values),
            vector_capacity(&self.0.function_scopes),
            vector_capacity(&self.0.static_blocks),
            vector_capacity(&self.0.heritage_edges),
            vector_capacity(&self.0.calls),
            vector_capacity(&self.0.bindings),
            vector_capacity(&self.0.reexports),
            vector_capacity(&self.0.assignments),
            vector_capacity(&self.0.function_writes),
            vector_capacity(&self.0.member_copies),
            vector_capacity(&self.0.element_types),
            vector_capacity(&self.0.element_returns),
            vector_capacity(&self.0.object_members),
            vector_capacity(&self.0.body_members),
            vector_capacity(&self.0.returned_objects),
            vector_capacity(&self.0.spread_values),
            vector_capacity(&self.0.unsupported_bindings),
            vector_capacity(&self.0.inherent_impls),
            vector_capacity(&self.0.rust_uses),
            vector_capacity(&self.0.rust_value_reads),
            vector_capacity(&self.0.rust_argument_value_reads),
            vector_capacity(&self.0.rust_generic_references),
            vector_capacity(&self.0.rust_crate_paths),
            vector_capacity(&self.0.rust_crate_visibility),
            vector_capacity(&self.0.rust_parent_module_references),
            vector_capacity(&self.0.rust_local_values),
            vector_capacity(&self.0.rust_local_types),
            vector_capacity(&self.0.rust_field_sites),
            vector_capacity(&self.0.rust_root_macro_calls),
            vector_capacity(&self.0.rust_textual_macro_calls),
            vector_capacity(&self.0.rust_macro_definitions),
            vector_capacity(&self.0.rust_macro_barriers),
            vector_capacity(&self.0.conditional_items),
            vector_capacity(&self.0.receiver_arguments),
            vector_capacity(&self.0.loops),
            vector_capacity(&self.0.expression_scopes),
            vector_capacity(&self.0.evaluated_outside),
            vector_capacity(&self.0.python_deletions),
            vector_capacity(&self.0.python_local_reads),
            vector_capacity(&self.0.python_captured_local_reads),
            vector_capacity(&self.0.python_receiver_reads),
            vector_capacity(&self.0.python_variadic_reads),
            vector_capacity(&self.0.python_initializers),
            vector_capacity(&self.0.python_keyword_arguments),
            vector_capacity(&self.0.non_none_guards),
            vector_capacity(&self.0.commonjs_writes),
            vector_capacity(&self.0.skippable),
            vector_capacity(&self.0.closure_calls),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
        .saturating_add(
            self.python_initializers()
                .iter()
                .fold(0_usize, |bytes, initializer| {
                    bytes.saturating_add(vector_capacity(&initializer.fields))
                }),
        )
        .saturating_add(
            self.0
                .rust_uses
                .iter()
                .map(|row| vector_capacity(&row.path))
                .fold(0, usize::saturating_add),
        )
        .saturating_add(
            self.0
                .closure_calls
                .iter()
                .map(|(_, arguments)| vector_capacity(arguments))
                .fold(0, usize::saturating_add),
        )
    }

    fn validate_counts(&self, limits: &SyntaxLimits) -> Result<(), RiftError> {
        let lengths = [
            self.commonjs_export()
                .map_or(0, |export| export.initialized.len()),
            self.references().len(),
            self.members().len(),
            self.member_aliases().len(),
            self.declared_types().len(),
            self.type_signatures().len(),
            self.parameters().len(),
            self.function_types().len(),
            self.return_types().len(),
            self.function_contexts().len(),
            self.member_callbacks().len(),
            self.member_type_shapes().len(),
            self.declared_returns().len(),
            self.destructured().len(),
            self.function_values().len(),
            self.function_scopes().len(),
            self.static_blocks().len(),
            self.heritage_edges().len(),
            self.calls().len(),
            self.bindings().len(),
            self.reexports().len(),
            self.assignments().len(),
            self.function_writes().len(),
            self.member_copies().len(),
            self.element_types().len(),
            self.element_returns().len(),
            self.object_members().len(),
            self.body_members().len(),
            self.returned_objects().len(),
            self.spread_values().len(),
            self.unsupported_bindings().len(),
            self.inherent_impls().len(),
            self.rust_uses().len(),
            self.rust_value_reads().len(),
            self.rust_argument_value_reads().len(),
            self.rust_generic_references().len(),
            self.rust_crate_paths().len(),
            self.rust_crate_visibility().len(),
            self.rust_parent_module_references().len(),
            self.rust_local_values().len(),
            self.rust_local_types().len(),
            self.rust_field_sites().len(),
            self.rust_root_macro_calls().len(),
            self.rust_textual_macro_calls().len(),
            self.rust_macro_definitions().len(),
            self.rust_macro_barriers().len(),
            self.conditional_items().len(),
            self.receiver_arguments().len(),
            self.loops().len(),
            self.expression_scopes().len(),
            self.evaluated_outside().len(),
            self.python_deletions().len(),
            self.python_local_reads().len(),
            self.python_captured_local_reads().len(),
            self.python_receiver_reads().len(),
            self.python_variadic_reads().len(),
            self.python_initializers().len(),
            self.python_keyword_arguments().len(),
            self.non_none_guards().len(),
            self.commonjs_writes().len(),
            self.skippable().len(),
            self.closure_calls().len(),
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
        Ok(())
    }

    /// Rust `use` rows, configuration items, path-call receiver arguments and closure calls.
    fn rust_rows_valid(&self, valid: &impl Fn(ByteRange) -> bool, limits: SyntaxLimits) -> bool {
        let imports = self.rust_uses().iter().all(|row| {
            valid(row.range)
                && row.visibility.is_none_or(valid)
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
        let conditional = self
            .conditional_items()
            .iter()
            .all(|item| valid(item.range) && valid(item.attribute))
            && self.receiver_arguments().iter().all(|(call, argument)| {
                valid(*call) && valid(*argument) && contains(*call, *argument)
            })
            && self.closure_calls().iter().all(|(call, arguments)| {
                valid(*call)
                    && arguments.len() <= limits.syntax_elements_max()
                    && arguments
                        .iter()
                        .all(|argument| valid(*argument) && contains(*call, *argument))
            });
        imports
            && conditional
            && self.rust_value_reads().iter().all(|read| valid(read.range))
            && self.rust_argument_value_reads().iter().copied().all(valid)
            && self
                .rust_generic_references()
                .windows(2)
                .all(|pair| pair[0].range < pair[1].range)
            && self.rust_generic_references().iter().all(|reference| {
                valid(reference.range)
                    && valid(reference.binding)
                    && matches!(reference.role, ReferenceRole::Read | ReferenceRole::Type)
            })
            && self
                .rust_local_values()
                .iter()
                .all(|(name, scope)| valid(*name) && valid(*scope) && scope.encloses(*name))
            && self
                .rust_local_types()
                .iter()
                .all(|(item, scope)| valid(*item) && valid(*scope) && scope.encloses(*item))
            && self.rust_crate_paths().iter().copied().all(valid)
            && self.rust_crate_visibility().iter().copied().all(valid)
            && self
                .rust_parent_module_references()
                .iter()
                .copied()
                .all(valid)
            && self.rust_macro_ranges_valid(valid)
            && self.rust_field_sites().iter().all(|site| {
                valid(site.owner)
                    && valid(site.field)
                    && site.owner.end <= site.field.start
                    && site.literal.is_none_or(|literal| {
                        valid(literal)
                            && literal.encloses(site.owner)
                            && literal.encloses(site.field)
                    })
            })
    }

    fn rust_macro_ranges_valid(&self, valid: &impl Fn(ByteRange) -> bool) -> bool {
        self.rust_root_macro_calls()
            .iter()
            .chain(self.rust_textual_macro_calls())
            .all(|call| valid(call.range) && valid(call.name) && call.range.encloses(call.name))
            && self.rust_macro_definitions().iter().all(|definition| {
                valid(definition.name)
                    && valid(definition.item)
                    && valid(definition.scope)
                    && definition.item.encloses(definition.name)
                    && definition.scope.encloses(definition.item)
            })
            && self
                .rust_macro_barriers()
                .iter()
                .all(|(item, scope)| valid(*item) && valid(*scope) && scope.encloses(*item))
    }

    /// Whether the facts that connect function values and bound names to written types
    /// hold source ranges.
    fn typed_values_valid(&self, valid: impl Fn(ByteRange) -> bool + Copy) -> bool {
        self.parameters().iter().all(|parameter| {
            valid(parameter.function) && valid(parameter.name) && parameter.owner.is_none_or(valid)
        }) && self
            .function_types()
            .iter()
            .chain(self.return_types())
            .chain(self.member_type_shapes())
            .all(|(name, written)| valid(*name) && valid(*written))
            && self.member_callbacks().iter().all(|context| {
                valid(context.parameter)
                    && valid(context.property)
                    && valid(context.callback)
                    && valid(context.asserted)
                    && context.intersection.is_none_or(valid)
            })
            && self.declared_returns().iter().all(|declared| {
                valid(declared.owner)
                    && valid(declared.function)
                    && valid(declared.annotation)
                    && declared.member.is_none_or(valid)
            })
            && self
                .destructured()
                .iter()
                .all(|(name, key, call)| valid(*name) && key.is_none_or(valid) && valid(*call))
            && self.function_contexts().iter().all(|(function, context)| {
                valid(*function)
                    && match context {
                        StaticFunctionContext::Initializer(range)
                        | StaticFunctionContext::Returned(range)
                        | StaticFunctionContext::Argument { call: range, .. } => valid(*range),
                    }
            })
    }

    /// Whether assignment targets, array element types and object literal members hold
    /// source ranges.
    fn written_values_valid(&self, valid: impl Fn(ByteRange) -> bool + Copy) -> bool {
        self.assignments()
            .iter()
            .chain(self.function_writes())
            .chain(self.commonjs_writes().iter().map(|(write, _)| write))
            .chain(self.skippable())
            .copied()
            .all(valid)
            && self.member_copies().iter().all(|copy| {
                valid(copy.assignment)
                    && valid(copy.target)
                    && valid(copy.value)
                    && copy.assignment.start == copy.target.start
                    && copy.assignment.end == copy.value.end
                    && copy.target.end < copy.value.start
            })
            && self
                .member_copies()
                .windows(2)
                .all(|pair| pair[0].target.start < pair[1].target.start)
            && self
                .element_types()
                .iter()
                .chain(self.element_returns())
                .all(|(name, element)| valid(*name) && valid(*element))
            && self
                .object_members()
                .iter()
                .all(|(function, object)| valid(*function) && contains(*object, *function))
            && self.body_members().iter().all(|row| {
                valid(row.member)
                    && contains(row.body, row.member)
                    && contains(row.holder, row.body)
            })
            && self
                .returned_objects()
                .iter()
                .all(|(owner, object)| valid(*owner) && object.is_none_or(valid))
            && self
                .spread_values()
                .iter()
                .all(|(name, spread)| valid(*name) && valid(*spread))
    }

    pub(crate) fn validate(&self, source: &str, limits: SyntaxLimits) -> Result<(), RiftError> {
        self.validate_counts(&limits)?;
        let valid = |range: ByteRange| {
            let start = usize::try_from(range.start).ok();
            let end = usize::try_from(range.end).ok();
            start
                .zip(end)
                .is_some_and(|(start, end)| start < end && source.get(start..end).is_some())
        };
        if self.commonjs_export().is_some_and(|export| {
            !valid(export.assignment)
                || !export.initialized.iter().copied().all(valid)
                || !export.initialized.windows(2).all(|pair| pair[0] < pair[1])
        }) {
            return errors::syntax::facts_range_invalid().fail();
        }
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
        let declared_types = self
            .declared_types()
            .iter()
            .all(|(name, annotation)| valid(*name) && valid(*annotation))
            && self.type_signatures().iter().all(|(name, _)| valid(*name))
            && self.typed_values_valid(valid);
        let assignments = self.written_values_valid(valid);
        let members = self.members().iter().all(|member| {
            valid(member.range)
                && valid(member.receiver)
                && member.range.encloses(member.receiver)
                && member.member.is_none_or(|name| {
                    valid(name) && member.range.encloses(name) && member.receiver.end <= name.start
                })
        });
        let unsupported = self.unsupported_bindings().iter().copied().all(valid);
        let implementations = self.inherent_impls().iter().all(|row| {
            valid(row.range) && valid(row.type_path) && contains(row.range, row.type_path)
        });
        let rust = self.rust_rows_valid(&valid, limits);
        let flow = self.python_flow_valid(valid)
            && self.python_local_reads_valid(source, valid)
            && self.python_initializers_valid(valid, limits);
        if !(edges
            && references
            && declared_types
            && functions
            && calls
            && bindings
            && reexports
            && assignments
            && members
            && self.member_aliases().iter().all(|alias| {
                valid(alias.name) && valid(alias.target) && alias.name.end <= alias.target.start
            })
            && self
                .member_aliases()
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
            && unsupported
            && implementations
            && rust
            && flow)
        {
            return errors::syntax::facts_range_invalid().fail();
        }
        Ok(())
    }

    fn python_initializers_valid(
        &self,
        valid: impl Fn(ByteRange) -> bool,
        limits: SyntaxLimits,
    ) -> bool {
        self.python_initializers().iter().all(|initializer| {
            valid(initializer.class)
                && valid(initializer.decorator)
                && initializer.decorator.end <= initializer.class.start
                && initializer
                    .base
                    .is_none_or(|base| valid(base) && initializer.class.encloses(base))
                && initializer.fields.len() <= limits.syntax_elements_max()
                && initializer.fields.iter().all(|field| {
                    valid(field.name)
                        && valid(field.annotation)
                        && initializer.class.encloses(field.name)
                        && initializer.class.encloses(field.annotation)
                        && field.name.end <= field.annotation.start
                })
        }) && self
            .python_keyword_arguments()
            .iter()
            .all(|&(call, name)| valid(call) && valid(name) && call.encloses(name))
    }

    fn python_local_reads_valid(&self, source: &str, valid: impl Fn(ByteRange) -> bool) -> bool {
        let spelling = |range: ByteRange| {
            source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
        };
        [
            self.python_local_reads(),
            self.python_captured_local_reads(),
            self.python_receiver_reads(),
            self.python_variadic_reads(),
        ]
        .into_iter()
        .all(|reads| {
            reads
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start)
                && reads.iter().all(|&(read, name)| {
                    valid(read)
                        && valid(name)
                        && name.end <= read.start
                        && spelling(read) == spelling(name)
                })
        })
    }

    fn python_flow_valid(&self, valid: impl Fn(ByteRange) -> bool) -> bool {
        self.loops()
            .iter()
            .chain(self.expression_scopes())
            .copied()
            .all(&valid)
            && self
                .evaluated_outside()
                .iter()
                .all(|(scope, iterable)| valid(*iterable) && contains(*scope, *iterable))
            && self.python_deletions().iter().all(|deletion| {
                valid(deletion.function)
                    && valid(deletion.block)
                    && valid(deletion.name)
                    && contains(deletion.function, deletion.block)
                    && contains(deletion.block, deletion.name)
            })
            && self
                .non_none_guards()
                .iter()
                .all(|(name, body)| valid(*name) && valid(*body) && name.end <= body.start)
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
            StaticBindingKind::JavaScriptTypeImport { module, export } => {
                valid(module) && valid(export)
            }
            StaticBindingKind::JavaScriptClassValue { body } => {
                valid(body) && self.scope.encloses(body) && self.name.end <= body.start
            }
            StaticBindingKind::JavaScriptInstance { constructor }
            | StaticBindingKind::JavaScriptConstInstance { constructor }
            | StaticBindingKind::PythonInstance { constructor } => valid(constructor),
            StaticBindingKind::PythonReceiver { class_name } => valid(class_name),
            StaticBindingKind::PythonAnnotation { annotation }
            | StaticBindingKind::PythonGuard { annotation } => valid(annotation),
            StaticBindingKind::PythonFunction {
                annotation,
                decorator,
            } => valid(annotation) && decorator.is_none_or(&valid),
            StaticBindingKind::PythonAlias { value } => valid(value),
            StaticBindingKind::PythonElement { iterable, position } => {
                valid(iterable) && position.is_none_or(|position| position.index < position.count)
            }
            StaticBindingKind::PythonUnpacked { value, position } => {
                valid(value) && position.index < position.count
            }
            StaticBindingKind::PythonMemberWrite { receiver } => valid(receiver),
            StaticBindingKind::RustUse { path } => valid(path),
            StaticBindingKind::RustExternCrate { crate_name } => {
                valid(crate_name)
                    && self.scope.encloses(crate_name)
                    && (crate_name == self.name || crate_name.end <= self.name.start)
            }
            StaticBindingKind::RustReceiver { type_path } => valid(type_path),
            StaticBindingKind::RustValue { initializer } => valid(initializer),
            StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::PythonNone
            | StaticBindingKind::PythonEmptyList
            | StaticBindingKind::PythonGlobal
            | StaticBindingKind::PythonNonlocal
            | StaticBindingKind::RustBlockType
            | StaticBindingKind::RustTypeParameter => true,
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

enum Grammar<'source> {
    Python(
        &'static python_edges::EdgePythonKinds,
        python_edges::Names<'source>,
    ),
    JavaScript(&'static javascript::Kinds, javascript::Documentation),
    /// With the places the parser could not read.
    Rust(
        &'static rust::Kinds,
        rift_core::LoopBudget,
        Vec<ByteRange>,
        rust::Macros<'source>,
    ),
}

pub(crate) struct Capture<'source> {
    grammar: Grammar<'source>,
    source: &'source str,
    facts: StaticFacts,
    edge_heritage: &'static heritage_edges::EdgeHeritageKinds,
}

impl<'source> Capture<'source> {
    pub(crate) fn for_javascript(source: &'source str) -> Self {
        Self {
            grammar: Grammar::JavaScript(javascript::kinds(), javascript::Documentation::default()),
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
            grammar: Grammar::JavaScript(
                javascript::edge_typescript_kinds(dialect),
                javascript::Documentation::default(),
            ),
            edge_heritage: heritage_edges::edge_typescript_kinds(dialect),
            source,
            facts: StaticFacts::default(),
        }
    }

    pub(crate) fn for_edge_python(source: &'source str) -> Self {
        Self {
            grammar: Grammar::Python(
                python_edges::edge_python_kinds(),
                python_edges::Names::default(),
            ),
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
                Vec::new(),
                rust::Macros::default(),
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
        if (syntax_element.is_error() || syntax_element.is_missing())
            && let Grammar::Rust(kinds, _, unread, _) = &mut self.grammar
        {
            kinds.record_error(visited, self.source, unread, &mut self.facts.0)?;
        }
        if syntax_element.is_missing() {
            return Ok(());
        }
        if syntax_element.is_extra() {
            if let Grammar::JavaScript(kinds, documentation) = &mut self.grammar {
                kinds.record_comment(visited, self.source, documentation, &mut self.facts.0)?;
            }
            return Ok(());
        }
        self.edge_heritage.record_edge(visited, &mut self.facts.0)?;
        match &mut self.grammar {
            Grammar::Python(kinds, names) => {
                kinds.record_edge(visited, self.source, &mut self.facts.0, names)
            }
            Grammar::JavaScript(kinds, documentation) => {
                kinds.record(visited, self.source, documentation, &mut self.facts.0)
            }
            Grammar::Rust(kinds, budget, _, macros) => {
                kinds.record(visited, self.source, &mut self.facts.0, budget, macros)
            }
        }
    }

    pub(crate) fn finish(mut self) -> StaticFacts {
        match self.grammar {
            Grammar::Rust(_, _, unread, _) => rust::Kinds::close_unread(unread, &mut self.facts.0),
            Grammar::JavaScript(_, _) => {
                javascript::settle_class_values(self.source, &mut self.facts.0);
            }
            Grammar::Python(_, _) => {}
        }
        self.facts
    }

    /// Finishes a Python capture with the facts that need the whole tree.
    pub(crate) fn finish_python(
        mut self,
        root: tree_sitter::Node<'_>,
    ) -> Result<(StaticFacts, bool), RiftError> {
        let Grammar::Python(kinds, names) = self.grammar else {
            return Ok((self.facts, false));
        };
        kinds.close_lists(root, self.source, &mut self.facts.0)?;
        let invalid_default = names.invalid_type_parameter_default;
        names.close(self.source, &mut self.facts.0);
        Ok((self.facts, invalid_default))
    }

    pub(crate) fn record_commonjs_export(&mut self, export: Option<StaticCommonJsExport>) {
        self.facts.0.commonjs_export = export;
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

/// The nearest ancestor of one of `kinds`, read from the parents the walk recorded.
///
/// `Node::parent` searches again from the root on every call, so a capture rule that
/// asks it for every syntax element costs the tree's width and depth each time.
fn enclosing<'walk, 'tree>(
    visited: Visited<'walk, 'tree>,
    kinds: &[u16],
) -> Option<Visited<'walk, 'tree>> {
    let mut parent = visited.parent();
    // Capture runs after the shared syntax walk has admitted each syntax element's depth.
    while let Some(candidate) = parent {
        if kinds.contains(&candidate.syntax_element().kind_id()) {
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

#[cfg(test)]
mod rust_local_read_tests {
    use super::StaticFacts;
    use crate::rust::RustSyntaxProvider;
    use crate::{SyntaxLimits, SyntaxProvider, SyntaxSource};

    #[test]
    fn test_rust_local_reads_restore_after_existing_field_references() {
        let source = "struct Item { value: u8 }\nfn choose(item: Item) -> u8 { let local = item.value; local + item.value }\n";
        let path = rift_core::ProjectPath::new("src/lib.rs").expect("path");
        let limits = SyntaxLimits::default();
        let parsed = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                limits,
            )
            .expect("original Rust source");
        let facts = parsed.facts().static_facts().expect("captured facts");
        let restored = StaticFacts::from_parts(source, limits, facts.0.clone())
            .expect("checked original references");
        let split = restored.rust_local_reference_start(source);
        let written = |reference: &super::StaticReference| {
            &source[usize::try_from(reference.target.start).expect("start")
                ..usize::try_from(reference.target.end).expect("end")]
        };
        assert_eq!(
            restored.references()[..split]
                .iter()
                .map(written)
                .collect::<Vec<_>>(),
            ["Item", "item.value", "item.value"]
        );
        assert_eq!(
            restored.references()[split..]
                .iter()
                .map(written)
                .collect::<Vec<_>>(),
            ["local"]
        );
        let mut earlier = facts.0.clone();
        earlier.references.truncate(split);
        let earlier = StaticFacts::from_parts(source, limits, earlier).expect("earlier Rust rows");
        assert_eq!(
            earlier.rust_local_reference_start(source),
            earlier.references().len()
        );
    }
}

#[cfg(test)]
mod python_local_read_tests {
    use super::{StaticFacts, StaticFactsParts};
    use crate::python::PythonSyntaxProvider;
    use crate::{ByteRange, SyntaxLimits, SyntaxProvider, SyntaxSource};

    #[test]
    fn test_python_parameter_capture_restores_body_reads_only() {
        let source = "name = 1\ndef pick(name: int = name):\n    return name\n";
        let occurrences = source.match_indices("name").collect::<Vec<_>>();
        assert_eq!(occurrences.len(), 4);
        let range = |index: usize| {
            let start = u64::try_from(occurrences[index].0).expect("source offset");
            ByteRange {
                start,
                end: start + 4,
            }
        };
        let captured = PythonSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &rift_core::ProjectPath::new("pick.py").expect("path"),
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("authored Python source");
        let parameter = captured
            .symbols()
            .iter()
            .find(|symbol| symbol.is_python_parameter())
            .expect("parameter syntax token");
        assert_eq!(parameter.qualified_name, "pick.name");
        assert_eq!(parameter.name_range, Some(range(1)));
        assert_eq!(parameter.item_range, range(1));
        let original = captured.facts().static_facts().expect("captured facts");
        assert_eq!(original.python_local_reads(), [(range(3), range(1))]);
        let restored = StaticFacts::from_parts(source, SyntaxLimits::default(), original.0.clone())
            .expect("restored read/name pair");
        assert_eq!(&restored, original);
    }

    #[test]
    fn test_python_captured_local_read_restores_separately_from_direct_reads() {
        let source = "def outer(parameter):\n    local = 1\n    def nested():\n        return local + parameter\n    return local\n";
        let local: Vec<_> = source
            .match_indices("local")
            .map(|(start, _)| {
                let start = u64::try_from(start).expect("source offset");
                ByteRange {
                    start,
                    end: start + 5,
                }
            })
            .collect();
        let captured = PythonSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &rift_core::ProjectPath::new("pick.py").expect("path"),
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("authored Python source");
        let original = captured.facts().static_facts().expect("captured facts");
        assert_eq!(original.python_local_reads(), [(local[2], local[0])]);
        assert_eq!(
            original.python_captured_local_reads(),
            [(local[1], local[0])]
        );
        let restored = StaticFacts::from_parts(source, SyntaxLimits::default(), original.0.clone())
            .expect("restored captured and direct read/name pairs");
        assert_eq!(&restored, original);
        let mut inverted = original.0.clone();
        inverted.python_captured_local_reads[0] = (local[0], local[1]);
        assert!(StaticFacts::from_parts(source, SyntaxLimits::default(), inverted).is_err());
    }

    #[test]
    fn test_python_local_read_restore_checks_anchor_and_order() {
        let source = "def run():\n    pièce = 1\n    return pièce + pièce\n";
        let ranges = source
            .match_indices("pièce")
            .map(|(start, name)| ByteRange {
                start: u64::try_from(start).expect("start"),
                end: u64::try_from(start + name.len()).expect("end"),
            })
            .collect::<Vec<_>>();
        let [first, read, later] = ranges[..] else {
            panic!("three sites");
        };
        let restore = |rows| {
            StaticFacts::from_parts(
                source,
                SyntaxLimits::default(),
                StaticFactsParts {
                    python_local_reads: rows,
                    ..StaticFactsParts::default()
                },
            )
        };
        let pairs = vec![(read, first), (later, first)];
        let captured = PythonSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &rift_core::ProjectPath::new("run.py").expect("path"),
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("authored Python source");
        let original = captured.facts().static_facts().expect("captured facts");
        assert_eq!(original.python_local_reads(), pairs);
        assert_eq!(
            &StaticFacts::from_parts(source, SyntaxLimits::default(), original.0.clone())
                .expect("restore exact captured facts"),
            original
        );
        let restored = restore(pairs.clone()).expect("retained local reads");
        assert_eq!(restored.python_local_reads(), pairs);
        assert!(restored.references().is_empty());
        assert!(
            restore(Vec::new())
                .expect("older facts")
                .python_local_reads()
                .is_empty()
        );
        let split = ByteRange {
            start: first.start + 3,
            end: first.end,
        };
        for rows in [
            vec![(read, later)],
            vec![(read, split)],
            vec![(read, ByteRange { start: 0, end: 3 })],
            vec![(later, first), (read, first)],
            vec![(read, first), (read, first)],
        ] {
            assert!(restore(rows).is_err());
        }
        let limits = SyntaxLimits::new(100, 1, 10).expect("limits");
        assert!(
            StaticFacts::from_parts(
                source,
                limits,
                StaticFactsParts {
                    python_local_reads: pairs,
                    ..StaticFactsParts::default()
                },
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod javascript_member_tests {
    use super::{StaticFacts, StaticFactsParts, StaticMember};
    use crate::{ByteRange, SyntaxLimits};

    #[test]
    fn test_javascript_member_restore_checks_original_spans() {
        let source = "value /* note */ . field";
        let range = ByteRange { start: 0, end: 24 };
        let receiver = ByteRange { start: 0, end: 5 };
        let member = ByteRange { start: 19, end: 24 };
        let parts = |receiver, member| StaticFactsParts {
            members: vec![StaticMember {
                range,
                receiver,
                member,
            }],
            ..StaticFactsParts::default()
        };
        let checked = StaticFacts::from_parts(
            source,
            SyntaxLimits::default(),
            parts(receiver, Some(member)),
        )
        .expect("original spans");
        assert_eq!(checked.members()[0].receiver, receiver);
        for invalid in [range, ByteRange { start: 24, end: 25 }] {
            assert!(
                StaticFacts::from_parts(
                    source,
                    SyntaxLimits::default(),
                    parts(invalid, Some(member))
                )
                .is_err()
            );
        }
        let limits = SyntaxLimits::new(100, 1, 10).expect("limits");
        let mut oversized = parts(receiver, None);
        oversized.members.push(oversized.members[0]);
        assert!(StaticFacts::from_parts(source, limits, oversized).is_err());
    }
}
