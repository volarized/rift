//! Accepts one configuration: its TOML document, then the environment
//! variables that override the document's keys.
//!
//! Every key a configuration model declares by a fixed name has one variable:
//! `RIFT_` followed by the key's path, members joined by `_` and uppercased.
//! `[providers.syntax] max_nodes` is `RIFT_PROVIDERS_SYNTAX_MAX_NODES`. A
//! variable's value replaces the document's, and the merged configuration
//! passes the same shape checks the document alone does; the caller then
//! checks its bounds.

use rift_protocol::configuration::ConfigurationViolation;
use rift_protocol::schema::{
    DeclaredKey, DocumentStep, ExpectedShape, declared_keys, declared_tables, document_steps,
    expected_shape, named_member,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::constants::WORKSPACE_CONFIGURATION_FILE;
use crate::error::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use crate::line::lines_inclusive;

/// Bytes a configuration document may hold, at most. The document states
/// bounded tables and entries; one this large is not configuration.
pub const CONFIGURATION_FILE_BYTES_MAX: u64 = 256 << 10;

/// The prefix of every variable naming a configuration key.
pub const ENVIRONMENT_PREFIX: &str = "RIFT";

/// Bytes one variable naming a configuration key may hold, at most.
const VARIABLE_VALUE_BYTES_MAX: usize = 64 << 10;

/// One configuration failure: why the document or a variable cannot be
/// accepted.
#[derive(Debug)]
pub enum ConfigurationFault {
    /// The document exists but its bytes could not be read.
    Unreadable {
        /// The document's path.
        path: String,
        /// The rendered I/O failure.
        io: String,
    },
    /// A directory stands where the configuration document belongs. Reading
    /// it can never succeed by retrying: the operator must remove or replace
    /// it with the document.
    IsDirectory {
        /// The directory's path.
        path: String,
    },
    /// The document is larger than configuration can be.
    Oversized {
        /// The document's size in bytes.
        bytes: u64,
        /// The accepted maximum in bytes.
        bytes_max: u64,
    },
    /// The document is not the documented TOML shape: a syntax error, an
    /// unknown key, a missing required key, or a malformed value.
    Malformed {
        /// Where the parser stopped, as `line <n> column <n>`. Absent when
        /// the parser named no position in the document.
        location: Option<String>,
        /// The documented key the parser stopped inside, members joined by
        /// `.`. Absent when it stopped before reaching one.
        key: Option<String>,
        /// What the documented shape accepts there, and a value it takes.
        /// Boxed so one refusal's evidence does not widen every configuration
        /// `Result` the crate returns.
        shape: Box<ExpectedShape>,
    },
    /// A variable names a key and holds a value the key's documented shape
    /// refuses.
    VariableMalformed {
        /// The variable's name, such as `RIFT_PROVIDERS_SYNTAX_MAX_NODES`.
        variable: String,
        /// The key the variable names, members joined by `.`.
        key: String,
        /// What the key accepts, and a value it takes.
        shape: Box<ExpectedShape>,
    },
    /// A variable names a configuration table and no key the table declares.
    VariableUnknown {
        /// The variable's name.
        variable: String,
        /// The variables the table's keys accept, sorted.
        accepted: Vec<String>,
    },
    /// A variable names a key and holds bytes that are not UTF-8.
    VariableNotUnicode {
        /// The variable's name.
        variable: String,
    },
    /// The configuration parsed and one of its values breaks a documented
    /// bound.
    Invalid {
        /// The bound the value breaks.
        violation: ConfigurationViolation,
        /// Variables that overrode a document key, sorted; the broken value
        /// may be one of theirs. With none, the broken value is the
        /// document's, and the context names the document.
        variables: Vec<String>,
    },
}

impl Fault for ConfigurationFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Unreadable { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::IsDirectory { .. }
            | Self::Oversized { .. }
            | Self::Malformed { .. }
            | Self::VariableMalformed { .. }
            | Self::VariableUnknown { .. }
            | Self::VariableNotUnicode { .. }
            | Self::Invalid { .. } => ErrorName::Wire(ErrorCode::ConfigurationInvalid),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Unreadable { path, io } => in_file([
                ErrorContext::new("path", path.clone()),
                ErrorContext::new("io", io.clone()),
            ]),
            Self::IsDirectory { path } => in_file([
                ErrorContext::new("path", path.clone()),
                ErrorContext::new("detail", "the path is a directory"),
            ]),
            Self::Oversized { bytes, bytes_max } => in_file([
                ErrorContext::new("bytes", bytes.to_string()),
                ErrorContext::new("bytes_max", bytes_max.to_string()),
            ]),
            Self::Malformed {
                location,
                key,
                shape,
            } => in_file(
                key.iter()
                    .map(|key| ErrorContext::new("key", key.clone()))
                    .chain(
                        location
                            .iter()
                            .map(|location| ErrorContext::new("location", location.clone())),
                    )
                    .chain(shape_context(shape)),
            ),
            Self::VariableMalformed {
                variable,
                key,
                shape,
            } => [
                ErrorContext::new("variable", variable.clone()),
                ErrorContext::new("key", key.clone()),
            ]
            .into_iter()
            .chain(shape_context(shape))
            .collect(),
            Self::VariableUnknown { variable, accepted } => vec![
                ErrorContext::new("variable", variable.clone()),
                ErrorContext::new("accepted", accepted.join(", ")),
            ],
            Self::VariableNotUnicode { variable } => vec![
                ErrorContext::new("variable", variable.clone()),
                ErrorContext::new("detail", "the value is not UTF-8"),
            ],
            Self::Invalid {
                violation,
                variables,
            } if variables.is_empty() => in_file(violation.context()),
            Self::Invalid {
                violation,
                variables,
            } => violation
                .context()
                .into_iter()
                .chain(std::iter::once(ErrorContext::new(
                    "variables",
                    variables.join(", "),
                )))
                .collect(),
        }
    }
}

/// One failure of the document, or of the configuration as a whole: the
/// document's name first, then the failure's own evidence.
fn in_file(evidence: impl IntoIterator<Item = ErrorContext>) -> Vec<ErrorContext> {
    std::iter::once(ErrorContext::new("file", WORKSPACE_CONFIGURATION_FILE))
        .chain(evidence)
        .collect()
}

/// What a refused key accepts, and a value it takes.
fn shape_context(shape: &ExpectedShape) -> Vec<ErrorContext> {
    let mut context = Vec::new();
    if !shape.accepted().is_empty() {
        context.push(ErrorContext::new("accepted", shape.accepted().join(", ")));
    }
    if let Some(example) = shape.example() {
        context.push(ErrorContext::new("example", example.to_string()));
    }
    context
}

/// Opaque configuration failure.
pub type ConfigurationError = Error<ConfigurationFault>;

/// The variables a configuration may be overridden from: every variable whose
/// name starts with `RIFT_`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigurationEnvironment {
    variables: Vec<(String, Option<String>)>,
}

impl ConfigurationEnvironment {
    /// The current process's variables. A value that is not UTF-8 is kept as
    /// such, so a variable naming a key refuses rather than vanishing.
    #[must_use]
    pub fn from_process() -> Self {
        let mut variables: Vec<(String, Option<String>)> = std::env::vars_os()
            .filter_map(|(name, value)| {
                let name = name.into_string().ok()?;
                is_prefixed(&name).then(|| (name, value.into_string().ok()))
            })
            .collect();
        variables.sort();
        Self { variables }
    }

    /// The given variables; those outside the prefix are left out.
    #[must_use]
    pub fn from_variables<Name, Text>(variables: impl IntoIterator<Item = (Name, Text)>) -> Self
    where
        Name: Into<String>,
        Text: Into<String>,
    {
        let mut variables: Vec<(String, Option<String>)> = variables
            .into_iter()
            .map(|(name, value)| (name.into(), Some(value.into())))
            .filter(|(name, _)| is_prefixed(name))
            .collect();
        variables.sort();
        Self { variables }
    }
}

/// Whether a variable name carries the configuration prefix.
fn is_prefixed(name: &str) -> bool {
    name.strip_prefix(ENVIRONMENT_PREFIX)
        .is_some_and(|rest| rest.starts_with('_'))
}

/// One configuration accepted from its document and environment.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedConfiguration<Model> {
    configuration: Model,
    variables: Vec<String>,
}

impl<Model> AcceptedConfiguration<Model> {
    /// The accepted configuration.
    #[must_use]
    pub const fn configuration(&self) -> &Model {
        &self.configuration
    }

    /// Variables that overrode a document key, sorted.
    #[must_use]
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Takes the configuration and the overriding variables apart.
    #[must_use]
    pub fn into_parts(self) -> (Model, Vec<String>) {
        (self.configuration, self.variables)
    }
}

/// The variable naming one declared key: the prefix, then the key's members
/// joined by `_` and uppercased.
#[must_use]
pub fn variable_name(key: &DeclaredKey) -> String {
    format!(
        "{ENVIRONMENT_PREFIX}_{}",
        key.path().join("_").to_ascii_uppercase()
    )
}

/// Accepts one configuration: `document`, when present, then every variable
/// in `environment` naming one of the model's keys.
///
/// A variable's value replaces the document's value for its key; a key no
/// variable names keeps the document's value, or the model's default. A
/// variable holding text the key takes, such as `4mb`, is that text; any
/// other value is read as one TOML value, so `RIFT_SOURCE_EXCLUDE` takes
/// `["docs/**"]`. A variable naming a configuration table and none of its
/// keys refuses, so a misspelled key cannot be silently ignored. The model's
/// value bounds are the caller's to check.
///
/// # Errors
///
/// Returns [`ConfigurationError`] when the document is larger than
/// configuration can be or is not the documented shape, or when a variable
/// naming the model's keys is not UTF-8, is not the key's shape, or names no
/// key its table declares.
pub fn accept_configuration<Model>(
    document: Option<&str>,
    environment: &ConfigurationEnvironment,
) -> Result<AcceptedConfiguration<Model>, ConfigurationError>
where
    Model: DeserializeOwned + JsonSchema + Default,
{
    let schema = schemars::schema_for!(Model).to_value();
    let overrides = matched_overrides(&schema, environment)?;
    let accepted = match document {
        Some(raw) => accept_document::<Model>(raw, &schema)?,
        None => Model::default(),
    };
    let Some(first) = overrides.first() else {
        return Ok(AcceptedConfiguration {
            configuration: accepted,
            variables: Vec::new(),
        });
    };
    let mut table = match document {
        Some(raw) => raw
            .parse::<toml::Table>()
            .map_err(|error| malformed_document(raw, error.span(), &[], &schema))?,
        None => toml::Table::new(),
    };
    for applied in &overrides {
        insert_value(&mut table, applied.key.path(), applied.value.clone());
    }
    let configuration = accept_merged::<Model>(table, first, &overrides, &schema)?;
    Ok(AcceptedConfiguration {
        configuration,
        variables: overrides
            .into_iter()
            .map(|applied| applied.variable)
            .collect(),
    })
}

/// One variable naming a declared key, with its value in document form.
struct Override {
    variable: String,
    key: DeclaredKey,
    value: toml::Value,
}

/// The variables naming the model's keys, in name order.
fn matched_overrides(
    schema: &Value,
    environment: &ConfigurationEnvironment,
) -> Result<Vec<Override>, ConfigurationError> {
    let keys = declared_keys(schema);
    let tables = table_prefixes(schema);
    let mut overrides = Vec::new();
    for (variable, value) in &environment.variables {
        let Some(key) = keys.iter().find(|key| variable_name(key) == *variable) else {
            refuse_unknown(variable, &tables, &keys)?;
            continue;
        };
        let Some(text) = value else {
            return Err(Error::new(ConfigurationFault::VariableNotUnicode {
                variable: variable.clone(),
            }));
        };
        let value = variable_value(variable, key, text, schema)?;
        overrides.push(Override {
            variable: variable.clone(),
            key: key.clone(),
            value,
        });
    }
    Ok(overrides)
}

/// The variable prefix of each of the model's tables, such as
/// `RIFT_PROVIDERS_`.
fn table_prefixes(schema: &Value) -> Vec<String> {
    declared_tables(schema)
        .into_iter()
        .map(|table| format!("{ENVIRONMENT_PREFIX}_{}_", table.to_ascii_uppercase()))
        .collect()
}

/// Refuses a variable that names one of the model's tables and none of its
/// keys; a variable outside every table belongs to something else.
fn refuse_unknown(
    variable: &str,
    tables: &[String],
    keys: &[DeclaredKey],
) -> Result<(), ConfigurationError> {
    let Some(table) = tables
        .iter()
        .find(|table| variable.starts_with(table.as_str()))
    else {
        return Ok(());
    };
    let mut accepted: Vec<String> = keys
        .iter()
        .map(variable_name)
        .filter(|name| name.starts_with(table.as_str()))
        .collect();
    accepted.sort();
    Err(Error::new(ConfigurationFault::VariableUnknown {
        variable: variable.to_owned(),
        accepted,
    }))
}

/// One variable's value in document form: the text itself for a textual key,
/// or the one TOML value the text spells.
fn variable_value(
    variable: &str,
    key: &DeclaredKey,
    text: &str,
    schema: &Value,
) -> Result<toml::Value, ConfigurationError> {
    if text.len() > VARIABLE_VALUE_BYTES_MAX {
        return Err(variable_malformed(variable, key, schema));
    }
    if key.is_textual() {
        return Ok(toml::Value::String(text.to_owned()));
    }
    parsed_value(text).ok_or_else(|| variable_malformed(variable, key, schema))
}

/// The one TOML value `text` spells, such as `250000`, `true`, or `["a"]`;
/// `None` when it spells none, or more than one.
fn parsed_value(text: &str) -> Option<toml::Value> {
    let mut table = format!("value = {text}").parse::<toml::Table>().ok()?;
    if table.len() != 1 {
        return None;
    }
    table.remove("value")
}

/// Places one value at a key's path, creating the tables above it.
fn insert_value(table: &mut toml::Table, path: &[String], value: toml::Value) {
    let Some((leaf, tables)) = path.split_last() else {
        return;
    };
    let mut current = table;
    for member in tables {
        // The document already passed its shape check, so a member above a
        // declared key holds a table whenever it is present.
        let Some(next) = current
            .entry(member.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
        else {
            return;
        };
        current = next;
    }
    current.insert(leaf.clone(), value);
}

/// Reads the documented shape out of one document, refusing an oversized one.
///
/// A refusal names the key the parser stopped inside, where in the document
/// it stopped, what the documented shape accepts there, and a value it takes.
/// The parser's own account is not carried: it speaks of Rust types and serde
/// grammar, which name nothing the operator can write.
fn accept_document<Model: DeserializeOwned>(
    raw: &str,
    schema: &Value,
) -> Result<Model, ConfigurationError> {
    let bytes = raw.len() as u64;
    if bytes > CONFIGURATION_FILE_BYTES_MAX {
        return Err(Error::new(ConfigurationFault::Oversized {
            bytes,
            bytes_max: CONFIGURATION_FILE_BYTES_MAX,
        }));
    }
    let deserializer = match toml::Deserializer::parse(raw) {
        Ok(deserializer) => deserializer,
        Err(error) => return Err(malformed_document(raw, error.span(), &[], schema)),
    };
    serde_path_to_error::deserialize(deserializer).map_err(|refused| {
        let steps = document_steps(refused.path());
        malformed_document(raw, refused.inner().span(), &steps, schema)
    })
}

/// Reads the merged document. The document alone already passed, and each
/// variable replaced one value, so a refusal names the variable whose key
/// shares the most members with where it stopped, or `first` when it shares
/// none: a key's table can refuse a variable that leaves a sibling unset.
fn accept_merged<Model: DeserializeOwned>(
    table: toml::Table,
    first: &Override,
    overrides: &[Override],
    schema: &Value,
) -> Result<Model, ConfigurationError> {
    serde_path_to_error::deserialize(toml::Value::Table(table)).map_err(|refused| {
        let stopped = document_steps(refused.path());
        let culprit = overrides
            .iter()
            .map(|applied| (shared_members(applied.key.path(), &stopped), applied))
            .filter(|(shared, _)| *shared > 0)
            .max_by_key(|(shared, _)| *shared)
            .map_or(first, |(_, applied)| applied);
        variable_malformed(&culprit.variable, &culprit.key, schema)
    })
}

/// Members a key's path and a refusal's path share from the root.
fn shared_members(key: &[String], stopped: &[DocumentStep<'_>]) -> usize {
    key.iter()
        .zip(stopped)
        .take_while(|(member, step)| matches!(step, DocumentStep::Member(name) if name == member))
        .count()
}

/// One refusal of the document's shape, described against the schema.
fn malformed_document(
    raw: &str,
    span: Option<std::ops::Range<usize>>,
    steps: &[DocumentStep<'_>],
    schema: &Value,
) -> ConfigurationError {
    let shape = expected_shape(schema, steps);
    Error::new(ConfigurationFault::Malformed {
        location: span.map(|span| position_of(raw, span.start)),
        key: named_member(&steps[..shape.followed()]),
        shape: Box::new(shape),
    })
}

/// One refusal of a variable's value, described against its key's schema.
fn variable_malformed(variable: &str, key: &DeclaredKey, schema: &Value) -> ConfigurationError {
    let steps: Vec<DocumentStep<'_>> = key
        .path()
        .iter()
        .map(|member| DocumentStep::Member(member))
        .collect();
    Error::new(ConfigurationFault::VariableMalformed {
        variable: variable.to_owned(),
        key: key.path().join("."),
        shape: Box::new(expected_shape(schema, &steps)),
    })
}

/// Where one byte offset stands in the document, counting lines and columns
/// from one, the way an editor does.
fn position_of(raw: &str, offset: usize) -> String {
    let mut line = 1;
    let mut consumed = 0;
    for text in lines_inclusive(raw) {
        if consumed + text.len() > offset {
            break;
        }
        consumed += text.len();
        line += 1;
    }
    let column = raw[consumed..offset.min(raw.len())].chars().count() + 1;
    format!("line {line} column {column}")
}

#[cfg(test)]
mod tests {
    use rift_protocol::configuration::{ByteSize, WorkspaceConfiguration};
    use rift_protocol::schema::{configuration_schema, declared_keys};

    use super::{
        ConfigurationEnvironment, ConfigurationError, ConfigurationFault, accept_configuration,
        variable_name,
    };
    use crate::error::{ErrorContext, Fault};

    fn accept(
        document: Option<&str>,
        variables: &[(&str, &str)],
    ) -> Result<(WorkspaceConfiguration, Vec<String>), ConfigurationError> {
        let environment = ConfigurationEnvironment::from_variables(variables.iter().copied());
        accept_configuration::<WorkspaceConfiguration>(document, &environment)
            .map(super::AcceptedConfiguration::into_parts)
    }

    fn context_value(error: &ConfigurationError, name: &str) -> Option<String> {
        error
            .fault()
            .context()
            .into_iter()
            .find(|entry| entry.key() == name)
            .map(|entry| entry.value().to_owned())
    }

    #[test]
    fn test_a_variable_prevails_over_the_document_and_the_default() {
        let document = "[providers.syntax]\nmax_nodes = 1000\nmax_depth = 64\n";
        let (configuration, variables) = accept(
            Some(document),
            &[
                ("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "5000000"),
                ("RIFT_PROVIDERS_SYNTAX_MAX_FILE", "16mb"),
            ],
        )
        .expect("both variables name keys");

        let syntax = configuration.providers.syntax;
        assert_eq!(syntax.max_nodes, 5_000_000);
        assert_eq!(syntax.max_file, ByteSize::from_bytes(16 << 20));
        assert_eq!(
            syntax.max_depth, 64,
            "a key no variable names keeps the document's value"
        );
        assert_eq!(
            variables,
            [
                "RIFT_PROVIDERS_SYNTAX_MAX_FILE",
                "RIFT_PROVIDERS_SYNTAX_MAX_NODES"
            ]
        );
    }

    #[test]
    fn test_variables_apply_without_a_document_and_read_toml_values() {
        let (configuration, _) = accept(
            None,
            &[
                ("RIFT_SOURCE_EXCLUDE", r#"["docs/**", "vendor/**"]"#),
                ("RIFT_SEARCH_VECTOR_DISABLED", "true"),
                ("RIFT_SERVER_PORT_RANGE_MIN", "41000"),
                ("RIFT_SERVER_PORT_RANGE_MAX", "41999"),
            ],
        )
        .expect("each value is its key's shape");

        let excluded: Vec<&str> = configuration
            .source
            .exclude
            .iter()
            .map(|pattern| pattern.0.as_str())
            .collect();
        assert_eq!(excluded, ["docs/**", "vendor/**"]);
        assert!(configuration.search.vector.disabled);
        assert_eq!(
            configuration.server.port_range.map(|range| range.min),
            Some(41_000)
        );
    }

    #[test]
    fn test_a_value_outside_the_key_shape_refuses_naming_the_variable() {
        for (variable, value) in [
            ("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "many"),
            ("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "1\nlogs = 2"),
            ("RIFT_PROVIDERS_SYNTAX_MAX_FILE", "16MB"),
        ] {
            let error = accept(None, &[(variable, value)]).expect_err(value);
            assert!(
                matches!(error.fault(), ConfigurationFault::VariableMalformed { .. }),
                "{error:?}"
            );
            assert_eq!(context_value(&error, "variable").as_deref(), Some(variable));
            assert!(context_value(&error, "example").is_some(), "{error}");
        }
    }

    #[test]
    fn test_a_refusal_at_a_key_table_names_the_variable_that_set_it() {
        let error = accept(
            None,
            &[
                ("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "5000000"),
                ("RIFT_SERVER_PORT_RANGE_MIN", "41000"),
            ],
        )
        .expect_err("a range needs both ends");
        assert_eq!(
            context_value(&error, "variable").as_deref(),
            Some("RIFT_SERVER_PORT_RANGE_MIN")
        );
    }

    #[test]
    fn test_a_variable_naming_a_table_and_no_key_refuses() {
        for variable in [
            "RIFT_PROVIDERS_SYNTAX_MAX_NODE",
            "RIFT_LANGUAGES_PYTHON_ENABLED",
        ] {
            let error = accept(None, &[(variable, "1")]).expect_err(variable);
            assert!(
                matches!(error.fault(), ConfigurationFault::VariableUnknown { .. }),
                "{error:?}"
            );
        }
        let error =
            accept(None, &[("RIFT_PROVIDERS_SYNTAX_MAX_NODE", "1")]).expect_err("misspelled");
        assert!(
            context_value(&error, "accepted")
                .is_some_and(|accepted| accepted.contains("RIFT_PROVIDERS_SYNTAX_MAX_NODES"))
        );
    }

    #[test]
    fn test_variables_outside_every_table_are_left_alone() {
        let (configuration, variables) = accept(
            None,
            &[
                ("RIFT_API_TOKEN", "secret"),
                ("RIFT_LIVE_SEARCH", "1"),
                ("RIFTX_PROVIDERS_SYNTAX_MAX_NODES", "1"),
                ("PATH", "/usr/bin"),
            ],
        )
        .expect("no variable names a table");
        assert_eq!(configuration, WorkspaceConfiguration::default());
        assert!(variables.is_empty());
    }

    #[test]
    fn test_a_variable_that_is_not_unicode_refuses() {
        let environment = ConfigurationEnvironment {
            variables: vec![("RIFT_PROVIDERS_SYNTAX_MAX_NODES".to_owned(), None)],
        };
        let error = accept_configuration::<WorkspaceConfiguration>(None, &environment)
            .expect_err("a key's variable must be UTF-8");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::VariableNotUnicode { .. }
        ));
    }

    #[test]
    fn test_a_document_refusal_keeps_its_location_beside_variables() {
        let error = accept(
            Some("[execution]\nmax_codes = \"16kb\"\n"),
            &[("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "5000000")],
        )
        .expect_err("the document names an unknown key");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Malformed { .. }
        ));
        assert_eq!(
            context_value(&error, "location").as_deref(),
            Some("line 2 column 1")
        );
    }

    #[test]
    fn test_an_oversized_document_refuses_before_parsing() {
        let oversized =
            "#".repeat(usize::try_from(super::CONFIGURATION_FILE_BYTES_MAX + 1).expect("fits"));
        let error = accept(Some(&oversized), &[]).expect_err("oversized");
        assert!(matches!(
            error.fault(),
            ConfigurationFault::Oversized { .. }
        ));
    }

    #[test]
    fn test_every_key_has_one_distinct_variable() {
        let keys = declared_keys(&configuration_schema());
        let mut names: Vec<String> = keys.iter().map(variable_name).collect();
        let declared = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            declared,
            "two keys must never share a variable"
        );
        assert!(names.contains(&"RIFT_PROVIDERS_SYNTAX_MAX_NODES".to_owned()));
    }

    #[test]
    fn test_invalid_names_the_variables_that_overrode_keys_or_else_the_file() {
        let invalid = |variables: &[&str]| ConfigurationFault::Invalid {
            violation: rift_protocol::configuration::ConfigurationViolation::LimitOutOfRange {
                field: "providers.syntax.max_nodes",
                value: 0,
                min: 1,
                max: 100_000_000,
            },
            variables: variables.iter().map(|name| (*name).to_owned()).collect(),
        };
        let file = ErrorContext::new("file", crate::constants::WORKSPACE_CONFIGURATION_FILE);

        let overridden = invalid(&["RIFT_PROVIDERS_SYNTAX_MAX_NODES"]).context();
        assert!(overridden.contains(&ErrorContext::new(
            "variables",
            "RIFT_PROVIDERS_SYNTAX_MAX_NODES"
        )));
        assert!(
            !overridden.contains(&file),
            "a value a variable may have set is not the document's: {overridden:?}"
        );
        assert!(invalid(&[]).context().contains(&file));
    }

    /// A position is counted the way an editor counts, from one, and a byte
    /// offset inside a line lands on that line.
    #[test]
    fn test_a_position_counts_lines_and_columns_from_one() {
        let raw = "alpha\nbeta\ngamma\n";
        assert_eq!(super::position_of(raw, 0), "line 1 column 1");
        assert_eq!(super::position_of(raw, 6), "line 2 column 1");
        assert_eq!(super::position_of(raw, 9), "line 2 column 4");
        assert_eq!(super::position_of(raw, raw.len()), "line 4 column 1");
    }
}
