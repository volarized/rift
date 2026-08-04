import { type EdgeLabel, Graph, type GraphLabel, layout, type NodeLabel } from "@dagrejs/dagre";

import type { FlowGraph, FlowLinkStyle } from "@/lib/mermaid-flowchart";

/**
 * Turns a parsed mermaid flowchart into an isometric scene: flat paths in
 * screen space, plus the upright anchors the renderer hangs text and icons on.
 *
 * Two halves, and the split is the whole idea.
 *
 *   1. *Where things go* is dagre's problem — the same layered layout mermaid
 *      itself runs for a flowchart. It works on a plane and knows nothing about
 *      this file.
 *   2. *What things look like* is ours. Dagre's plane becomes the ground plane
 *      of a 3D scene; nodes are extruded into slabs and every vertex is put
 *      through one isometric projection. Text is never projected — it is placed
 *      at a projected point and drawn upright, which is the only way a label in
 *      an isometric drawing stays a label.
 *
 * `@dagrejs/dagre` is a devDependency on purpose: this module is imported by a
 * React Server Component, so it runs once during `next build` and the layout is
 * baked into the exported HTML. Nothing here reaches the browser.
 */

/** Isometric: the two ground axes leave the origin at ±30° from horizontal. */
const COS30 = Math.cos(Math.PI / 6);
const SIN30 = Math.sin(Math.PI / 6);

/**
 * Geist Mono's advance width, as a fraction of the font size. Every label in
 * the diagram is set in it, so one constant is enough to size a plate without
 * a DOM to measure in.
 */
const ADVANCE = 0.6;

/** Line pitch for a multi-line label, as a fraction of the font size. */
const LEADING = 1.45;

export type IsoMetrics = {
  /** Node label size, in world units. */
  label: number;
  /** Edge label size, in world units. */
  note: number;
  /** Side of the square box an icon is drawn in, in world units. */
  icon: number;
  /** Space between a node's icon and its label. */
  gutter: number;
  /** Clearance between a caption and the edge it sits inside. */
  margin: number;
  /**
   * How much of a caption's own height is bought clearance on a plate, 0..1.
   *
   * A slab's top face is a parallelogram, so the free span narrows by `√3` for
   * every unit the caption reaches above or below the centre line. At `1` the
   * caption is fully inscribed and the plate ends up enormous next to the two
   * words on it; at `0` its corners graze the drawn edge. Part way is what
   * reads as a plate sized *to* its label.
   */
  fit: number;
  /** How far a plate stands off the floor. */
  thickness: number;
  /**
   * Dagre's separation between ranks, and within one.
   *
   * These want to be *larger* than they would be in a flat diagram, not
   * smaller. A gap of `n` along a ground axis projects to `0.866n` across the
   * page and `0.5n` down it, so separation that reads as generous on the plane
   * reads as touching once the plane is turned away from the eye.
   */
  rankSep: number;
  nodeSep: number;
  /** How far the ruled floor runs past a plate, in every direction. */
  overhang: number;
  /** Pitch of the ruled floor. Zero draws no floor. */
  floor: number;
  /** Corner radius on a connector's bends, in world units. */
  bend: number;
  /** Arrowhead length, in world units. */
  head: number;
};

/**
 * These are ratios, not sizes: the scene is drawn into a viewBox and scaled to
 * whatever width it is given, so only their proportion to `label` matters.
 * Everything that is *not* a glyph costs the labels legibility at a fixed
 * rendered width — which is the whole tension, because separation is also the
 * only thing that keeps two plates from reading as one.
 */
export const DEFAULT_METRICS: IsoMetrics = {
  label: 17,
  note: 13,
  icon: 21,
  gutter: 8,
  margin: 11,
  fit: 0.5,
  thickness: 11,
  rankSep: 78,
  nodeSep: 62,
  overhang: 46,
  floor: 26,
  bend: 16,
  head: 16,
};

export type Point = { x: number; y: number };

export type IsoPlate = {
  id: string;
  label: string[];
  classes: string[];
  /** The slab's outline, used to knock a hole in whatever is behind it. */
  silhouette: string;
  /** The three visible faces, drawn separately so each can carry its own tint. */
  top: string;
  right: string;
  left: string;
  /** Where this plate's upright icon and label are centred, in screen space. */
  anchor: Point;
  /** Painter's key: larger is nearer the eye. */
  depth: number;
};

export type IsoConnector = {
  key: string;
  /** The run, from the source plate's edge to the target plate's edge. */
  path: string;
  /** The arrowhead, lying flat on the plate tops. Null for a headless link. */
  head: string | null;
  style: FlowLinkStyle;
  label: string[];
  /** Where the upright edge label is centred. Null when the link has none. */
  labelAt: Point | null;
};

export type IsoScene = {
  viewBox: string;
  /** The drawing's own extent, so a caller can reason about its aspect. */
  width: number;
  height: number;
  plates: IsoPlate[];
  connectors: IsoConnector[];
  /** The ruled floor: one path per family of rules, or null when suppressed. */
  floor: { alongX: string; alongZ: string } | null;
  metrics: IsoMetrics;
};

/** A point in the scene — ground plane (x, z), height y — to the page. */
function project(x: number, y: number, z: number): Point {
  return { x: (x - z) * COS30, y: (x + z) * SIN30 - y };
}

function pathOf(points: Point[], close = true): string {
  const run = points.map((p, i) => `${i === 0 ? "M" : "L"}${round(p.x)} ${round(p.y)}`).join(" ");
  return close ? `${run} Z` : run;
}

/** Three decimals is well past what a rendered hairline can show. */
function round(value: number): number {
  return Math.round(value * 1000) / 1000;
}

/**
 * Label metrics without a DOM to measure in. Every label in the diagram is set
 * in one monospaced family, so its advance width is a constant — which is the
 * whole reason the design's display face can be relied on here.
 */
export function textWidth(lines: string[], size: number): number {
  return lines.reduce((widest, line) => Math.max(widest, line.length), 0) * size * ADVANCE;
}

export function textHeight(lines: string[], size: number): number {
  return lines.length === 0 ? 0 : size + (lines.length - 1) * size * LEADING;
}

/** Baseline-to-baseline pitch for a multi-line label. */
export function textPitch(size: number): number {
  return size * LEADING;
}

/**
 * The side of the square footprint a plate needs to hold its content upright.
 *
 * A slab's top face projects to a parallelogram, so a horizontal caption laid
 * across it does *not* get the plate's full width: at the centre line the free
 * span is `√3/2` of the shorter footprint side, and it narrows by `√3` for
 * every unit the text reaches above or below that line.
 *
 * Demanding the caption be *fully* inscribed — clearing both its corners —
 * costs the plate its whole caption height again, and produces a slab several
 * times the area of the two words sitting on it. `fit` buys back only part of
 * that, so the caption's corners come within a hair of the drawn edge instead
 * of sitting in the middle of an empty field. Plates stay square because the
 * free span is set by the *shorter* footprint side, so width spent along the
 * flow would buy a caption nothing.
 */
function footprintFor(content: { width: number; height: number }, metrics: IsoMetrics): number {
  return (content.width / 2 + metrics.margin) / COS30 + metrics.fit * content.height;
}

/**
 * Rounds the bends of a screen-space polyline. Dagre routes an edge as a
 * chain of straight runs; drawn as-is the corners are hard enough to read as
 * mistakes next to the slabs' drawn edges.
 */
function smooth(points: Point[], radius: number): string {
  if (points.length < 3) return pathOf(points, false);

  const parts = [`M${round(points[0].x)} ${round(points[0].y)}`];

  for (let i = 1; i < points.length - 1; i++) {
    const previous = points[i - 1];
    const corner = points[i];
    const next = points[i + 1];

    const back = trim(corner, previous, radius);
    const forward = trim(corner, next, radius);

    parts.push(`L${round(back.x)} ${round(back.y)}`);
    parts.push(`Q${round(corner.x)} ${round(corner.y)} ${round(forward.x)} ${round(forward.y)}`);
  }

  const last = points[points.length - 1];
  parts.push(`L${round(last.x)} ${round(last.y)}`);
  return parts.join(" ");
}

/** Steps from `from` towards `to`, by at most `distance` or half the run. */
function trim(from: Point, to: Point, distance: number): Point {
  const dx = to.x - from.x;
  const dy = to.y - from.y;
  const length = Math.hypot(dx, dy) || 1;
  const step = Math.min(distance, length / 2) / length;
  return { x: from.x + dx * step, y: from.y + dy * step };
}

type Bounds = { minX: number; minY: number; maxX: number; maxY: number };

function grow(bounds: Bounds, point: Point) {
  bounds.minX = Math.min(bounds.minX, point.x);
  bounds.minY = Math.min(bounds.minY, point.y);
  bounds.maxX = Math.max(bounds.maxX, point.x);
  bounds.maxY = Math.max(bounds.maxY, point.y);
}

export type IconLookup = (node: { id: string; classes: string[] }) => boolean;

/**
 * @param hasIcon Whether a node will be drawn with an icon beside its label.
 *   The scene has to know before it can size the plate, and only the renderer
 *   knows the icon registry — so it is asked.
 */
export function buildScene(
  flow: FlowGraph,
  hasIcon: IconLookup,
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

  // A node's own label after the fallback, kept for the second pass below —
  // dagre is told sizes, not text, and hands nothing back but coordinates.
  const labels = new Map<string, string[]>();

  for (const node of flow.nodes) {
    const label = node.label.length > 0 ? node.label : [node.id];
    const icon = hasIcon(node);
    labels.set(node.id, label);

    const side = footprintFor(
      {
        width: (icon ? metrics.icon + metrics.gutter : 0) + textWidth(label, metrics.label),
        height: Math.max(icon ? metrics.icon : 0, textHeight(label, metrics.label)),
      },
      metrics,
    );

    // Square, because the projection's free span is set by the *shorter* side:
    // a plate stretched along the flow would gain nothing a caption could use.
    graph.setNode(node.id, { width: side, height: side });
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
  layout(graph, { disableOptimalOrderHeuristic: true });

  const bounds: Bounds = {
    minX: Number.POSITIVE_INFINITY,
    minY: Number.POSITIVE_INFINITY,
    maxX: Number.NEGATIVE_INFINITY,
    maxY: Number.NEGATIVE_INFINITY,
  };

  const plates: IsoPlate[] = [];
  /** Each plate's footprint on the ground plane, for the floor to follow. */
  const tiles: Tile[] = [];

  for (const node of flow.nodes) {
    const placed = graph.node(node.id);
    if (!placed) continue;

    const cx = placed.x ?? 0;
    const cz = placed.y ?? 0;
    const hw = placed.width / 2;
    const hd = placed.height / 2;
    const t = metrics.thickness;

    tiles.push({ x0: cx - hw, x1: cx + hw, z0: cz - hd, z1: cz + hd });

    // Corners of the footprint, named for where they land on the page: `back`
    // is the far one, `front` the near one, `right` and `left` the flanks.
    const at = (sx: number, sz: number, y: number) => project(cx + sx * hw, y, cz + sz * hd);

    const back = at(-1, -1, t);
    const right = at(1, -1, t);
    const front = at(1, 1, t);
    const left = at(-1, 1, t);
    const rightFoot = at(1, -1, 0);
    const frontFoot = at(1, 1, 0);
    const leftFoot = at(-1, 1, 0);

    const plate: IsoPlate = {
      id: node.id,
      label: labels.get(node.id) ?? [node.id],
      classes: node.classes,
      silhouette: pathOf([back, right, rightFoot, frontFoot, leftFoot, left]),
      top: pathOf([back, right, front, left]),
      // The two faces the eye can see: the one facing along the flow, and the
      // one facing across it. The third and fourth are behind the solid.
      right: pathOf([right, front, frontFoot, rightFoot]),
      left: pathOf([front, left, leftFoot, frontFoot]),
      anchor: project(cx, t, cz),
      depth: cx + cz,
    };

    for (const point of [back, right, rightFoot, frontFoot, leftFoot, left]) grow(bounds, point);
    plates.push(plate);
  }

  // Painter's algorithm: the far plates first, so a near one covers them.
  plates.sort((a, b) => a.depth - b.depth);

  const connectors: IsoConnector[] = flow.edges.flatMap((edge, index) => {
    const routed = graph.edge({ v: edge.from, w: edge.to, name: String(index) });
    if (!routed?.points || routed.points.length < 2) return [];

    // Connectors run on the plate tops rather than the floor, so a link reads
    // as leaving one slab's surface and arriving at the next one's.
    const run = routed.points.map((p) => project(p.x, metrics.thickness, p.y));
    for (const point of run) grow(bounds, point);

    const tail = routed.points[routed.points.length - 2];
    const tip = routed.points[routed.points.length - 1];

    return [
      {
        key: `${edge.from}-${edge.to}-${index}`,
        path: smooth(run, metrics.bend),
        head: edge.arrow ? arrowhead(tail, tip, metrics) : null,
        style: edge.style,
        label: edge.label,
        labelAt:
          edge.label.length > 0 && routed.x !== undefined && routed.y !== undefined
            ? project(routed.x, metrics.thickness, routed.y)
            : null,
      },
    ];
  });

  // Edge labels are the one thing that can reach outside the plates.
  for (const connector of connectors) {
    if (!connector.labelAt) continue;
    const half = {
      x: textWidth(connector.label, metrics.note) / 2 + metrics.margin,
      y: textHeight(connector.label, metrics.note) / 2 + metrics.margin,
    };
    grow(bounds, { x: connector.labelAt.x - half.x, y: connector.labelAt.y - half.y });
    grow(bounds, { x: connector.labelAt.x + half.x, y: connector.labelAt.y + half.y });
  }

  const floor = ruledFloor(tiles, metrics, bounds);

  const width = bounds.maxX - bounds.minX;
  const height = bounds.maxY - bounds.minY;

  return {
    viewBox: `${round(bounds.minX)} ${round(bounds.minY)} ${round(width)} ${round(height)}`,
    width,
    height,
    plates,
    connectors,
    floor,
    metrics,
  };
}

/** A flat triangle on the plate tops, pointing along the link's last run. */
function arrowhead(tail: Point, tip: Point, metrics: IsoMetrics): string {
  const dx = tip.x - tail.x;
  const dz = tip.y - tail.y;
  const length = Math.hypot(dx, dz) || 1;
  const ux = dx / length;
  const uz = dz / length;

  const baseX = tip.x - ux * metrics.head;
  const baseZ = tip.y - uz * metrics.head;
  const half = metrics.head * 0.42;

  return pathOf([
    project(tip.x, metrics.thickness, tip.y),
    project(baseX - uz * half, metrics.thickness, baseZ + ux * half),
    project(baseX + uz * half, metrics.thickness, baseZ - ux * half),
  ]);
}

/** A plate's footprint on the ground plane. Axis-aligned, in dagre's space. */
type Tile = { x0: number; x1: number; z0: number; z1: number };

/** Merges a set of 1-D intervals, in place of a proper interval tree. */
function merge(ranges: [number, number][]): [number, number][] {
  const sorted = [...ranges].sort((a, b) => a[0] - b[0]);
  const out: [number, number][] = [];

  for (const [from, to] of sorted) {
    const last = out[out.length - 1];
    if (last && from <= last[1]) last[1] = Math.max(last[1], to);
    else out.push([from, to]);
  }

  return out;
}

/**
 * Graph paper, on the floor the plates stand on. Two families of rules, one
 * along each ground axis, which is what tells the eye the plane is receding
 * rather than the whole drawing being a set of odd hexagons.
 *
 * The floor follows the plates rather than boxing the whole graph. A diagram
 * runs on the isometric diagonal, so a rectangle drawn around it leaves two
 * large empty wedges at the corners it does not reach — grid over nothing,
 * which is louder than the connectors it sits behind. Instead each plate casts
 * its own footprint grown by `overhang`, the footprints are unioned, and a rule
 * is drawn only across the spans that union covers. With `overhang` past half
 * the separation the patches touch and the floor comes out as one band hugging
 * the drawing.
 */
function ruledFloor(
  tiles: Tile[],
  metrics: IsoMetrics,
  bounds: Bounds,
): { alongX: string; alongZ: string } | null {
  if (metrics.floor <= 0 || tiles.length === 0) return null;

  const over = metrics.overhang;
  const grown = tiles.map((t) => ({
    x0: t.x0 - over,
    x1: t.x1 + over,
    z0: t.z0 - over,
    z1: t.z1 + over,
  }));

  const extent = {
    x0: Math.min(...grown.map((t) => t.x0)),
    x1: Math.max(...grown.map((t) => t.x1)),
    z0: Math.min(...grown.map((t) => t.z0)),
    z1: Math.max(...grown.map((t) => t.z1)),
  };

  const rule = (from: Point, to: Point) => {
    grow(bounds, from);
    grow(bounds, to);
    return pathOf([from, to], false);
  };

  // Rule positions come off one lattice through the origin, so the two
  // families cross at shared points however the tiles happen to fall.
  const first = (at: number) => Math.ceil(at / metrics.floor) * metrics.floor;

  // Each family is one path with many subpaths: a rule carries no meaning on
  // its own, so there is nothing to be gained from an element apiece.
  const alongZ: string[] = [];
  for (let x = first(extent.x0); x <= extent.x1; x += metrics.floor) {
    const spans = merge(
      grown.filter((t) => x >= t.x0 && x <= t.x1).map((t) => [t.z0, t.z1] as [number, number]),
    );
    for (const [from, to] of spans) alongZ.push(rule(project(x, 0, from), project(x, 0, to)));
  }

  const alongX: string[] = [];
  for (let z = first(extent.z0); z <= extent.z1; z += metrics.floor) {
    const spans = merge(
      grown.filter((t) => z >= t.z0 && z <= t.z1).map((t) => [t.x0, t.x1] as [number, number]),
    );
    for (const [from, to] of spans) alongX.push(rule(project(from, 0, z), project(to, 0, z)));
  }

  return { alongX: alongX.join(" "), alongZ: alongZ.join(" ") };
}
