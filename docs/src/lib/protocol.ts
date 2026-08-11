/**
 * Loads one version's MCP schema and answers questions about JSON Schema. What
 * the *documentation* makes of them — the tool catalogue, the per-page tables of
 * contents, the search index — lives in `protocol-surface.ts`, which builds on
 * this. This module owns `mcp.json`; `config.ts` independently loads the generated
 * `rift.toml` schema. The MCP document's `rift:package` annotations identify
 * definitions that also cross the internal Protobuf seam.
 *
 * Every export that depends on the document is reached through
 * `protocolFor(version)`: the draft reads the live `protocol/` tree and a dated
 * version reads its frozen snapshot, so the same loader serves every documented
 * tree. The schema helpers below it are pure and version-free.
 *
 * Subschema walking is `json-schema-traverse` (the walker Ajv uses), not a
 * hand-rolled one: it already knows which keywords hold schemas, which hold
 * maps of schemas, and which hold plain data. That last distinction is the one
 * worth buying — `examples` is an array of *instances*, and a naive walker
 * descends into it and starts reading example payloads as if they were schemas.
 *
 * The files are read with `fs` rather than imported. `resolveJsonModule` would
 * infer a literal type for every definition and make each typecheck pay for it.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import traverse from "json-schema-traverse";
import type { JSONSchema } from "json-schema-typed/draft-2020-12";
import { protocolDirFor } from "@/lib/protocol-dir";
import type { DocVersion } from "@/lib/versions";

/** A schema object. The draft allows a bare boolean; these documents never use one. */
export type Schema = Exclude<JSONSchema, boolean>;

/**
 * Documentation pages still split shared values from MCP envelopes. The split
 * follows each definition's generated package annotation.
 */
export const PROTOCOL_FILES = ["core", "mcp"] as const;
export type ProtocolFile = (typeof PROTOCOL_FILES)[number];

interface EntryPointSeam {
  description?: string;
  [member: string]: unknown;
}

/**
 * One group in the reference, as `mcp.json` declares it. Which definitions
 * belong to which axis is part of the model, so it is stated in the schema
 * rather than kept as a list in the code that renders it.
 */
export interface AxisGroup {
  name: string;
  summary: string;
  /** Definitions that identify this group. Everything they reach joins it. */
  identifiedBy?: string[];
  /** Definitions pinned here outright, before any group closes over anything. */
  holds?: string[];
  /** Takes whatever no group claimed and is defined in this document. */
  residualOf?: ProtocolFile;
}

export interface ProtocolDocument extends Schema {
  $id: string;
  title: string;
  description: string;
  $defs: Record<string, Schema>;
  "rift:entryPoints"?: Record<string, EntryPointSeam | string>;
  "rift:axes"?: { description: string; groups: AxisGroup[] };
  "rift:targetTiers"?: {
    description: string;
    rules: { entry: string; pointer: string; tier: string }[];
  };
}

/** One version's loaded document and everything derived from it. */
export interface ProtocolData {
  documents: Record<ProtocolFile, ProtocolDocument>;
  defs: Record<string, Schema>;
  defNames: string[];
  defNamesByFile: Record<ProtocolFile, string[]>;
  homeOf: Record<string, ProtocolFile>;
  axisGroups: AxisGroup[];
  backlinks: Record<string, string[]>;
}

/** `allKeys` so keywords outside the walker's own table are still visited. */
const TRAVERSE_OPTIONS = { allKeys: true } as const;

/**
 * Every schema node inside a definition, including the definition itself.
 *
 * Anything under a `rift:` keyword is left out. Those carry Rift's own
 * annotations rather than subschemas, and `allKeys` cannot tell the difference —
 * without this, `rift:enumDescriptions` reads as a schema whose keywords are the
 * enum values it documents.
 */
function nodes(root: Schema): Schema[] {
  const found: Schema[] = [];
  traverse(root as traverse.SchemaObject, TRAVERSE_OPTIONS, (node, pointer) => {
    if (
      pointer
        .split("/")
        .some((segment) => segment.startsWith("rift:") || segment === "discriminator")
    )
      return;
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
  // Pydantic emits this beside a tagged oneOf. The branch schemas carry the
  // const tags that the reference renders.
  "discriminator",
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
 * The definition a local `$ref` names.
 */
export function refName(node: Schema | undefined): string | null {
  const ref = node?.$ref;
  if (typeof ref !== "string") return null;

  const [file, pointer] = ref.split("#");
  if (file !== "" || pointer === undefined || !pointer.startsWith("/$defs/")) {
    throw new Error(`unsupported $ref target: ${ref}`);
  }
  return pointer.slice("/$defs/".length);
}

/**
 * What an enum's individual values mean, where the schema says. The cast is
 * because `rift:` keywords are Rift's own and the JSON Schema types know
 * nothing about them.
 */
export function enumDescriptions(node: Schema): Record<string, string> | undefined {
  return (node as { "rift:enumDescriptions"?: Record<string, string> })["rift:enumDescriptions"];
}

export function isLossy(root: Schema): boolean {
  return nodes(root).some((node) => Object.keys(node).some((key) => LOSSY_KEYWORDS.has(key)));
}

function load(version: DocVersion): ProtocolData {
  const document = JSON.parse(
    readFileSync(join(protocolDirFor(version), "mcp.json"), "utf8"),
  ) as ProtocolDocument;

  const documents: Record<ProtocolFile, ProtocolDocument> = {
    core: document,
    mcp: document,
  };

  const defNamesByFile = Object.fromEntries(
    PROTOCOL_FILES.map((file) => [
      file,
      Object.entries(document.$defs)
        .filter(
          ([, schema]) =>
            (schema as Schema & { "rift:package"?: string })["rift:package"] === `rift.${file}`,
        )
        .map(([name]) => name),
    ]),
  ) as Record<ProtocolFile, string[]>;

  const defNames: string[] = PROTOCOL_FILES.flatMap((file) => defNamesByFile[file]);

  // Names are unique across the two documents — the split was a partition of
  // one namespace, not a rename — so the merged map keeps one anchor each.
  const defs: Record<string, Schema> = document.$defs;
  const homeOf: Record<string, ProtocolFile> = Object.fromEntries(
    PROTOCOL_FILES.flatMap((file) => defNamesByFile[file].map((name) => [name, file] as const)),
  );

  for (const name of defNames) {
    for (const node of nodes(defs[name])) {
      for (const key of Object.keys(node)) {
        if (key.startsWith("rift:") || KNOWN_KEYWORDS.has(key)) continue;
        throw new Error(
          `unhandled JSON Schema keyword \`${key}\` in ${version} ${name}. Teach src/components/protocol to render it before it reaches the page.`,
        );
      }
    }
  }

  // Every key of a `rift:enumDescriptions` is one of the values it documents.
  // A key that is not — a typo, or a value renamed out from under it — describes
  // nothing and reaches the page as silence. Incompleteness is fine: the values
  // are documented as the docs are written, and an undescribed one still renders.
  for (const name of defNames) {
    for (const node of nodes(defs[name])) {
      const described = enumDescriptions(node);
      if (!described) continue;
      const values = new Set(
        [node, ...(branches(node)?.list ?? [])]
          .flatMap((candidate) => {
            const target = refName(candidate);
            return (target ? defs[target]?.enum : candidate.enum) ?? [];
          })
          .filter((value) => typeof value === "string" || typeof value === "number")
          .map(String),
      );
      for (const value of Object.keys(described)) {
        if (!values.has(value)) {
          throw new Error(
            `\`rift:enumDescriptions\` in ${version} ${name} describes \`${value}\`, which is not one of its enum values`,
          );
        }
      }
    }
  }

  // Every `$ref` in the document resolves to a definition that exists.
  //
  // The whole document rather than `$defs`, and a plain walk rather than the
  // schema traverser: `rift:entryPoints` is not a schema, so the traverser does
  // not reach it, and it is where the tool and resource catalogue is declared.
  // A `$ref` there naming a definition that was renamed out from under it
  // produces a page with a link to nothing, and no other check catches it.
  const walk = (node: unknown, path: string): void => {
    if (Array.isArray(node)) {
      for (const [index, value] of node.entries()) walk(value, `${path}[${index}]`);
      return;
    }
    if (!node || typeof node !== "object") return;
    const ref = (node as { $ref?: unknown }).$ref;
    if (typeof ref === "string") {
      const target = refName(node as Schema);
      if (target && !(target in defs)) {
        throw new Error(
          `${version} mcp.json ${path} refers to \`${target}\`, which is not defined`,
        );
      }
    }
    for (const [key, value] of Object.entries(node)) {
      if (key === "examples" || key === "default") continue;
      walk(value, `${path}.${key}`);
    }
  };
  walk(documents.mcp, "mcp");

  // Definition references found in schema and Rift annotation objects.
  function references(value: unknown, found = new Set<string>()): Set<string> {
    if (Array.isArray(value)) {
      for (const item of value) references(item, found);
      return found;
    }
    if (!value || typeof value !== "object") return found;

    const node = value as Record<string, unknown>;
    if (typeof node.$ref === "string") {
      const target = refName(node as Schema);
      if (target) found.add(target);
    }
    for (const [key, child] of Object.entries(node)) {
      if (key === "rift:arguments") {
        if (typeof child === "string" && child in defs) {
          found.add(child);
          continue;
        }
        if (child && typeof child === "object") {
          for (const target of Object.values(child)) {
            if (typeof target === "string" && target in defs) found.add(target);
          }
          continue;
        }
      }
      if (key === "rift:contentTypes" && child && typeof child === "object") {
        for (const target of Object.values(child)) {
          if (typeof target === "string" && target in defs) found.add(target);
        }
        continue;
      }
      if (key !== "examples" && key !== "default") references(child, found);
    }
    return found;
  }

  const definitionEdges = new Map(
    defNames.map((name) => [name, [...references(defs[name])].filter((target) => target in defs)]),
  );

  // Every definition must contribute to a seam or a declared external projection.
  const reachable = new Set<string>();
  const pending = [...references(documents.mcp["rift:entryPoints"])];
  while (pending.length > 0) {
    const name = pending.pop() as string;
    if (reachable.has(name) || !(name in defs)) continue;
    reachable.add(name);
    pending.push(...(definitionEdges.get(name) ?? []));
  }
  const unreachable = defNames.filter((name) => !reachable.has(name));
  if (unreachable.length > 0) {
    throw new Error(
      `${version} protocol definitions do not reach a seam or projection: ${unreachable.join(", ")}`,
    );
  }

  // Recursive filters are the protocol's only definition cycle. Request bytes
  // bound filter nesting. RelationFilter.max_depth bounds graph traversal, and
  // ElementFilter.where descends into one entry of a list the entity already
  // holds, so it terminates on the data rather than on a bound of its own.
  const EXPECTED_CYCLES = new Set(["ElementFilter,Filter,RelationFilter"]);
  let nextIndex = 0;
  const indices = new Map<string, number>();
  const lowlinks = new Map<string, number>();
  const componentStack: string[] = [];
  const stacked = new Set<string>();
  const cycles: string[] = [];

  function connect(name: string): void {
    indices.set(name, nextIndex);
    lowlinks.set(name, nextIndex);
    nextIndex += 1;
    componentStack.push(name);
    stacked.add(name);

    for (const target of definitionEdges.get(name) ?? []) {
      if (!indices.has(target)) {
        connect(target);
        lowlinks.set(name, Math.min(lowlinks.get(name) as number, lowlinks.get(target) as number));
      } else if (stacked.has(target)) {
        lowlinks.set(name, Math.min(lowlinks.get(name) as number, indices.get(target) as number));
      }
    }

    if (lowlinks.get(name) !== indices.get(name)) return;
    const component: string[] = [];
    let current: string;
    do {
      current = componentStack.pop() as string;
      stacked.delete(current);
      component.push(current);
    } while (current !== name);

    const selfCycle = component.length === 1 && definitionEdges.get(name)?.includes(name);
    if (component.length > 1 || selfCycle) cycles.push(component.sort().join(","));
  }

  for (const name of defNames) if (!indices.has(name)) connect(name);
  const unexpectedCycles = cycles.filter((cycle) => !EXPECTED_CYCLES.has(cycle));
  const missingCycles = [...EXPECTED_CYCLES].filter((cycle) => !cycles.includes(cycle));
  if (unexpectedCycles.length > 0 || missingCycles.length > 0) {
    throw new Error(
      `${version} protocol definition cycles changed: found [${cycles.join("; ")}], expected [${[...EXPECTED_CYCLES].join("; ")}]`,
    );
  }

  // The reference's grouping, declared by `mcp.json`. Every name it mentions is
  // checked here: a group seeded by a definition that no longer exists silently
  // files its whole subtree somewhere else, which is the kind of drift a rename
  // causes and nothing catches.
  const axisGroups: AxisGroup[] = (() => {
    const declared = documents.mcp["rift:axes"];
    if (!declared) throw new Error(`${version} mcp.json is missing \`rift:axes\``);
    for (const group of declared.groups) {
      for (const name of [...(group.identifiedBy ?? []), ...(group.holds ?? [])]) {
        if (!(name in defs)) {
          throw new Error(
            `\`rift:axes\` group \`${group.name}\` names \`${name}\`, which is not defined`,
          );
        }
      }
      if (group.residualOf && !PROTOCOL_FILES.includes(group.residualOf)) {
        throw new Error(`\`rift:axes\` group \`${group.name}\` is residual of an unknown document`);
      }
    }
    return declared.groups;
  })();

  // Which definitions refer to a given one, across both documents.
  const backlinks: Record<string, string[]> = (() => {
    const edges = new Map<string, Set<string>>(defNames.map((name) => [name, new Set<string>()]));
    for (const from of defNames) {
      for (const node of nodes(defs[from])) {
        const target = refName(node);
        if (target && target !== from) edges.get(target)?.add(from);
      }
    }
    return Object.fromEntries([...edges].map(([name, set]) => [name, [...set].sort()]));
  })();

  return { documents, defs, defNames, defNamesByFile, homeOf, axisGroups, backlinks };
}

const loaded = new Map<DocVersion, ProtocolData>();

/** One version's document, loaded and validated once per build. */
export function protocolFor(version: DocVersion): ProtocolData {
  let data = loaded.get(version);
  if (!data) {
    data = load(version);
    loaded.set(version, data);
  }
  return data;
}
