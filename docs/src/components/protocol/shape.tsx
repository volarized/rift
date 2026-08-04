import type { ReactNode } from "react";
import { PropertyTable } from "@/components/protocol/property-table";
import { Prose } from "@/components/protocol/prose";
import { Constraints } from "@/components/protocol/type-expr";
import { defs } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";

/**
 * One named type, shown where it is used rather than in a list somewhere else.
 *
 * A tool's input and output are the two things a reader actually came for, so
 * they get their own heading here. The name links to the reference entry, which
 * is the same type with everything this view leaves out — its branches, its
 * examples, and what else refers to it.
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
        <a href={hrefFor(name)} className="no-underline hover:underline">
          {name}
        </a>
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
