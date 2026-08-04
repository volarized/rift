import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import { Constraints, TypeExpr } from "@/components/protocol/type-expr";
import { props, type Schema, sub } from "@/lib/protocol";

/** The inline object hiding behind a property, if there is one. */
function inlineObject(schema: Schema): Schema | null {
  if (schema.properties) return schema;
  const items = sub(schema.items);
  if (items?.properties) return items;
  return null;
}

/**
 * A definition's properties.
 *
 * Nested inline objects are rendered as a nested table rather than flattened to
 * the word `object` — there are only a handful of them in the schema, and each
 * is a real part of the contract.
 */
export function PropertyTable({ schema }: { schema: Schema }): ReactNode {
  const properties = props(schema);
  if (properties.length === 0) return null;

  const required = new Set(schema.required ?? []);

  return (
    <div className="my-4 overflow-x-auto">
      <table className="my-0 w-full text-sm">
        <thead>
          <tr>
            <th className="text-left">Property</th>
            <th className="text-left">Type</th>
            <th className="text-left">Description</th>
          </tr>
        </thead>
        <tbody>
          {properties.map(([name, property]) => {
            const nested = inlineObject(property);
            const constraints = <Constraints schema={property} />;
            return (
              <tr key={name} className="align-top">
                <td className="whitespace-nowrap">
                  <code className="text-[0.875em]">{name}</code>
                  {required.has(name) ? (
                    <sup className="ml-1 text-fd-muted-foreground text-[0.7em] uppercase tracking-wide">
                      req
                    </sup>
                  ) : null}
                </td>
                <td>
                  <TypeExpr schema={property} />
                </td>
                <td>
                  {property.description ? <Prose text={property.description} /> : null}
                  {property.description && constraints ? <br /> : null}
                  {constraints}
                  {nested ? (
                    <details className="mt-2">
                      <summary className="cursor-pointer text-fd-muted-foreground text-sm">
                        nested object
                      </summary>
                      <PropertyTable schema={nested} />
                    </details>
                  ) : null}
                  {!property.description && !constraints && !nested ? (
                    <span className="text-fd-muted-foreground">—</span>
                  ) : null}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {schema.additionalProperties === false ? (
        <p className="mt-2 text-fd-muted-foreground text-sm">
          Closed: no properties beyond those listed.
        </p>
      ) : null}
    </div>
  );
}
