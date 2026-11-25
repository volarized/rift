//! Leaves out extracted fields the publication would refuse.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::documentation::{
    DocumentationDigest, DocumentationHeading, DocumentationLink, DocumentationReferenceCandidate,
    DocumentationWarningKind,
};
use rift_protocol::read::TextRange;

use super::{Collected, CollectedBlock, DocumentationInput, warn};
use crate::documentation::DocumentationLimits;
use crate::documentation::publication::{contains, contains_link_range, text_fits, valid_text};

/// Leaves out each field of one source that its block or an accepted bound cannot hold.
///
/// The block, its text, and every other field stay, so one field never refuses a
/// publication. A field past a bound counts as `limit_exceeded`, a field its block cannot
/// place as `omitted_range`.
pub(super) fn leave_out_refused_fields(input: &DocumentationInput<'_>, facts: &mut Collected) {
    let limits = input.limits();
    let mut omissions = FieldOmissions::default();
    for block in &mut facts.blocks {
        omissions.admit_headings(block, limits);
        omissions.admit_language(block, limits);
    }
    let blocks: BTreeMap<_, _> = facts
        .blocks
        .iter()
        .map(|block| (&block.identity, &block.range))
        .collect();
    let mut addresses = BTreeSet::new();
    for links in [&mut facts.links, &mut facts.unresolved_links] {
        links.retain(|link| omissions.admits_link(link, &blocks, limits, &mut addresses));
    }
    facts
        .candidates
        .retain(|candidate| omissions.admits_candidate(candidate, &blocks, limits));
    let FieldOmissions {
        past_bound,
        unplaced,
        headings,
    } = omissions;
    let headings_past_bound = headings.values().filter(|past_bound| **past_bound).count() as u64;
    let headings_unplaced = headings.len() as u64 - headings_past_bound;
    warn(
        input,
        facts,
        DocumentationWarningKind::LimitExceeded,
        past_bound + headings_past_bound,
    );
    warn(
        input,
        facts,
        DocumentationWarningKind::OmittedRange,
        unplaced + headings_unplaced,
    );
}

type BlockRanges<'facts> = BTreeMap<&'facts DocumentationDigest, &'facts TextRange>;
type LinkAddresses = BTreeSet<(DocumentationDigest, u64, u64)>;

/// Fields left out of one source, by reason.
#[derive(Default)]
struct FieldOmissions {
    past_bound: u64,
    unplaced: u64,
    /// Each left out heading once, however many blocks stand under it, with whether a bound
    /// left it out.
    headings: BTreeMap<(u32, String), bool>,
}

impl FieldOmissions {
    fn admit_headings(&mut self, block: &mut CollectedBlock, limits: DocumentationLimits) {
        let mut previous_level = 0;
        block.heading_path.retain(|heading| {
            let DocumentationHeading { level, name } = heading;
            let past_bound = *level > limits.heading_depth_max || !text_fits(name, Some(&limits));
            let placed = *level > previous_level && !name.is_empty();
            if past_bound || !placed {
                self.headings.insert((*level, name.clone()), past_bound);
                return false;
            }
            previous_level = *level;
            true
        });
    }

    fn admit_language(&mut self, block: &mut CollectedBlock, limits: DocumentationLimits) {
        let refused = block
            .language
            .take_if(|language| !valid_text(language, Some(&limits)));
        match refused {
            Some(language) if language.is_empty() => self.unplaced += 1,
            Some(_) => self.past_bound += 1,
            None => {}
        }
    }

    fn admits_link(
        &mut self,
        link: &DocumentationLink,
        blocks: &BlockRanges<'_>,
        limits: DocumentationLimits,
        addresses: &mut LinkAddresses,
    ) -> bool {
        let placed = blocks
            .get(&link.block)
            .is_some_and(|block| contains_link_range(block, &link.range))
            && addresses.insert((link.block.clone(), link.range.start, link.range.end));
        self.admits(placed, text_fits(&link.authored, Some(&limits)))
    }

    fn admits_candidate(
        &mut self,
        candidate: &DocumentationReferenceCandidate,
        blocks: &BlockRanges<'_>,
        limits: DocumentationLimits,
    ) -> bool {
        let placed = blocks
            .get(&candidate.block)
            .is_some_and(|block| contains(block, &candidate.range));
        self.admits(placed, text_fits(&candidate.authored, Some(&limits)))
    }

    fn admits(&mut self, placed: bool, fits: bool) -> bool {
        match (placed, fits) {
            (false, _) => self.unplaced += 1,
            (true, false) => self.past_bound += 1,
            (true, true) => return true,
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationBlockKind, DocumentationHeading, DocumentationLink,
        DocumentationLinkResolution, DocumentationReferenceCandidate,
        DocumentationUnresolvedReason, DocumentationWarningKind,
    };
    use rift_protocol::read::TextRange;

    use super::super::{BlockDraft, Collected, append_block};
    use super::leave_out_refused_fields;
    use crate::documentation::collect_tests::input;
    use crate::documentation::content_digest;

    #[test]
    fn fields_without_a_place_are_left_out_and_counted() {
        let text = "prose and other prose";
        let input = input("guide.md", text);
        let mut facts = Collected::default();
        let heading = |level, name: &str| DocumentationHeading {
            level,
            name: name.to_owned(),
        };
        let block = append_block(
            &input,
            &mut facts,
            BlockDraft {
                range: TextRange { start: 0, end: 9 },
                line: 1,
                kind: DocumentationBlockKind::Code,
                structure: "code",
                headings: vec![heading(2, "Kept"), heading(2, "Level"), heading(3, "")],
                language: Some(String::new()),
                symbol: None,
            },
            &mut std::collections::BTreeMap::new(),
        )
        .expect("block");
        let link = |start, end| DocumentationLink {
            block: block.clone(),
            authored: "next.md".to_owned(),
            range: TextRange { start, end },
            resolution: DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            },
        };
        let mut missing_block = link(0, 5);
        missing_block.block = content_digest(b"missing");
        facts.links = vec![link(0, 5), link(6, 10), missing_block];
        facts.unresolved_links = vec![link(0, 5), link(6, 9)];
        let candidate = |start, end| DocumentationReferenceCandidate {
            block: block.clone(),
            range: TextRange { start, end },
            authored: "prose".to_owned(),
            language: None,
            reason: DocumentationUnresolvedReason::Missing,
        };
        facts.candidates = vec![candidate(0, 5), candidate(10, 15), candidate(3, 3)];

        leave_out_refused_fields(&input, &mut facts);

        assert_eq!(facts.blocks.len(), 1);
        assert_eq!(facts.blocks[0].heading_path, [heading(2, "Kept")]);
        assert_eq!(facts.blocks[0].language, None);
        assert_eq!(facts.links, [link(0, 5)]);
        assert_eq!(facts.unresolved_links, [link(6, 9)]);
        assert_eq!(facts.candidates, [candidate(0, 5)]);
        let warnings: Vec<_> = facts
            .warnings
            .iter()
            .map(|warning| (warning.kind, warning.count))
            .collect();
        // Two headings, one language, three links, two candidates.
        assert_eq!(warnings, [(DocumentationWarningKind::OmittedRange, 8)]);
    }
}
