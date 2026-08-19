import type { ReactNode } from "react";
import { protocolFor } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";
import type { DocVersion } from "@/lib/versions";

/** Where a type name written in prose points, or null if it names nothing documented. */
export type TypeResolver = (name: string) => string | null;

/** The default resolver: a span that is exactly a documented type links to it. */
export function documentedIn(version: DocVersion): TypeResolver {
  const { defs } = protocolFor(version);
  return (name) => (name in defs ? hrefFor(version, name) : null);
}

/**
 * Schema prose, with backtick spans rendered as code.
 *
 * These descriptions carry the contract's reasoning, not just its shape, so
 * they are rendered whole. React escapes text nodes, which is why none of the
 * MDX hazards apply here — a metavariable like "$NAME" and the regex in a
 * pattern are plain strings, not expressions to escape.
 *
 * A span that is exactly a type name becomes a link to it. The descriptions
 * already write type names in backticks, so the alternative is a reader who
 * reads a protocol type and has to go looking. Exact and whole-span, because
 * `Cargo.lock` and `rift://file/pkg/util.py` are also written in backticks and
 * neither is a type.
 */
export function Prose({
  text,
  version,
  resolve,
}: {
  text: string;
  version?: DocVersion;
  resolve?: TypeResolver;
}): ReactNode {
  const resolver = resolve ?? (version ? documentedIn(version) : () => null);
  return text.split(/(`[^`]*`)/g).map((part, index) => {
    if (index % 2 === 1) {
      const content = part.slice(1, -1);
      const href = resolver(content);
      return href ? (
        <a
          // biome-ignore lint/suspicious/noArrayIndexKey: split output is positional and stable
          key={index}
          href={href}
          className="font-mono text-[0.875em] no-underline hover:underline"
        >
          {content}
        </a>
      ) : (
        // biome-ignore lint/suspicious/noArrayIndexKey: split output is positional and stable
        <code key={index} className="text-[0.875em]">
          {content}
        </code>
      );
    }
    // biome-ignore lint/suspicious/noArrayIndexKey: split output is positional and stable
    return <span key={index}>{part}</span>;
  });
}
