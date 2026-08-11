import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { type ReferenceNode, surfaceFor } from "@/lib/protocol-surface";
import type { DocVersion } from "@/lib/versions";

function flatten(nodes: ReferenceNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    out.push(node.name);
    flatten(node.children, out);
  }
  return out;
}

/**
 * Every type, in the groups `mcp.json` declares under `rift:axes` — heading,
 * summary and membership all come from there, so the page cannot describe a
 * grouping the schema does not have.
 *
 * Definitions print in tree order, a type following the one that reaches it.
 * The nesting itself is carried by the table of contents, which is where an
 * outline of two hundred names is useful rather than something to scroll past.
 */
export function Reference({ version }: { version: DocVersion }): ReactNode {
  const { axes, axisSummary, referenceTree } = surfaceFor(version);
  return (
    <>
      {axes.map((axis) => {
        const names = flatten(referenceTree[axis] ?? []);
        if (names.length === 0) return null;
        return (
          <section key={axis}>
            <h2 id={`axis-${axis.toLowerCase()}`} className="scroll-m-28">
              {axis}
            </h2>
            <p>{axisSummary[axis]}</p>
            {names.map((name) => (
              <Definition key={name} name={name} version={version} />
            ))}
          </section>
        );
      })}
    </>
  );
}
