/**
 * core.proto, generated from core.json.
 *
 * The model is defined once, as JSON Schema, because that is what MCP speaks.
 * The adapter seam speaks protobuf and cannot $ref a JSON Schema, so it needs
 * the same messages in its own language. Generating them keeps the two from
 * drifting: there is no second place to edit.
 */
import { readFileSync, writeFileSync } from "node:fs";

const core = JSON.parse(readFileSync("protocol/core.json", "utf8"));
const defs = core.$defs;

const SCALAR = {
  string: "string",
  integer: "int64",
  number: "double",
  boolean: "bool",
};

const snake = (s) => s.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
const SCREAM = (s) => snake(s).toUpperCase();

const out = [];
const emitted = new Set();
/** Messages produced for inline objects, appended after their parent. */
let pending = [];
/**
 * Enums produced for inline properties, nested inside the message that owns
 * them. Protobuf scopes an enum's values to the enclosing namespace rather than
 * to the enum, so a top-level `Kind` and a top-level `State` cannot both spell
 * their zero value `UNSPECIFIED`. Nesting moves that namespace to the message,
 * which is what lets `CoverageScope.Kind.REQUEST` be written without repeating
 * the message name in every value.
 *
 * A stack, because a message minted for a union branch is built while its
 * parent is still open.
 */
const nesting = [];

function refName(node) {
  const ref = node?.$ref;
  if (typeof ref !== "string") return null;
  const [, pointer] = ref.split("#");
  return pointer?.startsWith("/$defs/") ? pointer.slice("/$defs/".length) : null;
}

/** A wrapper that only adds a description around a $ref is that ref. */
function unwrap(node) {
  return node;
}

/**
 * The proto type for a schema, minting a nested message where the schema is
 * inline. `field` is the property this type is being read for, and names an
 * enum nested into the message rather than one hoisted beside it.
 */
function typeOf(node, hint, field = hint) {
  const ref = refName(node);
  if (ref) {
    const target = defs[ref];
    // An alias to a scalar becomes that scalar: protobuf has no newtype.
    if (target && !target.properties && !target.oneOf && !target.enum && target.type in SCALAR) {
      return SCALAR[target.type];
    }
    return ref;
  }
  if (node.enum) {
    // Inside a message, the property's own name is enough: `Kind` reads as
    // `CoverageScope.Kind` from anywhere else. Only an enum with nowhere to
    // nest keeps the long name.
    const nested = nesting[nesting.length - 1];
    if (nested) {
      const name = field.replace(/(^|_)([a-z])/g, (_, __, ch) => ch.toUpperCase());
      nested.push({ name, schema: node });
      return name;
    }
    pending.push(enumBlock(hint, node));
    return hint;
  }
  if (node.const !== undefined) return typeof node.const === "string" ? "string" : "int64";
  if (node.type === "array") return `repeated ${typeOf(node.items ?? {}, hint, field)}`;
  if (node.properties || node.type === "object") {
    if (node.additionalProperties && typeof node.additionalProperties === "object") {
      return `map<string, ${typeOf(node.additionalProperties, `${hint}Value`, field)}>`;
    }
    if (node.properties) {
      pending.push(messageBlock(hint, node));
      return hint;
    }
    return "google.protobuf.Struct";
  }
  if (Array.isArray(node.type)) {
    // `["string","null"]` is an optional string; protobuf presence covers it.
    const real = node.type.filter((t) => t !== "null");
    return SCALAR[real[0]] ?? "string";
  }
  if (typeof node.type === "string") return SCALAR[node.type] ?? "string";
  if (node.oneOf) {
    const arms = node.oneOf.filter((b) => b.type !== "null");
    if (arms.length === 1) return typeOf(arms[0], hint, field);
    pending.push(unionBlock(hint, { oneOf: arms }));
    return hint;
  }
  return "google.protobuf.Value";
}

function comment(text, indent = "") {
  if (!text) return [];
  return String(text)
    .split("\n")
    .flatMap((para) => wrap(para, 88 - indent.length))
    .map((line) => `${indent}// ${line}`.trimEnd());
}

function wrap(text, width) {
  if (text === "") return [""];
  const words = text.split(/\s+/).filter(Boolean);
  const lines = [];
  let line = "";
  for (const word of words) {
    if (line && `${line} ${word}`.length > width) {
      lines.push(line);
      line = word;
    } else line = line ? `${line} ${word}` : word;
  }
  if (line) lines.push(line);
  return lines;
}

/**
 * `prefixed` spells every value with the enum's name in front of it, which is
 * what protobuf's scoping forces on a top-level enum. A nested enum owns the
 * message's namespace and can use the bare value — until a second enum nests
 * into the same message, at which point both would claim `UNSPECIFIED` and both
 * go back to prefixes.
 */
function enumBlock(name, schema, { indent = "", prefixed = true } = {}) {
  const prefix = prefixed ? `${SCREAM(name)}_` : "";
  const described = schema["rift:enumDescriptions"] ?? {};
  const lines = [...comment(schema.description, indent), `${indent}enum ${name} {`];
  lines.push(`${indent}  // Not set. Every value below is one the adapter chose.`);
  lines.push(`${indent}  ${prefix}UNSPECIFIED = 0;`);
  schema.enum.forEach((value, index) => {
    for (const line of comment(described[value], `${indent}  `)) lines.push(line);
    lines.push(`${indent}  ${prefix}${SCREAM(String(value))} = ${index + 1};`);
  });
  lines.push(`${indent}}`);
  return lines.join("\n");
}

/** A tagged union becomes a message with a `oneof`, one field per branch. */
function unionBlock(name, schema) {
  const arms = schema.oneOf.map((branch) => {
    const ref = refName(branch);
    if (ref) return { label: snake(ref), schema: branch, description: branch.description };
    const props = Object.entries(branch.properties ?? {});
    const constTag = props.find(([, p]) => p.const !== undefined);
    if (constTag) {
      return { label: snake(String(constTag[1].const)), schema: branch, description: branch.description };
    }
    // Discriminated by a disjoint enum rather than a const: the property that
    // carries it names the branch, since no single value does.
    const enumTag = props.find(([, p]) => p.enum);
    return { label: snake(enumTag?.[0] ?? "other"), schema: branch, description: branch.description };
  });

  const taken = new Set(["variant"]);
  for (const arm of arms) {
    let label = arm.label;
    for (let n = 2; taken.has(label); n += 1) label = `${arm.label}_${n}`;
    taken.add(label);
    arm.label = label;
  }

  const lines = [...comment(schema.description), `message ${name} {`, `  oneof variant {`];
  arms.forEach((arm, index) => {
    const ref = refName(arm.schema);
    let type;
    if (ref) type = ref;
    else {
      // A branch whose only content is its tag needs no payload message.
      const fields = Object.entries(arm.schema.properties ?? {}).filter(
        ([, p]) => p.const === undefined,
      );
      if (fields.length === 0) type = "google.protobuf.Empty";
      else {
        const inner = `${name}${arm.label.replace(/(^|_)([a-z])/g, (_, __, ch) => ch.toUpperCase())}`;
        pending.push(
          messageBlock(inner, {
            ...arm.schema,
            properties: Object.fromEntries(fields),
            required: (arm.schema.required ?? []).filter(
              (r) => arm.schema.properties?.[r]?.const === undefined,
            ),
          }),
        );
        type = inner;
      }
    }
    for (const line of comment(arm.description, "    ")) lines.push(line);
    lines.push(`    ${type} ${arm.label} = ${index + 1};`);
  });
  lines.push("  }", "}");
  return lines.join("\n");
}

function messageBlock(name, schema) {
  const entries = Object.entries(schema.properties ?? {});
  const required = new Set(schema.required ?? []);
  const fields = [];
  nesting.push([]);
  entries.forEach(([field, property], index) => {
    const hint = `${name}${field.replace(/(^|_)([a-z])/g, (_, __, ch) => ch.toUpperCase())}`;
    const type = typeOf(property, hint, field);
    const nullable =
      Array.isArray(property.type)
        ? property.type.includes("null")
        : (property.oneOf ?? []).some((b) => b.type === "null");
    const optional = !required.has(field) || nullable;
    const prefix = type.startsWith("repeated ") || type.startsWith("map<") ? "" : optional ? "optional " : "";
    for (const line of comment(property.description, "  ")) fields.push(line);
    fields.push(`  ${prefix}${type} ${field} = ${index + 1};`);
  });
  const nested = nesting.pop();
  const prefixed = nested.length > 1;

  const lines = [...comment(schema.description), `message ${name} {`];
  for (const declared of nested) {
    lines.push(enumBlock(declared.name, declared.schema, { indent: "  ", prefixed }), "");
  }
  lines.push(...fields, "}");
  return lines.join("\n");
}

function block(name, schema) {
  if (schema.enum) return enumBlock(name, schema);
  if (schema.oneOf) {
    const arms = schema.oneOf.filter((b) => b.type !== "null");
    // Branches that only assert which fields are present are a constraint, not
    // a union; protobuf cannot express them and the JSON Schema stays normative.
    if (arms.every((b) => Object.keys(b).length === 1 && b.required)) {
      return messageBlock(name, schema);
    }
    return unionBlock(name, { ...schema, oneOf: arms });
  }
  if (schema.properties) return messageBlock(name, schema);
  if (schema.additionalProperties && typeof schema.additionalProperties === "object") {
    return [
      ...comment(schema.description),
      `message ${name} {`,
      `  map<string, ${typeOf(schema.additionalProperties, `${name}Value`)}> entries = 1;`,
      "}",
    ].join("\n");
  }
  return null; // scalar alias: inlined at every use site
}

const SKIP = new Set(["ProtocolVersion", "JsonSchemaDocument"]);

for (const [name, schema] of Object.entries(defs)) {
  if (SKIP.has(name)) continue;
  pending = [];
  const body = block(name, schema);
  if (!body) continue;
  emitted.add(name);
  out.push(body, ...pending);
}

const header = `// Generated from core.json. Do not edit.
//
// The model is defined once, as JSON Schema, because that is what the MCP
// surface speaks. The adapter seam speaks protobuf and cannot reference a JSON
// Schema, so it needs the same messages in its own language. Generating them
// means there is no second place to edit and no way for the two to disagree.
//
// Scalar aliases — an identity that is a string with a pattern — inline as
// their scalar. Protobuf has no newtype, and a one-field wrapper message would
// would only add indirection. The pattern stays normative in core.json.

syntax = "proto3";

package rift.core.v1;

import "google/protobuf/empty.proto";
import "google/protobuf/struct.proto";
`;

writeFileSync("protocol/core.proto", `${header}\n${out.join("\n\n")}\n`);
console.log(`core.proto: ${emitted.size} messages from ${Object.keys(defs).length} definitions`);
