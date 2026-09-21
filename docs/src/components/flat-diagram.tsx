import type { ReactNode } from "react";

import { DiagramFullscreen } from "@/components/diagram-fullscreen";
import {
  buildScene,
  DEFAULT_METRICS,
  type Ground,
  type IsoMetrics,
  type IsoPlate,
} from "@/lib/iso-scene";
import { parseFlowchart } from "@/lib/mermaid-flowchart";

export type FlatDiagramNodePanel = {
  title: string;
  layout?: "row" | "column";
  options: ReadonlyArray<{
    label: string;
    note?: string;
    badge?: string;
    icon?: ReactNode;
    weight?: number;
  }>;
  minWidth?: number;
  minDepth?: number;
};

type FlatDiagramPort = "top" | "right" | "bottom" | "left";

export type FlatDiagramConnectorRoute = {
  from: FlatDiagramPort;
  to: FlatDiagramPort;
};

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
  /** Optional inset option boxes keyed by Mermaid node id. */
  nodePanels?: Readonly<Record<string, FlatDiagramNodePanel>>;
  /** Optional status badges keyed by Mermaid node id. */
  nodeBadges?: Readonly<Record<string, string>>;
  /** Optional status badges keyed by Mermaid subgraph id. */
  groupBadges?: Readonly<Record<string, string>>;
  /** Optional endpoint ports for connectors keyed as `from->to`. */
  connectorRoutes?: Readonly<Record<string, FlatDiagramConnectorRoute>>;
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

const STATUS_BADGE = { font: 12, height: 20, padding: 8 };
const TEXT_ADVANCE = 0.62;

function statusBadgeWidth(text: string) {
  return text.length * STATUS_BADGE.font * TEXT_ADVANCE + STATUS_BADGE.padding * 2;
}

function platePort(plate: IsoPlate, port: FlatDiagramPort): Ground {
  switch (port) {
    case "top":
      return [plate.x, plate.z - plate.depth / 2];
    case "right":
      return [plate.x + plate.width / 2, plate.z];
    case "bottom":
      return [plate.x, plate.z + plate.depth / 2];
    case "left":
      return [plate.x - plate.width / 2, plate.z];
  }
}

function StatusBadge({ text, x, y }: { text: string; x: number; y: number }) {
  const width = statusBadgeWidth(text);

  return (
    <g>
      <rect
        x={x}
        y={y}
        width={width}
        height={STATUS_BADGE.height}
        fill="var(--color-fd-background)"
        stroke="currentColor"
        strokeOpacity={0.4}
        strokeWidth={1.25}
      />
      <text
        x={x + width / 2}
        y={y + STATUS_BADGE.height / 2}
        textAnchor="middle"
        dominantBaseline="central"
        fontSize={STATUS_BADGE.font}
        fontWeight={500}
        fill="currentColor"
        fillOpacity={INK.label}
      >
        {text}
      </text>
    </g>
  );
}

export async function FlatDiagram({
  chart,
  alt,
  metrics,
  variant = "outline",
  icons = {},
  nodePanels = {},
  nodeBadges = {},
  groupBadges = {},
  connectorRoutes = {},
  className,
}: FlatDiagramProps) {
  const flow = await parseFlowchart(chart);
  const sceneMetrics = {
    ...DEFAULT_METRICS,
    ...FLAT_METRICS,
    ...metrics,
  };
  const scene = buildScene(
    flow,
    (node) => (icons[node.id] ? node.id : null),
    sceneMetrics,
    (node) => {
      const panel = nodePanels[node.id];
      const badge = nodeBadges[node.id];
      const header = node.label[0] ?? node.id;
      const badgeWidth = badge ? statusBadgeWidth(badge) : 0;
      const badgeFootprint = badge
        ? sceneMetrics.margin * 2 +
          (icons[node.id] ? sceneMetrics.icon + sceneMetrics.gutter : 0) +
          header.length * sceneMetrics.label * TEXT_ADVANCE +
          sceneMetrics.gutter +
          badgeWidth
        : 0;

      if (!panel && !badge) return null;
      return {
        width: Math.max(panel?.minWidth ?? 0, badgeFootprint),
        depth: panel?.minDepth,
      };
    },
    (edge, from, to) => {
      const route = connectorRoutes[`${edge.from}->${edge.to}`];
      return route ? [platePort(from, route.from), platePort(to, route.to)] : null;
    },
  );

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
        {scene.groups.map((group) => {
          const badge = groupBadges[group.id];
          const groupLeft = group.x - group.width / 2;
          const groupTop = group.z - group.depth / 2;

          return (
            <g key={group.id}>
              <rect
                x={groupLeft}
                y={groupTop}
                width={group.width}
                height={group.depth}
                rx={8}
                fill="none"
                stroke="currentColor"
                strokeOpacity={INK.group}
                strokeDasharray="3 5"
              />
              <text
                x={groupLeft + 10}
                y={groupTop + note * 1.3}
                fontSize={note}
                fill="currentColor"
                fillOpacity={noteOpacity}
              >
                {group.title.join(" ")}
              </text>
              {badge && (
                <StatusBadge
                  text={badge}
                  x={groupLeft + group.width - statusBadgeWidth(badge) - 10}
                  y={groupTop + 8}
                />
              )}
            </g>
          );
        })}
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
        {scene.plates.map((plate) => {
          const panel = nodePanels[plate.id];
          const badge = nodeBadges[plate.id];
          const plateLeft = plate.x - plate.width / 2;
          const plateTop = plate.z - plate.depth / 2;
          const hasBody = panel !== undefined || plate.label.length > 1;
          const iconTop = hasBody ? plateTop + margin : plate.z - icon / 2;
          const headerX = plateLeft + margin + (plate.mark ? icon + gutter : 0);
          const headerY = hasBody ? plateTop + margin + (plate.mark ? icon : label) / 2 : plate.z;

          return (
            <g key={plate.id}>
              <rect
                x={plateLeft}
                y={plateTop}
                width={plate.width}
                height={plate.depth}
                rx={variant === "soft" ? 10 : 5}
                fill={variant === "soft" ? "var(--color-fd-muted)" : "var(--color-fd-background)"}
                stroke="currentColor"
                strokeOpacity={variant === "soft" ? 0.18 : INK.plate}
              />
              {plate.mark && (
                <g transform={`translate(${plateLeft + margin} ${iconTop})`}>
                  <svg width={icon} height={icon} viewBox="0 0 24 24" aria-hidden="true">
                    {icons[plate.mark]}
                  </svg>
                </g>
              )}
              {panel ? (
                <>
                  <text
                    x={headerX}
                    y={headerY}
                    textAnchor="start"
                    dominantBaseline="central"
                    fontSize={label}
                    fill="currentColor"
                    fillOpacity={INK.label}
                  >
                    {panel.title}
                  </text>
                  {panel.options.map((option, index) => {
                    const gap = gutter;
                    const availableWidth = plate.width - margin * 2;
                    const availableDepth = plate.depth - margin * 2 - icon - gutter;
                    const optionsWidth = availableWidth - gap * (panel.options.length - 1);
                    const optionsDepth = availableDepth - gap * (panel.options.length - 1);
                    const totalWeight = panel.options.reduce(
                      (sum, item) => sum + (item.weight ?? 1),
                      0,
                    );
                    const priorWeight = panel.options
                      .slice(0, index)
                      .reduce((sum, item) => sum + (item.weight ?? 1), 0);
                    const isColumn = panel.layout === "column";
                    const optionWidth = isColumn
                      ? availableWidth
                      : (optionsWidth * (option.weight ?? 1)) / totalWeight;
                    const optionDepth = isColumn
                      ? optionsDepth / panel.options.length
                      : availableDepth;
                    const optionX = isColumn
                      ? plateLeft + margin
                      : plateLeft +
                        margin +
                        (optionsWidth * priorWeight) / totalWeight +
                        index * gap;
                    const optionY = isColumn
                      ? plateTop + margin + icon + gutter + index * (optionDepth + gap)
                      : plateTop + margin + icon + gutter;
                    const optionIconSize = icon;
                    const optionHasBody = option.note !== undefined;
                    const optionIconTop = optionHasBody
                      ? optionY + gutter / 2
                      : optionY + (optionDepth - optionIconSize) / 2;
                    const optionHeaderX =
                      optionX + gutter + (option.icon ? optionIconSize + gutter : 0);
                    const optionHeaderY = optionHasBody
                      ? optionY + gutter / 2 + note / 2
                      : optionY + optionDepth / 2;

                    return (
                      <g key={option.label}>
                        <rect
                          x={optionX}
                          y={optionY}
                          width={optionWidth}
                          height={optionDepth}
                          rx={6}
                          fill="var(--color-fd-background)"
                          stroke="currentColor"
                          strokeOpacity={0.18}
                        />
                        {option.icon && (
                          <g transform={`translate(${optionX + gutter} ${optionIconTop})`}>
                            <svg
                              width={optionIconSize}
                              height={optionIconSize}
                              viewBox="0 0 24 24"
                              aria-hidden="true"
                            >
                              {option.icon}
                            </svg>
                          </g>
                        )}
                        {option.badge && (
                          <StatusBadge
                            text={option.badge}
                            x={optionX + optionWidth - statusBadgeWidth(option.badge) - gutter / 2}
                            y={optionY + gutter / 2}
                          />
                        )}
                        <text
                          x={optionHeaderX}
                          y={optionHeaderY}
                          textAnchor="start"
                          dominantBaseline="central"
                          fontSize={note}
                          fill="currentColor"
                          fillOpacity={INK.label}
                        >
                          {option.label}
                        </text>
                        {option.note && (
                          <text
                            x={optionHeaderX}
                            y={optionHeaderY + note * 1.45}
                            textAnchor="start"
                            dominantBaseline="central"
                            fontSize={note}
                            fill="currentColor"
                            fillOpacity={noteOpacity}
                          >
                            {option.note}
                          </text>
                        )}
                      </g>
                    );
                  })}
                </>
              ) : (
                <>
                  <text
                    x={headerX}
                    y={headerY}
                    textAnchor="start"
                    dominantBaseline="central"
                    fontSize={label}
                    fill="currentColor"
                    fillOpacity={INK.label}
                  >
                    {plate.label[0]}
                  </text>
                  {plate.label.length > 1 && (
                    <text
                      x={headerX}
                      y={headerY + label * 1.45}
                      textAnchor="start"
                      dominantBaseline="central"
                      fontSize={label}
                      fill="currentColor"
                      fillOpacity={variant === "soft" ? 0.8 : INK.label}
                    >
                      {plate.label.slice(1).map((line, index) => (
                        <tspan key={line} x={headerX} dy={index === 0 ? 0 : label * 1.45}>
                          {line}
                        </tspan>
                      ))}
                    </text>
                  )}
                  {badge && (
                    <StatusBadge
                      text={badge}
                      x={plateLeft + plate.width - statusBadgeWidth(badge) - margin}
                      y={headerY - STATUS_BADGE.height / 2}
                    />
                  )}
                </>
              )}
            </g>
          );
        })}
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
