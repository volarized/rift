//! Named Python annotations used for static receiver lookup.

use rift_protocol::read::SymbolKind;
use rift_syntax::ByteRange;

use super::super::{File, Index, Target, Work};
use super::{Value, edge_python_binding};

impl<'source> Index<'source> {
    pub(super) fn python_annotation(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let text = file.text(annotation)?.trim().trim_matches(['\'', '"']);
        let mut alternatives = text
            .split('|')
            .map(str::trim)
            .filter(|name| *name != "None");
        let name = alternatives.next()?;
        if alternatives.next().is_some() {
            return None;
        }
        let name = name.split('[').next()?.trim();
        let mut names = name.split('.').map(str::trim);
        let binding = edge_python_binding(self, file, names.next()?, annotation.start, work)?;
        // Annotation lookup follows declarations and imports only, so cyclic annotations cannot recurse.
        let mut value = self.declaration_value(file, binding, work)?;
        for member in names {
            if !work.charge() {
                return None;
            }
            value = match value {
                Value::Module(name) => {
                    let file = self.files.get(self.edge_python.path(&name)?)?;
                    self.export_value(file, member, work)?
                }
                Value::Symbol(class) => Value::Symbol(self.python_own_member(class, member, work)?),
                Value::Instance(_) | Value::Member { .. } => return None,
            };
        }
        self.value_target(value)
            .filter(|target| target.symbol.kinds.contains(&SymbolKind::Constructible))
    }
}
