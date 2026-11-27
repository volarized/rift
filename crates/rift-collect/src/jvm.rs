//! Authored JVM occurrences from the shipped syntax providers.

use std::future::{Future, ready};

use rift_core::ProjectPath;
use rift_error::RiftError;
use rift_syntax::{JvmLanguage, JvmOccurrenceKind, SyntaxLimits, SyntaxSource};

use crate::{
    Batch, ByteRange, Category, CollectionInput, CollectionSource, Collector, Coverage, Evidence,
    OBSERVATIONS_MAX, Observation,
};

/// Collects the authored call, reference, and import positions in Java and Kotlin files.
#[derive(Debug, Default)]
pub struct JvmSyntaxCollector;

impl Collector for JvmSyntaxCollector {
    type Error = RiftError;

    fn collect(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> impl Future<Output = Result<Batch, Self::Error>> {
        ready(syntax_batch(input.selected(), category))
    }
}

fn syntax_batch(selected: &CollectionSource, category: Category) -> Result<Batch, RiftError> {
    let Some(language) = JvmLanguage::from_filename(selected.filename()) else {
        return Ok(Batch {
            coverage: Coverage::Unsupported,
            observations: Vec::new(),
        });
    };
    let filename = selected
        .filename()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let path = ProjectPath::new(filename)?;
    let source = SyntaxSource {
        path: &path,
        text: selected.text(),
    };
    let kind = match category {
        Category::Calls => JvmOccurrenceKind::Call,
        Category::References => JvmOccurrenceKind::Reference,
        Category::Imports => JvmOccurrenceKind::Import,
    };
    let found = language.occurrences(source, SyntaxLimits::default(), kind, OBSERVATIONS_MAX)?;
    let range = |range: rift_syntax::ByteRange| ByteRange {
        start: range.start,
        end: range.end,
    };
    Ok(Batch {
        coverage: if found.complete {
            Coverage::Complete
        } else {
            Coverage::Partial
        },
        observations: found
            .occurrences
            .into_iter()
            .map(|occurrence| Observation {
                id: format!("{}:{}", occurrence.range.start, occurrence.range.end),
                occurrence: range(occurrence.range),
                enclosing: occurrence.enclosing.map(range),
                target: None,
                evidence: Evidence::Syntax,
            })
            .collect(),
    })
}
