import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import { PropertyTable } from "@/components/protocol/property-table";
import { Constraints } from "@/components/protocol/type-expr";
import { defs } from "@/lib/protocol";

/**
 * One named type, shown where it is used rather than in a list somewhere else.
 *
 * A tool's input and output are the two things a reader actually came for, so
 * they get their own heading and carry the definition's anchor with them. That
 * is why the type reference at the bottom of each page skips whatever a surface
 * section already showed: every definition has exactly one home.
 */
export function Shape({ label, name }: { label: string; name: string }): ReactNode {
  const schema = defs[name];
  if (!schema) return null;

  return (
    <div className="my-6">
      <h4 id={name} className="scroll-m-20 font-mono text-base">
        <span className="mr-2 font-sans font-normal text-fd-muted-foreground text-sm uppercase tracking-wide">
          {label}
        </span>
        {name}
      </h4>
      {schema.description ? (
        <p>
          <Prose text={schema.description} />
        </p>
      ) : null}
      <Constraints schema={schema} />
      <PropertyTable schema={schema} />
    </div>
  );
}
