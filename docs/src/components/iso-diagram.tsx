import { IsoCanvas } from "@/components/iso-canvas";
import { markFor } from "@/lib/iso-marks";
import type { IsoMetrics } from "@/lib/iso-scene";
import { buildScene } from "@/lib/iso-scene";
import { parseFlowchart } from "@/lib/mermaid-flowchart";
import { cn } from "@/lib/utils";

/**
 * A mermaid flowchart, drawn as an isometric scene.
 *
 * The two halves run in different places, on purpose.
 *
 *   - *This* file is a Server Component. The mermaid parse and the dagre layout
 *     happen here, which means they happen once during the build: neither
 *     library is ever sent to a browser, and the page carries only the finished
 *     coordinates. That matters because `next.config.mjs` sets
 *     `output: "export"` - there is no runtime to lay a graph out on.
 *   - `IsoCanvas` is a Client Component, because the drawing is react-three-
 *     fiber. It receives the scene as plain JSON and never re-runs the layout.
 *
 * The seam between them is why the scene is plain data: numbers and strings
 * cross a server/client boundary, closures and components do not.
 */
export type IsoDiagramProps = {
  /** A mermaid flowchart definition. The graph, and only the graph. */
  chart: string;
  /**
   * What the diagram says, for anyone who cannot see it. Required, because a
   * canvas of eight plates and seven arrows is otherwise nothing at all.
   */
  alt: string;
  /** Overrides for the scene's proportions. See `IsoMetrics`. */
  metrics?: Partial<IsoMetrics>;
  className?: string;
};

export async function IsoDiagram({ chart, alt, metrics, className }: IsoDiagramProps) {
  const flow = await parseFlowchart(chart);
  const scene = buildScene(flow, markFor, metrics);

  return (
    <figure className={cn("my-8 min-w-0", className)}>
      <IsoCanvas scene={scene} alt={alt} />
    </figure>
  );
}
