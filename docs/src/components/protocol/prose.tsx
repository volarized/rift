import type { ReactNode } from "react";

/**
 * Schema prose, with backtick spans rendered as code.
 *
 * These descriptions carry the contract's reasoning, not just its shape, so
 * they are rendered whole. React escapes text nodes, which is why none of the
 * MDX hazards apply here — `RenderTemplate`'s "{{capture:NAME}}" and
 * `ProjectPath`'s pattern are plain strings, not expressions to escape.
 */
export function Prose({ text }: { text: string }): ReactNode {
  return text.split(/(`[^`]*`)/g).map((part, index) => {
    if (index % 2 === 1) {
      return (
        // biome-ignore lint/suspicious/noArrayIndexKey: split output is positional and stable
        <code key={index} className="text-[0.875em]">
          {part.slice(1, -1)}
        </code>
      );
    }
    // biome-ignore lint/suspicious/noArrayIndexKey: split output is positional and stable
    return <span key={index}>{part}</span>;
  });
}
