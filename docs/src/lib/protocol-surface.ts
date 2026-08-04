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
  defNames,
  defNamesByFile,
  defs,
  documents,
  homeOf,
  PROTOCOL_FILES,
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
// Which definitions the surface already anchors
// ---------------------------------------------------------------------------

/**
 * A definition gets exactly one heading. Types shown under a tool, resource, or
 * operation are anchored there; everything else falls to that page's reference
 * section. `ResourceTemplate` is deliberately not claimed by any one resource —
 * all five share it.
 */
const anchoredBySurface = new Set<string>([
  ...mcpTools.flatMap((tool) => [tool.params, tool.result]),
  ...mcpResources.map((resource) => resource.uri),
  ADAPTER_HELLO,
  ...adapterOperations.flatMap((operation) => [
    operation.request.name,
    ...operation.responses.map((response) => response.name),
  ]),
]);

/** The definitions a page lists in its reference section, in document order. */
export const referenceTypes = Object.fromEntries(
  PROTOCOL_FILES.map((file) => [
    file,
    defNamesByFile[file].filter((name) => !anchoredBySurface.has(name)),
  ]),
) as Record<ProtocolFile, string[]>;

// ---------------------------------------------------------------------------
// The reference, grouped by axis and nested by reference
// ---------------------------------------------------------------------------

/**
 * Which axis a type belongs to.
 *
 * Seeded from the identifiers, then closed over what each seed reaches. Order
 * matters: a type reachable from more than one axis is filed under the first
 * that claims it, and the axes are tried narrowest-first. What no axis reaches
 * is wire machinery — cursors, coverage, error shapes, adapter frames — and is
 * grouped as such rather than filed somewhere it does not belong.
 */
const AXIS_SEEDS: [string, string[]][] = [
  [
    "Temporal",
    ["GitRevision", "GitOid", "SnapshotRef", "RevisionRef", "SemanticSnapshotDigest", "Timestamp"],
  ],
  [
    "Physical",
    [
      "FileId",
      "File",
      "Leaf",
      "LeafId",
      "TextRange",
      "ProjectPath",
      "SourceSpan",
      "PathSelector",
      "LeafFacet",
      "LeafRegion",
    ],
  ],
  [
    "Semantic",
    [
      "SymbolId",
      "Symbol",
      "Relationship",
      "Signature",
      "TypeExpression",
      "Documentation",
      "ExactKind",
      "SymbolFacet",
      "SymbolOrigin",
      "LanguageId",
    ],
  ],
];

export const AXES = [...AXIS_SEEDS.map(([name]) => name), "Protocol", "MCP", "Adapter"] as const;

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

export const axisOf: Record<string, string> = (() => {
  const assigned: Record<string, string> = {};
  for (const [axis, seeds] of AXIS_SEEDS) {
    const stack = seeds.filter((s) => s in defs);
    while (stack.length > 0) {
      const name = stack.pop() as string;
      if (assigned[name]) continue;
      assigned[name] = axis;
      stack.push(...outgoing(name));
    }
  }
  // What no axis reaches is the wire itself. Grouping that by the document
  // that defines it says more than one bucket of everything left over.
  const BY_HOME: Record<string, string> = { core: "Protocol", mcp: "MCP", adapter: "Adapter" };
  for (const name of defNames) assigned[name] ??= BY_HOME[homeOf[name]] ?? "Protocol";
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

function referenceSection(_file: ProtocolFile) {
  // Types moved to their own page; a surface page carries only its surface.
  return { toc: [], headings: [], contents: [] };
}

function mcpPage(): PageData {
  const reference = referenceSection("mcp");
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
      ...reference.toc,
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
        ...reference.headings,
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
        ...reference.contents,
      ],
    },
  };
}

function adapterPage(): PageData {
  const reference = referenceSection("adapter");
  return {
    toc: [
      { title: "Handshake", url: "#handshake", depth: 2 },
      { title: "Operations", url: "#operations", depth: 2 },
      ...adapterOperations.map((operation) => ({
        title: operation.op,
        url: `#op-${operation.op}`,
        depth: 3,
      })),
      ...reference.toc,
    ],
    structuredData: {
      headings: [
        { id: "handshake", content: "Handshake" },
        { id: "operations", content: "Operations" },
        ...adapterOperations.map((operation) => ({
          id: `op-${operation.op}`,
          content: operation.op,
        })),
        ...reference.headings,
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
        ...reference.contents,
      ],
    },
  };
}

function corePage(): PageData {
  const names = defNamesByFile.core;
  return {
    toc: [
      { title: "Type reference", url: "#type-reference", depth: 2 },
      ...names.map((name) => ({ title: name, url: `#${name}`, depth: 3 })),
    ],
    structuredData: {
      headings: [
        { id: "type-reference", content: "Type reference" },
        ...names.map((name) => ({ id: name, content: name })),
      ],
      contents: [
        { heading: undefined, content: documents.core.description },
        ...names.flatMap(prose),
      ],
    },
  };
}

function referencePage(): PageData {
  const outline = AXES.map((axis) => ({
    axis,
    names: (() => {
      const out: string[] = [];
      const walk = (nodes: ReferenceNode[]) => {
        for (const n of nodes) {
          out.push(n.name);
          walk(n.children);
        }
      };
      walk(referenceTree[axis] ?? []);
      return out;
    })(),
  })).filter((e) => e.names.length > 0);

  return {
    toc: outline.flatMap(({ axis, names }) => [
      { title: axis, url: `#axis-${axis.toLowerCase()}`, depth: 2 },
      ...names.map((name) => ({ title: name, url: `#${name}`, depth: 3 })),
    ]),
    structuredData: {
      headings: outline.flatMap(({ axis, names }) => [
        { id: `axis-${axis.toLowerCase()}`, content: axis },
        ...names.map((name) => ({ id: name, content: name })),
      ]),
      contents: outline.flatMap(({ names }) => names.flatMap(prose)),
    },
  };
}

/** Page data by URL, for the docs page and the search route to look up. */
export const pageData: Record<string, PageData> = {
  [pageUrl("mcp")]: mcpPage(),
  [pageUrl("adapter")]: adapterPage(),
  [pageUrl("core")]: corePage(),
  [`${PROTOCOL_ROOT}/reference`]: referencePage(),
};
