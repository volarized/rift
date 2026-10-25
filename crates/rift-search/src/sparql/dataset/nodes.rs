use super::{
    Digest, HashSet, Literal, Projection, QueryError, QueryFacts, Sha256, Value, blank, child,
    failure, invalid, iri, vocabulary,
};

pub(super) fn count(facts: &QueryFacts) -> Result<usize, QueryError> {
    facts.nodes.iter().try_fold(0usize, |count, source| {
        count
            .checked_add(source.nodes.len())
            .ok_or_else(|| failure::bound("quads_max"))
    })
}

pub(super) fn validate(facts: &QueryFacts) -> Result<(), QueryError> {
    let mut selected = HashSet::new();
    for source in &facts.nodes {
        source.validate()?;
        if !selected.insert(source.unit.as_str()) {
            return Err(failure::invalid("captured syntax source repeats"));
        }
        let catalog = catalog(facts, source.unit.as_str())?;
        if let Some(text) = &catalog.text {
            if u64::try_from(text.len()).map_err(invalid)? != source.bytes
                || data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
                    != catalog.digest.as_ref().ok_or_else(missing_source)?.as_str()
            {
                return Err(missing_source());
            }
            for row in &source.nodes {
                for range in std::iter::once(&row.node.range)
                    .chain(row.node.regions.iter().map(|region| &region.range))
                {
                    let start = usize::try_from(range.start).map_err(invalid)?;
                    let end = usize::try_from(range.end).map_err(invalid)?;
                    if text.get(start..end).is_none() {
                        return Err(missing_source());
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn project(facts: &QueryFacts, output: &mut Projection) -> Result<(), QueryError> {
    for source in &facts.nodes {
        let catalog = catalog(facts, source.unit.as_str())?;
        let root = blank(&serde_json::to_vec(&(&source.unit, &catalog.digest)).map_err(invalid)?)?;
        for row in &source.nodes {
            let subject = child(&root, "nodes", row.ordinal)?;
            output.class(subject.clone(), "Node")?;
            let mut fields = serde_json::to_value(&row.node).map_err(invalid)?;
            let fields = fields.as_object_mut().ok_or_else(missing_source)?;
            fields.remove("id");
            fields.remove("unit");
            fields.remove("parent");
            output.project_value(subject.clone(), Value::Object(std::mem::take(fields)))?;
            output.push(
                subject.clone(),
                vocabulary("id")?,
                Literal::new_simple_literal(row.node.id.0.as_str()).into(),
            )?;
            output.push(
                subject.clone(),
                vocabulary("unit")?,
                iri(source.unit.as_str())?,
            )?;
            if let Some(parent) = row.parent {
                output.push(
                    subject,
                    vocabulary("parent")?,
                    child(&root, "nodes", parent)?,
                )?;
            }
        }
    }
    Ok(())
}

fn catalog<'a>(
    facts: &'a QueryFacts,
    unit: &str,
) -> Result<&'a crate::query::QuerySource, QueryError> {
    let mut sources = facts
        .sources
        .iter()
        .filter(|source| source.unit.as_str() == unit);
    let source = sources.next().ok_or_else(missing_source)?;
    if source.digest.is_none()
        || sources.any(|other| other.digest != source.digest || other.text != source.text)
    {
        return Err(missing_source());
    }
    Ok(source)
}

fn missing_source() -> QueryError {
    failure::invalid("captured syntax source bytes or digest do not match")
}

#[cfg(test)]
mod tests;
