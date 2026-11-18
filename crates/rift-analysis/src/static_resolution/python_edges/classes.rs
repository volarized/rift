//! Captured Python class order and member lookup.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;

use super::super::{Index, Target, Work};

type Key<'source> = (&'source str, &'source str);

#[derive(Default)]
pub(in crate::static_resolution) struct ClassOrders<'source>(
    BTreeMap<Key<'source>, ClassOrder<'source>>,
);

struct ClassOrder<'source> {
    classes: Vec<Key<'source>>,
    complete: bool,
}

impl<'source> ClassOrder<'source> {
    fn partial(class: Key<'source>) -> Self {
        Self {
            classes: vec![class],
            complete: false,
        }
    }
}

fn key(target: Target<'_>) -> Key<'_> {
    (
        target.file.path.as_str(),
        target.symbol.qualified_name.as_str(),
    )
}

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_class_orders(
        &self,
        work: &mut Work,
    ) -> Option<ClassOrders<'source>> {
        let mut pending = BTreeMap::new();
        for file in self
            .files
            .values()
            .filter(|file| file.document.facts.language().name == "python")
        {
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return None;
                }
                if !symbol.kinds.contains(&SymbolKind::Constructible) {
                    continue;
                }
                let target = Target {
                    file: file.document,
                    symbol,
                };
                let mut bases = Vec::new();
                for edge in file.facts.heritage_edges() {
                    if !work.charge() {
                        return None;
                    }
                    if Some(edge.from) == symbol.name_range {
                        bases.push(
                            self.edge_python_name(file, edge.target, work)
                                .filter(|target| {
                                    target.symbol.kinds.contains(&SymbolKind::Constructible)
                                })
                                .map(key),
                        );
                    }
                }
                pending.insert(key(target), bases);
            }
        }
        let mut orders = BTreeMap::new();
        while !pending.is_empty() {
            let mut ready = Vec::new();
            for (class, bases) in &pending {
                if !work.charge() {
                    return None;
                }
                let mut available = true;
                for base in bases.iter().flatten() {
                    if !work.charge() {
                        return None;
                    }
                    available &= orders.contains_key(base);
                }
                if available {
                    ready.push(*class);
                }
            }
            if ready.is_empty() {
                break;
            }
            for class in ready {
                let bases = pending.remove(&class)?;
                let order = merge_order(class, &bases, &orders, work);
                if work.exhausted {
                    return None;
                }
                orders.insert(class, order.unwrap_or_else(|| ClassOrder::partial(class)));
            }
        }
        for class in pending.into_keys() {
            if !work.charge() {
                return None;
            }
            orders.insert(class, ClassOrder::partial(class));
        }
        Some(ClassOrders(orders))
    }

    pub(in crate::static_resolution) fn python_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !class.symbol.kinds.contains(&SymbolKind::Constructible) {
            let file = self.files.get(class.file.path.as_str())?;
            let annotation = file.python_annotations.get(&class.symbol.name_range?.end)?;
            let class = self.python_annotation(file, *annotation, work)?;
            return self
                .python_own_member(class, name)
                .or_else(|| self.python_base_member(class, name, work));
        }
        if let Some(target) = self.python_own_member(class, name) {
            return Some(target);
        }
        self.python_base_member(class, name, work)
    }

    pub(super) fn python_own_member(
        &self,
        class: Target<'source>,
        name: &str,
    ) -> Option<Target<'source>> {
        let file = self.files.get(class.file.path.as_str())?;
        file.qualified_target(&format!("{}.{name}", class.symbol.qualified_name))
    }

    pub(super) fn python_base_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let order = self.edge_python_classes.0.get(&key(class))?;
        for &(path, qualified) in order.classes.iter().skip(1) {
            if !work.charge() {
                return None;
            }
            let base = self.files.get(path)?.qualified_target(qualified)?;
            if let Some(target) = self.python_own_member(base, name) {
                return Some(target);
            }
        }
        None
    }
}

fn merge_order<'source>(
    class: Key<'source>,
    bases: &[Option<Key<'source>>],
    orders: &BTreeMap<Key<'source>, ClassOrder<'source>>,
    work: &mut Work,
) -> Option<ClassOrder<'source>> {
    let mut partial = ClassOrder::partial(class);
    if let [base] = bases {
        let Some(base) = base else {
            return Some(partial);
        };
        let parent = orders.get(base)?;
        for &class in &parent.classes {
            if !work.charge() {
                return None;
            }
            partial.classes.push(class);
        }
        partial.complete = parent.complete;
        return Some(partial);
    }
    let mut known = BTreeSet::new();
    let mut complete = true;
    for base in bases {
        if !work.charge() {
            return None;
        }
        match base {
            Some(base) => {
                if !known.insert(*base) {
                    return None;
                }
                complete &= orders.get(base)?.complete;
            }
            None => complete = false,
        }
    }
    if !complete {
        // The first direct base precedes its own bases in every valid class order.
        // Unknown ancestors of another base can change the order after that point.
        if let Some(Some(base)) = bases.first() {
            partial.classes.push(*base);
        }
        return Some(partial);
    }
    let mut sequences = Vec::new();
    let mut direct = Vec::new();
    for base in bases {
        if !work.charge() {
            return None;
        }
        let base = (*base)?;
        direct.push(base);
        sequences.push(orders.get(&base)?.classes.as_slice());
    }
    sequences.push(direct.as_slice());
    let mut order = vec![class];
    while sequences.iter().any(|sequence| !sequence.is_empty()) {
        let mut selected = None;
        for sequence in &sequences {
            if !work.charge() {
                return None;
            }
            let Some(candidate) = sequence.first() else {
                continue;
            };
            let mut in_tail = false;
            for other in &sequences {
                for item in other.iter().skip(1) {
                    if !work.charge() {
                        return None;
                    }
                    in_tail |= item == candidate;
                }
            }
            if !in_tail {
                selected = Some(*candidate);
                break;
            }
        }
        let selected = selected?;
        order.push(selected);
        for sequence in &mut sequences {
            if !work.charge() {
                return None;
            }
            if sequence.first() == Some(&selected) {
                *sequence = &sequence[1..];
            }
        }
    }
    Some(ClassOrder {
        classes: order,
        complete: true,
    })
}
