import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { AXES, axisOf, type ReferenceNode, referenceTree } from "@/lib/protocol-surface";

const AXIS_BLURB: Record<string, string> = {
  Semantic: "What the compiler knows: symbols, what they carry, and how they connect.",
  Physical: "Where it is written: files, the leaves of their syntax trees, and ranges of bytes.",
  Temporal: "Which state you are looking at: revisions and the objects git names them by.",
  Protocol: "Shared machinery no axis owns: coverage, diagnostics, kinds, extensions.",
  MCP: "Shapes that exist only on the agent-facing surface — parameters, results, resource payloads.",
  Adapter:
    "Shapes that exist only on the adapter wire — frames, mirror lifecycle, streamed analysis.",
};

function flatten(nodes: ReferenceNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    out.push(node.name);
    flatten(node.children, out);
  }
  return out;
}

/**
 * Every type, grouped by the axis it belongs to.
 *
 * Definitions print in tree order — a type follows the one that reaches it —
 * and the nesting itself is carried by the table of contents, which is where
 * an outline of two hundred names is useful rather than something to scroll
 * past. See `outlineOf` in `protocol-surface.ts`.
 */
export function Reference(): ReactNode {
  return (
    <>
      {AXES.map((axis) => {
        const names = flatten(referenceTree[axis] ?? []);
        if (names.length === 0) return null;
        return (
          <section key={axis}>
            <h2 id={`axis-${axis.toLowerCase()}`} className="scroll-m-20">
              {axis}
            </h2>
            <p>{AXIS_BLURB[axis]}</p>
            <p className="text-fd-muted-foreground text-sm">{names.length} types</p>
            {names.map((name) => (
              <Definition key={name} name={name} />
            ))}
          </section>
        );
      })}
    </>
  );
}

export { axisOf };
