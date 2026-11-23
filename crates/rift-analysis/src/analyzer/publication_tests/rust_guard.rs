//! Exact stored facts for calls through an authored Rust guard.

use super::*;
use rift_protocol::read::{EdgeDerivation, EdgeProvider, TextRange};

const MANIFEST: &str = "[package]\nname='beacon'\nversion='1.0.0'\nedition='2021'\n";
const SOURCE: &str = r"use core::ops::{Deref, DerefMut};
pub struct Guard<T, F: FnMut(&mut T)> { value: T, dropfn: F }
impl<T, F: FnMut(&mut T)> Deref for Guard<T, F> { type Target = T; fn deref(&self) -> &T { &self.value } }
impl<T, F: FnMut(&mut T)> DerefMut for Guard<T, F> { fn deref_mut(&mut self) -> &mut T { &mut self.value } }
impl<T, F: FnMut(&mut T)> Drop for Guard<T, F> { fn drop(&mut self) { (self.dropfn)(&mut self.value) } }
pub fn guard<T, F: FnMut(&mut T)>(value: T, dropfn: F) -> Guard<T, F> { Guard { value, dropfn } }
pub struct Table { pub items: usize }
impl Table {
    pub fn reset(&mut self) { self.items = 0; }
    pub fn fill(&mut self) { self.items = 1; }
    pub fn clear(&mut self) {
        let mut this = guard(self, |this| this.reset());
        this.fill();
    }
    pub fn direct(&mut self) { let this: &mut Table = self; this.reset(); }
}
";
const TUPLE: &str = "impl Table { pub fn tuple(&mut self) { let _owner = guard((0, self), |(index, entry)| { *index += 1; entry.reset() }); } }\n";
const INHERENT: &str = "impl<T, F: FnMut(&mut T)> Guard<T, F> { fn fill(&mut self) {} }\n";
const DEREF_MUT: &str = "impl<T, F: FnMut(&mut T)> DerefMut for Guard<T, F> { fn deref_mut(&mut self) -> &mut T { &mut self.value } }\n";
const PREFIX: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";

fn analyze(source: &str) -> PackageAnalysis {
    fixture::package_result_with_metadata(
        ShippedLanguage::Rust,
        vec![("src/lib.rs", source)],
        None,
        Some(("Cargo.toml", MANIFEST)),
    )
    .expect("captured Rust package")
}

fn member_pair(source: &str, caller: &str, site: &str, target: Option<&str>) {
    let (publication, files, semantics, _) = analyze(source).into_parts();
    publication.validate_facts().expect("valid package facts");
    let [file] = files.as_slice() else {
        panic!("one source unit")
    };
    let facts = file
        .file()
        .syntax()
        .static_facts()
        .expect("authored Rust facts");
    let unit = file.placement().unit().to_string();
    let start = u64::try_from(source.find(site).expect("written call")).expect("start");
    let end = start + u64::try_from(site.len()).expect("end");
    assert!(
        facts
            .calls()
            .iter()
            .any(|call| call.range.start == start && call.range.end == end)
    );
    let caller = format!("{PREFIX}/{caller}");
    for (kind, end) in [(EdgeKind::Calls, end), (EdgeKind::Reads, end - 2)] {
        let range = TextRange { start, end };
        let complete = |edge: &rift_protocol::index::StoredEdge| {
            edge.kinds == [kind]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|location| location.unit.0 == unit && location.range == range)
        };
        let stored = semantics
            .edges()
            .edges()
            .map(crate::edge::Edge::to_protocol)
            .filter(complete)
            .collect::<Vec<_>>();
        let published = publication
            .edges
            .iter()
            .filter(|edge| complete(edge))
            .collect::<Vec<_>>();
        if let Some(target) = target {
            assert_eq!(stored.len(), 1, "{site}: {kind:?}");
            let [edge] = stored.as_slice() else {
                unreachable!()
            };
            assert_eq!(edge.from.as_str(), caller);
            assert_eq!(edge.to.as_str(), format!("{PREFIX}/{target}"));
            assert_eq!(
                (edge.provider, edge.derivation),
                (EdgeProvider::Static, EdgeDerivation::Resolution)
            );
            assert_eq!(published, [edge]);
        } else {
            assert!(
                stored.is_empty() && published.is_empty(),
                "{site}: {kind:?}"
            );
        }
    }
}

#[test]
fn local_deref_of_a_returned_guard_stores_its_member_call_and_read() {
    assert_eq!(&SOURCE[756..767], "this.fill()");
    member_pair(SOURCE, "Table/clear", "this.fill()", Some("Table/fill"));
    member_pair(SOURCE, "Table/clear", "this.reset()", Some("Table/reset"));
    member_pair(
        &format!("{SOURCE}{TUPLE}"),
        "Table/tuple",
        "entry.reset()",
        Some("Table/reset"),
    );
    member_pair(
        &format!("{SOURCE}{INHERENT}"),
        "Table/clear",
        "this.fill()",
        Some("Guard/fill"),
    );
}

#[test]
fn returned_guard_refuses_unproved_deref_targets_and_bounds() {
    for source in [
        SOURCE.replace(DEREF_MUT, ""),
        SOURCE.replace("type Target = T;", "type Target = Option<T>;"),
        SOURCE.replace("-> Guard<T, F> { Guard", "-> Guard<F, T> { Guard"),
        SOURCE.replace("guard(self, |this|", "guard::<_, _>(self, |this|"),
        SOURCE.replace("impl<T, F: FnMut(&mut T)> Deref for", "impl<T: Copy, F: FnMut(&mut T)> Deref for"),
        SOURCE.replace("use core::ops::{Deref, DerefMut};", "use core::ops::DerefMut;\ntrait Deref { type Target; fn deref(&self) -> &Self::Target; }"),
        SOURCE.replace("impl<T, F: FnMut(&mut T)> Deref for", "#[cfg(unavailable)]\nimpl<T, F: FnMut(&mut T)> Deref for"),
    ] {
        member_pair(&source, "Table/clear", "this.fill()", None);
    }
}
