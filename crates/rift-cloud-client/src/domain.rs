//! Domain values produced from validated global package responses.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde_json::Value;

use crate::{
    ClientError, HitLocation, PackageIdentity, PackageSearchHit, PackageSearchHitContributingField,
    PackageSymbol, SourceKind, SourceLocationKind, Symbol, SymbolFacet, SymbolOrigin, TextRange,
    TypeBinding, TypeBindingOrigin, TypeBindingRole, TypeExpression,
};

/// One package declaration returned by global search in the local read model.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageSearchCandidate {
    /// Package that owns declaration.
    pub package: rift_protocol::read::PackageIdentity,
    /// Ranking identity for declaration.
    pub identity: rift_ranking::DocumentIdentity,
    /// Search hit carrying declaration.
    pub hit: rift_protocol::read::SearchHit,
    /// Identifier match class established by global index.
    pub match_class: rift_ranking::IdentifierMatchClass,
}

/// One package declaration returned by global symbol lookup in the local read model.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageSymbolCandidate {
    /// Package that owns declaration.
    pub package: rift_protocol::read::PackageIdentity,
    /// Stable identity of declaration symbol.
    pub symbol_identity: rift_protocol::read::SymbolId,
    /// Symbol hit carrying declaration.
    pub hit: rift_protocol::read::GetSymbolHit,
    /// Identifier match class established by global index.
    pub match_class: rift_ranking::IdentifierMatchClass,
}

impl TryFrom<PackageSearchHit> for PackageSearchCandidate {
    type Error = ClientError;

    fn try_from(value: PackageSearchHit) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<&PackageSearchHit> for PackageSearchCandidate {
    type Error = ClientError;

    fn try_from(value: &PackageSearchHit) -> Result<Self, Self::Error> {
        let (qualified_name, unit, line) = validate_location(
            &value.package,
            &value.symbol,
            &value.unit,
            &value.range,
            value.line,
            value.source.as_deref(),
        )?;
        let package = package_identity(&value.package);
        let symbol = convert_symbol(&value.symbol)?;
        let identity = rift_ranking::DocumentIdentity::for_unit(&unit, &qualified_name)
            .map_err(|_| invalid("symbol_identity"))?;
        let match_class = crate::ranking_match_class(&value.match_class)?;
        let hit = rift_protocol::read::SearchHit {
            hit: rift_protocol::read::SearchHitTarget::Symbol {
                symbol: Box::new(symbol),
            },
            score: None,
            matched_by: matched_fields(&value.contributing_fields),
            source: value.source.clone(),
            range: Some(text_range(&value.range)),
            line: Some(line),
            path: None,
            unit: Some(protocol_unit(&value.unit)),
            traversal_path: None,
            distance: None,
            change: None,
        };
        Ok(Self {
            package,
            identity,
            hit,
            match_class,
        })
    }
}

impl TryFrom<PackageSymbol> for PackageSymbolCandidate {
    type Error = ClientError;

    fn try_from(value: PackageSymbol) -> Result<Self, Self::Error> {
        Self::try_from(&value)
    }
}

impl TryFrom<&PackageSymbol> for PackageSymbolCandidate {
    type Error = ClientError;

    fn try_from(value: &PackageSymbol) -> Result<Self, Self::Error> {
        let (_, _, line) = validate_location(
            &value.package,
            &value.symbol,
            &value.unit,
            &value.range,
            value.line,
            value.source.as_deref(),
        )?;
        let package = package_identity(&value.package);
        let symbol = convert_symbol(&value.symbol)?;
        let symbol_identity = symbol
            .id
            .clone()
            .ok_or_else(|| invalid("symbol_identity"))?;
        let match_class = crate::ranking_match_class(&value.match_class)?;
        let hit = rift_protocol::read::GetSymbolHit {
            symbol,
            path: None,
            unit: Some(protocol_unit(&value.unit)),
            range: text_range(&value.range),
            line,
            node: None,
            source: value.source.clone(),
            history: None,
        };
        Ok(Self {
            package,
            symbol_identity,
            hit,
            match_class,
        })
    }
}

fn validate_location(
    package: &PackageIdentity,
    symbol: &Symbol,
    unit: &str,
    range: &TextRange,
    line: i64,
    source: Option<&str>,
) -> Result<(String, rift_core::SourceUnitId, u64), ClientError> {
    let packages = HashSet::from([crate::package_key(package)]);
    let qualified_name = crate::validate_hit_common(
        package,
        symbol,
        HitLocation {
            unit,
            range,
            line,
            source,
        },
        &packages,
        crate::SOURCE_BYTES_MAX,
    )?;
    let source_unit =
        rift_core::SourceUnitId::parse(unit).map_err(|_| invalid("source_identity"))?;
    let line = u64::try_from(line).map_err(|_| invalid("location"))?;
    Ok((qualified_name, source_unit, line))
}

fn package_identity(value: &PackageIdentity) -> rift_protocol::read::PackageIdentity {
    rift_protocol::read::PackageIdentity {
        manager: value.manager.clone(),
        name: value.name.clone(),
        version: value.version.clone(),
    }
}

fn protocol_unit(value: &str) -> rift_protocol::read::SourceUnitId {
    rift_protocol::read::SourceUnitId(value.to_owned())
}

fn text_range(value: &TextRange) -> rift_protocol::read::TextRange {
    rift_protocol::read::TextRange {
        start: value.start,
        end: value.end,
    }
}

fn matched_fields(
    values: &[PackageSearchHitContributingField],
) -> Vec<rift_protocol::read::MatchedField> {
    let mut fields = Vec::new();
    for value in values {
        let field = match value {
            PackageSearchHitContributingField::Name
            | PackageSearchHitContributingField::QualifiedName => {
                rift_protocol::read::MatchedField::Name
            }
            PackageSearchHitContributingField::Documentation => {
                rift_protocol::read::MatchedField::Documentation
            }
            PackageSearchHitContributingField::Signature => {
                rift_protocol::read::MatchedField::Signature
            }
            PackageSearchHitContributingField::DeclarationSource => {
                rift_protocol::read::MatchedField::Content
            }
            PackageSearchHitContributingField::Unknown => rift_protocol::read::MatchedField::Ranked,
        };
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    if fields.is_empty() {
        fields.push(rift_protocol::read::MatchedField::Ranked);
    }
    fields
}

fn convert_symbol(value: &Symbol) -> Result<rift_protocol::read::Symbol, ClientError> {
    Ok(rift_protocol::read::Symbol {
        id: value.id.clone().map(rift_protocol::read::SymbolId),
        language: language(&value.language)?,
        name: value.name.clone(),
        kind: rift_protocol::read::ExactKind(value.kind.clone()),
        facets: value
            .facets
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_facet)
            .collect(),
        origin: convert_origin(value.origin.as_ref()),
        container: value.container.clone().map(rift_protocol::read::SymbolId),
        modifiers: value.modifiers.clone().unwrap_or_default(),
        visibility: value.visibility.clone(),
        types: value
            .types
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        signatures: value
            .signatures
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_signature)
            .collect::<Result<_, _>>()?,
        documentation: value
            .documentation
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_documentation)
            .collect(),
        extensions: convert_extensions(value.extensions.as_ref())?,
        document_local: value.document_local.unwrap_or(false),
    })
}

fn convert_origin(value: Option<&SymbolOrigin>) -> rift_protocol::read::SymbolOrigin {
    let Some(value) = value else {
        return rift_protocol::read::SymbolOrigin {
            location: Some(rift_protocol::read::SourceLocationKind::Project),
            package: None,
            source_kind: rift_protocol::read::SourceKind::Authored,
        };
    };
    rift_protocol::read::SymbolOrigin {
        location: value.location.as_ref().map(convert_location),
        package: value.package.as_ref().map(package_identity),
        source_kind: convert_source_kind(&value.source_kind),
    }
}

fn convert_source_kind(value: &SourceKind) -> rift_protocol::read::SourceKind {
    match value {
        SourceKind::Authored => rift_protocol::read::SourceKind::Authored,
        SourceKind::Generated => rift_protocol::read::SourceKind::Generated,
        SourceKind::Synthetic => rift_protocol::read::SourceKind::Synthetic,
    }
}

fn convert_location(value: &SourceLocationKind) -> rift_protocol::read::SourceLocationKind {
    match value {
        SourceLocationKind::Project => rift_protocol::read::SourceLocationKind::Project,
        SourceLocationKind::Dependency => rift_protocol::read::SourceLocationKind::Dependency,
        SourceLocationKind::Stdlib => rift_protocol::read::SourceLocationKind::Stdlib,
        SourceLocationKind::External => rift_protocol::read::SourceLocationKind::External,
    }
}

fn convert_facet(value: &SymbolFacet) -> rift_protocol::read::SymbolFacet {
    use rift_protocol::read::SymbolFacet as Protocol;
    match value {
        SymbolFacet::Namespace => Protocol::Namespace,
        SymbolFacet::Module => Protocol::Module,
        SymbolFacet::TypeType => Protocol::Type,
        SymbolFacet::Value => Protocol::Value,
        SymbolFacet::Callable => Protocol::Callable,
        SymbolFacet::Member => Protocol::Member,
        SymbolFacet::MemberContainer => Protocol::MemberContainer,
        SymbolFacet::Parameter => Protocol::Parameter,
        SymbolFacet::TypeParameter => Protocol::TypeParameter,
        SymbolFacet::Constructible => Protocol::Constructible,
        SymbolFacet::Extensible => Protocol::Extensible,
        SymbolFacet::Implementable => Protocol::Implementable,
        SymbolFacet::Macro => Protocol::Macro,
        SymbolFacet::Test => Protocol::Test,
        SymbolFacet::Annotation => Protocol::Annotation,
        SymbolFacet::Extension => Protocol::Extension,
        SymbolFacet::Variant => Protocol::Variant,
        SymbolFacet::Enumeration => Protocol::Enumeration,
        SymbolFacet::Alias => Protocol::Alias,
        SymbolFacet::Property => Protocol::Property,
        SymbolFacet::Abstract => Protocol::Abstract,
        SymbolFacet::Constructor => Protocol::Constructor,
        SymbolFacet::Static => Protocol::Static,
        SymbolFacet::Mutable => Protocol::Mutable,
        SymbolFacet::Deprecated => Protocol::Deprecated,
        SymbolFacet::Public => Protocol::Public,
        SymbolFacet::Entrypoint => Protocol::Entrypoint,
        SymbolFacet::Operator => Protocol::Operator,
        SymbolFacet::Async => Protocol::Async,
        SymbolFacet::Generator => Protocol::Generator,
    }
}

fn convert_documentation(
    value: &crate::generated::Documentation,
) -> rift_protocol::read::Documentation {
    rift_protocol::read::Documentation {
        format: match value.format {
            crate::generated::DocumentationFormat::Plain => {
                rift_protocol::read::DocumentationFormat::Plain
            }
            crate::generated::DocumentationFormat::Markdown => {
                rift_protocol::read::DocumentationFormat::Markdown
            }
        },
        text: value.text.clone(),
    }
}

fn convert_signature(
    value: &crate::generated::Signature,
) -> Result<rift_protocol::read::Signature, ClientError> {
    Ok(rift_protocol::read::Signature {
        display: value.display.clone(),
        links: value
            .links
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_signature_link)
            .collect(),
        language: language(&value.language)?,
        receiver: value.receiver.as_ref().map(convert_parameter).transpose()?,
        parameters: value
            .parameters
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_parameter)
            .collect::<Result<_, _>>()?,
        returns: value
            .returns
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        type_parameters: value
            .type_parameters
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .map(rift_protocol::read::SymbolId)
            .collect(),
        throws: value
            .throws
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_expression)
            .collect::<Result<_, _>>()?,
        effects: value.effects.clone().unwrap_or_default(),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn convert_signature_link(
    value: &crate::generated::SignatureLink,
) -> rift_protocol::read::SignatureLink {
    rift_protocol::read::SignatureLink {
        range: text_range(&value.range),
        symbol: rift_protocol::read::SymbolId(value.symbol.clone()),
    }
}

fn convert_parameter(
    value: &crate::generated::Parameter,
) -> Result<rift_protocol::read::Parameter, ClientError> {
    Ok(rift_protocol::read::Parameter {
        name: value.name.clone(),
        node: value.node.clone().map(rift_protocol::read::NodeId),
        types: value
            .types
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(convert_type_binding)
            .collect::<Result<_, _>>()?,
        optional: value.optional,
        variadic: value.variadic,
        default: value.default.clone(),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn convert_type_binding(
    value: &TypeBinding,
) -> Result<rift_protocol::read::TypeBinding, ClientError> {
    Ok(rift_protocol::read::TypeBinding {
        role: match value.role {
            TypeBindingRole::Receiver => rift_protocol::read::TypeBindingRole::Receiver,
            TypeBindingRole::Parameter => rift_protocol::read::TypeBindingRole::Parameter,
            TypeBindingRole::Return => rift_protocol::read::TypeBindingRole::Return,
            TypeBindingRole::Field => rift_protocol::read::TypeBindingRole::Field,
            TypeBindingRole::Bound => rift_protocol::read::TypeBindingRole::Bound,
            TypeBindingRole::Element => rift_protocol::read::TypeBindingRole::Element,
            TypeBindingRole::Key => rift_protocol::read::TypeBindingRole::Key,
            TypeBindingRole::Error => rift_protocol::read::TypeBindingRole::Error,
            TypeBindingRole::Underlying => rift_protocol::read::TypeBindingRole::Underlying,
            TypeBindingRole::Yielded => rift_protocol::read::TypeBindingRole::Yielded,
            TypeBindingRole::Awaited => rift_protocol::read::TypeBindingRole::Awaited,
            TypeBindingRole::Discriminant => rift_protocol::read::TypeBindingRole::Discriminant,
        },
        origin: match value.origin {
            TypeBindingOrigin::Declared => rift_protocol::read::TypeBindingOrigin::Declared,
            TypeBindingOrigin::Inferred => rift_protocol::read::TypeBindingOrigin::Inferred,
            TypeBindingOrigin::Expected => rift_protocol::read::TypeBindingOrigin::Expected,
        },
        r#type: convert_type_expression(&value.r#type)?,
    })
}

fn convert_type_expression(
    value: &TypeExpression,
) -> Result<rift_protocol::read::TypeExpression, ClientError> {
    Ok(rift_protocol::read::TypeExpression {
        language: language(&value.language)?,
        source: value.source.clone(),
        resolved: value.resolved.clone().map(rift_protocol::read::SymbolId),
        extensions: convert_extensions(value.extensions.as_ref())?,
    })
}

fn language(value: &str) -> Result<rift_protocol::read::Language, ClientError> {
    rift_protocol::read::Language::from_identity_segment(value).map_err(|_| invalid("language"))
}

fn convert_extensions(
    value: Option<&Value>,
) -> Result<rift_protocol::read::Extensions, ClientError> {
    static EXTENSION_KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^[a-z0-9]+(?:[.-][a-z0-9]+)+\.[A-Za-z][A-Za-z0-9_-]*$")
            .expect("extension key pattern")
    });
    let Some(value) = value else {
        return Ok(rift_protocol::read::Extensions::default());
    };
    let Value::Object(entries) = value else {
        return Err(invalid("extensions"));
    };
    let mut extensions = BTreeMap::new();
    for (key, value) in entries {
        if !EXTENSION_KEY.is_match(key) {
            return Err(invalid("extensions"));
        }
        let Value::Object(fields) = value else {
            return Err(invalid("extensions"));
        };
        let version = fields
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version > 0)
            .ok_or_else(|| invalid("extensions"))?;
        let data = fields
            .get("data")
            .cloned()
            .ok_or_else(|| invalid("extensions"))?;
        if fields
            .keys()
            .any(|field| field != "version" && field != "data")
        {
            return Err(invalid("extensions"));
        }
        extensions.insert(
            rift_protocol::read::ExtensionKey(key.clone()),
            rift_protocol::read::ExtensionValue { version, data },
        );
    }
    Ok(rift_protocol::read::Extensions(extensions))
}

fn invalid(field: &'static str) -> ClientError {
    ClientError::InvalidResponseField { field }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn package() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: "helper".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn symbol(package: &PackageIdentity) -> Symbol {
        let id = rift_core::symbol_identity("rust", "src/lib.rs", "helper_beacon");
        Symbol {
            id: Some(id),
            language: "rust".to_owned(),
            name: "helper_beacon".to_owned(),
            kind: "function".to_owned(),
            origin: Some(SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                source_kind: SourceKind::Authored,
            }),
            documentation: Some(vec![crate::generated::Documentation {
                format: crate::generated::DocumentationFormat::Markdown,
                text: "Beacon docs".to_owned(),
            }]),
            facets: Some(vec![SymbolFacet::Callable]),
            ..Default::default()
        }
    }

    fn unit() -> String {
        "rift://source/cargo/helper@1.0.0/src/lib.rs".to_owned()
    }

    #[test]
    fn search_candidate_maps_protocol_fields_and_identity() {
        let package = package();
        let hit = PackageSearchHit {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            contributing_fields: vec![
                PackageSearchHitContributingField::QualifiedName,
                PackageSearchHitContributingField::DeclarationSource,
            ],
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: Some("pub fn helper_beacon() {}".to_owned()),
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSearchCandidate::try_from(hit).expect("valid package hit");
        assert_eq!(candidate.package.name, "helper");
        assert_eq!(
            candidate.match_class,
            rift_ranking::IdentifierMatchClass::NameExact
        );
        assert_eq!(
            candidate.identity.as_unit(),
            Some((unit().as_str(), "helper_beacon"))
        );
        assert_eq!(candidate.hit.line, Some(3));
        assert_eq!(candidate.hit.matched_by.len(), 2);
        assert!(matches!(
            candidate.hit.hit,
            rift_protocol::read::SearchHitTarget::Symbol { .. }
        ));
    }

    #[test]
    fn symbol_candidate_maps_nested_symbol_without_json_roundtrip() {
        let package = package();
        let mut symbol = symbol(&package);
        symbol.extensions = Some(serde_json::json!({
            "org.example.fact": {"version": 1, "data": {"stable": true}}
        }));
        symbol.signatures = Some(vec![crate::generated::Signature {
            display: "helper_beacon(value: Value)".to_owned(),
            language: "rust".to_owned(),
            parameters: Some(vec![crate::generated::Parameter {
                name: Some("value".to_owned()),
                optional: false,
                variadic: false,
                ..Default::default()
            }]),
            ..Default::default()
        }]);
        let hit = PackageSymbol {
            package,
            symbol,
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::QualifiedExact,
            source: None,
            additional_properties: HashMap::new(),
        };
        let candidate = PackageSymbolCandidate::try_from(hit).expect("valid package symbol");
        assert_eq!(
            candidate.match_class,
            rift_ranking::IdentifierMatchClass::QualifiedExact
        );
        assert_eq!(
            candidate.symbol_identity.0,
            candidate.hit.symbol.id.as_ref().expect("id").0
        );
        assert_eq!(candidate.hit.symbol.documentation[0].text, "Beacon docs");
        assert_eq!(
            candidate.hit.symbol.signatures[0].parameters[0]
                .name
                .as_deref(),
            Some("value")
        );
        assert_eq!(candidate.hit.symbol.extensions.0.len(), 1);
    }

    #[test]
    fn candidate_rejects_invalid_source_identity() {
        let package = package();
        let mut hit = PackageSymbol {
            package: package.clone(),
            symbol: symbol(&package),
            unit: unit(),
            range: TextRange { start: 8, end: 24 },
            line: 3,
            match_class: crate::generated::IdentifierMatchClass::NameExact,
            source: None,
            additional_properties: HashMap::new(),
        };
        hit.unit = "rift://source/cargo/other@1.0.0/src/lib.rs".to_owned();
        assert_eq!(
            PackageSymbolCandidate::try_from(hit),
            Err(ClientError::InvalidResponseField {
                field: "source_identity"
            })
        );
    }
}
