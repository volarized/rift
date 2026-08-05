/**
 * The MCP surface, derived from the schemas: what an agent can call, what it
 * sends, and what comes back.
 *
 * `protocol.ts` loads the documents and knows about JSON Schema. This knows
 * about Rift: that `mcp.tools` and `mcp.resources` declare their members with a
 * params and a result type, and that a resource's URI template is a const
 * inside `ResourceTemplate`. The adapter seam is protobuf and is catalogued by
 * `proto.ts` from its service definition.
 *
 * Each page also gets its own table of contents and search index here, because
 * fumadocs builds both from the MDX abstract syntax tree and these pages have
 * component bodies — see `pageData` at the bottom.
 */

import type { StructuredData } from "fumadocs-core/mdx-plugins";
import type { TableOfContents } from "fumadocs-core/server";
import { sectionId } from "@/components/protocol/adapter";
import {
  adapterOwned,
  adapterServices,
  protoMessages,
  protoSections,
  scipEnums,
  scipMessages,
  scipServices,
} from "@/lib/proto";
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

/** Resolve one local definition reference and leave inline schemas unchanged. */
function shape(node: unknown): Schema | undefined {
  const direct = sub(node as Schema | undefined);
  const target = refName(direct);
  return target ? defs[target] : direct;
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
    const resolved = shape(branch);
    if (!resolved) continue;
    const properties = Object.fromEntries(props(resolved));
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
  const uriTemplate = uriTemplateFor(name);
  if (!uriTemplate) throw new Error(`resource \`${name}\` is missing a ResourceTemplate branch`);
  return { name, description: member.description, uriTemplate, uri, link };
});

function branchNames(name: string): string[] {
  return (defs[name]?.oneOf ?? []).flatMap((branch) => {
    const resolved = shape(branch);
    const value = resolved ? Object.fromEntries(props(resolved)).name?.const : undefined;
    return typeof value === "string" ? [value] : [];
  });
}

function assertSameFamilies(label: string, actual: string[]): void {
  const expected = mcpResources.map((resource) => resource.name).sort();
  const found = [...new Set(actual)].sort();
  if (expected.join("\0") !== found.join("\0")) {
    throw new Error(
      `${label} resource families are [${found.join(", ")}], expected [${expected.join(", ")}]`,
    );
  }
}

assertSameFamilies("ResourceTemplate", branchNames("ResourceTemplate"));
assertSameFamilies("ResourceLink", branchNames("ResourceLink"));

const repositoryProperties = Object.fromEntries(props(defs.RepositoryResourcePayload));
const advertisedResources =
  shape(repositoryProperties.resources?.items)?.enum?.filter(
    (value): value is string => typeof value === "string",
  ) ?? [];
assertSameFamilies("RepositoryResourcePayload.resources", advertisedResources);

const readContents = Object.fromEntries(props(defs.ResourceReadResult)).contents;
const readItems = shape(readContents?.items);
const readFamilies = (readItems?.oneOf ?? []).flatMap((branch) => {
  const resolved = shape(branch);
  if (!resolved) return [];
  const mime = Object.fromEntries(props(resolved)).mimeType?.const;
  const match =
    typeof mime === "string" ? /^application\/vnd\.rift\.([a-z_]+)\+json$/.exec(mime) : null;
  return match ? [match[1]] : [];
});
assertSameFamilies("ResourceReadResult", readFamilies);

const advertisedTools =
  shape(repositoryProperties.tools?.items)?.enum?.filter(
    (value): value is string => typeof value === "string",
  ) ?? [];
const toolNames = mcpTools.map((tool) => tool.name).sort();
if (toolNames.join("\0") !== [...advertisedTools].sort().join("\0")) {
  throw new Error(
    `RepositoryResourcePayload.tools are [${advertisedTools.join(", ")}], expected [${toolNames.join(", ")}]`,
  );
}

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
 * its own — if the grouping looks wrong, `mcp.json` is where it is wrong.
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

const unassignedDefinitions = defNames.filter((name) => !axisOf[name]);
if (unassignedDefinitions.length > 0) {
  throw new Error(`protocol definitions have no axis: ${unassignedDefinitions.join(", ")}`);
}

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
          if (inbound.has(target) && !placed.has(target)) children.push(build(target, depth + 1));
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

/**
 * The adapter page is protobuf, so its outline comes from the service and the
 * messages the adapter file declares — not from `defs`, which holds only the
 * JSON Schema half.
 */
function adapterPage(): PageData {
  const service = adapterServices[0];
  // The file's own section banners decide the grouping, so a message that moves
  // between sections moves on the page without anything here changing.
  const grouped = protoSections.flatMap((section) => [
    { name: section.name, id: sectionId(section.name), depth: 2 },
    ...section.types.map((name) => ({ name, id: `msg-${name}`, depth: 3 })),
  ]);

  // `Transport` and `What crosses this seam` are headings in the MDX, so remark
  // already produced them. Only what the component renders belongs here.
  return {
    toc: [
      { title: "Service", url: "#service", depth: 2 },
      ...(service?.rpcs ?? []).map((rpc) => ({
        title: rpc.name,
        url: `#rpc-${rpc.name}`,
        depth: 3,
      })),
      ...grouped.map((entry) => ({ title: entry.name, url: `#${entry.id}`, depth: entry.depth })),
    ],
    structuredData: {
      headings: [
        { id: "service", content: "Service" },
        ...(service?.rpcs ?? []).map((rpc) => ({ id: `rpc-${rpc.name}`, content: rpc.name })),
        ...grouped.map((entry) => ({ id: entry.id, content: entry.name })),
      ],
      contents: [
        ...(service?.rpcs ?? []).flatMap((rpc) =>
          rpc.comment ? [{ heading: `rpc-${rpc.name}`, content: rpc.comment }] : [],
        ),
        ...protoMessages
          .filter((message) => adapterOwned.has(message.name))
          .flatMap((message) => [
            ...(message.comment
              ? [{ heading: `msg-${message.name}`, content: message.comment }]
              : []),
            ...message.fields.flatMap((field) =>
              field.comment
                ? [{ heading: `msg-${message.name}`, content: `${field.name}: ${field.comment}` }]
                : [],
            ),
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

function scipPage(): PageData {
  const declarations = [...scipMessages, ...scipEnums];
  const rpcs = scipServices.flatMap((service) => service.rpcs);
  return {
    toc: [
      ...rpcs.map((rpc) => ({
        title: rpc.name,
        url: `#scip-rpc-${rpc.name}`,
        depth: 3,
      })),
      ...declarations.map((value) => ({
        title: value.name,
        url: `#scip-${value.name}`,
        depth: 3,
      })),
    ],
    structuredData: {
      headings: [
        ...rpcs.map((rpc) => ({ id: `scip-rpc-${rpc.name}`, content: rpc.name })),
        ...declarations.map((value) => ({ id: `scip-${value.name}`, content: value.name })),
      ],
      contents: [
        ...rpcs.flatMap((rpc) =>
          rpc.comment ? [{ heading: `scip-rpc-${rpc.name}`, content: rpc.comment }] : [],
        ),
        ...declarations.flatMap((value) => [
          { heading: `scip-${value.name}`, content: value.name },
          ...(value.comment
            ? [{ heading: `scip-${value.name}`, content: value.comment }]
            : []),
        ]),
      ],
    },
  };
}

/** Page data by URL, for the docs page and the search route to look up. */
export const pageData: Record<string, PageData> = {
  [pageUrl("mcp")]: mcpPage(),
  [`${PROTOCOL_ROOT}/scip`]: scipPage(),
  [`${PROTOCOL_ROOT}/adapter`]: adapterPage(),
  // No entry for `core`: it is plain prose, so remark already sees every
  // heading it has. Its types are anchored on the reference page.
  [`${PROTOCOL_ROOT}/reference`]: referencePage(),
};
