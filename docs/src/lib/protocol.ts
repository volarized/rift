/**
 * Loads the protocol schemas and answers questions about JSON Schema. What the
 * *documentation* makes of them — the tool catalogue, the per-page tables of
 * contents, the search index — lives in `protocol-surface.ts`, which builds on
 * this. There is no generated file anywhere: the schemas are the only source.
 *
 * The contract is split across three documents that reference each other by
 * `$id`-relative `$ref` — `adapter.json` and `mcp.json` both point at
 * `core.json`, and nothing points back. Definition names are unique across all
 * three, which is what lets a cross-document reference resolve to one anchor;
 * `refName` accepts both the local and the cross-file form and is the only
 * place that knows a `$ref` can name a file at all.
 *
 * Subschema walking is `json-schema-traverse` (the walker Ajv uses), not a
 * hand-rolled one: it already knows which keywords hold schemas, which hold
 * maps of schemas, and which hold plain data. That last distinction is the one
 * worth buying — `examples` is an array of *instances*, and a naive walker
 * descends into it and starts reading example payloads as if they were schemas.
 *
 * The files are read with `fs` rather than imported. `resolveJsonModule` would
 * infer a literal type for all 421 definitions and make every typecheck pay
 * for it.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import traverse from "json-schema-traverse";
import type { JSONSchema } from "json-schema-typed/draft-2020-12";

/** A schema object. The draft allows a bare boolean; these documents never use one. */
export type Schema = Exclude<JSONSchema, boolean>;

/** The page's URL. Shared so the TOC and search overrides agree on which page they mean. */
export const PROTOCOL_PAGE_URL = "/docs/protocol";

/**
 * Read order is dependency order: core first, then the two documents that
 * depend on it. The page presents them the same way.
 */
export const PROTOCOL_FILES = ["core", "mcp", "adapter"] as const;
export type ProtocolFile = (typeof PROTOCOL_FILES)[number];

interface EntryPointSeam {
  description?: string;
  [member: string]: unknown;
}

interface ProtocolDocument extends Schema {
  $id: string;
  title: string;
  description: string;
  $defs: Record<string, Schema>;
  "rift:entryPoints"?: Record<string, EntryPointSeam | string>;
  "rift:targetTiers"?: {
    description: string;
    rules: { entry: string; pointer: string; tier: string }[];
  };
}

const PROTOCOL_DIR = join(process.cwd(), "..", "protocol");

export const documents = Object.fromEntries(
  PROTOCOL_FILES.map((file) => [
    file,
    JSON.parse(readFileSync(join(PROTOCOL_DIR, `${file}.json`), "utf8")) as ProtocolDocument,
  ]),
) as Record<ProtocolFile, ProtocolDocument>;

/** Definition names per file, in each document's own `$defs` order. */
export const defNamesByFile = Object.fromEntries(
  PROTOCOL_FILES.map((file) => [file, Object.keys(documents[file].$defs)]),
) as Record<ProtocolFile, string[]>;

export const defNames: string[] = PROTOCOL_FILES.flatMap((file) => defNamesByFile[file]);

/**
 * Every definition, merged. Names are unique across the three documents — the
 * split was a partition of one namespace, not a rename — and the assertion
 * below keeps it that way, because a collision would silently make two
 * definitions share one anchor.
 */
export const defs: Record<string, Schema> = {};
export const homeOf: Record<string, ProtocolFile> = {};
for (const file of PROTOCOL_FILES) {
  for (const [name, schema] of Object.entries(documents[file].$defs)) {
    if (name in defs) {
      throw new Error(
        `definition \`${name}\` is defined in both ${homeOf[name]}.json and ${file}.json`,
      );
    }
    defs[name] = schema;
    homeOf[name] = file;
  }
}

/** `allKeys` so keywords outside the walker's own table are still visited. */
const TRAVERSE_OPTIONS = { allKeys: true } as const;

/** Every schema node inside a definition, including the definition itself. */
function nodes(root: Schema): Schema[] {
  const found: Schema[] = [];
  traverse(root as traverse.SchemaObject, TRAVERSE_OPTIONS, (node) => {
    found.push(node as Schema);
  });
  return found;
}

/**
 * Keywords the components know how to render. Anything outside this set would
 * reach the page as silence, so it fails the build instead. The schemas are
 * normative; a reference that quietly drops a constraint is worse than none.
 */
const KNOWN_KEYWORDS = new Set([
  "$ref",
  "$schema",
  "$id",
  "title",
  "description",
  "type",
  "const",
  "enum",
  "default",
  "examples",
  "format",
  "properties",
  "required",
  "additionalProperties",
  "minProperties",
  "propertyNames",
  "items",
  "minItems",
  "maxItems",
  "uniqueItems",
  "contains",
  "minLength",
  "maxLength",
  "pattern",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "multipleOf",
  "oneOf",
  "anyOf",
  "allOf",
  "not",
  "if",
  "then",
  "else",
  "contentEncoding",
  "contentMediaType",
  "contentSchema",
]);

/** Keywords whose meaning the structured layout cannot express on its own. */
const LOSSY_KEYWORDS = new Set(["allOf", "if", "then", "else", "not", "contains", "contentSchema"]);

/**
 * Draft 2020-12 permits a bare `true`/`false` in any schema position. These
 * documents only ever use one, as `additionalProperties: false`, which the
 * property table reads directly — so everywhere else a boolean means "nothing
 * to render" rather than a case to handle.
 */
export function sub(node: JSONSchema | undefined): Schema | undefined {
  return typeof node === "object" && node !== null ? node : undefined;
}

/** A schema's properties, minus any boolean ones. */
export function props(node: Schema): [string, Schema][] {
  return Object.entries(node.properties ?? {}).flatMap(([name, value]) => {
    const child = sub(value);
    return child ? [[name, child] as [string, Schema]] : [];
  });
}

/** A union's branches, and which keyword declared them. */
export function branches(node: Schema): { keyword: "oneOf" | "anyOf"; list: Schema[] } | null {
  for (const keyword of ["oneOf", "anyOf"] as const) {
    const list = node[keyword];
    if (list) return { keyword, list: list.flatMap((branch) => sub(branch) ?? []) };
  }
  return null;
}

/**
 * The definition a `$ref` names, whether it is local (`#/$defs/X`) or
 * cross-file (`core.json#/$defs/X`). Both resolve to a bare name because the
 * page renders all three documents together and names are unique across them.
 */
export function refName(node: Schema | undefined): string | null {
  const ref = node?.$ref;
  if (typeof ref !== "string") return null;

  const [file, pointer] = ref.split("#");
  if (pointer === undefined || !pointer.startsWith("/$defs/")) {
    throw new Error(`unsupported $ref target: ${ref}`);
  }
  if (file !== "" && !PROTOCOL_FILES.includes(file.replace(/\.json$/, "") as ProtocolFile)) {
    throw new Error(`$ref names an unknown protocol document: ${ref}`);
  }
  return pointer.slice("/$defs/".length);
}

export function isLossy(root: Schema): boolean {
  return nodes(root).some((node) => Object.keys(node).some((key) => LOSSY_KEYWORDS.has(key)));
}

for (const name of defNames) {
  for (const node of nodes(defs[name])) {
    for (const key of Object.keys(node)) {
      if (key.startsWith("rift:") || KNOWN_KEYWORDS.has(key)) continue;
      throw new Error(
        `unhandled JSON Schema keyword \`${key}\` in ${name}. Teach src/components/protocol to render it before it reaches the page.`,
      );
    }
    // Fails the build on a ref that names a document we do not load.
    refName(node);
  }
}

/** Which definitions refer to a given one, across all three documents. */
export const backlinks: Record<string, string[]> = (() => {
  const edges = new Map<string, Set<string>>(defNames.map((name) => [name, new Set<string>()]));
  for (const from of defNames) {
    for (const node of nodes(defs[from])) {
      const target = refName(node);
      if (target && target !== from) edges.get(target)?.add(from);
    }
  }
  return Object.fromEntries([...edges].map(([name, set]) => [name, [...set].sort()]));
})();
