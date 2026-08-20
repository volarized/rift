use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Write as _};

use serde_json::{Map, Value};

const READ_ROOTS: &[&str] = &[
    "GetSymbolParams",
    "GetSymbolResult",
    "NodesParams",
    "NodesResult",
    "SearchParams",
    "SearchResult",
];
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while",
];
const NON_RAW_KEYWORDS: &[&str] = &["crate", "self", "Self", "super"];

#[derive(Debug)]
pub(crate) struct GeneratedSource {
    pub contracts: String,
    pub read: String,
}

#[derive(Debug)]
struct CodegenError(String);

impl fmt::Display for CodegenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CodegenError {}

type CodegenResult<T> = Result<T, CodegenError>;

struct Field<'a> {
    name: &'a str,
    type_name: String,
    required: bool,
    default: Option<&'a Value>,
    schema: &'a Value,
}

struct Generator<'a> {
    document: &'a Value,
    definitions: &'a Map<String, Value>,
}

pub(crate) fn generate(source: &str) -> Result<GeneratedSource, Box<dyn Error>> {
    let document: Value = serde_json::from_str(source)?;
    let definitions = document
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| CodegenError("canonical schema has no object $defs".to_owned()))?;
    let generator = Generator {
        document: &document,
        definitions,
    };
    Ok(GeneratedSource {
        contracts: generator.contracts()?,
        read: generator.read()?,
    })
}

impl Generator<'_> {
    fn read(&self) -> CodegenResult<String> {
        let selected = self.definition_closure(READ_ROOTS)?;
        let mut union_members = BTreeSet::new();
        for name in &selected {
            let body = self.definition(name)?;
            union_members.extend(Self::union_members(body)?);
        }

        let mut output = String::from(
            "// @generated from protocol/mcp.json by rift-protocol; do not edit.\n\
             // Generated read-only tool DTOs.\n\n\
             use schemars::JsonSchema;\n\
             use serde::{Deserialize, Deserializer, Serialize};\n\
             use std::collections::BTreeMap;\n\n\
             fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>\n\
             where\n\
                 D: Deserializer<'de>,\n\
                 T: Deserialize<'de>,\n\
             {\n\
                 Option::<T>::deserialize(deserializer)\n\
             }\n\n",
        );
        for name in selected.difference(&union_members) {
            output.push_str(&self.definition_source(name, self.definition(name)?)?);
        }
        Ok(output)
    }

    fn contracts(&self) -> CodegenResult<String> {
        let tools = self
            .document
            .pointer("/rift:entryPoints/mcp.tools")
            .and_then(Value::as_object)
            .ok_or_else(|| CodegenError("canonical schema has no MCP tool catalog".to_owned()))?;
        let mut output = String::from(
            "// @generated from protocol/mcp.json by rift-protocol; do not edit.\n\
             // Generated MCP tool contract metadata.\n\n\
             /// Request and result models for one MCP tool.\n\
             #[derive(Clone, Copy, Debug, Eq, PartialEq)]\n\
             pub struct ToolContract {\n\
                 /// MCP tool name.\n\
                 pub name: &'static str,\n\
                 /// Protocol request model name.\n\
                 pub request_model: &'static str,\n\
             /// Protocol result model name.\n\
             pub result_model: &'static str,\n\
             /// Canonical JSON fixture accepted by protocol model.\n\
             pub minimal_request_json: &'static str,\n\
             }\n\n\
             /// Read-only MCP tools implemented by this release.\n\
             pub const TOOL_CONTRACTS: &[ToolContract] = &[\n",
        );
        for name in ["get_symbol", "search"] {
            let tool = tools
                .get(name)
                .ok_or_else(|| CodegenError(format!("canonical schema has no {name} tool")))?;
            let request = reference_name(required_field(tool, "params")?)?;
            let result = reference_name(required_field(tool, "result")?)?;
            let minimal_request = required_field(tool, "minimalRequest")?.to_string();
            push_line(&mut output, format_args!("    ToolContract {{"));
            push_line(&mut output, format_args!("        name: {name:?},"));
            push_line(
                &mut output,
                format_args!("        request_model: {request:?},"),
            );
            push_line(
                &mut output,
                format_args!("        result_model: {result:?},"),
            );
            push_line(
                &mut output,
                format_args!("        minimal_request_json: {minimal_request:?},"),
            );
            push_line(&mut output, format_args!("    }},"));
        }
        output.push_str("];\n");
        Ok(output)
    }

    fn definition_closure(&self, roots: &[&str]) -> CodegenResult<BTreeSet<String>> {
        let mut selected = BTreeSet::new();
        let mut pending = roots.iter().map(ToString::to_string).collect::<Vec<_>>();
        while let Some(name) = pending.pop() {
            if selected.contains(&name) {
                continue;
            }
            let body = self.definition(&name)?;
            selected.insert(name);
            pending.extend(schema_references(body));
        }
        Ok(selected)
    }

    fn definition(&self, name: &str) -> CodegenResult<&Value> {
        self.definitions
            .get(name)
            .ok_or_else(|| CodegenError(format!("schema references missing definition {name}")))
    }

    fn union_members(body: &Value) -> CodegenResult<Vec<String>> {
        let Some(branches) = union_branches(body)? else {
            return Ok(Vec::new());
        };
        Ok(branches
            .iter()
            .filter_map(|branch| reference_name(branch).ok())
            .collect())
    }

    fn definition_source(&self, name: &str, body: &Value) -> CodegenResult<String> {
        if body.get("enum").is_some() {
            return Self::enum_source(name, body);
        }
        if union_branches(body)?.is_some() {
            return self.union_source(name, body);
        }
        match body.get("type").and_then(Value::as_str) {
            Some("string" | "integer" | "number" | "boolean") => self.scalar_source(name, body),
            Some("object")
                if body
                    .get("additionalProperties")
                    .is_some_and(Value::is_object) =>
            {
                self.map_source(name, body)
            }
            Some("object") if body.get("properties").is_some_and(Value::is_object) => {
                self.object_source(name, body)
            }
            _ => Err(CodegenError(format!(
                "cannot lower definition {name}: {body}"
            ))),
        }
    }

    fn scalar_source(&self, name: &str, body: &Value) -> CodegenResult<String> {
        let type_name = self.type_name(body, name, true)?;
        let mut output = String::new();
        push_line(
            &mut output,
            format_args!("/// Generated scalar DTO `{name}`."),
        );
        output.push_str(
            "#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]\n\
             #[serde(transparent)]\n\
             #[schemars(transparent)]\n",
        );
        push_line(&mut output, format_args!("pub struct {name}("));
        for attribute in constraints(body) {
            push_line(&mut output, format_args!("    {attribute}"));
        }
        push_line(&mut output, format_args!("    pub {type_name},"));
        output.push_str(");\n\n");
        Ok(output)
    }

    fn enum_source(name: &str, body: &Value) -> CodegenResult<String> {
        let variants = body
            .get("enum")
            .and_then(Value::as_array)
            .ok_or_else(|| CodegenError(format!("enum {name} has no values")))?;
        let mut output = String::new();
        push_line(
            &mut output,
            format_args!("/// Generated string enum `{name}`."),
        );
        output.push_str(
            "#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]\n",
        );
        push_line(&mut output, format_args!("pub enum {name} {{"));
        for variant in variants {
            let wire = variant.as_str().ok_or_else(|| {
                CodegenError(format!("enum {name} contains non-string value {variant}"))
            })?;
            push_line(&mut output, format_args!("    /// Wire value `{wire}`."));
            push_line(&mut output, format_args!("    #[serde(rename = {wire:?})]"));
            push_line(&mut output, format_args!("    {},", pascal(wire)));
        }
        output.push_str("}\n\n");
        Ok(output)
    }

    fn map_source(&self, name: &str, body: &Value) -> CodegenResult<String> {
        let key_schema = body.get("propertyNames").unwrap_or(&Value::Null);
        let key = if key_schema.is_null() {
            "String".to_owned()
        } else {
            self.type_name(key_schema, name, true)?
        };
        let value = self.type_name(required_field(body, "additionalProperties")?, name, true)?;
        Ok(format!(
            "/// Generated map DTO `{name}`.\n\
             #[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]\n\
             #[serde(transparent)]\n\
             #[schemars(transparent)]\n\
             pub struct {name}(pub BTreeMap<{key}, {value}>);\n\n"
        ))
    }

    fn object_source(&self, name: &str, body: &Value) -> CodegenResult<String> {
        let mut output = String::new();
        push_line(
            &mut output,
            format_args!("/// Generated object DTO `{name}`."),
        );
        output.push_str(
            "#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]\n\
             #[serde(deny_unknown_fields)]\n",
        );
        for keyword in ["allOf", "anyOf"] {
            if let Some(value) = body.get(keyword) {
                push_line(
                    &mut output,
                    format_args!("#[schemars(extend({keyword:?} = {value}))]"),
                );
            }
        }
        push_line(&mut output, format_args!("pub struct {name} {{"));
        let fields = self.fields(name, name, body, true)?;
        for field in &fields {
            Self::push_field(&mut output, name, field, true);
        }
        output.push_str("}\n");
        Self::push_defaults(&mut output, name, &fields)?;
        output.push('\n');
        Ok(output)
    }

    fn union_source(&self, name: &str, body: &Value) -> CodegenResult<String> {
        let branches = union_branches(body)?
            .ok_or_else(|| CodegenError(format!("union {name} does not contain a branch list")))?;
        let discriminator = body.get("discriminator").and_then(Value::as_object);
        let tag = discriminator
            .and_then(|value| value.get("propertyName"))
            .and_then(Value::as_str)
            .or_else(|| inferred_union_tag(branches));
        let tags = union_tags(discriminator, branches, tag)?;
        let mut output = String::new();
        if tag.is_none() {
            self.push_singleton_enums(&mut output, branches)?;
        }
        push_line(
            &mut output,
            format_args!("/// Generated union DTO `{name}`."),
        );
        output.push_str("#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]\n");
        match tag {
            Some(tag) => push_line(
                &mut output,
                format_args!("#[serde(tag = {tag:?}, deny_unknown_fields)]"),
            ),
            None => output.push_str("#[serde(untagged, deny_unknown_fields)]\n"),
        }
        push_line(&mut output, format_args!("pub enum {name} {{"));
        for branch in branches {
            self.push_union_variant(&mut output, name, branch, tag, &tags)?;
        }
        output.push_str("}\n");
        for branch in branches {
            let target = branch_name(branch)?;
            let fields = self.fields(&target, name, self.branch_body(branch)?, tag.is_none())?;
            Self::push_defaults(&mut output, &target, &fields)?;
        }
        output.push('\n');
        Ok(output)
    }

    fn push_singleton_enums(&self, output: &mut String, branches: &[Value]) -> CodegenResult<()> {
        for branch in branches {
            let target = branch_name(branch)?;
            let properties = object_field(self.branch_body(branch)?, "properties")?;
            for (field, schema) in properties {
                if let Some(value) = schema.get("const").and_then(Value::as_str) {
                    let type_name = format!("{target}{}", pascal(field));
                    push_singleton_enum(output, &type_name, value);
                }
            }
        }
        Ok(())
    }

    fn push_union_variant(
        &self,
        output: &mut String,
        union: &str,
        branch: &Value,
        tag: Option<&str>,
        tags: &BTreeMap<String, String>,
    ) -> CodegenResult<()> {
        let target = branch_name(branch)?;
        let wire = tags.get(&target);
        let variant = wire.map_or_else(
            || target.strip_prefix(union).unwrap_or(&target).to_owned(),
            |value| pascal(value),
        );
        if let Some(wire) = wire {
            push_line(output, format_args!("    #[serde(rename = {wire:?})]"));
        }
        push_line(
            output,
            format_args!("    /// Generated `{target}` union branch."),
        );
        push_line(output, format_args!("    {variant} {{"));
        for field in self.fields(&target, union, self.branch_body(branch)?, tag.is_none())? {
            Self::push_field(output, &target, &field, false);
        }
        output.push_str("    },\n");
        Ok(())
    }

    fn fields<'a>(
        &self,
        owner: &str,
        recursion_owner: &str,
        body: &'a Value,
        include_constants: bool,
    ) -> CodegenResult<Vec<Field<'a>>> {
        let required = string_set(body.get("required"))?;
        let properties = object_field(body, "properties")?;
        properties
            .iter()
            .filter(|(_, schema)| include_constants || schema.get("const").is_none())
            .map(|(name, schema)| {
                let type_name = if schema.get("const").is_some() {
                    format!("{owner}{}", pascal(name))
                } else {
                    self.type_name(schema, recursion_owner, false)?
                };
                Ok(Field {
                    name,
                    type_name,
                    required: required.contains(name.as_str()),
                    default: schema.get("default"),
                    schema,
                })
            })
            .collect()
    }

    fn push_field(output: &mut String, owner: &str, field: &Field<'_>, public: bool) {
        push_line(output, format_args!("    /// JSON field `{}`.", field.name));
        let snake_name = snake(field.name);
        if NON_RAW_KEYWORDS.contains(&snake_name.as_str()) {
            push_line(
                output,
                format_args!("    #[serde(rename = {:?})]", field.name),
            );
        }
        if !field.required {
            match field.default {
                None | Some(Value::Null) => output.push_str("    #[serde(default)]\n"),
                Some(_) => push_line(
                    output,
                    format_args!(
                        "    #[serde(default = {:?})]",
                        default_name(owner, field.name)
                    ),
                ),
            }
        } else if field.type_name.starts_with("Option<") {
            output.push_str(
                "    #[serde(deserialize_with = \"deserialize_required_option\")]\n\
                 #[schemars(required)]\n",
            );
        }
        for attribute in constraints(field.schema) {
            push_line(output, format_args!("    {attribute}"));
        }
        let visibility = if public { "pub " } else { "" };
        push_line(
            output,
            format_args!(
                "    {visibility}{}: {},",
                identifier(field.name),
                field.type_name
            ),
        );
    }

    fn push_defaults(output: &mut String, owner: &str, fields: &[Field<'_>]) -> CodegenResult<()> {
        for field in fields {
            let Some(value) = field.default.filter(|value| !value.is_null()) else {
                continue;
            };
            let expression = Self::default_expression(field, value)?;
            output.push('\n');
            push_line(
                output,
                format_args!(
                    "fn {}() -> {} {{",
                    default_name(owner, field.name),
                    field.type_name
                ),
            );
            push_line(output, format_args!("    {expression}"));
            output.push_str("}\n");
        }
        Ok(())
    }

    fn default_expression(field: &Field<'_>, value: &Value) -> CodegenResult<String> {
        match value {
            Value::Bool(value) => Ok(value.to_string()),
            Value::Number(value) => Ok(value.to_string()),
            Value::String(value) => match reference_name(field.schema) {
                Ok(target) => Ok(format!("{target}::{}", pascal(value))),
                Err(_) => Ok(format!("String::from({value:?})")),
            },
            _ => Err(CodegenError(format!(
                "cannot lower default {} for {}",
                value, field.name
            ))),
        }
    }

    fn type_name(&self, schema: &Value, owner: &str, indirect: bool) -> CodegenResult<String> {
        if let Ok(target) = reference_name(schema) {
            if !indirect && self.definition_reaches(&target, owner, &mut BTreeSet::new())? {
                return Ok(format!("Box<{target}>"));
            }
            return Ok(target);
        }
        if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
            let real = branches
                .iter()
                .filter(|branch| branch.get("type") != Some(&Value::String("null".to_owned())))
                .collect::<Vec<_>>();
            if real.len() == 1 && real.len() != branches.len() {
                return Ok(format!(
                    "Option<{}>",
                    self.type_name(real[0], owner, indirect)?
                ));
            }
        }
        if schema.as_object().is_some_and(Map::is_empty) {
            return Ok("serde_json::Value".to_owned());
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("array") => {
                let item = schema.get("items").unwrap_or(&Value::Null);
                Ok(format!("Vec<{}>", self.type_name(item, owner, true)?))
            }
            Some("object")
                if schema
                    .get("additionalProperties")
                    .is_some_and(Value::is_object) =>
            {
                let value =
                    self.type_name(required_field(schema, "additionalProperties")?, owner, true)?;
                Ok(format!("BTreeMap<String, {value}>"))
            }
            Some("string") => Ok("String".to_owned()),
            Some("integer") if minimum_is_non_negative(schema) => Ok("u64".to_owned()),
            Some("integer") => Ok("i64".to_owned()),
            Some("number") => Ok("f64".to_owned()),
            Some("boolean") => Ok("bool".to_owned()),
            None => Ok("serde_json::Value".to_owned()),
            _ => Err(CodegenError(format!(
                "cannot lower field in {owner}: {schema}"
            ))),
        }
    }

    fn definition_reaches(
        &self,
        start: &str,
        target: &str,
        visited: &mut BTreeSet<String>,
    ) -> CodegenResult<bool> {
        if start == target {
            return Ok(true);
        }
        if !visited.insert(start.to_owned()) {
            return Ok(false);
        }
        for reference in schema_references(self.definition(start)?) {
            if self.definition_reaches(&reference, target, visited)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn branch_body<'a>(&'a self, branch: &'a Value) -> CodegenResult<&'a Value> {
        match reference_name(branch) {
            Ok(target) => self.definition(&target),
            Err(_) if branch.is_object() => Ok(branch),
            Err(error) => Err(error),
        }
    }
}

fn union_branches(body: &Value) -> CodegenResult<Option<&[Value]>> {
    let is_union = body.pointer("/rift:proto/oneof").is_some();
    if !is_union {
        return Ok(None);
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(branches) = body.get(keyword).and_then(Value::as_array) {
            return Ok(Some(branches));
        }
    }
    Err(CodegenError("protocol union has no branch list".to_owned()))
}

fn union_tags(
    discriminator: Option<&Map<String, Value>>,
    branches: &[Value],
    tag: Option<&str>,
) -> CodegenResult<BTreeMap<String, String>> {
    if let Some(mapping) = discriminator
        .and_then(|value| value.get("mapping"))
        .and_then(Value::as_object)
    {
        return mapping
            .iter()
            .map(|(wire, reference)| {
                let target = reference.as_str().ok_or_else(|| {
                    CodegenError("union mapping reference is not a string".to_owned())
                })?;
                Ok((reference_name_from_str(target)?, wire.clone()))
            })
            .collect();
    }
    let Some(tag) = tag else {
        return Ok(BTreeMap::new());
    };
    branches
        .iter()
        .map(|branch| {
            let name = branch_name(branch)?;
            let wire = branch
                .pointer(&format!("/properties/{tag}/const"))
                .and_then(Value::as_str)
                .ok_or_else(|| CodegenError(format!("union branch {name} has no {tag} tag")))?;
            Ok((name, wire.to_owned()))
        })
        .collect()
}

fn inferred_union_tag(branches: &[Value]) -> Option<&str> {
    let first = branches.first()?.get("properties")?.as_object()?;
    first.iter().find_map(|(name, schema)| {
        schema.get("const")?;
        branches
            .iter()
            .all(|branch| {
                branch
                    .pointer(&format!("/properties/{name}/const"))
                    .is_some()
            })
            .then_some(name.as_str())
    })
}

fn branch_name(branch: &Value) -> CodegenResult<String> {
    reference_name(branch).or_else(|_| {
        branch
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| CodegenError(format!("inline union branch has no title: {branch}")))
    })
}

fn schema_references(node: &Value) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    collect_schema_references(node, &mut references);
    references
}

fn collect_schema_references(node: &Value, references: &mut BTreeSet<String>) {
    match node {
        Value::Array(values) => {
            for value in values {
                collect_schema_references(value, references);
            }
        }
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && let Ok(name) = reference_name_from_str(reference)
            {
                references.insert(name);
            }
            for value in object.values() {
                collect_schema_references(value, references);
            }
        }
        _ => {}
    }
}

fn reference_name(schema: &Value) -> CodegenResult<String> {
    let reference = schema
        .get("$ref")
        .and_then(Value::as_str)
        .ok_or_else(|| CodegenError(format!("schema branch is not a reference: {schema}")))?;
    reference_name_from_str(reference)
}

fn reference_name_from_str(reference: &str) -> CodegenResult<String> {
    reference
        .strip_prefix("#/$defs/")
        .map(ToOwned::to_owned)
        .ok_or_else(|| CodegenError(format!("unsupported schema reference {reference}")))
}

fn required_field<'a>(body: &'a Value, name: &str) -> CodegenResult<&'a Value> {
    body.get(name)
        .ok_or_else(|| CodegenError(format!("schema object has no {name}: {body}")))
}

fn object_field<'a>(body: &'a Value, name: &str) -> CodegenResult<&'a Map<String, Value>> {
    body.get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| CodegenError(format!("schema field {name} is not an object")))
}

fn string_set(value: Option<&Value>) -> CodegenResult<BTreeSet<&str>> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    value
        .as_array()
        .ok_or_else(|| CodegenError("required field list is not an array".to_owned()))?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| CodegenError("required field is not a string".to_owned()))
        })
        .collect()
}

fn push_singleton_enum(output: &mut String, name: &str, value: &str) {
    push_line(
        output,
        format_args!("/// Generated constant field `{value}`."),
    );
    output.push_str(
        "#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]\n",
    );
    push_line(output, format_args!("pub enum {name} {{"));
    push_line(output, format_args!("    /// Wire value `{value}`."));
    push_line(output, format_args!("    #[serde(rename = {value:?})]"));
    push_line(output, format_args!("    {},", pascal(value)));
    output.push_str("}\n\n");
}

fn constraints(schema: &Value) -> Vec<String> {
    let body = nullable_body(schema).unwrap_or(schema);
    let mut constraints = Vec::new();
    push_range_constraint(&mut constraints, body);
    push_length_constraint(&mut constraints, body);
    if let Some(pattern) = body.get("pattern").and_then(Value::as_str) {
        constraints.push(format!(
            "#[schemars(regex(pattern = {}))]",
            raw_string(pattern)
        ));
    }
    constraints
}

fn push_range_constraint(constraints: &mut Vec<String>, body: &Value) {
    let minimum = body.get("minimum");
    let maximum = body.get("maximum");
    if minimum.is_none() && maximum.is_none() {
        return;
    }
    let suffix = match body.get("type").and_then(Value::as_str) {
        Some("integer") if minimum_is_non_negative(body) => "_u64",
        Some("integer") => "_i64",
        _ => "",
    };
    let mut bounds = Vec::new();
    if let Some(value) = minimum {
        bounds.push(format!("min = {}{suffix}", number_literal(value)));
    }
    if let Some(value) = maximum {
        bounds.push(format!("max = {}{suffix}", number_literal(value)));
    }
    constraints.push(format!("#[schemars(range({}))]", bounds.join(", ")));
}

fn push_length_constraint(constraints: &mut Vec<String>, body: &Value) {
    let minimum = body.get("minLength").or_else(|| body.get("minItems"));
    let maximum = body.get("maxLength").or_else(|| body.get("maxItems"));
    if minimum.is_none() && maximum.is_none() {
        return;
    }
    let mut bounds = Vec::new();
    if let Some(value) = minimum {
        bounds.push(format!("min = {value}"));
    }
    if let Some(value) = maximum {
        bounds.push(format!("max = {value}"));
    }
    constraints.push(format!("#[schemars(length({}))]", bounds.join(", ")));
}

fn nullable_body(schema: &Value) -> Option<&Value> {
    let branches = schema.get("anyOf")?.as_array()?;
    let mut real = branches
        .iter()
        .filter(|branch| branch.get("type").and_then(Value::as_str) != Some("null"));
    let body = real.next()?;
    real.next().is_none().then_some(body)
}

fn minimum_is_non_negative(schema: &Value) -> bool {
    schema
        .get("minimum")
        .and_then(Value::as_i64)
        .is_none_or(|minimum| minimum >= 0)
}

fn number_literal(value: &Value) -> String {
    let text = value.to_string();
    let Some((sign, digits)) = text
        .strip_prefix('-')
        .map(|digits| ("-", digits))
        .or(Some(("", text.as_str())))
    else {
        return text;
    };
    if digits.contains(['.', 'e', 'E']) {
        return text;
    }
    let mut grouped = String::with_capacity(text.len() + text.len() / 3);
    grouped.push_str(sign);
    let first = digits.len() % 3;
    if first != 0 {
        grouped.push_str(&digits[..first]);
        if digits.len() > first {
            grouped.push('_');
        }
    }
    for (index, chunk) in digits.as_bytes()[first..].chunks(3).enumerate() {
        if index > 0 {
            grouped.push('_');
        }
        grouped.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    grouped
}

fn raw_string(value: &str) -> String {
    let mut hashes = String::new();
    while value.contains(&format!("\"{hashes}")) {
        hashes.push('#');
    }
    format!("r{hashes}\"{value}\"{hashes}")
}

fn default_name(owner: &str, field: &str) -> String {
    format!("default_{}_{}", snake(owner), snake(field))
}

fn identifier(name: &str) -> String {
    let identifier = snake(name);
    if NON_RAW_KEYWORDS.contains(&identifier.as_str()) {
        return format!("{identifier}_");
    }
    if RUST_KEYWORDS.contains(&identifier.as_str()) {
        return format!("r#{identifier}");
    }
    identifier
}

fn snake(name: &str) -> String {
    let mut output = String::with_capacity(name.len());
    let mut previous_lowercase = false;
    for character in name.chars() {
        if character.is_uppercase() && previous_lowercase {
            output.push('_');
        }
        output.extend(character.to_lowercase());
        previous_lowercase = character.is_lowercase();
    }
    output
}

fn pascal(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(characters).collect()
            })
        })
        .collect()
}

fn push_line(output: &mut String, arguments: fmt::Arguments<'_>) {
    if output.write_fmt(arguments).is_err() {
        unreachable!("writing generated Rust into String cannot fail");
    }
    output.push('\n');
}
