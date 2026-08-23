import { buildScene, type IsoMetrics } from "@/lib/iso-scene";
import { parseFlowchart } from "@/lib/mermaid-flowchart";
import { cn } from "@/lib/utils";

/**
 * A mermaid flowchart, drawn flat.
 *
 * Same pipeline as `IsoDiagram` — mermaid parse, dagre layout — but the scene
 * is rendered as plain SVG in the page's own plane instead of being stood up
 * under the isometric camera. The layout module already works on a flat ground
 * plane (the camera is what makes its sibling isometric), so this renderer maps
 * `x` across the page and `z` down it and draws.
 *
 * A Server Component like its sibling: the layout runs once during the build
 * and the page carries only the finished SVG.
 */
export type FlatDiagramProps = {
  /** A mermaid flowchart definition. The graph, and only the graph. */
  chart: string;
  /** What the diagram says, for anyone who cannot see it. */
  alt: string;
  /** Overrides for the scene's proportions. See `IsoMetrics`. */
  metrics?: Partial<IsoMetrics>;
  className?: string;
};

/**
 * Flat drawings need less separation than projected ones: nothing is
 * foreshortened, so the isometric defaults read as sparse on the page.
 */
const FLAT_METRICS: Partial<IsoMetrics> = {
  minDepth: 46,
  rankSep: 56,
  nodeSep: 34,
};

/** Clearance kept around the drawing inside the viewBox. */
const FRAME_PADDING = 10;

/** Ink opacities, matched to the isometric renderer's edge-first look. */
const INK = { plate: 0.55, connector: 0.45, label: 0.9, note: 0.6, group: 0.3 };

export async function FlatDiagram({ chart, alt, metrics, className }: FlatDiagramProps) {
  const flow = await parseFlowchart(chart);
  const scene = buildScene(flow, () => null, { ...FLAT_METRICS, ...metrics });

  const x0 = scene.bounds.x0 - FRAME_PADDING;
  const y0 = scene.bounds.z0 - FRAME_PADDING;
  const width = scene.bounds.x1 - scene.bounds.x0 + FRAME_PADDING * 2;
  const height = scene.bounds.z1 - scene.bounds.z0 + FRAME_PADDING * 2;
  const { label, note } = scene.metrics;

  return (
    <figure className={cn("my-8 min-w-0", className)}>
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
              fillOpacity={INK.note}
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
              rx={5}
              fill="var(--color-fd-background)"
              stroke="currentColor"
              strokeOpacity={INK.plate}
            />
            <text
              x={plate.x}
              y={plate.z - ((plate.label.length - 1) * label * 1.45) / 2}
              textAnchor="middle"
              dominantBaseline="central"
              fontSize={label}
              fill="currentColor"
              fillOpacity={INK.label}
            >
              {plate.label.map((line, index) => (
                <tspan key={line} x={plate.x} dy={index === 0 ? 0 : label * 1.45}>
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
              fillOpacity={INK.note}
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
    </figure>
  );
}
