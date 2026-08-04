import {
  DatabaseIcon,
  GitBranchIcon,
  PlugsConnectedIcon,
  RobotIcon,
} from "@phosphor-icons/react/dist/ssr";

import { Logo } from "@/components/logo";
import { buildScene, type IsoMetrics, textHeight, textPitch, textWidth } from "@/lib/iso-scene";
import { parseFlowchart } from "@/lib/mermaid-flowchart";
import { cn } from "@/lib/utils";

/**
 * A mermaid flowchart, drawn as an isometric scene.
 *
 * This is a Server Component with no client half: the mermaid parse and the
 * dagre layout both run during `next build`, and what ships is a static SVG.
 * That is not an optimisation, it is the requirement — `next.config.mjs` sets
 * `output: "export"`, so there is no runtime to render on, and shipping
 * mermaid to the browser to redraw a diagram that never changes would be
 * several hundred kilobytes for nothing.
 *
 * Two rules the drawing follows, both of which come from the failure mode this
 * replaces — a mermaid SVG under a skew matrix:
 *
 *   - Nothing that carries meaning is projected. Plates, connectors and
 *     arrowheads are real geometry put through the isometric transform; labels
 *     and icons are placed at a projected *point* and then drawn upright.
 *   - Text is painted last, over everything. An isometric scene has no depth
 *     buffer, only a painter's order, and a label half-covered by a slab is
 *     worse than no diagram at all.
 *
 * Colour is entirely `currentColor` and `var(--background)`, so the drawing
 * inverts with the theme without a single hard-coded value.
 */

/**
 * An icon, asked to draw itself into a box in the diagram's own coordinate
 * space. Everything here is a nested `<svg>`, which is how an icon component
 * written for the page can be placed inside the scene without being scaled by
 * the projection.
 */
export type IsoIcon = (box: { x: number; y: number; size: number }) => React.ReactNode;

/**
 * Mermaid class to icon. A diagram picks its own icons in its own source —
 * `agent1["agent 1"]:::agent` — so nothing about a particular picture is
 * wired in here.
 */
export const ISO_ICONS: Record<string, IsoIcon> = {
  // The rift mark itself, for the processes that are rift. It is drawn as a
  // mesh of strings, and the mesh silts up into a blob below about 24px — so
  // this placement takes the sparse variant with a much heavier stroke.
  rift: ({ x, y, size }) => (
    <Logo
      x={x}
      y={y}
      width={size}
      height={size}
      stitches={5}
      strokeWidth={3.4}
      stringOpacity={0.55}
    />
  ),
  agent: ({ x, y, size }) => <RobotIcon x={x} y={y} width={size} height={size} weight="light" />,
  store: ({ x, y, size }) => <DatabaseIcon x={x} y={y} width={size} height={size} weight="light" />,
  git: ({ x, y, size }) => <GitBranchIcon x={x} y={y} width={size} height={size} weight="light" />,
  adapter: ({ x, y, size }) => (
    <PlugsConnectedIcon x={x} y={y} width={size} height={size} weight="light" />
  ),
};

/** The first class on a node that names an icon wins. */
function iconFor(node: { classes: string[] }, icons: Record<string, IsoIcon>): IsoIcon | null {
  for (const name of node.classes) {
    const icon = icons[name];
    if (icon) return icon;
  }
  return null;
}

/**
 * Ink weights. A slab is read from its drawn edges, not from its fill: the
 * faces carry only enough tint to tell one plane from another, and the tint is
 * `currentColor`, so it is the page's own ink at both themes.
 */
const INK = {
  top: { fill: 0.03, stroke: 0.4 },
  right: { fill: 0.07, stroke: 0.22 },
  left: { fill: 0.11, stroke: 0.22 },
  silhouette: 0.5,
  /**
   * The connectors are the only thing in the drawing that carries an argument,
   * so they are the heaviest ink on the page — heavier than the plates they
   * join, and well clear of the floor they cross.
   */
  connector: 0.82,
  floor: 0.06,
  icon: 0.7,
  note: 0.7,
};

/**
 * The smallest a node label is allowed to render, in CSS pixels. The design
 * system's own section labels sit at 11px, so this is the family's floor, not
 * an arbitrary one.
 */
const FLOOR = 11;

export type IsoDiagramProps = {
  /** A mermaid flowchart definition. The graph, and only the graph. */
  chart: string;
  /**
   * What the diagram says, for anyone who cannot see it. Required, because an
   * SVG of eight plates and seven arrows is otherwise nothing at all.
   */
  alt: string;
  /** Icon registry, merged over the default one. */
  icons?: Record<string, IsoIcon>;
  /** Overrides for the scene's proportions. See `IsoMetrics`. */
  metrics?: Partial<IsoMetrics>;
  className?: string;
};

export async function IsoDiagram({ chart, alt, icons, metrics, className }: IsoDiagramProps) {
  const registry = icons ? { ...ISO_ICONS, ...icons } : ISO_ICONS;

  const flow = await parseFlowchart(chart);
  const scene = buildScene(flow, (node) => iconFor(node, registry) !== null, metrics);
  const { label, note, icon, gutter, margin } = scene.metrics;

  // An isometric drawing is always about √3 times wider than it is tall, so a
  // graph of any size is a wide picture — and squeezed into a phone it would
  // scale its labels down to nothing. Below the width that keeps them at
  // `FLOOR` pixels the figure scrolls sideways instead of shrinking.
  const minWidth = Math.round((scene.width * FLOOR) / label);

  return (
    <figure className={cn("my-8 min-w-0 text-foreground", className)}>
      {/* `min-w-0` on both, so a flex or grid ancestor shrinks the scroller to
          the column rather than being widened by the drawing inside it. */}
      <div className="min-w-0 max-w-full overflow-x-auto">
        <svg
          viewBox={scene.viewBox}
          width="100%"
          role="img"
          aria-label={alt}
          style={{ minWidth }}
          className="block h-auto w-full"
        >
          <title>{alt}</title>

          {/* Graph paper on the floor the plates stand on. Two families of
            rules, one along each ground axis — without them the plates read
            as a set of odd hexagons rather than solids on a plane. */}
          {scene.floor ? (
            <g fill="none" stroke="currentColor" strokeOpacity={INK.floor} strokeWidth={1}>
              <path d={scene.floor.alongX} />
              <path d={scene.floor.alongZ} />
            </g>
          ) : null}

          {/* The slabs, far ones first. Each is knocked out in the page's own
            colour before its faces are drawn, so a near plate occludes a far
            one instead of letting it show through. */}
          {scene.plates.map((plate) => (
            <g key={plate.id}>
              <path d={plate.silhouette} fill="var(--background)" />
              <Face d={plate.left} ink={INK.left} />
              <Face d={plate.right} ink={INK.right} />
              <Face d={plate.top} ink={INK.top} />
              <path
                d={plate.silhouette}
                fill="none"
                stroke="currentColor"
                strokeOpacity={INK.silhouette}
                strokeWidth={1.25}
                strokeLinejoin="round"
              />
            </g>
          ))}

          {/* Connectors over the slabs. Dagre routes around the plates, so the
            only thing an edge crosses is its own endpoints' top faces. */}
          {scene.connectors.map((connector) => (
            <g
              key={connector.key}
              stroke="currentColor"
              strokeOpacity={INK.connector}
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path
                d={connector.path}
                fill="none"
                strokeWidth={connector.style === "thick" ? 3 : 1.7}
                strokeDasharray={connector.style === "dotted" ? "6 6" : undefined}
              />
              {/* Filled *and* stroked: at the size a docs page renders this,
                  a bare triangle loses a third of its area to antialiasing. */}
              {connector.head ? (
                <path d={connector.head} fill="currentColor" fillOpacity={INK.connector} />
              ) : null}
            </g>
          ))}

          {/* Everything upright, painted last so nothing can cover it. */}
          {scene.plates.map((plate) => {
            const draw = iconFor(plate, registry);
            const width = textWidth(plate.label, label) + (draw ? icon + gutter : 0);
            const left = plate.anchor.x - width / 2;

            return (
              <g key={plate.id}>
                {draw ? (
                  <g opacity={INK.icon}>
                    {draw({ x: left, y: plate.anchor.y - icon / 2, size: icon })}
                  </g>
                ) : null}
                <Caption
                  lines={plate.label}
                  size={label}
                  x={draw ? left + icon + gutter : left}
                  y={plate.anchor.y}
                  anchor="start"
                  className="font-mono"
                />
              </g>
            );
          })}

          {scene.connectors.map((connector) =>
            connector.labelAt ? (
              <g key={connector.key}>
                {/* A knocked-out plate behind the label: an edge label sits on
                  the connector it names, and has to win. */}
                <rect
                  x={connector.labelAt.x - textWidth(connector.label, note) / 2 - margin}
                  y={connector.labelAt.y - textHeight(connector.label, note) / 2 - margin * 0.7}
                  width={textWidth(connector.label, note) + margin * 2}
                  height={textHeight(connector.label, note) + margin * 1.4}
                  fill="var(--background)"
                />
                <Caption
                  lines={connector.label}
                  size={note}
                  x={connector.labelAt.x}
                  y={connector.labelAt.y}
                  anchor="middle"
                  opacity={INK.note}
                  className="font-mono"
                />
              </g>
            ) : null,
          )}
        </svg>
      </div>
    </figure>
  );
}

function Face({ d, ink }: { d: string; ink: { fill: number; stroke: number } }) {
  return (
    <path
      d={d}
      fill="currentColor"
      fillOpacity={ink.fill}
      stroke="currentColor"
      strokeOpacity={ink.stroke}
      strokeWidth={1}
      strokeLinejoin="round"
    />
  );
}

/**
 * A label, upright, centred vertically on `y`. SVG has no multi-line text, so
 * a run of `<tspan>` on a shared pitch stands in for one.
 */
function Caption({
  lines,
  size,
  x,
  y,
  anchor,
  opacity,
  className,
}: {
  lines: string[];
  size: number;
  x: number;
  y: number;
  anchor: "start" | "middle";
  opacity?: number;
  className?: string;
}) {
  const pitch = textPitch(size);
  const first = y - ((lines.length - 1) * pitch) / 2;

  return (
    <text
      x={x}
      y={first}
      textAnchor={anchor}
      dominantBaseline="central"
      fill="currentColor"
      fillOpacity={opacity}
      fontSize={size}
      className={className}
    >
      {lines.map((line, index) => (
        <tspan
          // biome-ignore lint/suspicious/noArrayIndexKey: the lines of one label are a fixed list in reading order, and two of them can legitimately carry the same text
          key={index}
          x={x}
          dy={index === 0 ? 0 : pitch}
        >
          {line}
        </tspan>
      ))}
    </text>
  );
}
