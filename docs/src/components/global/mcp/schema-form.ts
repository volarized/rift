import {
  dereference,
  type JsonPrimitive,
  type JsonSchema,
  type JsonValue,
} from "@/components/json-tree";

/**
 * Maps a tool's `inputSchema` to form fields and back to `arguments`.
 *
 * A property gets a control when one control can hold every value its schema
 * accepts: a string, a number, a boolean, or one value of an `enum`. Any other
 * property (an array, an object, a `oneOf`, a union of types) is edited as
 * JSON. The mapping is lossless in both directions: a value a control cannot
 * hold switches its field to JSON, and an argument the schema does not name is
 * kept beside the fields.
 */

export type FieldKind = "string" | "number" | "integer" | "boolean" | "enum" | "json";

export type FieldSpec = {
  name: string;
  kind: FieldKind;
  required: boolean;
  description?: string;
  /** The schema's own word for the type: `string`, `array`, `oneOf`. */
  typeLabel: string;
  enumValues: JsonPrimitive[];
  minimum?: number;
  maximum?: number;
  defaultValue?: JsonValue;
  examples: JsonValue[];
};

export type FieldState = {
  /** Text of a text, number or JSON control; the index of the chosen `enum` value. */
  text: string;
  /** State of a boolean control; `null` leaves the argument out. */
  flag: boolean | null;
  /** The value does not fit the control, so the field holds it as JSON in `text`. */
  asJson: boolean;
};

export type FormState = {
  fields: Record<string, FieldState>;
  /** Arguments the schema does not name, set in JSON mode and sent as they are. */
  extra: Record<string, JsonValue>;
};

export type Arguments = Record<string, JsonValue>;

const EMPTY_FIELD: FieldState = { text: "", flag: null, asJson: false };

function isPrimitive(value: JsonValue | undefined): value is JsonPrimitive {
  return value === null || ["string", "number", "boolean"].includes(typeof value);
}

function nonNullTypes(schema: JsonSchema): string[] {
  if (Array.isArray(schema.type)) return schema.type.filter((type) => type !== "null");
  return schema.type ? [schema.type] : [];
}

/** `anyOf: [T, null]` and `oneOf: [T, null]` are an optional `T`: the form reads `T`. */
function unwrapNullable(schema: JsonSchema, root: JsonSchema): JsonSchema {
  const variants = schema.anyOf ?? schema.oneOf;
  if (!variants || schema.allOf) return schema;
  const present = variants
    .map((variant) => dereference(variant, root))
    .filter((variant) => variant.type !== "null");
  if (present.length !== 1) return schema;
  const { anyOf: _anyOf, oneOf: _oneOf, ...outer } = schema;
  return { ...present[0], ...outer };
}

function classify(schema: JsonSchema): { kind: FieldKind; typeLabel: string } {
  if (schema.const !== undefined && isPrimitive(schema.const)) {
    return { kind: "enum", typeLabel: "const" };
  }
  if (schema.enum?.every(isPrimitive)) return { kind: "enum", typeLabel: "enum" };
  for (const keyword of ["oneOf", "anyOf", "allOf"] as const) {
    if (schema[keyword]) return { kind: "json", typeLabel: keyword };
  }
  const types = nonNullTypes(schema);
  if (types.length !== 1) {
    return { kind: "json", typeLabel: types.length > 1 ? types.join(" | ") : "any" };
  }
  const [type] = types;
  if (type === "string" || type === "number" || type === "integer" || type === "boolean") {
    return { kind: type, typeLabel: type };
  }
  return { kind: "json", typeLabel: type };
}

/** The fields of a tool's `inputSchema`, in the order the schema declares them. */
export function fieldSpecs(inputSchema: JsonSchema): FieldSpec[] {
  const root = dereference(inputSchema, inputSchema);
  const required = new Set(root.required ?? []);
  return Object.entries(root.properties ?? {}).map(([name, property]) => {
    const schema = unwrapNullable(dereference(property, inputSchema), inputSchema);
    const enumValues =
      schema.const !== undefined && isPrimitive(schema.const)
        ? [schema.const]
        : (schema.enum ?? []).filter(isPrimitive);
    return {
      name,
      ...classify(schema),
      required: required.has(name),
      description: schema.description,
      enumValues,
      minimum: schema.minimum,
      maximum: schema.maximum,
      defaultValue: schema.default,
      examples: Array.isArray(schema.examples) ? schema.examples : [],
    };
  });
}

/** Whole-argument examples a schema carries at its root. */
export function argumentExamples(inputSchema: JsonSchema): Arguments[] {
  const examples = dereference(inputSchema, inputSchema).examples;
  if (!Array.isArray(examples)) return [];
  return examples.filter(
    (example): example is Arguments =>
      typeof example === "object" && example !== null && !Array.isArray(example),
  );
}

export function emptyForm(): FormState {
  return { fields: {}, extra: {} };
}

export function fieldState(form: FormState, name: string): FieldState {
  return form.fields[name] ?? EMPTY_FIELD;
}

/** The state that makes a field's control show `value`. */
export function fieldFromValue(spec: FieldSpec, value: JsonValue): FieldState {
  const asJson: FieldState = { text: JSON.stringify(value, null, 2), flag: null, asJson: true };
  switch (spec.kind) {
    case "string":
      return typeof value === "string" && value !== "" ? { ...EMPTY_FIELD, text: value } : asJson;
    case "number":
    case "integer":
      return typeof value === "number" ? { ...EMPTY_FIELD, text: String(value) } : asJson;
    case "boolean":
      return typeof value === "boolean" ? { ...EMPTY_FIELD, flag: value } : asJson;
    case "enum": {
      const index = spec.enumValues.indexOf(value as JsonPrimitive);
      return index === -1 ? asJson : { ...EMPTY_FIELD, text: String(index) };
    }
    case "json":
      return { ...asJson, asJson: false };
  }
}

export function formFromArguments(specs: FieldSpec[], args: Arguments): FormState {
  const form = emptyForm();
  const known = new Map(specs.map((spec) => [spec.name, spec]));
  for (const [name, value] of Object.entries(args)) {
    const spec = known.get(name);
    if (spec) form.fields[name] = fieldFromValue(spec, value);
    else form.extra[name] = value;
  }
  return form;
}

type FieldResult = { value?: JsonValue; error?: string };

function parseJsonField(text: string): FieldResult {
  try {
    return { value: JSON.parse(text) as JsonValue };
  } catch (error) {
    return { error: `Not valid JSON: ${error instanceof Error ? error.message : String(error)}` };
  }
}

function readField(spec: FieldSpec, state: FieldState): FieldResult {
  if (spec.kind === "boolean" && !state.asJson) {
    if (state.flag !== null) return { value: state.flag };
    // A required boolean has no "left out" state: unchecked sends `false`.
    return spec.required ? { value: false } : {};
  }
  const text = spec.kind === "string" && !state.asJson ? state.text : state.text.trim();
  if (text === "") return spec.required ? { error: "Required." } : {};
  if (state.asJson || spec.kind === "json") return parseJsonField(text);
  switch (spec.kind) {
    case "string":
      return { value: text };
    case "enum":
      return { value: spec.enumValues[Number(text)] ?? null };
    default: {
      const value = Number(text);
      if (!Number.isFinite(value)) return { error: "Enter a number." };
      if (spec.kind === "integer" && !Number.isInteger(value)) {
        return { error: "Enter a whole number." };
      }
      if (spec.minimum !== undefined && value < spec.minimum) {
        return { error: `Must be at least ${spec.minimum}.` };
      }
      if (spec.maximum !== undefined && value > spec.maximum) {
        return { error: `Must be at most ${spec.maximum}.` };
      }
      return { value };
    }
  }
}

/** The `arguments` a form holds, and the error of every field that has one. */
export function argumentsFromForm(
  specs: FieldSpec[],
  form: FormState,
): { args: Arguments; errors: Record<string, string> } {
  const args: Arguments = {};
  const errors: Record<string, string> = {};
  for (const spec of specs) {
    const { value, error } = readField(spec, fieldState(form, spec.name));
    if (error) errors[spec.name] = error;
    else if (value !== undefined) args[spec.name] = value;
  }
  return { args: { ...args, ...form.extra }, errors };
}

/** Parses the text of JSON mode: `arguments` is a JSON object. */
export function parseArguments(text: string): { args?: Arguments; error?: string } {
  if (text.trim() === "") return { args: {} };
  const { value, error } = parseJsonField(text);
  if (error) return { error };
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { error: "Arguments are a JSON object." };
  }
  return { args: value };
}

/** One line of constraints for a field's help text: type, bounds, default. */
export function constraintText(spec: FieldSpec): string {
  const parts = [spec.typeLabel];
  if (spec.minimum !== undefined && spec.maximum !== undefined) {
    parts.push(`${spec.minimum} to ${spec.maximum}`);
  } else if (spec.minimum !== undefined) {
    parts.push(`at least ${spec.minimum}`);
  } else if (spec.maximum !== undefined) {
    parts.push(`at most ${spec.maximum}`);
  }
  if (spec.defaultValue !== undefined) parts.push(`default ${JSON.stringify(spec.defaultValue)}`);
  return parts.join(", ");
}

/** Placeholder of a field: its first example, else its default, as the control shows it. */
export function placeholderText(spec: FieldSpec): string | undefined {
  const sample = spec.examples[0] ?? spec.defaultValue;
  if (sample === undefined) {
    if (spec.kind !== "json") return undefined;
    return spec.typeLabel === "array" ? "[]" : spec.typeLabel === "object" ? "{}" : "JSON";
  }
  return spec.kind === "string" && typeof sample === "string" ? sample : JSON.stringify(sample);
}
