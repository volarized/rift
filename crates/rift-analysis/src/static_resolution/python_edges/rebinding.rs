//! Python class names assigned again after their class statement.

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind};

use super::super::{File, Work};

/// The class statement of a name whose later assignments in the same scope keep the class
/// bound: the class itself (`Name = Name`) or a call that receives it (`Name = wrap(Name)`).
/// Any other assigned value leaves the selection to the caller.
pub(super) fn python_class_rebinding<'source>(
    file: &File<'source>,
    bindings: &[&'source StaticBinding],
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if bindings.len() < 2 {
        return None;
    }
    let mut class = None;
    for &binding in bindings {
        if !work.charge() {
            return None;
        }
        let statement = file.target_at(binding.name).is_some_and(|target| {
            target.symbol.kinds.contains(&SymbolKind::Constructible)
                && target.symbol.name_range == Some(binding.name)
        });
        if statement {
            if class.is_some() {
                return None;
            }
            class = Some(binding);
        }
    }
    let class = class?;
    let name = file.text(class.name)?;
    for &binding in bindings {
        if !work.charge() {
            return None;
        }
        if std::ptr::eq(binding, class) {
            continue;
        }
        if binding.scope != class.scope || binding.name.start < class.available_from {
            return None;
        }
        let kept = match binding.kind {
            StaticBindingKind::PythonAlias { value } => file.text(value)?.trim() == name,
            StaticBindingKind::PythonInstance { constructor } => {
                python_call_receives(file, constructor, name, work)?
            }
            _ => false,
        };
        if !kept {
            return None;
        }
    }
    Some(class)
}

/// Whether the call whose callee is written at `callee` passes `name` as a positional argument.
fn python_call_receives(
    file: &File<'_>,
    callee: ByteRange,
    name: &str,
    work: &mut Work,
) -> Option<bool> {
    for call in file.python_calls.get(&callee.start).into_iter().flatten() {
        if !work.charge() {
            return None;
        }
        if call.callee_range != callee {
            continue;
        }
        let arguments = file.text(ByteRange {
            start: callee.end,
            end: call.range.end,
        })?;
        let arguments = arguments.trim().strip_prefix('(')?.strip_suffix(')')?;
        return Some(arguments.split(',').any(|argument| argument.trim() == name));
    }
    Some(false)
}
