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
    mod model_oracle {
        use super::*;
        use crate::sparql::{PreparedQuery, QueryCancellation};
        use rift_protocol::query::{QueryAnswer, RdfTerm, RdfTriple};
        use rift_protocol::read::Symbol;
        use serde_json::{Value, json};

        type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

        struct Model<'a>(&'a [RdfTriple]);

        impl Model<'_> {
            fn objects<'a>(&'a self, subject: &RdfTerm, key: &str) -> Vec<&'a RdfTerm> {
                let predicate = format!("{}{key}", crate::sparql::VOCABULARY);
                self.0
                    .iter()
                    .filter_map(|triple| {
                        (triple.subject == *subject
                            && triple.predicate
                                == RdfTerm::Iri {
                                    value: predicate.clone(),
                                })
                        .then_some(&triple.object)
                    })
                    .collect()
            }

            fn optional<'a>(&'a self, subject: &RdfTerm, key: &str) -> Result<Option<&'a RdfTerm>> {
                let values = self.objects(subject, key);
                match values.as_slice() {
                    [] => Ok(None),
                    [value] => Ok(Some(*value)),
                    _ => Err(format!("multiple {key} values").into()),
                }
            }

            fn one<'a>(&'a self, subject: &RdfTerm, key: &str) -> Result<&'a RdfTerm> {
                self.optional(subject, key)?
                    .ok_or_else(|| format!("missing {key}").into())
            }

            fn string(&self, subject: &RdfTerm, key: &str) -> Result<String> {
                match self.one(subject, key)? {
                    RdfTerm::Literal { value, datatype }
                        if datatype == "http://www.w3.org/2001/XMLSchema#string" =>
                    {
                        Ok(value.clone())
                    }
                    _ => Err(format!("{key} must be a string literal").into()),
                }
            }

            fn iri(&self, subject: &RdfTerm, key: &str) -> Result<String> {
                match self.one(subject, key)? {
                    RdfTerm::Iri { value } => Ok(value.clone()),
                    _ => Err(format!("{key} must be an IRI").into()),
                }
            }

            fn integer(&self, subject: &RdfTerm, key: &str) -> Result<u64> {
                match self.one(subject, key)? {
                    RdfTerm::Literal { value, datatype }
                        if datatype == "http://www.w3.org/2001/XMLSchema#integer" =>
                    {
                        Ok(value.parse()?)
                    }
                    _ => Err(format!("{key} must be an integer literal").into()),
                }
            }

            fn boolean(&self, subject: &RdfTerm, key: &str) -> Result<bool> {
                match self.one(subject, key)? {
                    RdfTerm::Literal { value, datatype }
                        if datatype == "http://www.w3.org/2001/XMLSchema#boolean" =>
                    {
                        Ok(value.parse()?)
                    }
                    _ => Err(format!("{key} must be a boolean literal").into()),
                }
            }

            fn array<'a>(&'a self, subject: &RdfTerm, key: &str) -> Result<Vec<&'a RdfTerm>> {
                let mut indexed = self
                    .objects(subject, key)
                    .into_iter()
                    .map(|entry| Ok((self.integer(entry, "index")?, entry)))
                    .collect::<Result<Vec<_>>>()?;
                indexed.sort_by_key(|(index, _)| *index);
                for (position, (index, _)) in indexed.iter().enumerate() {
                    assert_eq!(*index, u64::try_from(position)?);
                }
                Ok(indexed.into_iter().map(|(_, entry)| entry).collect())
            }

            fn integers(&self, subject: &RdfTerm, key: &str) -> Result<Vec<u64>> {
                self.array(subject, key)?
                    .into_iter()
                    .map(|entry| self.integer(entry, "value"))
                    .collect()
            }

            fn origin(&self, subject: &RdfTerm) -> Result<Value> {
                let origin = self.one(subject, "origin")?;
                assert!(self.optional(origin, "package")?.is_none());
                assert!(self.optional(origin, "runtime")?.is_none());
                Ok(json!({
                    "location": self.string(origin, "location")?,
                    "source_kind": self.string(origin, "source_kind")?
                }))
            }

            fn signature(&self, subject: &RdfTerm) -> Result<Value> {
                let parameters = self.array(subject, "parameters")?.into_iter().map(|parameter| {
                    Ok(json!({"name":self.string(parameter,"name")?,"optional":self.boolean(parameter,"optional")?,"variadic":self.boolean(parameter,"variadic")?}))
                }).collect::<Result<Vec<_>>>()?;
                let effects = self
                    .array(subject, "effects")?
                    .into_iter()
                    .map(|effect| self.string(effect, "value"))
                    .collect::<Result<Vec<_>>>()?;
                let type_parameters = self
                    .array(subject, "type_parameters")?
                    .into_iter()
                    .map(|entry| self.iri(entry, "value"))
                    .collect::<Result<Vec<_>>>()?;
                let mut signature = json!({"display":self.string(subject,"display")?,"language":self.string(subject,"language")?,"parameters":parameters,"effects":effects,"type_parameters":type_parameters});
                if let Some(extensions) = self.optional(subject, "extensions")? {
                    let RdfTerm::Literal { value, datatype } = extensions else {
                        return Err("extension JSON literal".into());
                    };
                    assert_eq!(datatype, "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON");
                    signature["extensions"] = serde_json::from_str(value)?;
                }
                Ok(signature)
            }

            fn type_use(&self, subject: &RdfTerm) -> Result<Value> {
                let term = self.one(subject, "type")?;
                let mut expression = json!({"language":self.string(term,"language")?,"source":self.string(term,"source")?});
                if self.optional(term, "resolved")?.is_some() {
                    expression["resolved"] = json!(self.iri(term, "resolved")?);
                }
                Ok(
                    json!({"role":self.string(subject,"role")?,"origin":self.string(subject,"origin")?,"type":expression}),
                )
            }

            fn documentation(&self, subject: &RdfTerm) -> Result<Value> {
                Ok(
                    json!({"format":self.string(subject,"format")?,"text":self.string(subject,"text")?}),
                )
            }

            fn symbol(&self, subject: &RdfTerm) -> Result<Symbol> {
                let signatures = self
                    .array(subject, "signatures")?
                    .into_iter()
                    .map(|entry| self.signature(entry))
                    .collect::<Result<Vec<_>>>()?;
                let types = self
                    .array(subject, "types")?
                    .into_iter()
                    .map(|entry| self.type_use(entry))
                    .collect::<Result<Vec<_>>>()?;
                let documentation = self
                    .array(subject, "documentation")?
                    .into_iter()
                    .map(|entry| self.documentation(entry))
                    .collect::<Result<Vec<_>>>()?;
                let mut symbol = json!({"name":self.string(subject,"name")?,"kind":self.string(subject,"kind")?,"language":self.string(subject,"language")?,"origin":self.origin(subject)?,"document_local":self.boolean(subject,"document_local")?,"signatures":signatures,"types":types,"documentation":documentation});
                if let Some(id) = self.optional(subject, "id")? {
                    let RdfTerm::Iri { value } = id else {
                        return Err("canonical symbol IRI".into());
                    };
                    symbol["id"] = json!(value);
                    assert_eq!(subject, id);
                } else {
                    assert!(matches!(subject, RdfTerm::BlankNode { .. }));
                }
                Ok(serde_json::from_value(symbol)?)
            }

            fn associations(
                &self,
                symbol: &RdfTerm,
                declaration: &RdfTerm,
                key: &str,
                indices: &[u64],
            ) -> Result<()> {
                let entries = self.array(symbol, key)?;
                let linked = self.objects(declaration, key);
                let mut actual = std::collections::BTreeSet::new();
                for resource in linked {
                    assert!(entries.contains(&resource));
                    actual.insert(self.integer(resource, "index")?);
                }
                assert_eq!(actual, indices.iter().copied().collect());
                Ok(())
            }

            fn declaration(
                &self,
                symbol: &RdfTerm,
                subject: &RdfTerm,
            ) -> Result<rift_protocol::symbol_read::SymbolDeclaration> {
                let range = self.one(subject, "range")?;
                let signatures = self.integers(subject, "signature_indices")?;
                let types = self.integers(subject, "type_indices")?;
                let documentation = self.integers(subject, "documentation_indices")?;
                self.associations(symbol, subject, "signatures", &signatures)?;
                self.associations(symbol, subject, "types", &types)?;
                self.associations(symbol, subject, "documentation", &documentation)?;
                let mut declaration = json!({"origin":self.origin(subject)?,"range":{"start":self.integer(range,"start")?,"end":self.integer(range,"end")?},"line":self.integer(subject,"line")?,"source_complete":self.boolean(subject,"source_complete")?,"signature_indices":signatures,"type_indices":types,"documentation_indices":documentation});
                if self.optional(subject, "path")?.is_some() {
                    declaration["path"] = json!(self.string(subject, "path")?);
                }
                if let Some(unit) = self.optional(subject, "unit")? {
                    let RdfTerm::Iri { value } = unit else {
                        return Err("physical source unit IRI".into());
                    };
                    declaration["unit"] = json!(value);
                }
                assert_ne!(
                    declaration.get("path").is_some(),
                    declaration.get("unit").is_some()
                );
                if self.optional(subject, "source")?.is_some() {
                    declaration["source"] = json!(self.string(subject, "source")?);
                }
                Ok(serde_json::from_value(declaration)?)
            }
        }

        fn representative_models() -> Result<QueryFacts> {
            let mut facts = fixture()?;
            let mut original = facts.symbols.pop().ok_or("original symbol")?;
            let target = facts.symbols.remove(0);
            let target_id = target.symbol.id.as_ref().ok_or("target symbol identity")?;
            let original_id = original
                .symbol
                .id
                .as_ref()
                .ok_or("original symbol identity")?;
            let mut value = serde_json::to_value(&original.symbol)?;
            value["types"][0]["type"]["resolved"] = json!(target_id);
            value["signatures"][0]["type_parameters"] =
                json!([original_id, target_id, original_id]);
            for field in ["signatures", "types", "documentation"] {
                let mut entries = value[field].as_array().ok_or("model array")?.clone();
                entries.push(entries[0].clone());
                value[field] = json!(entries);
            }
            for signature in value["signatures"].as_array_mut().ok_or("signatures")? {
                let parameter = signature["parameters"][0].clone();
                signature["parameters"]
                    .as_array_mut()
                    .ok_or("parameters")?
                    .push(parameter);
            }
            original.symbol = serde_json::from_value(value)?;
            let mut declaration = serde_json::to_value(&original.declarations[0])?;
            declaration["signature_indices"] = json!([1, 0]);
            declaration["type_indices"] = json!([0, 1]);
            declaration["documentation_indices"] = json!([1, 0]);
            let bytes = declaration["source"]
                .as_str()
                .ok_or("declaration source")?
                .len();
            declaration["range"]["end"] = json!(bytes);
            original.declarations = vec![serde_json::from_value(declaration)?];
            let mut anonymous = original.clone();
            anonymous.symbol.id = None;
            anonymous.symbol.name = "anonymous".into();
            facts.symbols = vec![original, anonymous, target];
            Ok(facts)
        }

        #[test]
        fn rdf_reconstructs_original_symbol_models_and_declaration_associations() -> Result<()> {
            let facts = representative_models()?;
            let mut expected = facts.symbols.clone();
            expected.sort_by(|left, right| left.symbol.name.cmp(&right.symbol.name));
            let answer = PreparedQuery::parse(
                "CONSTRUCT { ?subject ?predicate ?object } WHERE { ?subject ?predicate ?object }",
                limits(),
            )?
            .evaluate(facts, limits(), QueryCancellation::default())?;
            let QueryAnswer::Graph { triples } = answer else {
                return Err("complete RDF graph".into());
            };
            let model = Model(&triples);
            let class = RdfTerm::Iri {
                value: format!("{}Symbol", crate::sparql::VOCABULARY),
            };
            let predicate = RdfTerm::Iri {
                value: "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".into(),
            };
            let mut reconstructed = Vec::new();
            for triple in &triples {
                if triple.predicate != predicate || triple.object != class {
                    continue;
                }
                let symbol = model.symbol(&triple.subject)?;
                for binding in model.array(&triple.subject, "types")? {
                    let expression = model.one(binding, "type")?;
                    if let Some(resolved) = model.optional(expression, "resolved")? {
                        assert!(matches!(resolved, RdfTerm::Iri { .. }));
                        assert_eq!(model.string(resolved, "name")?, "first");
                    }
                }
                for signature in model.array(&triple.subject, "signatures")? {
                    let parameters = model.array(signature, "type_parameters")?;
                    if !parameters.is_empty() {
                        let names = parameters
                            .into_iter()
                            .map(|entry| model.string(model.one(entry, "value")?, "name"))
                            .collect::<Result<Vec<_>>>()?;
                        assert_eq!(names, ["third", "first", "third"]);
                    }
                }
                let declarations = model
                    .objects(&triple.subject, "declarations")
                    .into_iter()
                    .map(|declaration| model.declaration(&triple.subject, declaration))
                    .collect::<Result<Vec<_>>>()?;
                reconstructed.push(QuerySymbol {
                    symbol,
                    declarations,
                });
            }
            reconstructed.sort_by(|left, right| left.symbol.name.cmp(&right.symbol.name));
            assert_eq!(
                serde_json::to_value(reconstructed)?,
                serde_json::to_value(expected)?
            );
            Ok(())
        }
    }
}
