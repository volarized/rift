"use client";

import { Dialog } from "@base-ui/react/dialog";
import { Popover } from "@base-ui/react/popover";
import { ChevronRight, Maximize2, X } from "lucide-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import mcpDocumentJson from "../../public/mcp.json";

type JsonPrimitive = boolean | number | string | null;
type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

type JsonSchema = {
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
  minimum?: number;
  oneOf?: JsonSchema[];
  properties?: Record<string, JsonSchema>;
  required?: string[];
  title?: string;
  type?: string | string[];
  "rift:since"?: string;
};

type McpToolDefinition = {
  description: string;
  input_schema: JsonSchema;
  name: string;
  output_schema: JsonSchema;
};

type McpDocument = {
  tools: McpToolDefinition[];
};

const mcpDocument = mcpDocumentJson as unknown as McpDocument;
const RECURSION_DEPTH_MAX = 8;

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

function dereference(schema: JsonSchema, root: JsonSchema): JsonSchema {
  if (!schema.$ref) return schema;
  const target = localReference(root, schema.$ref);
  if (!target) return schema;

  const { $ref: _reference, ...siblings } = schema;
  return { ...target, ...siblings };
}

function schemaType(schema: JsonSchema): string | undefined {
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

type JsonNodeProps = {
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
  fallbackDescription,
  name,
  root,
  schema,
  value,
}: JsonNodeProps) {
  const expandable = typeof value === "object" && value !== null;
  const [open, setOpen] = useState(true);
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
      {entries.map(([entryName, entryValue], index) => {
        const entrySchema = array
          ? (resolved.items ?? {})
          : (resolved.properties?.[entryName] ?? {});
        return (
          <JsonNode
            key={array ? JSON.stringify(entryValue) : entryName}
            name={array ? undefined : entryName}
            fallbackDescription={resolved.description}
            value={entryValue}
            schema={entrySchema}
            root={root}
            depth={depth + 1}
            comma={index < entries.length - 1}
          />
        );
      })}
      <div style={indentation} className="leading-6 text-muted-foreground">
        <span className="ml-4">
          {closing}
          {suffix}
        </span>
      </div>
    </div>
  );
}

function PayloadTree({ schema, value }: { schema: JsonSchema; value: JsonValue }) {
  const resolved = dereference(schema, schema);

  if (typeof value !== "object" || value === null) {
    return (
      <div className="min-w-max whitespace-nowrap px-2 py-3 font-mono text-[0.75rem] sm:text-[0.8125rem]">
        <div className="leading-6 text-foreground/75">{primitiveText(value)}</div>
      </div>
    );
  }

  const array = Array.isArray(value);
  const entries = Array.isArray(value)
    ? value.map((item, index) => [String(index), item] as const)
    : Object.entries(value as Record<string, JsonValue>);
  return (
    <div className="min-w-max whitespace-nowrap px-2 py-3 font-mono text-[0.75rem] sm:text-[0.8125rem]">
      <div className="leading-6 text-muted-foreground">{array ? "[" : "{"}</div>
      {entries.map(([name, entryValue], index) => {
        const entrySchema = array ? (resolved.items ?? {}) : (resolved.properties?.[name] ?? {});
        return (
          <JsonNode
            key={array ? JSON.stringify(entryValue) : name}
            name={array ? undefined : name}
            fallbackDescription={resolved.description}
            value={entryValue}
            schema={entrySchema}
            root={schema}
            depth={1}
            comma={index < entries.length - 1}
          />
        );
      })}
      <div className="leading-6 text-muted-foreground">{array ? "]" : "}"}</div>
    </div>
  );
}

function schemaTypeText(schema: JsonSchema, root: JsonSchema): string {
  const resolved = dereference(schema, root);
  if (resolved.const !== undefined) return JSON.stringify(resolved.const);
  if (resolved.enum) return resolved.enum.map((value) => JSON.stringify(value)).join(" | ");
  if (resolved.oneOf) return "oneOf";
  if (resolved.anyOf) return "anyOf";
  if (resolved.allOf) return "allOf";
  return Array.isArray(resolved.type)
    ? resolved.type.join(" | ")
    : (schemaType(resolved) ?? "unknown");
}

function SchemaProperties({
  schema,
  root,
  depth = 0,
  references = new Set<string>(),
}: {
  schema: JsonSchema;
  root: JsonSchema;
  depth?: number;
  references?: Set<string>;
}) {
  const resolved = dereference(schema, root);
  if (depth >= RECURSION_DEPTH_MAX || (schema.$ref && references.has(schema.$ref))) {
    return <code className="text-xs">{schema.$ref ?? schemaTypeText(schema, root)}</code>;
  }
  const nextReferences = new Set(references);
  if (schema.$ref) nextReferences.add(schema.$ref);
  const groups = [
    ["oneOf", resolved.oneOf],
    ["anyOf", resolved.anyOf],
    ["allOf", resolved.allOf],
  ] as const;
  return (
    <div className="space-y-3">
      {groups.map(([keyword, variants]) =>
        variants?.map((variant, index) => {
          const branch = dereference(variant, root);
          return (
            <details
              key={`${keyword}-${JSON.stringify(variant)}`}
              open={depth < 1}
              className="border-l border-border pl-3"
            >
              <summary className="cursor-pointer text-xs font-medium">
                {keyword} {index + 1}:{" "}
                {branch.title ?? branch.description ?? schemaTypeText(branch, root)}
              </summary>
              <div className="mt-2">
                <SchemaProperties
                  schema={variant}
                  root={root}
                  depth={depth + 1}
                  references={nextReferences}
                />
              </div>
            </details>
          );
        }),
      )}
      {resolved.properties ? (
        <table className="w-full border-collapse text-left text-xs">
          <thead>
            <tr className="border-b border-border text-muted-foreground">
              <th className="py-2 pr-3">Property</th>
              <th className="py-2 pr-3">Type</th>
              <th className="py-2 pr-3">Required</th>
              <th className="py-2">Description</th>
            </tr>
          </thead>
          <tbody>
            {Object.entries(resolved.properties).map(([name, property]) => {
              const field = dereference(property, root);
              const nested =
                field.properties ||
                field.items ||
                typeof field.additionalProperties === "object" ||
                field.oneOf ||
                field.anyOf ||
                field.allOf;
              return (
                <tr key={name} className="border-b border-border align-top">
                  <td className="py-2 pr-3 font-mono">{name}</td>
                  <td className="py-2 pr-3 font-mono">{schemaTypeText(property, root)}</td>
                  <td className="py-2 pr-3">{resolved.required?.includes(name) ? "yes" : "no"}</td>
                  <td className="py-2">
                    {field.description ? <p>{field.description}</p> : null}
                    {field.default !== undefined ? (
                      <p className="mt-1">
                        Default: <code>{JSON.stringify(field.default)}</code>
                      </p>
                    ) : null}
                    {nested ? (
                      <details className="mt-2" open={depth < 1}>
                        <summary className="cursor-pointer">Properties</summary>
                        <div className="mt-2">
                          <SchemaProperties
                            schema={property}
                            root={root}
                            depth={depth + 1}
                            references={nextReferences}
                          />
                        </div>
                      </details>
                    ) : null}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      ) : null}
      {resolved.items ? (
        <div>
          <p className="mb-2 text-xs font-medium">Items</p>
          <SchemaProperties
            schema={resolved.items}
            root={root}
            depth={depth + 1}
            references={nextReferences}
          />
        </div>
      ) : null}
      {typeof resolved.additionalProperties === "boolean" ? (
        <p className="text-xs">
          Additional properties: <code>{String(resolved.additionalProperties)}</code>
        </p>
      ) : null}
      {typeof resolved.additionalProperties === "object" ? (
        <div>
          <p className="mb-2 text-xs font-medium">Additional properties</p>
          <SchemaProperties
            schema={resolved.additionalProperties}
            root={root}
            depth={depth + 1}
            references={nextReferences}
          />
        </div>
      ) : null}
      {!resolved.properties &&
      !resolved.items &&
      !groups.some(([, variants]) => variants) &&
      typeof resolved.additionalProperties !== "object" ? (
        <code className="text-xs">{schemaTypeText(schema, root)}</code>
      ) : null}
    </div>
  );
}

function JsonShape({
  fullscreen = false,
  label,
  schema,
}: {
  fullscreen?: boolean;
  label: "input" | "output";
  schema: JsonSchema;
}) {
  const authored =
    Array.isArray(schema.examples) && schema.examples.length > 0 ? schema.examples : undefined;
  const exampleLabel = label === "input" ? "Request" : "Response";
  const payload = authored ? (
    <div>
      {authored.map((example, index) => (
        <div
          key={JSON.stringify(example)}
          className={index > 0 ? "min-w-max border-t border-border" : "min-w-max"}
        >
          <div className="px-2 pt-3 font-sans text-xs font-semibold text-foreground">
            {authored.length > 1 ? `${exampleLabel} ${index + 1}:` : `${exampleLabel}:`}
          </div>
          <PayloadTree schema={schema} value={example} />
        </div>
      ))}
    </div>
  ) : null;

  return (
    <div
      className={
        fullscreen
          ? "flex min-h-0 min-w-0 flex-col overflow-hidden bg-background"
          : "flex max-h-[85vh] min-w-0 flex-col overflow-hidden bg-background"
      }
    >
      <div className="flex shrink-0 items-center border-b border-border px-3 py-2 font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground">
        <span>{label}</span>
      </div>
      <div className="min-h-0 min-w-0 flex-1 overflow-auto">
        <div className="border-b border-border px-3 py-3">
          <h4 className="mb-3 text-xs font-semibold">
            {label === "input" ? "Arguments" : "Result properties"}
          </h4>
          <SchemaProperties schema={schema} root={schema} />
        </div>
        {payload}
      </div>
    </div>
  );
}

export function McpTool({ name }: { name: string }) {
  const tool = mcpDocument.tools.find((candidate) => candidate.name === name);
  if (!tool) throw new Error(`MCP schema has no tool named ${name}`);

  const planned = tool.input_schema["rift:since"] === undefined;
  return (
    <section
      id={`tool-${tool.name}`}
      className="not-prose my-8 scroll-mt-24 border-y border-border py-5"
    >
      <header className="mb-4">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="m-0 font-mono text-lg font-medium tracking-tight">{tool.name}</h3>
          {planned ? (
            <Badge
              variant="outline"
              className="font-mono text-[0.625rem] uppercase tracking-[0.12em]"
            >
              Planned
            </Badge>
          ) : null}
        </div>
        <p className="mt-2 max-w-3xl whitespace-pre-line text-sm leading-relaxed text-muted-foreground">
          {tool.description}
        </p>
      </header>
      <Dialog.Root>
        <div className="overflow-hidden border border-border bg-border">
          <div className="flex items-center justify-between border-b border-border bg-background px-3 py-1.5">
            <span className="font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground">
              Payloads
            </span>
            <Dialog.Trigger
              render={
                <Button
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Open ${tool.name} payloads in fullscreen`}
                />
              }
            >
              <Maximize2 aria-hidden="true" />
            </Dialog.Trigger>
          </div>
          <div className="grid grid-cols-1 gap-px md:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]">
            <JsonShape label="input" schema={tool.input_schema} />
            <JsonShape label="output" schema={tool.output_schema} />
          </div>
        </div>
        <Dialog.Portal>
          <Dialog.Backdrop className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm" />
          <Dialog.Viewport className="fixed inset-0 z-[60] flex p-2 sm:p-4">
            <Dialog.Popup className="flex min-h-0 w-full flex-col overflow-hidden border border-border bg-background shadow-2xl outline-none">
              <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-2">
                <Dialog.Title className="font-mono text-sm font-medium">
                  {tool.name} payloads
                </Dialog.Title>
                <Dialog.Close
                  render={
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={`Close ${tool.name} payloads fullscreen view`}
                    />
                  }
                >
                  <X aria-hidden="true" />
                </Dialog.Close>
              </div>
              <div className="grid min-h-0 flex-1 grid-cols-1 grid-rows-2 gap-px bg-border md:grid-cols-[minmax(0,1fr)_minmax(0,2fr)] md:grid-rows-1">
                <JsonShape fullscreen label="input" schema={tool.input_schema} />
                <JsonShape fullscreen label="output" schema={tool.output_schema} />
              </div>
            </Dialog.Popup>
          </Dialog.Viewport>
        </Dialog.Portal>
      </Dialog.Root>
    </section>
  );
}
