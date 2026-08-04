import { Fragment, type ReactNode } from "react";
import { branches, refName, type Schema, sub } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";

/**
 * A link to wherever a definition's heading lives. Each definition is rendered
 * once, on the page for the document that owns it, so a type used by both the
 * MCP surface and the adapter wire resolves to the same anchor from either.
 */
export function TypeLink({ name }: { name: string }): ReactNode {
  return (
    <a href={hrefFor(name)} className="font-mono text-[0.875em] no-underline hover:underline">
      {name}
    </a>
  );
}

function Literal({ value }: { value: unknown }): ReactNode {
  return (
    <code className="text-[0.875em]">
      {typeof value === "string" ? value : JSON.stringify(value)}
    </code>
  );
}

/**
 * Renders each item with `render`, separated by `separator`. Takes the items
 * rather than pre-built elements so the keys land on the elements this actually
 * creates, instead of on wrappers around an array the caller already built.
 */
function Joined<T>({
  items,
  separator,
  render,
}: {
  // readonly: json-schema-typed types every schema array as `MaybeReadonlyArray`.
  items: readonly T[];
  separator: string;
  render: (item: T) => ReactNode;
}): ReactNode {
  return items.map((item, index) => (
    // biome-ignore lint/suspicious/noArrayIndexKey: positional list over a fixed schema array
    <Fragment key={index}>
      {index > 0 ? <span className="text-fd-muted-foreground"> {separator} </span> : null}
      {render(item)}
    </Fragment>
  ));
}

/**
 * A schema as one inline phrase, with every `$ref` a link.
 *
 * Deliberately shallow: an inline object renders as `object` and is expanded by
 * the caller into its own table, so a cell never grows a second table inside
 * itself.
 */
export function TypeExpr({ schema }: { schema: Schema | undefined }): ReactNode {
  if (!schema) return <Literal value="any" />;

  const ref = refName(schema);
  if (ref) return <TypeLink name={ref} />;

  if ("const" in schema) return <Literal value={schema.const} />;
  if (schema.enum) {
    return (
      <Joined items={schema.enum} separator="|" render={(value) => <Literal value={value} />} />
    );
  }

  const union = branches(schema);
  if (union) {
    return (
      <Joined items={union.list} separator="|" render={(branch) => <TypeExpr schema={branch} />} />
    );
  }
  if (schema.allOf) {
    return (
      <Joined
        items={schema.allOf}
        separator="&"
        render={(branch) => <TypeExpr schema={sub(branch)} />}
      />
    );
  }

  if (schema.type === "array") {
    const items = sub(schema.items);
    if (!items) return <Literal value="array" />;
    return (
      <>
        <span className="text-fd-muted-foreground">array of </span>
        <TypeExpr schema={items} />
      </>
    );
  }

  if (schema.type === "object" || schema.properties) {
    const values = sub(schema.additionalProperties);
    if (values) {
      return (
        <>
          <span className="text-fd-muted-foreground">map </span>
          <TypeExpr schema={sub(schema.propertyNames) ?? { type: "string" }} />
          <span className="text-fd-muted-foreground"> to </span>
          <TypeExpr schema={values} />
        </>
      );
    }
    return <Literal value="object" />;
  }

  if (Array.isArray(schema.type)) {
    return (
      <Joined items={schema.type} separator="|" render={(value) => <Literal value={value} />} />
    );
  }
  if (typeof schema.type === "string") return <Literal value={schema.type} />;
  return <Literal value="any" />;
}

/** Everything narrowing a type that `TypeExpr` does not already show. */
export function Constraints({ schema }: { schema: Schema }): ReactNode {
  const parts: { label: string; value: unknown }[] = [];
  const push = (label: string, value: unknown) => {
    if (value !== undefined) parts.push({ label, value });
  };

  push("format", schema.format);
  push("pattern", schema.pattern);
  push("min length", schema.minLength);
  push("max length", schema.maxLength);
  push("min", schema.minimum);
  push("max", schema.maximum);
  push("exclusive min", schema.exclusiveMinimum);
  push("exclusive max", schema.exclusiveMaximum);
  push("multiple of", schema.multipleOf);
  push("min items", schema.minItems);
  push("max items", schema.maxItems);
  push("min properties", schema.minProperties);
  push("encoding", schema.contentEncoding);
  push("media type", schema.contentMediaType);
  // `default` is a column of its own in the property table.

  if (parts.length === 0 && !schema.uniqueItems) return null;

  return (
    <span className="text-fd-muted-foreground text-sm">
      {parts.map(({ label, value }, index) => (
        <Fragment key={label}>
          {index > 0 ? " · " : null}
          {label} <Literal value={value} />
        </Fragment>
      ))}
      {schema.uniqueItems ? <>{parts.length > 0 ? " · " : null}unique items</> : null}
    </span>
  );
}
