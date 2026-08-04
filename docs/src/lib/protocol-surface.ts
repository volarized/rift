/**
 * The protocol's *surface*, derived from the schemas: what you can call, what
 * you send, and what comes back.
 *
 * `protocol.ts` loads the documents and knows about JSON Schema. This knows
 * about Rift: that `mcp.tools` has five members each with a params and a result
 * type, that every adapter request frame carries the `op` it belongs to as a
 * const, and that streaming operations answer with a chunk frame and an end
 * frame. Pairing operations by their `op` const rather than by name means the
 * catalogue cannot drift from the wire.
 *
 * Each page also gets its own table of contents and search index here, because
 * fumadocs builds both from the MDX abstract syntax tree and these pages have
 * component bodies — see `pageData` at the bottom.
 */

import type { StructuredData } from "fumadocs-core/mdx-plugins";
import type { TableOfContents } from "fumadocs-core/server";
import {
  axisGroups,
  defNames,
  defNamesByFile,
  defs,
  documents,
  type ProtocolFile,
  props,
  refName,
  type Schema,
  sub,
} from "@/lib/protocol";

const BASE_PATH = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export const PROTOCOL_ROOT = "/docs/protocol";

/** Which page renders a definition's heading. One anchor per definition, everywhere. */
export function hrefFor(name: string): string {
  return `${BASE_PATH}${PROTOCOL_ROOT}/reference/#${name}`;
}

export function pageUrl(file: ProtocolFile): string {
  return `${PROTOCOL_ROOT}/${file}`;
}

// ---------------------------------------------------------------------------
// MCP surface
// ---------------------------------------------------------------------------

interface SeamMember {
  description?: string;
  [role: string]: unknown;
}

function seam(file: ProtocolFile, name: string): [string, SeamMember][] {
  const entries = documents[file]["rift:entryPoints"]?.[name];
  if (!entries || typeof entries !== "object") return [];
  return Object.entries(entries).filter(
    (entry): entry is [string, SeamMember] =>
      entry[0] !== "description" && typeof entry[1] === "object" && entry[1] !== null,
  );
}

/** The definition a seam member points at for one role, e.g. `search`'s `params`. */
function role(member: SeamMember, key: string): string | null {
  return refName(sub(member[key] as Schema | undefined));
}

export interface ToolEntry {
  name: string;
  description?: string;
  params: string;
  result: string;
}

export const mcpTools: ToolEntry[] = seam("mcp", "mcp.tools").map(([name, member]) => {
  const params = role(member, "params");
  const result = role(member, "result");
  if (!params || !result) throw new Error(`tool \`${name}\` is missing a params or result type`);
  return { name, description: member.description, params, result };
});

export interface ResourceEntry {
  name: string;
  description?: string;
  uriTemplate?: string;
  uri: string;
  link: string;
}

/**
 * Resource URI templates live as consts inside `ResourceTemplate`'s union, one
 * branch per resource, tagged by the same `name` the seam uses.
 */
function uriTemplateFor(resource: string): string | undefined {
  for (const branch of defs.ResourceTemplate?.oneOf ?? []) {
    const shape = sub(branch);
    if (!shape) continue;
    const properties = Object.fromEntries(props(shape));
    if (properties.name?.const === resource) {
      const template = properties.uriTemplate?.const;
      return typeof template === "string" ? template : undefined;
    }
  }
  return undefined;
}

export const mcpResources: ResourceEntry[] = seam("mcp", "mcp.resources").map(([name, member]) => {
  const uri = role(member, "uri");
  const link = role(member, "link");
  if (!uri || !link) throw new Error(`resource \`${name}\` is missing a uri or link type`);
  return { name, description: member.description, uriTemplate: uriTemplateFor(name), uri, link };
});

// ---------------------------------------------------------------------------
// Adapter surface
// ---------------------------------------------------------------------------

/** The value of a const-valued property, if the schema pins one. */
function constOf(schema: Schema, property: string): string | undefined {
  const value = Object.fromEntries(props(schema))[property]?.const;
  return typeof value === "string" ? value : undefined;
}

export interface AdapterFrame {
  /** The definition name, which is also its anchor. */
  name: string;
  /** The fully qualified `type` tag the frame carries on the wire. */
  tag: string;
}

export interface AdapterOperation {
  op: string;
  description?: string;
  request: AdapterFrame;
  responses: AdapterFrame[];
}

export const adapterOperations: AdapterOperation[] = (() => {
  const requests = new Map<string, { frame: AdapterFrame; description?: string }>();
  const responses = new Map<string, AdapterFrame[]>();

  for (const name of defNamesByFile.adapter) {
    const schema = defs[name];
    const op = constOf(schema, "op");
    const tag = constOf(schema, "type");
    if (!op || !tag) continue;

    if (tag.startsWith("request.")) {
      requests.set(op, { frame: { name, tag }, description: schema.description });
    } else if (tag.startsWith("response.")) {
      responses.set(op, [...(responses.get(op) ?? []), { name, tag }]);
    }
  }

  // `AdapterOp` and the request frames are two lists of the same thing, and
  // they drifted: five operations outlived the frames that sent them. Whichever
  // side is edited, the other has to follow.
  const declared = new Set(defs.AdapterOp?.enum ?? []);
  for (const op of requests.keys()) {
    if (!declared.has(op)) throw new Error(`\`${op}\` has a request frame but is not in AdapterOp`);
  }
  for (const op of declared) {
    if (!requests.has(op)) throw new Error(`AdapterOp names \`${op}\`, which has no request frame`);
  }

  return [...requests.entries()]
    .map(([op, { frame, description }]) => ({
      op,
      description,
      request: frame,
      // chunk before end, so a streaming operation reads in wire order
      responses: (responses.get(op) ?? []).sort((a, b) => a.tag.localeCompare(b.tag)),
    }))
    .sort((a, b) => a.op.localeCompare(b.op));
})();

/** `hello` is a frame rather than an operation: no request, no `op`, sent once. */
export const ADAPTER_HELLO = "AdapterHello";

// ---------------------------------------------------------------------------
// The reference, grouped by axis and nested by reference
// ---------------------------------------------------------------------------

export const AXES: string[] = axisGroups.map((group) => group.name);

/** A group's one-line summary, for the page to print under its heading. */
export const axisSummary: Record<string, string> = Object.fromEntries(
  axisGroups.map((group) => [group.name, group.summary]),
);

function outgoing(name: string): string[] {
  const schema = defs[name];
  if (!schema) return [];
  const out = new Set<string>();
  const walk = (node: unknown): void => {
    if (Array.isArray(node)) {
      for (const value of node) walk(value);
      return;
    }
    if (!node || typeof node !== "object") return;
    const target = refName(node as Schema);
    if (target) out.add(target);
    for (const value of Object.values(node)) walk(value);
  };
  walk(schema);
  out.delete(name);
  return [...out];
}

/**
 * Which group a definition belongs to, run exactly as `rift:axes` describes it:
 * pin what is held, place every identifying definition, close each group over
 * what its own reach, then hand the remainder to the group that claims its
 * document. This function is the declaration's interpreter and holds no list of
 * its own — if the grouping looks wrong, `core.json` is where it is wrong.
 */
export const axisOf: Record<string, string> = (() => {
  const assigned: Record<string, string> = {};

  for (const group of axisGroups) {
    for (const name of group.holds ?? []) assigned[name] = group.name;
  }
  // Before any closure, so a group cannot claim another's identifier on the way
  // past: `Leaf` reaches `SymbolId`, and the identity of a symbol is semantic.
  for (const group of axisGroups) {
    for (const name of group.identifiedBy ?? []) assigned[name] ??= group.name;
  }
  for (const group of axisGroups) {
    const stack = [...(group.identifiedBy ?? [])];
    while (stack.length > 0) {
      const name = stack.pop() as string;
      for (const target of outgoing(name)) {
        if (assigned[target]) continue;
        assigned[target] = group.name;
        stack.push(target);
      }
    }
  }
  for (const group of axisGroups) {
    if (!group.residualOf) continue;
    for (const name of defNamesByFile[group.residualOf]) assigned[name] ??= group.name;
  }
  return assigned;
})();

export interface ReferenceNode {
  name: string;
  children: ReferenceNode[];
}

/**
 * One tree per axis. A type hangs under the first type that references it, so
 * reading down a branch follows the model; a flat list of 220 names does not
 * say which type reaches which.
 */
export const referenceTree: Record<string, ReferenceNode[]> = (() => {
  const trees: Record<string, ReferenceNode[]> = {};
  for (const axis of AXES) {
    const members = defNames.filter((n) => axisOf[n] === axis);
    const inbound = new Map<string, number>(members.map((m) => [m, 0]));
    for (const name of members) {
      for (const target of outgoing(name)) {
        if (inbound.has(target) && target !== name)
          inbound.set(target, (inbound.get(target) ?? 0) + 1);
      }
    }
    const placed = new Set<string>();
    const build = (name: string, depth: number): ReferenceNode => {
      placed.add(name);
      // Checked inside the loop, not by filtering first: two siblings can both
      // reference the same type, and a filter that ran to completion would let
      // each of them claim it.
      const children: ReferenceNode[] = [];
      if (depth < 3) {
        for (const target of outgoing(name)) {
          if (axisOf[target] === axis && !placed.has(target))
            children.push(build(target, depth + 1));
        }
      }
      return { name, children };
    };

    // Roots first, then whatever a root never reached — a cycle has no member
    // with no inbound edge, so without the second pass those types vanish.
    // Built one at a time: `placed` has to be current when the next root is
    // considered, or every type is chosen as a root before any nesting happens.
    const roots = members.filter((m) => (inbound.get(m) ?? 0) === 0);
    const order = [...roots, ...members.filter((m) => !roots.includes(m))];
    const built: ReferenceNode[] = [];
    for (const name of order) {
      if (!placed.has(name)) built.push(build(name, 0));
    }
    trees[axis] = built;
  }
  return trees;
})();

// ---------------------------------------------------------------------------
// Per-page table of contents and search index
// ---------------------------------------------------------------------------

interface PageData {
  toc: TableOfContents;
  structuredData: StructuredData;
}

function prose(name: string): { heading: string; content: string }[] {
  const schema = defs[name];
  const parts: string[] = [];
  if (schema.description) parts.push(schema.description);
  for (const [property, value] of props(schema)) {
    if (value.description) parts.push(`${property}: ${value.description}`);
  }
  return parts.map((content) => ({ heading: name, content }));
}

function mcpPage(): PageData {
  return {
    toc: [
      { title: "Tools", url: "#tools", depth: 2 },
      ...mcpTools.map((tool) => ({ title: tool.name, url: `#tool-${tool.name}`, depth: 3 })),
      { title: "Resources", url: "#resources", depth: 2 },
      ...mcpResources.map((resource) => ({
        title: resource.name,
        url: `#resource-${resource.name}`,
        depth: 3,
      })),
    ],
    structuredData: {
      headings: [
        { id: "tools", content: "Tools" },
        ...mcpTools.map((tool) => ({ id: `tool-${tool.name}`, content: tool.name })),
        { id: "resources", content: "Resources" },
        ...mcpResources.map((resource) => ({
          id: `resource-${resource.name}`,
          content: resource.name,
        })),
      ],
      contents: [
        ...mcpTools.flatMap((tool) => [
          ...(tool.description
            ? [{ heading: `tool-${tool.name}`, content: tool.description }]
            : []),
          ...prose(tool.params),
          ...prose(tool.result),
        ]),
        ...mcpResources.flatMap((resource) => [
          ...(resource.description
            ? [{ heading: `resource-${resource.name}`, content: resource.description }]
            : []),
          ...prose(resource.uri),
        ]),
      ],
    },
  };
}

function adapterPage(): PageData {
  return {
    toc: [
      { title: "Handshake", url: "#handshake", depth: 2 },
      { title: "Operations", url: "#operations", depth: 2 },
      ...adapterOperations.map((operation) => ({
        title: operation.op,
        url: `#op-${operation.op}`,
        depth: 3,
      })),
    ],
    structuredData: {
      headings: [
        { id: "handshake", content: "Handshake" },
        { id: "operations", content: "Operations" },
        ...adapterOperations.map((operation) => ({
          id: `op-${operation.op}`,
          content: operation.op,
        })),
      ],
      contents: [
        ...prose(ADAPTER_HELLO),
        ...adapterOperations.flatMap((operation) => [
          ...(operation.description
            ? [{ heading: `op-${operation.op}`, content: operation.description }]
            : []),
          ...prose(operation.request.name),
          ...operation.responses.flatMap((response) => prose(response.name)),
        ]),
      ],
    },
  };
}

/**
 * The reference tree, flattened depth-first with each name's depth kept.
 *
 * The nesting is the table of contents, not page furniture: an outline printed
 * above two hundred definitions is a second copy of the sidebar that scrolls
 * away. Axes sit at depth 2, a root type at 3, and each level below it one
 * deeper — `build` stops at three levels, so nothing exceeds depth 6.
 */
function outlineOf(axis: string): { name: string; depth: number }[] {
  const out: { name: string; depth: number }[] = [];
  const walk = (nodes: ReferenceNode[], depth: number) => {
    for (const node of nodes) {
      out.push({ name: node.name, depth });
      walk(node.children, depth + 1);
    }
  };
  walk(referenceTree[axis] ?? [], 3);
  return out;
}

function referencePage(): PageData {
  const outline = AXES.map((axis) => ({ axis, entries: outlineOf(axis) })).filter(
    (entry) => entry.entries.length > 0,
  );

  return {
    toc: outline.flatMap(({ axis, entries }) => [
      { title: axis, url: `#axis-${axis.toLowerCase()}`, depth: 2 },
      ...entries.map(({ name, depth }) => ({ title: name, url: `#${name}`, depth })),
    ]),
    structuredData: {
      headings: outline.flatMap(({ axis, entries }) => [
        { id: `axis-${axis.toLowerCase()}`, content: axis },
        ...entries.map(({ name }) => ({ id: name, content: name })),
      ]),
      contents: outline.flatMap(({ entries }) => entries.flatMap(({ name }) => prose(name))),
    },
  };
}

/** Page data by URL, for the docs page and the search route to look up. */
export const pageData: Record<string, PageData> = {
  [pageUrl("mcp")]: mcpPage(),
  [pageUrl("adapter")]: adapterPage(),
  // No entry for `core`: it is prose and nothing else, so remark already sees
  // every heading it has. Its types are anchored on the reference page.
  [`${PROTOCOL_ROOT}/reference`]: referencePage(),
};
