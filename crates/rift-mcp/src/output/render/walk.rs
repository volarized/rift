//! Text of the hits an edge walk reached, written as a tree.
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
//! symbol starts with `↳` and the edge read from the symbol above it, then names the symbol
//! the hop reached. A symbol that is a hit of the answer writes the lines of that hit: the
//! declaration, the line that starts with its location behind `in`, and the symbol identity
//! behind `at`. Any other symbol writes its symbol identity behind `at`. The root starts with its
//! declaration, or with its identity when it is no hit. A blank line precedes every symbol except
//! the first entry of the section.
//!
//! An outgoing hop writes each kind of its edge as it is spelled on the wire, with
//! underscores as spaces (`calls`, `depends on`). An incoming hop writes the kind read from the
//! other end (`called by`, `dependency of`). Several kinds are joined by `, `, and a
//! edge whose derivation is `heuristic` starts with `probably`. An edge an external
//! producer emitted ends with ` | ` and the producer's name and revision (`↳ called |
//! demo-indexer 0.6.1`). When every kind ends in `by`, the head line leaves that word out and the
//! declaration line starts with it.

use rift_protocol::read::{Edge, EdgeDerivation, EdgeKind, SymbolId};
use rift_protocol::search::{GraphHop, HopDirection, SearchHit};

use super::facts::spaced_name;
use super::layout::Lines;
use super::search;
use crate::output::text::{TextError, TextWriter};

/// Mark that starts a symbol under the root.
const BRANCH: &str = "↳";
/// Word written before the declaration of a symbol whose edge ends in it.
const BY: &str = "by";
/// End of an edge kind that moves to the declaration line.
const BY_ENDING: &str = " by";
/// Word written before the location of a symbol.
const PLACE: &str = "in";
/// Word written before the symbol identity of a symbol.
const IDENTITY: &str = "at";
/// Word written before the edge of a heuristic hop.
const HEURISTIC: &str = "probably ";
/// Separates an edge from the external producer that emitted it.
const PRODUCER_SEPARATOR: &str = " | ";
/// Text between two kinds of one edge.
const KIND_SEPARATOR: &str = ", ";

/// The words a symbol of a walk writes before the lines that name its symbol.
#[derive(Clone, Copy)]
pub(super) enum Labels {
    /// An item outside a walk: no line starts with a word.
    Item,
    /// The root: the declaration is the head line, the location follows `in`, and the identity
    /// follows `at`.
    Root,
    /// A symbol under the root: the location follows `in` and the identity follows `at`. The
    /// declaration follows `by` when the edge ends in that word.
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

/// The head line of a symbol under the root.
struct Lead {
    /// The mark and the edge: `↳ called`.
    text: String,
    /// Whether the edge ends in `by`, which the declaration line then starts with.
    by: bool,
}

/// One symbol of a walk.
struct WalkSymbol<'a> {
    /// Identity of the symbol.
    id: &'a SymbolId,
    /// Levels under the root.
    depth: usize,
    /// Head line of a symbol under the root. The root has none.
    lead: Option<Lead>,
    /// The hit that is this symbol, when the answer holds one.
    hit: Option<&'a SearchHit>,
    /// Places, among the symbols, of the symbols directly under this one, in answer order.
    children: Vec<usize>,
}

/// The hits of a `search` answer, split into the trees of its walk and the hits outside it.
pub(super) struct Walk<'a> {
    /// Hits no edge path reached, in answer order.
    outside: Vec<&'a SearchHit>,
    /// Every symbol of every tree.
    symbols: Vec<WalkSymbol<'a>>,
    /// Places, among the symbols, of the roots, in answer order.
    roots: Vec<usize>,
}

impl<'a> Walk<'a> {
    /// Splits `hits`. A hit outside the walk that is the symbol a tree starts from becomes the
    /// root of that tree.
    ///
    /// # Errors
    ///
    /// Fails when an edge kind has no unit wire name.
    pub(super) fn of(hits: &'a [SearchHit]) -> Result<Self, TextError> {
        let mut walk = Self {
            outside: Vec::new(),
            symbols: Vec::new(),
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
        if let Some(symbol) = self.symbols.get_mut(place) {
            symbol.hit = Some(hit);
        }
        Ok(())
    }

    /// The place of the root that is `start`, added when no tree starts from it yet.
    fn root(&mut self, start: &'a SymbolId) -> usize {
        let known = self.roots.iter().copied().find(|root| {
            self.symbols
                .get(*root)
                .is_some_and(|symbol| symbol.id == start)
        });
        known.unwrap_or_else(|| {
            let place = self.symbols.len();
            self.symbols.push(WalkSymbol {
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

    /// The place of the symbol `hop` reaches under `parent`, added when `parent` has none.
    ///
    /// The `last` hop of a path never lands on a symbol that already is a hit, so every hit keeps
    /// a symbol of its own.
    fn child(&mut self, parent: usize, hop: &'a GraphHop, last: bool) -> Result<usize, TextError> {
        let (_, reached) = ends_of(hop);
        let above = self.symbols.get(parent);
        let known = above.and_then(|above| {
            above.children.iter().copied().find(|child| {
                self.symbols
                    .get(*child)
                    .is_some_and(|symbol| symbol.id == reached && !(last && symbol.hit.is_some()))
            })
        });
        if let Some(known) = known {
            return Ok(known);
        }
        let depth = above.map_or(0, |above| above.depth).saturating_add(1);
        let place = self.symbols.len();
        self.symbols.push(WalkSymbol {
            id: reached,
            depth,
            lead: Some(lead_of(hop)?),
            hit: None,
            children: Vec::new(),
        });
        if let Some(above) = self.symbols.get_mut(parent) {
            above.children.push(place);
        }
        Ok(place)
    }

    /// Moves each hit outside the walk that is the starting symbol of a tree to that root.
    fn claim_roots(&mut self) {
        for root in &self.roots {
            let Some(symbol) = self.symbols.get_mut(*root) else {
                continue;
            };
            let found = self
                .outside
                .iter()
                .position(|hit| search::symbol_id(hit) == Some(symbol.id));
            if let Some(found) = found {
                symbol.hit = Some(self.outside.remove(found));
            }
        }
    }

    /// Writes every tree, each symbol before the symbols under it.
    ///
    /// # Errors
    ///
    /// Returns the first failure of `out`, or of a hit whose enum field has no unit wire name.
    pub(super) fn write(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let mut opens_section = self.outside.is_empty();
        let mut pending: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(symbol) = pending.pop().and_then(|place| self.symbols.get(place)) {
            if !opens_section {
                out.blank_line()?;
            }
            opens_section = false;
            let lead = symbol.lead.as_ref();
            let mut lines =
                Lines::walk_symbol(out, symbol.depth, lead.map(|lead| lead.text.clone()));
            match (lead, symbol.hit) {
                (None, Some(hit)) => search::walk_symbol(&mut lines, hit, Labels::Root)?,
                (None, None) => lines.head(&format!("{IDENTITY} {}", symbol.id.0))?,
                (Some(lead), hit) => {
                    lines.head("")?;
                    match hit {
                        Some(hit) => {
                            search::walk_symbol(&mut lines, hit, Labels::Branch { by: lead.by })?;
                        }
                        None => lines.line(0, &format!("{IDENTITY} {}", symbol.id.0))?,
                    }
                }
            }
            pending.extend(symbol.children.iter().rev());
        }
        Ok(())
    }
}

/// The symbol a hop starts from and the symbol it reaches.
///
/// An outgoing hop followed the edge from its `from` to its `to`; an incoming hop went
/// against it.
fn ends_of(hop: &GraphHop) -> (&SymbolId, &SymbolId) {
    let GraphHop { edge, direction } = hop;
    match direction {
        HopDirection::Outgoing => (&edge.from, &edge.to),
        HopDirection::Incoming => (&edge.to, &edge.from),
    }
}

/// The mark and the edge of `hop`, read from the symbol above it: `↳ called`, with `by`
/// left to the declaration line.
fn lead_of(hop: &GraphHop) -> Result<Lead, TextError> {
    let GraphHop { edge, direction } = hop;
    let Edge {
        from: _,
        kind: _,
        kinds,
        to: _,
        occurrence: _,
        providers,
        derivation,
        confidence: _,
        producer,
        extensions: _,
    } = edge;
    let mut phrases = Vec::with_capacity(kinds.len());
    for kind in kinds {
        phrases.push(match direction {
            HopDirection::Outgoing => spaced_name(kind)?,
            HopDirection::Incoming => reversed(*kind).to_owned(),
        });
    }
    let guess = if *derivation == EdgeDerivation::Heuristic {
        HEURISTIC
    } else {
        ""
    };
    let cut: Option<Vec<&str>> = phrases
        .iter()
        .map(|phrase| phrase.strip_suffix(BY_ENDING))
        .collect();
    let (words, by) = match cut {
        Some(cut) if !cut.is_empty() => (cut.join(KIND_SEPARATOR), true),
        Some(_) | None => (phrases.join(KIND_SEPARATOR), false),
    };
    let produced = producer.as_ref().map_or_else(String::new, |producer| {
        format!(
            "{PRODUCER_SEPARATOR}{} {}",
            producer.name, producer.revision
        )
    });
    let analyses = if providers.is_empty() {
        String::new()
    } else {
        format!(
            " (providers: {})",
            providers
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(Lead {
        text: format!("{BRANCH} {guess}{words}{analyses}{produced}")
            .trim_end()
            .to_owned(),
        by,
    })
}

/// The edge `kind` read from its target back to its source.
fn reversed(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Contains => "contained by",
        EdgeKind::Declares => "declared by",
        EdgeKind::Augments => "augmented by",
        EdgeKind::References => "referenced by",
        EdgeKind::Calls => "called by",
        EdgeKind::Constructs => "constructed by",
        EdgeKind::Reads => "read by",
        EdgeKind::Writes => "written by",
        EdgeKind::Imports => "imported by",
        EdgeKind::Exports => "exported by",
        EdgeKind::Extends => "extended by",
        EdgeKind::Implements => "implemented by",
        EdgeKind::HasType => "type of",
        EdgeKind::Overrides => "overridden by",
        EdgeKind::Aliases => "aliased by",
        EdgeKind::Generates => "generated by",
        EdgeKind::DependsOn => "dependency of",
        EdgeKind::AnnotatedBy => "annotates",
        EdgeKind::Throws => "thrown by",
        EdgeKind::Catches => "caught by",
        EdgeKind::BoundedBy => "bounds",
        EdgeKind::Instantiates => "instantiated by",
        EdgeKind::Specializes => "specialized by",
        EdgeKind::Overloads => "overloaded by",
        EdgeKind::MixesIn => "mixed in by",
        EdgeKind::Embeds => "embedded by",
        EdgeKind::Tests => "tested by",
        EdgeKind::Configures => "configured by",
        EdgeKind::Binds => "bound by",
    }
}
