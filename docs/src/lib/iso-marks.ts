/**
 * Which mermaid class puts which mark on a plate.
 *
 * Only the *names* live here. What a mark actually looks like is a React
 * component, and the renderer that holds those is a client component — so the
 * two halves have to meet on something that survives serialisation. A string
 * does; a component does not.
 *
 * A diagram picks its own marks in its own source — `agent1["agent 1"]:::agent`
 * — so nothing about a particular picture is wired in here either.
 */
export const MARK_NAMES = ["rift", "agent", "store", "adapter", "config", "hook"] as const;

export type MarkName = (typeof MARK_NAMES)[number];

const NAMES = new Set<string>(MARK_NAMES);

/** The first class on a node that names a mark wins. */
export function markFor(node: { classes: string[] }): MarkName | null {
  for (const name of node.classes) {
    if (NAMES.has(name)) return name as MarkName;
  }
  return null;
}
