import type { ReactNode } from "react";

import { DiagramFullscreen } from "@/components/diagram-fullscreen";
import { buildScene, type IsoMetrics } from "@/lib/iso-scene";
import { parseFlowchart } from "@/lib/mermaid-flowchart";

/**
 * A mermaid flowchart, drawn flat. The one diagram renderer the docs use.
 *
 * Mermaid parse, dagre layout, then plain SVG in the page's own plane: the
 * layout module works on a flat ground plane, so this renderer maps `x`
 * across the page and `z` down it and draws.
 *
 * A Server Component: the layout runs once during the build and the page
 * carries only the finished SVG. `DiagramFullscreen` is the Client Component
 * around it - a click opens the same SVG in a fullscreen dialog with pan and
 * zoom.
 */
export type FlatDiagramProps = {
  /** A mermaid flowchart definition. The graph, and only the graph. */
  chart: string;
  /** What the diagram says, for anyone who cannot see it. */
  alt: string;
  /** Overrides for the scene's proportions. See `IsoMetrics`. */
  metrics?: Partial<IsoMetrics>;
  /** Muted, rounded node boxes, or the default background-filled outlines. */
  variant?: "outline" | "soft";
  /** Optional icons keyed by Mermaid node id. */
  icons?: Readonly<Record<string, ReactNode>>;
  className?: string;
};

/**
 * The layout module's default separation reads as sparse on the page, where
 * nothing is foreshortened, so these pull the nodes closer.
 */
const FLAT_METRICS: Partial<IsoMetrics> = {
  minDepth: 46,
  rankSep: 56,
  nodeSep: 34,
};

/** Clearance kept around the drawing inside the viewBox. */
const FRAME_PADDING = 10;

/** Default ink opacities. Soft boxes use the docs' muted background. */
const INK = { plate: 0.55, connector: 0.45, label: 0.9, note: 0.6, group: 0.3 };

export async function FlatDiagram({
  chart,
  alt,
  metrics,
  variant = "outline",
  icons = {},
  className,
}: FlatDiagramProps) {
  const flow = await parseFlowchart(chart);
  const scene = buildScene(flow, (node) => (icons[node.id] ? node.id : null), {
    ...FLAT_METRICS,
    ...metrics,
  });

  const x0 = scene.bounds.x0 - FRAME_PADDING;
  const y0 = scene.bounds.z0 - FRAME_PADDING;
  const width = scene.bounds.x1 - scene.bounds.x0 + FRAME_PADDING * 2;
  const height = scene.bounds.z1 - scene.bounds.z0 + FRAME_PADDING * 2;
  const { label, note, icon, gutter, margin } = scene.metrics;
  const noteOpacity = variant === "soft" ? 0.8 : INK.note;

  return (
    <DiagramFullscreen className={className}>
      <svg
        viewBox={`${x0} ${y0} ${width} ${height}`}
        role="img"
        aria-label={alt}
        className="w-full h-auto font-mono"
      >
        {scene.groups.map((group) => (
          <g key={group.id}>
            <rect
              x={group.x - group.width / 2}
              y={group.z - group.depth / 2}
              width={group.width}
              height={group.depth}
              rx={8}
              fill="none"
              stroke="currentColor"
              strokeOpacity={INK.group}
              strokeDasharray="3 5"
            />
            <text
              x={group.x - group.width / 2 + 10}
              y={group.z - group.depth / 2 + note * 1.3}
              fontSize={note}
              fill="currentColor"
              fillOpacity={noteOpacity}
            >
              {group.title.join(" ")}
            </text>
          </g>
        ))}
        {scene.connectors.map((connector) => (
          <g key={connector.key} opacity={INK.connector}>
            <path
              d={connector.points
                .map(([x, z], index) => `${index === 0 ? "M" : "L"}${x} ${z}`)
                .join(" ")}
              fill="none"
              stroke="currentColor"
              strokeWidth={connector.style === "thick" ? 2 : 1}
              strokeDasharray={connector.style === "dotted" ? "2 4" : undefined}
            />
            {connector.head && (
              <polygon
                points={connector.head.map(([x, z]) => `${x},${z}`).join(" ")}
                fill="currentColor"
              />
            )}
          </g>
        ))}
        {scene.plates.map((plate) => (
          <g key={plate.id}>
            <rect
              x={plate.x - plate.width / 2}
              y={plate.z - plate.depth / 2}
              width={plate.width}
              height={plate.depth}
              rx={variant === "soft" ? 10 : 5}
              fill={variant === "soft" ? "var(--color-fd-muted)" : "var(--color-fd-background)"}
              stroke="currentColor"
              strokeOpacity={variant === "soft" ? 0.18 : INK.plate}
            />
            {plate.mark && (
              <g
                transform={`translate(${plate.x - plate.width / 2 + margin} ${plate.z - icon / 2})`}
              >
                <svg width={icon} height={icon} viewBox="0 0 24 24" aria-hidden="true">
                  {icons[plate.mark]}
                </svg>
              </g>
            )}
            <text
              x={plate.x + (plate.mark ? (icon + gutter) / 2 : 0)}
              y={plate.z - ((plate.label.length - 1) * label * 1.45) / 2}
              textAnchor="middle"
              dominantBaseline="central"
              fontSize={label}
              fill="currentColor"
              fillOpacity={INK.label}
            >
              {plate.label.map((line, index) => (
                <tspan
                  key={line}
                  x={plate.x + (plate.mark ? (icon + gutter) / 2 : 0)}
                  dy={index === 0 ? 0 : label * 1.45}
                  fillOpacity={variant === "soft" && index > 0 ? 0.8 : 1}
                >
                  {line}
                </tspan>
              ))}
            </text>
          </g>
        ))}
        {scene.connectors.map((connector) =>
          connector.labelAt ? (
            <text
              key={`${connector.key}-label`}
              x={connector.labelAt[0]}
              y={connector.labelAt[1]}
              textAnchor="middle"
              dominantBaseline="central"
              fontSize={note}
              fill="currentColor"
              fillOpacity={noteOpacity}
              stroke="var(--color-fd-background)"
              strokeWidth={4}
              paintOrder="stroke"
            >
              {connector.label.map((line, index) => (
                <tspan key={line} x={connector.labelAt?.[0]} dy={index === 0 ? 0 : note * 1.45}>
                  {line}
                </tspan>
              ))}
            </text>
          ) : null,
        )}
      </svg>
    </DiagramFullscreen>
  );
}
