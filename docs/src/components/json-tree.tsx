"use client";

import { Popover } from "@base-ui/react/popover";
import { ChevronRight } from "lucide-react";
import { useState } from "react";

export type JsonPrimitive = boolean | number | string | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type JsonSchema = {
  $defs?: Record<string, JsonSchema>;
  $ref?: string;
  additionalProperties?: boolean | JsonSchema;
  allOf?: JsonSchema[];
  anyOf?: JsonSchema[];
  const?: JsonValue;
  default?: JsonValue;
  description?: string;
  enum?: JsonValue[];
  examples?: JsonValue[];
  items?: JsonSchema;
  maximum?: number;
  minimum?: number;
  oneOf?: JsonSchema[];
  properties?: Record<string, JsonSchema>;
  required?: string[];
  title?: string;
  type?: string | string[];
  "rift:since"?: string;
};

const PAYLOAD_CLASS =
  "min-w-max whitespace-nowrap px-2 py-3 font-mono text-[0.75rem] sm:text-[0.8125rem]";

function localReference(root: JsonSchema, reference: string): JsonSchema | undefined {
  if (!reference.startsWith("#/")) return undefined;

  let current: unknown = root;
  for (const encodedSegment of reference.slice(2).split("/")) {
    const segment = encodedSegment.replaceAll("~1", "/").replaceAll("~0", "~");
    if (typeof current !== "object" || current === null || !(segment in current)) return undefined;
    current = (current as Record<string, unknown>)[segment];
  }
  return current as JsonSchema;
}

export function dereference(schema: JsonSchema, root: JsonSchema): JsonSchema {
  if (!schema.$ref) return schema;
  const target = localReference(root, schema.$ref);
  if (!target) return schema;

  const { $ref: _reference, ...siblings } = schema;
  return { ...target, ...siblings };
}

export function schemaType(schema: JsonSchema): string | undefined {
  if (Array.isArray(schema.type)) return schema.type.find((type) => type !== "null");
  return schema.type;
}

function primitiveText(value: JsonPrimitive): string {
  return value === null ? "null" : JSON.stringify(value);
}

function DescriptionKey({ name, description }: { name: string; description?: string }) {
  const label = JSON.stringify(name);
  if (!description) return <span className="text-foreground">{label}</span>;

  return (
    <Popover.Root>
      <Popover.Trigger
        openOnHover
        delay={180}
        className="cursor-help border-b border-dotted border-foreground/35 text-foreground outline-none focus-visible:border-foreground"
      >
        {label}
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner sideOffset={8} align="start" className="z-[70]">
          <Popover.Popup className="max-w-[min(22rem,calc(100vw-2rem))] border border-border bg-popover px-3 py-2 font-sans text-xs leading-relaxed text-popover-foreground shadow-lg outline-none">
            <Popover.Description>{description}</Popover.Description>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}

function ShowMore({
  depth,
  hidden,
  onShow,
  step,
}: {
  depth: number;
  hidden: number;
  onShow: (update: (shown: number) => number) => void;
  step: number;
}) {
  return (
    <div style={{ paddingInlineStart: `${depth * 0.75}rem` }} className="leading-6">
      <button
        type="button"
        onClick={() => onShow((shown) => shown + step)}
        className="ml-4 text-muted-foreground underline underline-offset-4 hover:text-foreground focus-visible:text-foreground focus-visible:outline-none"
      >
        Show {Math.min(hidden, step)} more of {hidden}
      </button>
    </div>
  );
}

/**
 * Bounds for a tree whose value is not authored. Both default to no bound, the
 * rendering of the documentation examples.
 */
type TreeBounds = {
  /** Most entries one object or array lists before a "show more" row. */
  entryLimit?: number;
  /** Deepest level that starts expanded; deeper nodes start collapsed. */
  openDepth?: number;
};

type JsonNodeProps = TreeBounds & {
  comma?: boolean;
  depth: number;
  fallbackDescription?: string;
  name?: string;
  root: JsonSchema;
  schema: JsonSchema;
  value: JsonValue;
};

function JsonNode({
  comma = false,
  depth,
  entryLimit = Number.POSITIVE_INFINITY,
  fallbackDescription,
  name,
  openDepth = Number.POSITIVE_INFINITY,
  root,
  schema,
  value,
}: JsonNodeProps) {
  const expandable = typeof value === "object" && value !== null;
  const [open, setOpen] = useState(depth <= openDepth);
  const [shown, setShown] = useState(entryLimit);
  const resolved = dereference(schema, root);
  const suffix = comma ? "," : "";
  const indentation = { paddingInlineStart: `${depth * 0.75}rem` };
  const key =
    name === undefined ? null : (
      <>
        <DescriptionKey name={name} description={resolved.description ?? fallbackDescription} />
        <span className="text-muted-foreground">: </span>
      </>
    );

  if (!expandable) {
    return (
      <div style={indentation} className="min-w-0 break-words leading-6">
        {key}
        <span className="text-foreground/75">{primitiveText(value)}</span>
        <span className="text-muted-foreground">{suffix}</span>
      </div>
    );
  }

  const array = Array.isArray(value);
  const entries = array
    ? value.map((item, index) => [String(index), item] as const)
    : Object.entries(value);
  const opening = array ? "[" : "{";
  const closing = array ? "]" : "}";

  if (!open) {
    return (
      <div style={indentation} className="flex min-w-0 items-baseline leading-6">
        <button
          type="button"
          aria-label={`Expand ${name ?? "value"}`}
          aria-expanded={false}
          onClick={() => setOpen(true)}
          className="mr-0.5 inline-flex size-4 shrink-0 items-center justify-center text-muted-foreground hover:text-foreground focus-visible:outline-none"
        >
          <ChevronRight className="size-3" aria-hidden="true" />
        </button>
        <span className="min-w-0 break-words">
          {key}
          <span className="text-muted-foreground">
            {array ? "[…]" : "{…}"}
            {suffix}
          </span>
        </span>
      </div>
    );
  }

  const hidden = Math.max(0, entries.length - shown);

  return (
    <div>
      <div style={indentation} className="flex min-w-0 items-baseline leading-6">
        <button
          type="button"
          aria-label={`Collapse ${name ?? "value"}`}
          aria-expanded
          onClick={() => setOpen(false)}
          className="mr-0.5 inline-flex size-4 shrink-0 rotate-90 items-center justify-center text-muted-foreground hover:text-foreground focus-visible:outline-none"
        >
          <ChevronRight className="size-3" aria-hidden="true" />
        </button>
        <span>
          {key}
          <span className="text-muted-foreground">{opening}</span>
        </span>
      </div>
      {entries.slice(0, shown).map(([entryName, entryValue], index) => {
        const entrySchema = array
          ? (resolved.items ?? {})
          : (resolved.properties?.[entryName] ?? {});
        return (
          <JsonNode
            key={entryName}
            name={array ? undefined : entryName}
            fallbackDescription={resolved.description}
            value={entryValue}
            schema={entrySchema}
            root={root}
            depth={depth + 1}
            comma={index < entries.length - 1}
            entryLimit={entryLimit}
            openDepth={openDepth}
          />
        );
      })}
      {hidden > 0 ? (
        <ShowMore hidden={hidden} step={entryLimit} depth={depth + 1} onShow={setShown} />
      ) : null}
      <div style={indentation} className="leading-6 text-muted-foreground">
        <span className="ml-4">
          {closing}
          {suffix}
        </span>
      </div>
    </div>
  );
}

/**
 * A JSON value as a collapsible tree. A key whose schema carries a description
 * shows it on hover.
 */
export function PayloadTree({
  className = PAYLOAD_CLASS,
  entryLimit = Number.POSITIVE_INFINITY,
  openDepth,
  schema,
  value,
}: TreeBounds & { className?: string; schema: JsonSchema; value: JsonValue }) {
  const [shown, setShown] = useState(entryLimit);
  const resolved = dereference(schema, schema);

  if (typeof value !== "object" || value === null) {
    return (
      <div className={className}>
        <div className="leading-6 text-foreground/75">{primitiveText(value)}</div>
      </div>
    );
  }

  const array = Array.isArray(value);
  const entries = Array.isArray(value)
    ? value.map((item, index) => [String(index), item] as const)
    : Object.entries(value as Record<string, JsonValue>);
  const hidden = Math.max(0, entries.length - shown);
  return (
    <div className={className}>
      <div className="leading-6 text-muted-foreground">{array ? "[" : "{"}</div>
      {entries.slice(0, shown).map(([name, entryValue], index) => {
        const entrySchema = array ? (resolved.items ?? {}) : (resolved.properties?.[name] ?? {});
        return (
          <JsonNode
            key={name}
            name={array ? undefined : name}
            fallbackDescription={resolved.description}
            value={entryValue}
            schema={entrySchema}
            root={schema}
            depth={1}
            comma={index < entries.length - 1}
            entryLimit={entryLimit}
            openDepth={openDepth}
          />
        );
      })}
      {hidden > 0 ? (
        <ShowMore hidden={hidden} step={entryLimit} depth={1} onShow={setShown} />
      ) : null}
      <div className="leading-6 text-muted-foreground">{array ? "]" : "}"}</div>
    </div>
  );
}
