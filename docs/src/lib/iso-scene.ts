import { type EdgeLabel, Graph, type GraphLabel, layout, type NodeLabel } from "@dagrejs/dagre";

import type { FlowGraph, FlowLinkStyle } from "@/lib/mermaid-flowchart";

/**
 * Turns a parsed mermaid flowchart into a scene laid out on a ground plane.
 *
 * Two halves, and the split is the whole idea.
 *
 *   1. *Where things go* is dagre's problem — the same layered layout mermaid
 *      itself runs for a flowchart. It works on a plane and knows nothing about
 *      how that plane is going to be looked at.
 *   2. *What things look like* belongs to the renderer, which stands the plane
 *      up in three.js under an orthographic camera on the isometric diagonal.
 *
 * Nothing here projects anything. The scene comes out flat and the camera does
 * the rest — which is what lets the renderer lay live DOM into the same plane
 * as the geometry and have the two agree exactly.
 *
 * Sizes are in CSS pixels, because every size in the diagram is ultimately set
 * by a font size and a label's advance width is only knowable in pixels. The
 * renderer maps pixels to world units with a single constant.
 *
 * `@dagrejs/dagre` is a devDependency on purpose: this module is imported by a
 * React Server Component, so it runs once during the build and the layout is
 * baked into the page. Nothing here reaches the browser.
 */

/**
 * Geist Mono's advance width, as a fraction of the font size. Every label in
 * the diagram is set in it, so one constant is enough to size a plate without
 * a DOM to measure in.
 */
const ADVANCE = 0.6;

/** Line pitch for a multi-line label, as a fraction of the font size. */
const LEADING = 1.45;

export type IsoMetrics = {
  /** Node label size, in CSS pixels. Everything else is set against it. */
  label: number;
  /** Edge label size. */
  note: number;
  /** Side of the square box an icon is drawn in. */
  icon: number;
  /** Space between a node's icon and its label. */
  gutter: number;
  /** Clearance between a caption and the edge it sits inside. */
  margin: number;
  /**
   * The least depth a plate may have, across the flow.
   *
   * A caption is usually one line, so sizing a plate to it alone would give a
   * ribbon — and a ribbon seen at 30° is a sliver. This is what keeps a plate
   * reading as a slab.
   */
  minDepth: number;
  /**
   * How far a plate stands off the floor. Zero draws it flat.
   *
   * Height buys nothing here: a plate is read from its drawn edges and its
   * caption, and the extra two faces only add ink and z-order to reason about.
   */
  thickness: number;
  /**
   * Dagre's separation between ranks, and within one.
   *
   * These want to be *larger* than they would be in a flat diagram. A gap of
   * `n` along a ground axis projects to `0.866n` across the page and `0.5n`
   * down it, so separation that reads as generous on the plane reads as
   * touching once the plane is turned away from the eye.
   */
  rankSep: number;
  nodeSep: number;
  /**
   * Pitch of the ruled floor. Zero draws no floor.
   *
   * The floor itself is the renderer's: it is infinite and follows the camera,
   * so the only thing the layout has an opinion about is how fine it is.
   */
  floor: number;
  /** Corner radius on a connector's bends. */
  bend: number;
  /** Arrowhead length. */
  head: number;
};

export const DEFAULT_METRICS: IsoMetrics = {
  label: 13,
  note: 11,
  icon: 15,
  gutter: 7,
  margin: 15,
  minDepth: 60,
  thickness: 0,
  rankSep: 74,
  nodeSep: 54,
  floor: 24,
  bend: 14,
  head: 13,
};

/** A point on the ground plane, in CSS pixels. */
export type Ground = [x: number, z: number];

export type IsoPlate = {
  id: string;
  label: string[];
  /** Which mark the renderer should set on the plate, or null for none. */
  mark: string | null;
  /** Centre of the footprint. */
  x: number;
  z: number;
  /** Footprint, along the flow and across it. */
  width: number;
  depth: number;
};

export type IsoConnector = {
  key: string;
  /** The run, from one plate's edge to the next, with its bends rounded. */
  points: Ground[];
  /** The arrowhead, as a triangle lying in the plane. Null for a bare link. */
  head: Ground[] | null;
  style: FlowLinkStyle;
  label: string[];
  labelAt: Ground | null;
};

export type IsoScene = {
  plates: IsoPlate[];
  connectors: IsoConnector[];
  /** Everything's extent, so the camera can frame it. */
  bounds: { x0: number; x1: number; z0: number; z1: number };
  metrics: IsoMetrics;
};

/**
 * The isometric basis, as scale factors rather than vectors.
 *
 * An orthographic camera on the (1,1,1) diagonal projects a ground-plane point
 * to `RIGHT·(x − z)` across the page and `−RISE·(x + z)` down it, and a height
 * `y` to `LIFT·y` up it. That is the whole of the projection, and the only
 * reason this module knows it is `frameSpan` — a caller has to reserve the
 * right amount of page before three.js has drawn anything.
 */
const RIGHT = Math.SQRT1_2;
const RISE = 1 / Math.sqrt(6);
const LIFT = Math.sqrt(2 / 3);

/**
 * How much page the scene needs, in CSS pixels, before it is scaled to fit.
 * Only the *ratio* matters to a caller — it fixes the box the drawing sits in
 * so the page does not reflow when the canvas comes up.
 */
export function frameSpan(scene: IsoScene): { width: number; height: number } {
  const span = scene.bounds.x1 - scene.bounds.x0 + (scene.bounds.z1 - scene.bounds.z0);
  return { width: RIGHT * span, height: RISE * span + LIFT * scene.metrics.thickness };
}

/**
 * Label metrics without a DOM to measure in. Every label in the diagram is set
 * in one monospaced family, so its advance width is a constant — which is the
 * whole reason the design's display face can be relied on here.
 */
function textWidth(lines: string[], size: number): number {
  return lines.reduce((widest, line) => Math.max(widest, line.length), 0) * size * ADVANCE;
}

function textHeight(lines: string[], size: number): number {
  return lines.length === 0 ? 0 : size + (lines.length - 1) * size * LEADING;
}

/**
 * The caption's own footprint, icon included. Because the caption ends up lying
 * *in* the ground plane rather than upright over it, this is simply a rectangle
 * — the plate is sized to it directly, with no allowance for a projection to
 * eat into. That is most of why the plates are so much tighter than they were
 * when the text stood up.
 */
function captionSize(label: string[], hasMark: boolean, metrics: IsoMetrics) {
  return {
    width: (hasMark ? metrics.icon + metrics.gutter : 0) + textWidth(label, metrics.label),
    height: Math.max(hasMark ? metrics.icon : 0, textHeight(label, metrics.label)),
  };
}

/** Steps from `from` towards `to`, by at most `distance` or half the run. */
function trim(from: Ground, to: Ground, distance: number): Ground {
  const dx = to[0] - from[0];
  const dz = to[1] - from[1];
  const length = Math.hypot(dx, dz) || 1;
  const step = Math.min(distance, length / 2) / length;
  return [from[0] + dx * step, from[1] + dz * step];
}

/** Points sampled across each rounded corner. */
const CORNER_STEPS = 6;

/**
 * Rounds the bends of a polyline, as a denser polyline. Dagre routes an edge as
 * a chain of straight runs, and drawn as-is the corners are hard enough to read
 * as mistakes next to the slabs' drawn edges. Sampled rather than left as
 * curves because the renderer draws a line through points, not a path.
 */
function round(points: Ground[], radius: number): Ground[] {
  if (points.length < 3) return points;

  const out: Ground[] = [points[0]];

  for (let i = 1; i < points.length - 1; i++) {
    const corner = points[i];
    const back = trim(corner, points[i - 1], radius);
    const forward = trim(corner, points[i + 1], radius);

    out.push(back);
    for (let step = 1; step <= CORNER_STEPS; step++) {
      const t = step / (CORNER_STEPS + 1);
      const a = (1 - t) * (1 - t);
      const b = 2 * (1 - t) * t;
      const c = t * t;
      out.push([
        a * back[0] + b * corner[0] + c * forward[0],
        a * back[1] + b * corner[1] + c * forward[1],
      ]);
    }
    out.push(forward);
  }

  out.push(points[points.length - 1]);
  return out;
}

/** A flat triangle in the ground plane, pointing along the link's last run. */
function arrowhead(tail: Ground, tip: Ground, metrics: IsoMetrics): Ground[] {
  const dx = tip[0] - tail[0];
  const dz = tip[1] - tail[1];
  const length = Math.hypot(dx, dz) || 1;
  const ux = dx / length;
  const uz = dz / length;

  const baseX = tip[0] - ux * metrics.head;
  const baseZ = tip[1] - uz * metrics.head;
  const half = metrics.head * 0.44;

  return [
    [tip[0], tip[1]],
    [baseX - uz * half, baseZ + ux * half],
    [baseX + uz * half, baseZ - ux * half],
  ];
}

/** A plate's footprint on the ground plane. */
type Tile = { x0: number; x1: number; z0: number; z1: number };

/**
 * @param markFor The mark a node carries, or null. The scene has to know before
 *   it can size the plate, and only the renderer holds the registry of what a
 *   mark actually looks like — so it is asked for the name alone. A name is
 *   also all that will survive the trip to a client component.
 */
export function buildScene(
  flow: FlowGraph,
  markFor: (node: { id: string; classes: string[] }) => string | null,
  overrides: Partial<IsoMetrics> = {},
): IsoScene {
  const metrics = { ...DEFAULT_METRICS, ...overrides };

  const graph = new Graph<GraphLabel, NodeLabel, EdgeLabel>({ multigraph: true });
  graph.setGraph({
    rankdir: flow.direction,
    ranksep: metrics.rankSep,
    nodesep: metrics.nodeSep,
    marginx: 0,
    marginy: 0,
  });
  graph.setDefaultEdgeLabel(() => ({}));

  // A node's label and mark after the fallback, kept for the second pass below
  // — dagre is told sizes, not content, and hands back only coordinates.
  const content = new Map<string, { label: string[]; mark: string | null }>();

  for (const node of flow.nodes) {
    const label = node.label.length > 0 ? node.label : [node.id];
    const mark = markFor(node);
    content.set(node.id, { label, mark });

    const caption = captionSize(label, mark !== null, metrics);
    graph.setNode(node.id, {
      width: caption.width + metrics.margin * 2,
      height: Math.max(metrics.minDepth, caption.height + metrics.margin * 2),
    });
  }

  flow.edges.forEach((edge, index) => {
    // Dagre reserves a whole rank for a labelled edge, so a label only gets
    // room if it is measured here. Unlabelled links stay zero-sized.
    const width = textWidth(edge.label, metrics.note);
    const height = textHeight(edge.label, metrics.note);

    graph.setEdge(
      edge.from,
      edge.to,
      {
        width: width > 0 ? width + metrics.margin * 2 : 0,
        height: height > 0 ? height + metrics.margin * 2 : 0,
        labelpos: "c",
        labeloffset: 0,
      },
      String(index),
    );
  });

  // Dagre's newer "optimal order" pass re-sorts a rank whenever it can do so
  // without adding a crossing — which, on a graph with no crossings to remove,
  // means it reverses ranks for free and the author's declaration order is
  // lost. In a diagram that is not a free choice: `agent 1` has to come out
  // above `agent 2`. The classic median-and-transpose ordering keeps it.
  //
  // `useDynamic: false` because dagre 3.x otherwise caches the previous layout
  // in module state to stabilize incremental relayouts of one graph — and
  // feeds it into the next layout's acyclic phase. Laying out a *different*
  // graph next (three diagrams on one page) inherits that state, the rank
  // phase silently no-ops, and `initOrder` crashes on rank `null`.
  layout(graph, { disableOptimalOrderHeuristic: true, useDynamic: false });

  const plates: IsoPlate[] = [];
  const tiles: Tile[] = [];

  for (const node of flow.nodes) {
    const placed = graph.node(node.id);
    if (!placed) continue;

    const held = content.get(node.id);
    const x = placed.x ?? 0;
    const z = placed.y ?? 0;

    plates.push({
      id: node.id,
      label: held?.label ?? [node.id],
      mark: held?.mark ?? null,
      x,
      z,
      width: placed.width,
      depth: placed.height,
    });

    tiles.push({
      x0: x - placed.width / 2,
      x1: x + placed.width / 2,
      z0: z - placed.height / 2,
      z1: z + placed.height / 2,
    });
  }

  const connectors: IsoConnector[] = flow.edges.flatMap((edge, index) => {
    const routed = graph.edge({ v: edge.from, w: edge.to, name: String(index) });
    if (!routed?.points || routed.points.length < 2) return [];

    const run: Ground[] = routed.points.map((p) => [p.x, p.y]);

    return [
      {
        key: `${edge.from}-${edge.to}-${index}`,
        points: round(run, metrics.bend),
        head: edge.arrow ? arrowhead(run[run.length - 2], run[run.length - 1], metrics) : null,
        style: edge.style,
        label: edge.label,
        labelAt:
          edge.label.length > 0 && routed.x !== undefined && routed.y !== undefined
            ? [routed.x, routed.y]
            : null,
      },
    ];
  });

  const bounds = {
    x0: Number.POSITIVE_INFINITY,
    x1: Number.NEGATIVE_INFINITY,
    z0: Number.POSITIVE_INFINITY,
    z1: Number.NEGATIVE_INFINITY,
  };

  const cover = ([x, z]: Ground) => {
    bounds.x0 = Math.min(bounds.x0, x);
    bounds.x1 = Math.max(bounds.x1, x);
    bounds.z0 = Math.min(bounds.z0, z);
    bounds.z1 = Math.max(bounds.z1, z);
  };

  for (const tile of tiles) {
    cover([tile.x0, tile.z0]);
    cover([tile.x1, tile.z1]);
  }
  for (const connector of connectors) for (const point of connector.points) cover(point);

  return { plates, connectors, bounds, metrics };
}
