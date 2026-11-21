//! Resolve the callback type written for a member assignment's object literal.

use rift_syntax::{ByteRange, StaticMemberCallback, SyntaxSymbol};

use super::{File, Index, Work};

/// Each asserted type must establish whether it declares the assigned property.
enum Property<'source> {
    Absent,
    Declared(&'source SyntaxSymbol),
}

impl<'source> Index<'source> {
    pub(super) fn javascript_member_callback<'index>(
        &'index self,
        file: &'index File<'source>,
        context: &StaticMemberCallback,
        work: &mut Work,
    ) -> Option<(&'index File<'source>, ByteRange)> {
        let property = file.text(context.property)?;
        let callback = file.text(context.callback)?;
        let mut found = None;
        for asserted in std::iter::once(context.asserted).chain(context.intersection) {
            if !work.charge() || self.javascript_type_parameter(file, asserted, work) {
                return None;
            }
            let owner = self.javascript_declared_type(file, asserted, work)?;
            let source = self.files.get(owner.file.path.as_str())?;
            let declarations = source
                .by_qualified_name
                .get(owner.symbol.qualified_name.as_str())?;
            if declarations.len() != 1 {
                return None;
            }
            let body = source
                .callbacks
                .member_type_shape(owner.symbol.name_range?)?;
            let Property::Declared(property) =
                contextual_property(source, &owner.symbol.qualified_name, body, property, work)?
            else {
                continue;
            };
            let shape = source.callbacks.member_type_shape(property.name_range?)?;
            let Property::Declared(callback) =
                contextual_property(source, &property.qualified_name, shape, callback, work)?
            else {
                return None;
            };
            if found
                .replace(self.javascript_function_type(source, callback.name_range?, work)?)
                .is_some()
            {
                return None;
            }
        }
        found
    }
}

/// The single written property in `body`. A signature in a conditional's test is outside `body`.
fn contextual_property<'source>(
    file: &File<'source>,
    owner: &str,
    body: ByteRange,
    member: &str,
    work: &mut Work,
) -> Option<Property<'source>> {
    let qualified = format!("{owner}.{member}");
    let mut found = None;
    for &symbol in file
        .by_qualified_name
        .get(qualified.as_str())
        .into_iter()
        .flatten()
    {
        if !work.charge() {
            return None;
        }
        if !body.encloses(symbol.item_range) {
            continue;
        }
        if symbol.syntax_element_kind != Some("property_signature")
            || found.replace(symbol).is_some()
        {
            return None;
        }
    }
    Some(found.map_or(Property::Absent, Property::Declared))
}
