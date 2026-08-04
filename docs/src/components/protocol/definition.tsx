import type { ReactNode } from "react";
import { PropertyTable } from "@/components/protocol/property-table";
import { Prose } from "@/components/protocol/prose";
import { Constraints, TypeExpr, TypeLink } from "@/components/protocol/type-expr";
import {
  backlinks,
  branches,
  defs,
  enumDescriptions,
  homeOf,
  isLossy,
  props,
  refName,
  type Schema,
  sub,
} from "@/lib/protocol";

/**
 * The property a union branch is tagged by. Most carry a `const`; eleven unions
 * in the schema are tagged by a disjoint enum instead, so fall back to that
 * rather than labelling those branches "variant 3".
 */
function discriminant(branch: Schema): string | null {
  const properties = props(branch);
  for (const [name, property] of properties) {
    if ("const" in property) return `${name} = ${JSON.stringify(property.const)}`;
  }
  for (const [name, property] of properties) {
    if (property.enum)
      return `${name} = ${property.enum.map((value) => JSON.stringify(value)).join(" | ")}`;
  }
  return null;
}

function Variants({ schema }: { schema: Schema }): ReactNode {
  const union = branches(schema);
  if (!union) return null;

  // `SearchParams` and friends: branches that only assert which fields must be
  // present, rather than describing alternative shapes.
  const constraintOnly = union.list.every(
    (branch) => Object.keys(branch).length === 1 && branch.required,
  );
  if (constraintOnly) {
    return (
      <p>
        At least one of:{" "}
        {union.list.map((branch, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
          <span key={index}>
            {index > 0 ? " | " : null}
            <code className="text-[0.875em]">{(branch.required ?? []).join(" + ")}</code>
          </span>
        ))}
        .
      </p>
    );
  }

  return (
    <div className="my-4">
      <p className="font-medium">{union.keyword === "oneOf" ? "One of" : "Any of"}</p>
      <ul>
        {union.list.map((branch, index) => {
          if (!branch.properties) {
            return (
              // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
              <li key={index}>
                <TypeExpr schema={branch} />
                {branch.description ? (
                  <>
                    {" — "}
                    <Prose text={branch.description} />
                  </>
                ) : null}
              </li>
            );
          }
          const label = discriminant(branch);
          return (
            // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
            <li key={index}>
              <details>
                <summary className="cursor-pointer">
                  <code className="text-[0.875em]">{label ?? `variant ${index + 1}`}</code>
                </summary>
                {branch.description ? (
                  <p>
                    <Prose text={branch.description} />
                  </p>
                ) : null}
                <PropertyTable schema={branch} />
              </details>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** A definition's enum values, described one by one where the schema says what they mean. */
function EnumValues({ schema }: { schema: Schema }): ReactNode {
  if (!schema.enum) return null;
  const described = enumDescriptions(schema);
  const label = (value: unknown) => (typeof value === "string" ? value : JSON.stringify(value));

  if (!described) {
    return (
      <p>
        Values:{" "}
        {schema.enum.map((value, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
          <span key={index}>
            {index > 0 ? " · " : null}
            <code className="text-[0.875em]">{label(value)}</code>
          </span>
        ))}
      </p>
    );
  }

  return (
    <div className="my-4">
      <p className="font-medium">Values</p>
      <ul>
        {schema.enum.map((value, index) => {
          const description = described[label(value)];
          return (
            // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
            <li key={index}>
              <code className="text-[0.875em]">{label(value)}</code>
              {description ? (
                <>
                  {" — "}
                  <Prose text={description} />
                </>
              ) : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** The shape of a definition, whichever of the schema's forms it takes. */
function Shape({ schema }: { schema: Schema }): ReactNode {
  const alias = refName(schema);
  const mapValues = schema.properties ? undefined : sub(schema.additionalProperties);
  const items = sub(schema.items);

  return (
    <>
      {alias ? (
        <p>
          Alias of <TypeLink name={alias} />.
        </p>
      ) : null}

      <EnumValues schema={schema} />

      {"const" in schema && !schema.enum ? (
        <p>
          Constant: <code className="text-[0.875em]">{JSON.stringify(schema.const)}</code>
        </p>
      ) : null}

      <Variants schema={schema} />
      <PropertyTable schema={schema} />

      {mapValues ? (
        <p>
          Map: <TypeExpr schema={sub(schema.propertyNames) ?? { type: "string" }} /> to{" "}
          <TypeExpr schema={mapValues} />
        </p>
      ) : null}

      {schema.type === "array" && items ? (
        <>
          <p>
            Items: <TypeExpr schema={items} />
          </p>
          {items.properties ? <PropertyTable schema={items} /> : null}
        </>
      ) : null}
    </>
  );
}

export function Definition({ name }: { name: string }): ReactNode {
  const schema = defs[name];
  const referrers = backlinks[name] ?? [];
  const examples = schema.examples ?? [];

  return (
    <section>
      <h3 id={name} className="scroll-m-20 font-mono">
        {name}
        {/* Core is the shared vocabulary and the default; the other two are
            where a type exists on one seam only, which is worth saying. */}
        {homeOf[name] === "core" ? null : (
          <span className="ml-2 rounded bg-fd-muted px-1.5 py-0.5 align-middle font-mono font-normal text-[0.6em] text-fd-muted-foreground uppercase tracking-wide">
            {homeOf[name]}
          </span>
        )}
      </h3>

      {schema.description ? (
        <p>
          <Prose text={schema.description} />
        </p>
      ) : null}

      <Constraints schema={schema} />
      <Shape schema={schema} />

      {examples.map((example, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
        <details key={index}>
          <summary className="cursor-pointer text-fd-muted-foreground text-sm">
            {examples.length > 1 ? `Example ${index + 1}` : "Example"}
          </summary>
          <pre>
            <code>{JSON.stringify(example, null, 2)}</code>
          </pre>
        </details>
      ))}

      {/*
        Conditional keywords the layout above cannot express. Emitting the
        verbatim schema is the honest fallback: a constraint that reaches the
        page as silence is worse than one that reaches it as JSON.
      */}
      {isLossy(schema) ? (
        <details>
          <summary className="cursor-pointer text-fd-muted-foreground text-sm">
            Conditional constraints (raw JSON Schema)
          </summary>
          <pre>
            <code>{JSON.stringify(schema, null, 2)}</code>
          </pre>
        </details>
      ) : null}

      {referrers.length > 0 ? (
        <details open={referrers.length <= 12}>
          <summary className="cursor-pointer text-fd-muted-foreground text-sm">
            Referenced by {referrers.length} {referrers.length === 1 ? "definition" : "definitions"}
          </summary>
          <p className="text-sm">
            {referrers.map((referrer, index) => (
              <span key={referrer}>
                {index > 0 ? " · " : null}
                <TypeLink name={referrer} />
              </span>
            ))}
          </p>
        </details>
      ) : null}
    </section>
  );
}
