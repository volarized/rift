//! Python names typed by the elements of the container they iterate or unpack.

use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, PythonPosition, StaticBinding, StaticBindingKind, StaticCall, StaticCallee,
};

use super::super::{File, Index, PythonAppends, Target, Work};
use super::annotations::{annotation_text, slice_range, top_level_parts};
use super::classes::TYPING_MODULES;
use super::{Value, edge_python_binding};

/// Type arguments a container takes and what iterating it produces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    /// One type argument, the element.
    Sequence,
    /// The first type argument is the element.
    Generator,
    /// Key and value type arguments; iteration produces keys.
    Mapping,
    /// One type argument for each part, or one repeated by `...`.
    Tuple,
}

/// Containers of the `builtins` module.
const BUILTIN_CONTAINERS: [NamedContainer; 5] = [
    ("list", Container::Sequence),
    ("set", Container::Sequence),
    ("frozenset", Container::Sequence),
    ("dict", Container::Mapping),
    ("tuple", Container::Tuple),
];

/// A container's name in its module.
type NamedContainer = (&'static str, Container);

/// Containers of `typing` and `typing_extensions`.
const TYPING_CONTAINERS: [NamedContainer; 18] = [
    ("List", Container::Sequence),
    ("Set", Container::Sequence),
    ("FrozenSet", Container::Sequence),
    ("Sequence", Container::Sequence),
    ("MutableSequence", Container::Sequence),
    ("Iterable", Container::Sequence),
    ("Iterator", Container::Sequence),
    ("Collection", Container::Sequence),
    ("AbstractSet", Container::Sequence),
    ("MutableSet", Container::Sequence),
    ("Deque", Container::Sequence),
    ("Generator", Container::Generator),
    ("Dict", Container::Mapping),
    ("DefaultDict", Container::Mapping),
    ("OrderedDict", Container::Mapping),
    ("Mapping", Container::Mapping),
    ("MutableMapping", Container::Mapping),
    ("Tuple", Container::Tuple),
];

/// Containers of `collections.abc`.
const ABSTRACT_CONTAINERS: [NamedContainer; 10] = [
    ("Sequence", Container::Sequence),
    ("MutableSequence", Container::Sequence),
    ("Iterable", Container::Sequence),
    ("Iterator", Container::Sequence),
    ("Collection", Container::Sequence),
    ("Set", Container::Sequence),
    ("MutableSet", Container::Sequence),
    ("Generator", Container::Generator),
    ("Mapping", Container::Mapping),
    ("MutableMapping", Container::Mapping),
];

/// Containers of `collections`.
const COLLECTIONS_CONTAINERS: [NamedContainer; 3] = [
    ("deque", Container::Sequence),
    ("defaultdict", Container::Mapping),
    ("OrderedDict", Container::Mapping),
];

/// Containers of the standard library, by the modules that provide them.
const LIBRARY_CONTAINERS: [(&[&str], &[NamedContainer]); 3] = [
    (&TYPING_MODULES, &TYPING_CONTAINERS),
    (&["collections.abc"], &ABSTRACT_CONTAINERS),
    (&["collections"], &COLLECTIONS_CONTAINERS),
];

/// A type that a source file states.
enum Type<'index, 'source> {
    /// A type expression.
    Written(&'index File<'source>, ByteRange),
    /// A tuple of two type expressions: one entry of a mapping.
    Entry(&'index File<'source>, [ByteRange; 2]),
    /// Expressions whose values all have the type.
    Values(&'index File<'source>, Vec<ByteRange>),
    /// A container whose elements have the type.
    Container(Box<Self>),
}

/// The type arguments of a container annotation.
struct Arguments {
    container: Container,
    members: Vec<ByteRange>,
    /// Whether the tuple annotation repeats its first argument.
    repeated: bool,
}

impl Arguments {
    /// The type argument that names each element, when one does.
    fn element(&self) -> Option<ByteRange> {
        match self.container {
            Container::Sequence | Container::Generator | Container::Mapping => {
                self.members.first().copied()
            }
            Container::Tuple => self.repeated.then(|| self.members.first().copied())?,
        }
    }
}

impl<'source> Index<'source> {
    /// The class of each element a loop over `iterable` binds, or of one part of each element.
    pub(super) fn python_element(
        &self,
        file: &File<'source>,
        iterable: ByteRange,
        position: Option<PythonPosition>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let element = self.python_element_type(file, iterable, position, work)?;
        self.python_type_class(&element, work)
    }

    /// The class of one part of `value`.
    pub(super) fn python_unpacked(
        &self,
        file: &File<'source>,
        value: ByteRange,
        position: PythonPosition,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let unpacked = self.python_value_type(file, value, work)?;
        let part = self.python_part(unpacked, position, work)?;
        self.python_type_class(&part, work)
    }

    fn python_type_class(
        &self,
        stated: &Type<'_, 'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match stated {
            Type::Written(file, range) => self.python_annotation(file, *range, work),
            Type::Values(file, values) => self.python_values_class(file, values, work),
            Type::Entry(..) | Type::Container(_) => None,
        }
    }

    /// The one class every expression constructs or holds an instance of. The first expression
    /// stands for all of them where an element depends on its own list.
    fn python_values_class(
        &self,
        file: &File<'source>,
        values: &[ByteRange],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let first = *values.first()?;
        work.python_expression(file.document.path.as_str(), first, |work| {
            let mut agreed: Option<Target<'source>> = None;
            for &value in values {
                let class = self.python_value_class(file, value, work)?;
                if agreed.is_some_and(|known| {
                    known.file.path != class.file.path
                        || known.symbol.qualified_name != class.symbol.qualified_name
                }) {
                    return None;
                }
                agreed = Some(class);
            }
            agreed
        })
    }

    fn python_value_class(
        &self,
        file: &File<'source>,
        value: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        work.python_lookup(|work| {
            if let Some(call) = call_at(file, value) {
                let callee = self.edge_python_call(file, call, work)?;
                return self.python_call_result(callee, work);
            }
            let value = self.python_name(file, value, work)?;
            match self.python_receiver(value, work)? {
                Value::Instance(class) => Some(class),
                Value::Symbol(_) | Value::Member { .. } | Value::Module(_) => None,
            }
        })
    }

    fn python_element_type<'index>(
        &'index self,
        file: &'index File<'source>,
        iterable: ByteRange,
        position: Option<PythonPosition>,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        let iterated = match self.python_value_type(file, iterable, work)? {
            Type::Written(file, annotation) => {
                let element = self.python_arguments(file, annotation, work)?.element()?;
                Type::Written(file, element)
            }
            Type::Container(element) => *element,
            Type::Entry(..) | Type::Values(..) => return None,
        };
        match position {
            Some(position) => self.python_part(iterated, position, work),
            None => Some(iterated),
        }
    }

    /// The type of one part when a value of `unpacked` is unpacked into `position.count` parts.
    fn python_part<'index>(
        &self,
        unpacked: Type<'index, 'source>,
        position: PythonPosition,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        let index = usize::try_from(position.index).ok()?;
        let count = usize::try_from(position.count).ok()?;
        match unpacked {
            Type::Container(element) => Some(*element),
            Type::Entry(file, members) if count == members.len() => {
                Some(Type::Written(file, *members.get(index)?))
            }
            Type::Entry(..) => None,
            Type::Written(file, annotation) => {
                let arguments = self.python_arguments(file, annotation, work)?;
                let part = match arguments.element() {
                    Some(element) => element,
                    None if count == arguments.members.len() => *arguments.members.get(index)?,
                    None => return None,
                };
                Some(Type::Written(file, part))
            }
            Type::Values(file, values) => {
                let mut parts = Vec::new();
                for value in values {
                    if !work.charge() {
                        return None;
                    }
                    let written = file.text(value)?;
                    let tuple = written.strip_prefix('(')?.strip_suffix(')')?;
                    let members = top_level_parts(tuple, ',', work)?;
                    if members.len() != count {
                        return None;
                    }
                    parts.push(slice_range(value, written, members.get(index)?)?);
                }
                Some(Type::Values(file, parts))
            }
        }
    }

    /// The type stated for the value of an expression: by the annotation of the called
    /// function, of the name or of the member, by the container its name is an element of, or
    /// by the values appended to its empty list.
    fn python_value_type<'index>(
        &'index self,
        file: &'index File<'source>,
        value: ByteRange,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        work.python_expression(file.document.path.as_str(), value, |work| {
            if let Some(call) = call_at(file, value) {
                return self.python_call_type(file, call, work);
            }
            let text = file.text(value)?.trim();
            if text.contains('.') {
                let declaration = match self.python_name(file, value, work)? {
                    Value::Symbol(target)
                    | Value::Member {
                        declaration: target,
                        ..
                    } => target,
                    Value::Instance(_) | Value::Module(_) => return None,
                };
                return self.python_declared_type(declaration, work);
            }
            let binding = edge_python_binding(self, file, text, value.start, work)?;
            if let StaticBindingKind::PythonEmptyList = binding.kind {
                return self.python_appended_type(file, binding, value.start, work);
            }
            if let StaticBindingKind::PythonImport { .. } = binding.kind {
                let declaration = self.value_target(self.binding_value(file, binding, work)?)?;
                return self.python_declared_type(declaration, work);
            }
            self.python_binding_type(file, binding.kind, work)
        })
    }

    fn python_call_type<'index>(
        &'index self,
        file: &'index File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        if let StaticCallee::Member { receiver, member } = call.callee
            && let Some(view) = self.python_mapping_view(file, receiver, member, work)
        {
            return Some(view);
        }
        let callee = self.edge_python_call(file, call, work)?;
        self.python_returned_type(callee)
    }

    /// The iterable of keys, values or entries a method of a declared mapping returns.
    fn python_mapping_view<'index>(
        &'index self,
        file: &'index File<'source>,
        receiver: ByteRange,
        member: ByteRange,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        let view = file.text(member)?;
        if !["keys", "values", "items"].contains(&view) {
            return None;
        }
        let Type::Written(source, annotation) = self.python_value_type(file, receiver, work)?
        else {
            return None;
        };
        let arguments = self.python_arguments(source, annotation, work)?;
        if arguments.container != Container::Mapping {
            return None;
        }
        let &[key, value] = arguments.members.as_slice() else {
            return None;
        };
        let element = match view {
            "keys" => Type::Written(source, key),
            "values" => Type::Written(source, value),
            _ => Type::Entry(source, [key, value]),
        };
        Some(Type::Container(Box::new(element)))
    }

    /// The return type a function declares.
    fn python_returned_type(&self, callable: Target<'source>) -> Option<Type<'_, 'source>> {
        let file = self.files.get(callable.file.path.as_str())?;
        match *file.python_values.get(&callable.symbol.name_range?.end)? {
            StaticBindingKind::PythonFunction { annotation, .. } => {
                Some(Type::Written(file, annotation))
            }
            _ => None,
        }
    }

    /// The type stated for the value a declaration holds.
    fn python_declared_type(
        &self,
        declaration: Target<'source>,
        work: &mut Work,
    ) -> Option<Type<'_, 'source>> {
        if declaration.symbol.kinds.contains(&SymbolKind::Property) {
            let (file, annotation) = self.python_property_annotation(declaration, work)?;
            return Some(Type::Written(file, annotation));
        }
        let file = self.files.get(declaration.file.path.as_str())?;
        let kind = *file
            .python_values
            .get(&declaration.symbol.name_range?.end)?;
        self.python_binding_type(file, kind, work)
    }

    fn python_binding_type<'index>(
        &'index self,
        file: &'index File<'source>,
        kind: StaticBindingKind,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        match kind {
            StaticBindingKind::PythonAnnotation { annotation } => {
                Some(Type::Written(file, annotation))
            }
            StaticBindingKind::PythonAlias { value } => self.python_value_type(file, value, work),
            StaticBindingKind::PythonInstance { constructor } => {
                let callable = self.edge_python_name(file, constructor, work)?;
                self.python_returned_type(callable)
            }
            StaticBindingKind::PythonElement { iterable, position } => {
                self.python_element_type(file, iterable, position, work)
            }
            StaticBindingKind::PythonUnpacked { value, position } => {
                let unpacked = self.python_value_type(file, value, work)?;
                self.python_part(unpacked, position, work)
            }
            _ => None,
        }
    }

    /// A list that one name of a callable holds from an empty literal, typed by the values
    /// appended to it before `position`, or anywhere in a loop around both.
    ///
    /// The captured binding states that every other use of the name leaves its elements as
    /// they were.
    fn python_appended_type<'index>(
        &'index self,
        file: &'index File<'source>,
        list: &'source StaticBinding,
        position: u64,
        work: &mut Work,
    ) -> Option<Type<'index, 'source>> {
        let owner = file.enclosing_callable(list.name)?;
        let reader = file.enclosing_callable(ByteRange {
            start: position,
            end: position.checked_add(1)?,
        })?;
        if owner.symbol.item_range != list.scope || reader.symbol.item_range != list.scope {
            return None;
        }
        let mut values = Vec::new();
        for (call, value) in self.python_appends(file, list, work)? {
            let repeated = || {
                file.facts
                    .loops()
                    .iter()
                    .any(|range| range.contains(position) && range.contains(call.start))
            };
            if !work.charge() {
                return None;
            }
            if call.start < position || repeated() {
                values.push(value);
            }
        }
        (!values.is_empty()).then(|| Type::Container(Box::new(Type::Values(file, values))))
    }

    /// The `append` calls on a list and the appended expressions, read once for each list.
    fn python_appends(
        &self,
        file: &File<'source>,
        list: &'source StaticBinding,
        work: &mut Work,
    ) -> Option<PythonAppends> {
        let key = (file.document.path.as_str().as_ptr() as usize, list.name);
        if let Some(known) = work.python_appends.get(&key) {
            return known.clone();
        }
        let found = self.python_append_calls(file, list, work);
        work.python_appends.insert(key, found.clone());
        found
    }

    fn python_append_calls(
        &self,
        file: &File<'source>,
        list: &'source StaticBinding,
        work: &mut Work,
    ) -> Option<PythonAppends> {
        let name = file.text(list.name)?;
        let mut appends = Vec::new();
        for call in file
            .python_calls
            .range(list.scope.start..list.scope.end)
            .flat_map(|(_, calls)| calls)
        {
            if !work.charge() {
                return None;
            }
            let StaticCallee::Member { receiver, member } = call.callee else {
                continue;
            };
            if file.text(receiver) != Some(name) || file.text(member) != Some("append") {
                continue;
            }
            let held = edge_python_binding(self, file, name, receiver.start, work)?;
            if held != list || file.enclosing_callable(call.range)?.symbol.item_range != list.scope
            {
                return None;
            }
            appends.push((call.range, single_argument(file, call, work)?));
        }
        Some(appends)
    }

    /// The type arguments of a container annotation, also inside `Optional[...]` and beside
    /// `None`.
    fn python_arguments(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        work: &mut Work,
    ) -> Option<Arguments> {
        work.python_lookup(|work| {
            let annotation = self.python_stated_annotation(file, annotation, work)?;
            let written = file.text(annotation)?;
            let text = annotation_text(written);
            let mut alternatives = top_level_parts(text, '|', work)?
                .into_iter()
                .filter(|part| *part != "None");
            let (Some(text), None) = (alternatives.next(), alternatives.next()) else {
                return None;
            };
            let (head, inner) = text.strip_suffix(']')?.split_once('[')?;
            let container =
                self.python_container(file, slice_range(annotation, written, head.trim())?, work)?;
            let members = top_level_parts(inner, ',', work)?;
            let repeated =
                container == Container::Tuple && matches!(members.as_slice(), [_, "..."]);
            let expected = match container {
                Container::Sequence => members.len() == 1,
                Container::Mapping => members.len() == 2,
                Container::Generator | Container::Tuple => !members.is_empty(),
            };
            if !expected {
                return None;
            }
            Some(Arguments {
                container,
                members: members
                    .into_iter()
                    .map(|member| slice_range(annotation, written, member))
                    .collect::<Option<_>>()?,
                repeated,
            })
        })
    }

    fn python_container(
        &self,
        file: &File<'source>,
        head: ByteRange,
        work: &mut Work,
    ) -> Option<Container> {
        let text = file.text(head)?;
        if let Some((_, container)) = BUILTIN_CONTAINERS.iter().find(|(name, _)| *name == text) {
            return (!file.by_name.contains_key(text) && !file.blocked(head.start, work))
                .then_some(*container);
        }
        let qualified = self.python_library_name(file, head, work)?;
        let (module, name) = qualified.rsplit_once('.')?;
        LIBRARY_CONTAINERS
            .iter()
            .filter(|(modules, _)| modules.contains(&module))
            .flat_map(|(_, containers)| containers.iter())
            .find(|(container, _)| *container == name)
            .map(|(_, container)| *container)
    }
}

/// The call written at exactly `range`.
fn call_at<'source>(file: &File<'source>, range: ByteRange) -> Option<&'source StaticCall> {
    file.python_calls
        .get(&range.start)?
        .iter()
        .copied()
        .find(|call| call.range == range)
}

/// The one positional argument of a call.
fn single_argument(file: &File<'_>, call: &StaticCall, work: &mut Work) -> Option<ByteRange> {
    let arguments = ByteRange {
        start: call.callee_range.end,
        end: call.range.end,
    };
    let written = file.text(arguments)?;
    let inner = written.trim().strip_prefix('(')?.strip_suffix(')')?;
    let parts = top_level_parts(inner, ',', work)?;
    let [value] = parts.as_slice() else {
        return None;
    };
    slice_range(arguments, written, value)
}
