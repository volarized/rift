import { parse } from "mermaid-parser-bundle";

/**
 * Mermaid's own flowchart grammar, run at build time, normalised into the
 * handful of facts the isometric renderer needs.
 *
 * The point of authoring diagrams in mermaid is that the *graph* stays text:
 * nodes, edges and edge labels, editable in place, with no hand-placed
 * coordinates anywhere. What we do not want is mermaid's renderer — it draws a
 * flat SVG we would then have to skew, and a skewed glyph is not a glyph any
 * more. So mermaid is the input language and stops there; positions come from
 * dagre (see `iso-scene.ts`) and the drawing is ours.
 *
 * `mermaid` itself cannot be the dependency here. This module is imported by a
 * React Server Component and runs during `next build`, where there is no DOM,
 * and mermaid reaches for `DOMPurify` — and therefore `window` — before it will
 * parse a single line. `mermaid-parser-bundle` is mermaid's parsers with the
 * renderer stripped out: the same jison grammar, no d3, no DOM, no
 * dependencies. So the grammar is still mermaid's, not ours.
 */

export type FlowDirection = "TB" | "BT" | "LR" | "RL";

/** How a link is drawn. Mermaid's three stroke weights, kept as it reports them. */
export type FlowLinkStyle = "normal" | "thick" | "dotted";

export type FlowNode = {
  id: string;
  /** The label, one entry per line. */
  label: string[];
  /**
   * Mermaid classes, in declaration order — from either `a:::agent` or
   * `class a agent`. The renderer reads these to pick a node's icon, so this
   * is how a diagram says "this one is an agent".
   */
  classes: string[];
};

export type FlowEdge = {
  from: string;
  to: string;
  /** The link's label, one entry per line. Empty when it carries none. */
  label: string[];
  /** `-->` carries a head; `---` does not. */
  arrow: boolean;
  style: FlowLinkStyle;
};

/** A mermaid `subgraph`: a titled box drawn around the nodes it contains. */
export type FlowGroup = {
  id: string;
  /** The subgraph's title, one entry per line. */
  title: string[];
  /** Ids of the leaf nodes inside. Nested subgraphs are not supported. */
  nodes: string[];
};

export type FlowGraph = {
  direction: FlowDirection;
  /** In declaration order, which is also the order dagre sees them in. */
  nodes: FlowNode[];
  edges: FlowEdge[];
  groups: FlowGroup[];
};

/**
 * The slice of mermaid's flowchart database we read. The bundle deliberately
 * types `db` as `unknown` — the shape differs per diagram type — so the
 * contract is restated here rather than pulled in wholesale.
 */
type FlowchartDb = {
  getDirection: () => string | undefined;
  getVertices: () => Map<string, { text?: string; classes?: string[] }>;
  getEdges: () => {
    start: string;
    end: string;
    text?: string;
    /** `arrow_point`, `arrow_open`, `arrow_cross`, `arrow_circle`, … */
    type?: string;
    /** `normal`, `thick`, `dotted`. */
    stroke?: string;
  }[];
  getSubGraphs: () => { id: string; nodes: string[]; title?: string }[];
};

const DIRECTIONS = new Set<FlowDirection>(["TB", "BT", "LR", "RL"]);

/** Splits a mermaid label on its line break. Everything else is left alone. */
function lines(text: string | undefined): string[] {
  if (!text) return [];

  return text
    .split(/<br\s*\/?>/i)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

// Mermaid's parsers write into one database per diagram type, shared across
// `parse` calls, and the database is only read back after an await. Sibling
// Server Components parse concurrently, so without ordering one diagram's
// read can observe another diagram's parse. This queue holds each
// parse-and-read pair together, one diagram at a time.
let parseTurn: Promise<unknown> = Promise.resolve();

export function parseFlowchart(source: string): Promise<FlowGraph> {
  const turn = parseTurn.then(() => parseFlowchartNow(source));
  parseTurn = turn.catch(() => undefined);
  return turn;
}

async function parseFlowchartNow(source: string): Promise<FlowGraph> {
  const { type, db } = await parse(source);

  // `flowchart` and `graph` both land on `flowchart-v2`. Anything else parsed
  // fine but has a database this renderer cannot read.
  if (!type.startsWith("flowchart")) {
    throw new Error(`IsoDiagram expects a mermaid flowchart, got "${type}"`);
  }

  const flow = db as FlowchartDb;

  // Mermaid folds `graph TD` into `TB` itself, and leaves the direction unset
  // when the header carried none.
  const read = flow.getDirection();
  const direction = DIRECTIONS.has(read as FlowDirection) ? (read as FlowDirection) : "TB";

  // The vertex map is in declaration order, which is the order dagre will
  // break ties in, so a diagram's reading order survives the layout.
  const nodes: FlowNode[] = [...flow.getVertices()].map(([id, vertex]) => ({
    id,
    label: lines(vertex.text),
    classes: vertex.classes ?? [],
  }));

  const edges: FlowEdge[] = flow.getEdges().map((edge) => ({
    from: edge.start,
    to: edge.end,
    label: lines(edge.text),
    // Every link type mermaid has apart from `arrow_open` (`---`) terminates
    // in some kind of head; we draw them all as the one arrowhead.
    arrow: (edge.type ?? "arrow_point") !== "arrow_open",
    style: edge.stroke === "thick" || edge.stroke === "dotted" ? edge.stroke : "normal",
  }));

  // Subgraph member lists can name nested subgraphs; keeping only leaf ids is
  // what "nested subgraphs are not supported" means in practice.
  const vertexIds = new Set(nodes.map((node) => node.id));
  const groups: FlowGroup[] = flow
    .getSubGraphs()
    .map((sub) => ({
      id: sub.id,
      title: lines(sub.title ?? sub.id),
      nodes: sub.nodes.filter((id) => vertexIds.has(id)),
    }))
    .filter((group) => group.nodes.length > 0);

  return { direction, nodes, edges, groups };
}
