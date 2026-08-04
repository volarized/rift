import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { homeOf } from "@/lib/protocol";
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

/** Which document defines a type. Core is the default, so it carries no badge. */
function Badge({ name }: { name: string }): ReactNode {
  const home = homeOf[name];
  if (home === "core") return null;
  return (
    <span className="ml-2 rounded bg-fd-muted px-1.5 py-0.5 align-middle font-mono font-normal text-[0.6em] text-fd-muted-foreground uppercase tracking-wide">
      {home}
    </span>
  );
}

function Branch({ node, depth }: { node: ReferenceNode; depth: number }): ReactNode {
  return (
    <li>
      <a href={`#${node.name}`} className="font-mono text-sm no-underline hover:underline">
        {node.name}
      </a>
      <Badge name={node.name} />
      {node.children.length > 0 ? (
        <ul className="my-0 border-fd-border border-l pl-4">
          {node.children.map((child) => (
            <Branch key={child.name} node={child} depth={depth + 1} />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

function flatten(nodes: ReferenceNode[], out: string[] = []): string[] {
  for (const node of nodes) {
    out.push(node.name);
    flatten(node.children, out);
  }
  return out;
}

/**
 * Every type, grouped by the axis it belongs to and nested under the type that
 * reaches it. A flat list of two hundred names says nothing about which type
 * reaches which; the tree is the shape of the model.
 *
 * Each axis prints its tree first, then the definitions in that same order, so
 * scrolling a section follows the same path as reading its outline.
 */
export function Reference(): ReactNode {
  return (
    <>
      {AXES.map((axis) => {
        const tree = referenceTree[axis] ?? [];
        if (tree.length === 0) return null;
        const names = flatten(tree);
        return (
          <section key={axis}>
            <h2 id={`axis-${axis.toLowerCase()}`} className="scroll-m-20">
              {axis}
            </h2>
            <p>{AXIS_BLURB[axis]}</p>
            <p className="text-fd-muted-foreground text-sm">{names.length} types</p>
            <ul className="my-4">
              {tree.map((node) => (
                <Branch key={node.name} node={node} depth={0} />
              ))}
            </ul>
            {names.map((name) => (
              <Definition key={name} name={name} />
            ))}
          </section>
        );
      })}
    </>
  );
}

/** Exported for the page's table of contents and search index. */
export function referenceOutline(): { axis: string; names: string[] }[] {
  return AXES.map((axis) => ({ axis, names: flatten(referenceTree[axis] ?? []) })).filter(
    (entry) => entry.names.length > 0,
  );
}

export { axisOf };
