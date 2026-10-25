use super::{
    HashMap, Pattern, Projection, QueryError, QueryFacts, QueryLimits, Term, VirtualDataset,
    provider::ProviderIndex, term_bytes,
};
use oxigraph::sparql::CancellationToken;

/// Resource selectors into the original symbol records, including their nested metadata.
pub(super) struct SymbolIndex {
    by_resource: HashMap<Term, Vec<usize>>,
    current: usize,
    owners: usize,
    bytes: usize,
    complete: bool,
}

impl SymbolIndex {
    pub(super) fn is_full(&self) -> bool {
        !self.complete
    }

    pub(super) fn disable(&mut self) {
        self.complete = false;
    }

    pub(super) fn new(
        facts: &QueryFacts,
        providers: &ProviderIndex,
        limits: QueryLimits,
        cancellation: CancellationToken,
    ) -> Result<Option<Self>, QueryError> {
        let mut projection = Projection::new(
            Pattern {
                subject: None,
                predicate: None,
                object: None,
                graph_name: None,
            },
            limits,
            cancellation,
        )?;
        projection.symbol_subjects = Some(Self {
            by_resource: HashMap::new(),
            current: 0,
            owners: 0,
            bytes: 0,
            complete: true,
        });
        // Use the same visitor as quad projection so every declaration, type, signature,
        // documentation entry and nested resource selects its original symbol record.
        for (index, (symbol, subject)) in facts.symbols.iter().zip(&providers.symbols).enumerate() {
            if let Some(subjects) = &mut projection.symbol_subjects {
                subjects.current = index;
            }
            projection.symbol(subject, symbol)?;
            if projection
                .symbol_subjects
                .as_ref()
                .is_some_and(|subjects| !subjects.complete)
            {
                break;
            }
        }
        // This index only reduces work. If it cannot fit, retain the complete scan.
        Ok(projection
            .symbol_subjects
            .filter(|subjects| subjects.complete))
    }

    pub(super) fn record(&mut self, subject: Term, limits: QueryLimits) {
        if !self.complete
            || self
                .by_resource
                .get(&subject)
                .is_some_and(|owners| owners.last() == Some(&self.current))
        {
            return;
        }
        let resource_bytes = if self.by_resource.contains_key(&subject) {
            Some(0)
        } else {
            term_bytes(&subject).checked_add(size_of::<Term>())
        };
        let Some(bytes) = resource_bytes
            .and_then(|bytes| self.bytes.checked_add(bytes))
            .and_then(|bytes| bytes.checked_add(size_of::<usize>()))
        else {
            self.complete = false;
            return;
        };
        if self.owners >= limits.quads_max || bytes > limits.rdf_bytes_max {
            self.complete = false;
            return;
        }
        self.bytes = bytes;
        self.owners += 1;
        self.by_resource
            .entry(subject)
            .or_default()
            .push(self.current);
    }
}

impl VirtualDataset {
    pub(super) fn symbols(&self, output: &mut Projection) -> Result<(), QueryError> {
        if output.relationship_predicate() {
            return Ok(());
        }
        if let (Some(subject), Some(index)) = (&output.pattern.subject, self.symbols.as_ref()) {
            if let Some(owners) = index.by_resource.get(subject) {
                for &index in owners {
                    output.symbol(&self.providers.symbols[index], &self.facts.symbols[index])?;
                }
            }
            return Ok(());
        }
        for (symbol, subject) in self.facts.symbols.iter().zip(&self.providers.symbols) {
            output.symbol(subject, symbol)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::super::{
        GraphName, Pattern, QueryFacts, QueryLimits, QuerySymbol, Term, VirtualDataset, blank,
    };
    use super::SymbolIndex;
    use oxigraph::sparql::CancellationToken;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    type Quad = (Option<Term>, Term, Term, Term);

    fn limits() -> QueryLimits {
        QueryLimits {
            query_bytes_max: 16_384,
            algebra_nodes_max: 4096,
            quads_max: 100_000,
            rdf_bytes_max: 16_777_216,
            result_items_max: 20_000,
            result_bytes_max: 16_777_216,
            input_bytes_max: None,
            output_bytes_max: None,
            material_bytes_max: None,
            heap_bytes_max: None,
            diagnostic_bytes_max: None,
        }
    }

    fn fixture() -> Result<QueryFacts, Box<dyn std::error::Error>> {
        let mut facts = QueryFacts::default();
        for (identity, name) in [
            ("shared", "first"),
            ("shared", "second"),
            ("other", "third"),
        ] {
            let mut symbol: rift_protocol::read::Symbol = serde_json::from_value(
                serde_json::json!({
                    "id": format!("rift://symbol/local/rust/fixture/{identity}"),
                    "name": name, "kind": "function", "language": "rust",
                    "signatures": [{
                        "display": format!("fn {name}(value: u32)"), "language": "rust",
                        "parameters": [{"name":"value", "optional":false, "variadic":false}],
                        "effects": ["async", "unsafe"]
                    }],
                    "types": [{"role":"return", "origin":"declared", "type":{"language":"rust", "source":"u32"}}],
                    "documentation": [{"format":"markdown", "text":format!("# {name}\nλ") }]
                }),
            )?;
            symbol.signatures[0].extensions =
                rift_protocol::read::Extensions::from(std::collections::BTreeMap::from([(
                    rift_protocol::read::ExtensionKey("org.rift.fixture".to_owned()),
                    rift_protocol::read::ExtensionValue {
                        version: 1,
                        data: serde_json::json!({"array":[1,"two"]}),
                    },
                )]));
            let declaration = serde_json::from_value(serde_json::json!({
                "origin": serde_json::to_value(&symbol.origin)?,
                "path": format!("{name}.rs"), "range":{"start":0,"end":12}, "line":1,
                "source": format!("fn {name}() {{}}"), "source_complete":true,
                "signature_indices":[0], "type_indices":[0], "documentation_indices":[0]
            }))?;
            facts.symbols.push(QuerySymbol {
                symbol,
                declarations: vec![declaration],
            });
        }
        Ok(facts)
    }

    fn quads(
        dataset: &VirtualDataset,
        pattern: &Pattern,
    ) -> Result<HashSet<Quad>, super::QueryError> {
        Ok(dataset
            .project(pattern)?
            .into_iter()
            .map(|quad| (quad.graph_name, quad.subject, quad.predicate, quad.object))
            .collect())
    }

    #[test]
    fn bound_resources_keep_every_field_from_all_original_symbol_records() -> TestResult {
        let selected = VirtualDataset::new(fixture()?, limits(), CancellationToken::new())?;
        let mut complete = selected.clone();
        complete.symbols = Arc::new(None);
        let pattern = Pattern {
            subject: None,
            predicate: None,
            object: None,
            graph_name: Some(GraphName::DefaultGraph),
        };
        let all = quads(&complete, &pattern)?;
        assert!(all.len() > 40);
        let mut subjects = all
            .iter()
            .map(|(_, subject, _, _)| subject.clone())
            .collect::<HashSet<_>>();
        subjects.insert(blank(b"unknown symbol metadata resource")?);
        for subject in subjects {
            let bound = Pattern {
                subject: Some(subject.clone()),
                ..pattern.clone()
            };
            assert_eq!(quads(&selected, &bound)?, quads(&complete, &bound)?);
            for (_, _, predicate, object) in all.iter().filter(|(_, owner, _, _)| owner == &subject)
            {
                let bound = Pattern {
                    predicate: Some(predicate.clone()),
                    object: Some(object.clone()),
                    ..bound.clone()
                };
                assert_eq!(quads(&selected, &bound)?, quads(&complete, &bound)?);
            }
        }
        Ok(())
    }

    #[test]
    fn subject_index_capacity_falls_back_without_changing_the_graph() -> TestResult {
        let selected = VirtualDataset::new(fixture()?, limits(), CancellationToken::new())?;
        let mut bounded = limits();
        bounded.rdf_bytes_max = 1;
        assert!(
            SymbolIndex::new(
                &selected.facts,
                &selected.providers,
                bounded,
                CancellationToken::new()
            )?
            .is_none()
        );
        bounded = limits();
        bounded.quads_max = 10;
        assert!(
            SymbolIndex::new(
                &selected.facts,
                &selected.providers,
                bounded,
                CancellationToken::new()
            )?
            .is_none()
        );
        let mut fallback = selected.clone();
        fallback.symbols = Arc::new(None);
        let pattern = Pattern {
            subject: None,
            predicate: None,
            object: None,
            graph_name: None,
        };
        assert_eq!(quads(&selected, &pattern)?, quads(&fallback, &pattern)?);
        Ok(())
    }

    #[test]
    fn metadata_index_capacity_does_not_refuse_an_unrelated_relationship_question() -> TestResult {
        let mut facts = fixture()?;
        facts.symbols.truncate(1);
        facts.symbols[0].declarations.clear();
        for _ in 0..32 {
            facts.symbols[0].symbol.signatures[0]
                .parameters
                .push(serde_json::from_value(
                    serde_json::json!({"name":"value", "optional":false, "variadic":false}),
                )?);
        }
        let mut bounded = limits();
        bounded.quads_max = 10;
        let selected = VirtualDataset::new(facts, bounded, CancellationToken::new())?;
        assert!(selected.symbols.is_none());
        let pattern = Pattern {
            subject: None,
            predicate: Some(super::super::vocabulary("calls")?),
            object: None,
            graph_name: Some(GraphName::DefaultGraph),
        };
        assert!(quads(&selected, &pattern)?.is_empty());
        assert!(
            quads(
                &selected,
                &Pattern {
                    subject: None,
                    predicate: None,
                    object: None,
                    graph_name: Some(GraphName::DefaultGraph),
                }
            )
            .is_err(),
            "the normal metadata projection still enforces its bound"
        );
        Ok(())
    }
}
