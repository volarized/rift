//! Text of the hits a relationship walk reached, written as a tree.
//!
//! Each indent level is one tab, shown here as two spaces.
//!
//! ```text
//! 3 results
//!   at rift://symbol/rust/src/config.rs/load_config
//!
//!     ↳ referenced
//!       by fn run(args: &Arguments) -> Result<(), ConfigError>
//!       in src/app.rs:20
//!       at rift://symbol/rust/src/app.rs/run
//!
//!       ↳ referenced
//!         by async fn main() -> Result<(), ConfigError>
//!         in src/main.rs:9
//!         at rift://symbol/rust/src/main.rs/main
//!
//!     ↳ referenced
//!       by fn reload_config() -> Result<(), ConfigError>
//!       in src/watch.rs:41
//!       at rift://symbol/rust/src/watch.rs/reload_config
//! ```
//!
//! The root is the symbol the walk starts from. Each hop of a `traversal_path` is one level: the
//! node starts with `↳` and the relationship read from the node above it, then names the symbol
//! the hop reached. A node that is a hit of the answer writes the lines of that hit: the
//! declaration, the line that starts with its location behind `in`, and the symbol identity
//! behind `at`. Any other node writes its symbol identity behind `at`. The root starts with its
//! declaration, or with its identity when it is no hit. A blank line precedes every node except
//! the first entry of the section.
//!
//! An outgoing hop writes each facet of its relationship as it is spelled on the wire, with
//! underscores as spaces (`calls`, `depends on`). An incoming hop writes the facet read from the
//! other end (`called by`, `dependency of`). Several facets are joined by `, `, and a
//! relationship whose derivation is `heuristic` starts with `probably`. When every facet ends in
//! `by`, the head line leaves that word out and the declaration line starts with it.

use rift_protocol::read::{Relationship, RelationshipDerivation, RelationshipFacet, SymbolId};
use rift_protocol::search::{GraphHop, HopDirection, SearchHit};

use super::facts::spaced_name;
use super::layout::Lines;
use super::search;
use crate::output::text::{TextError, TextWriter};

/// Mark that starts a node under the root.
const BRANCH: &str = "↳";
/// Word written before the declaration of a node whose relationship ends in it.
const BY: &str = "by";
/// End of a relationship facet that moves to the declaration line.
const BY_ENDING: &str = " by";
/// Word written before the location of a node.
const PLACE: &str = "in";
/// Word written before the symbol identity of a node.
const IDENTITY: &str = "at";
/// Word written before the relationship of a heuristic hop.
const HEURISTIC: &str = "probably ";
/// Text between two facets of one relationship.
const FACET_SEPARATOR: &str = ", ";

/// The words a node of a walk writes before the lines that name its symbol.
#[derive(Clone, Copy)]
pub(super) enum Labels {
    /// An item outside a walk: no line starts with a word.
    Item,
    /// The root: the declaration is the head line, the location follows `in`, and the identity
    /// follows `at`.
    Root,
    /// A node under the root: the location follows `in` and the identity follows `at`. The
    /// declaration follows `by` when the relationship ends in that word.
    Branch {
        /// Whether the declaration follows `by`.
        by: bool,
    },
}

impl Labels {
    /// The word before the declaration, when it has one.
    pub(super) fn declaration(self) -> Option<&'static str> {
        matches!(self, Self::Branch { by: true }).then_some(BY)
    }

    /// The word before the location, when it has one.
    pub(super) fn place(self) -> Option<&'static str> {
        (!matches!(self, Self::Item)).then_some(PLACE)
    }

    /// The word before the symbol identity, when it has one.
    pub(super) fn identity(self) -> Option<&'static str> {
        (!matches!(self, Self::Item)).then_some(IDENTITY)
    }
}

/// The head line of a node under the root.
struct Lead {
    /// The mark and the relationship: `↳ called`.
    text: String,
    /// Whether the relationship ends in `by`, which the declaration line then starts with.
    by: bool,
}

/// One symbol of a walk.
struct Node<'a> {
    /// Identity of the symbol.
    id: &'a SymbolId,
    /// Levels under the root.
    depth: usize,
    /// Head line of a node under the root. The root has none.
    lead: Option<Lead>,
    /// The hit that is this symbol, when the answer holds one.
    hit: Option<&'a SearchHit>,
    /// Places, among the nodes, of the nodes directly under this one, in answer order.
    children: Vec<usize>,
}

/// The hits of a `search` answer, split into the trees of its walk and the hits outside it.
pub(super) struct Walk<'a> {
    /// Hits no relationship path reached, in answer order.
    outside: Vec<&'a SearchHit>,
    /// Every node of every tree.
    nodes: Vec<Node<'a>>,
    /// Places, among the nodes, of the roots, in answer order.
    roots: Vec<usize>,
}

impl<'a> Walk<'a> {
    /// Splits `hits`. A hit outside the walk that is the symbol a tree starts from becomes the
    /// root of that tree.
    ///
    /// # Errors
    ///
    /// Fails when a relationship facet has no unit wire name.
    pub(super) fn of(hits: &'a [SearchHit]) -> Result<Self, TextError> {
        let mut walk = Self {
            outside: Vec::new(),
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        for hit in hits {
            match search::walked(hit) {
                Some((first, rest)) => walk.reach(hit, first, rest)?,
                None => walk.outside.push(hit),
            }
        }
        walk.claim_roots();
        Ok(walk)
    }

    /// The hits written as items before the trees.
    pub(super) fn outside(&self) -> &[&'a SearchHit] {
        &self.outside
    }

    /// Adds the path of `hit` to the tree of its starting symbol and puts `hit` at its end.
    fn reach(
        &mut self,
        hit: &'a SearchHit,
        first: &'a GraphHop,
        rest: &'a [GraphHop],
    ) -> Result<(), TextError> {
        let (start, _) = ends_of(first);
        let mut place = self.root(start);
        let mut hops = std::iter::once(first).chain(rest).peekable();
        while let Some(hop) = hops.next() {
            let last = hops.peek().is_none();
            place = self.child(place, hop, last)?;
        }
        if let Some(node) = self.nodes.get_mut(place) {
            node.hit = Some(hit);
        }
        Ok(())
    }

    /// The place of the root that is `start`, added when no tree starts from it yet.
    fn root(&mut self, start: &'a SymbolId) -> usize {
        let known = self
            .roots
            .iter()
            .copied()
            .find(|root| self.nodes.get(*root).is_some_and(|node| node.id == start));
        known.unwrap_or_else(|| {
            let place = self.nodes.len();
            self.nodes.push(Node {
                id: start,
                depth: 0,
                lead: None,
                hit: None,
                children: Vec::new(),
            });
            self.roots.push(place);
            place
        })
    }

    /// The place of the node `hop` reaches under `parent`, added when `parent` has none.
    ///
    /// The `last` hop of a path never lands on a node that already is a hit, so every hit keeps
    /// a node of its own.
    fn child(&mut self, parent: usize, hop: &'a GraphHop, last: bool) -> Result<usize, TextError> {
        let (_, reached) = ends_of(hop);
        let above = self.nodes.get(parent);
        let known = above.and_then(|above| {
            above.children.iter().copied().find(|child| {
                self.nodes
                    .get(*child)
                    .is_some_and(|node| node.id == reached && !(last && node.hit.is_some()))
            })
        });
        if let Some(known) = known {
            return Ok(known);
        }
        let depth = above.map_or(0, |above| above.depth).saturating_add(1);
        let place = self.nodes.len();
        self.nodes.push(Node {
            id: reached,
            depth,
            lead: Some(lead_of(hop)?),
            hit: None,
            children: Vec::new(),
        });
        if let Some(above) = self.nodes.get_mut(parent) {
            above.children.push(place);
        }
        Ok(place)
    }

    /// Moves each hit outside the walk that is the starting symbol of a tree to that root.
    fn claim_roots(&mut self) {
        for root in &self.roots {
            let Some(node) = self.nodes.get_mut(*root) else {
                continue;
            };
            let found = self
                .outside
                .iter()
                .position(|hit| search::symbol_id(hit) == Some(node.id));
            if let Some(found) = found {
                node.hit = Some(self.outside.remove(found));
            }
        }
    }

    /// Writes every tree, each node before the nodes under it.
    ///
    /// # Errors
    ///
    /// Returns the first failure of `out`, or of a hit whose enum field has no unit wire name.
    pub(super) fn write(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let mut opens_section = self.outside.is_empty();
        let mut pending: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(node) = pending.pop().and_then(|place| self.nodes.get(place)) {
            if !opens_section {
                out.blank_line()?;
            }
            opens_section = false;
            let lead = node.lead.as_ref();
            let mut lines = Lines::node(out, node.depth, lead.map(|lead| lead.text.clone()));
            match (lead, node.hit) {
                (None, Some(hit)) => search::walk_node(&mut lines, hit, Labels::Root)?,
                (None, None) => lines.head(&format!("{IDENTITY} {}", node.id.0))?,
                (Some(lead), hit) => {
                    lines.head("")?;
                    match hit {
                        Some(hit) => {
                            search::walk_node(&mut lines, hit, Labels::Branch { by: lead.by })?;
                        }
                        None => lines.line(0, &format!("{IDENTITY} {}", node.id.0))?,
                    }
                }
            }
            pending.extend(node.children.iter().rev());
        }
        Ok(())
    }
}

/// The symbol a hop starts from and the symbol it reaches.
///
/// An outgoing hop followed the relationship from its `from` to its `to`; an incoming hop went
/// against it.
fn ends_of(hop: &GraphHop) -> (&SymbolId, &SymbolId) {
    let GraphHop {
        relationship,
        direction,
    } = hop;
    match direction {
        HopDirection::Outgoing => (&relationship.from, &relationship.to),
        HopDirection::Incoming => (&relationship.to, &relationship.from),
    }
}

/// The mark and the relationship of `hop`, read from the node above it: `↳ called`, with `by`
/// left to the declaration line.
fn lead_of(hop: &GraphHop) -> Result<Lead, TextError> {
    let GraphHop {
        relationship,
        direction,
    } = hop;
    let Relationship {
        from: _,
        kind: _,
        facets,
        to: _,
        evidence: _,
        derivation,
        confidence: _,
        extensions: _,
    } = relationship;
    let mut phrases = Vec::with_capacity(facets.len());
    for facet in facets {
        phrases.push(match direction {
            HopDirection::Outgoing => spaced_name(facet)?,
            HopDirection::Incoming => reversed(*facet).to_owned(),
        });
    }
    let guess = if *derivation == RelationshipDerivation::Heuristic {
        HEURISTIC
    } else {
        ""
    };
    let cut: Option<Vec<&str>> = phrases
        .iter()
        .map(|phrase| phrase.strip_suffix(BY_ENDING))
        .collect();
    let (words, by) = match cut {
        Some(cut) if !cut.is_empty() => (cut.join(FACET_SEPARATOR), true),
        Some(_) | None => (phrases.join(FACET_SEPARATOR), false),
    };
    Ok(Lead {
        text: format!("{BRANCH} {guess}{words}").trim_end().to_owned(),
        by,
    })
}

/// The relationship `facet` read from its target back to its source.
fn reversed(facet: RelationshipFacet) -> &'static str {
    match facet {
        RelationshipFacet::Contains => "contained by",
        RelationshipFacet::Declares => "declared by",
        RelationshipFacet::Augments => "augmented by",
        RelationshipFacet::References => "referenced by",
        RelationshipFacet::Calls => "called by",
        RelationshipFacet::Constructs => "constructed by",
        RelationshipFacet::Reads => "read by",
        RelationshipFacet::Writes => "written by",
        RelationshipFacet::Imports => "imported by",
        RelationshipFacet::Exports => "exported by",
        RelationshipFacet::Extends => "extended by",
        RelationshipFacet::Implements => "implemented by",
        RelationshipFacet::HasType => "type of",
        RelationshipFacet::Overrides => "overridden by",
        RelationshipFacet::Aliases => "aliased by",
        RelationshipFacet::Generates => "generated by",
        RelationshipFacet::DependsOn => "dependency of",
        RelationshipFacet::AnnotatedBy => "annotates",
        RelationshipFacet::Throws => "thrown by",
        RelationshipFacet::Catches => "caught by",
        RelationshipFacet::BoundedBy => "bounds",
        RelationshipFacet::Instantiates => "instantiated by",
        RelationshipFacet::Specializes => "specialized by",
        RelationshipFacet::Overloads => "overloaded by",
        RelationshipFacet::MixesIn => "mixed in by",
        RelationshipFacet::Embeds => "embedded by",
        RelationshipFacet::Tests => "tested by",
        RelationshipFacet::Configures => "configured by",
        RelationshipFacet::Binds => "bound by",
    }
}
