import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { defNamesByFile } from "@/lib/protocol";

/**
 * The shared model, as a type reference.
 *
 * The narrative that explains *why* these types exist lives in the page's MDX,
 * above this component — it is written by hand rather than derived, because the
 * chain from "point Rift at a repository" to "here is a symbol" is a story the
 * schema does not tell about itself.
 */
export function CoreReference(): ReactNode {
  return (
    <>
      <h2 id="type-reference" className="scroll-m-20">
        Type reference
      </h2>
      <p>
        All {defNamesByFile.core.length} types this document defines, in schema order. Related types sit
        next to each other, so reading top to bottom follows the model rather than the alphabet.
      </p>
      {defNamesByFile.core.map((name) => (
        <Definition key={name} name={name} />
      ))}
    </>
  );
}
