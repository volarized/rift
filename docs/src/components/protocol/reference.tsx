import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import {
  AXES,
  axisOf,
  axisSummary,
  type ReferenceNode,
  referenceTree,
} from "@/lib/protocol-surface";

function flatten(nodes: ReferenceNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    out.push(node.name);
    flatten(node.children, out);
  }
  return out;
}

/**
 * Every type, in the groups `core.json` declares under `rift:axes` — heading,
 * summary and membership all come from there, so the page cannot describe a
 * grouping the schema does not have.
 *
 * Definitions print in tree order, a type following the one that reaches it.
 * The nesting itself is carried by the table of contents, which is where an
 * outline of two hundred names is useful rather than something to scroll past.
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
            <p>{axisSummary[axis]}</p>
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
