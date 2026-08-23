"use client";

import { Popover } from "@base-ui/react/popover";
import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import mcpDocumentJson from "../../public/mcp.json";

type JsonPrimitive = boolean | number | string | null;
type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

type JsonSchema = {
  $defs?: Record<string, JsonSchema>;
  $ref?: string;
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

function isNullSchema(schema: JsonSchema, root: JsonSchema): boolean {
  const resolved = dereference(schema, root);
  return resolved.type === "null" || resolved.const === null;
}

function structuralSchema(schema: JsonSchema, root: JsonSchema): JsonSchema {
  const resolved = dereference(schema, root);
  const hasOwnShape = resolved.properties !== undefined || resolved.type !== undefined;
  const variants = hasOwnShape ? undefined : (resolved.oneOf ?? resolved.anyOf);
  if (variants) {
    const selected = variants.find((variant) => !isNullSchema(variant, root)) ?? variants[0];
    return {
      ...structuralSchema(selected, root),
      description: resolved.description ?? structuralSchema(selected, root).description,
    };
  }

  if (!resolved.properties && resolved.allOf) {
    const selected = resolved.allOf.find((part) => {
      const candidate = dereference(part, root);
      return candidate.properties !== undefined || candidate.type !== undefined;
    });
    if (selected) return { ...structuralSchema(selected, root), description: resolved.description };
  }
  return resolved;
}

function schemaType(schema: JsonSchema): string | undefined {
  if (Array.isArray(schema.type)) return schema.type.find((type) => type !== "null");
  return schema.type;
}

function terminalExample(schema: JsonSchema): JsonPrimitive {
  const type = schemaType(schema);
  if (type === "boolean") return false;
  if (type === "integer" || type === "number") return schema.minimum ?? 0;
  if (type === "null") return null;
  return "string";
}

function exampleFromSchema(
  schema: JsonSchema,
  root: JsonSchema,
  depth = 0,
  references = new Set<string>(),
): JsonValue {
  const reference = schema.$ref;
  const resolved = structuralSchema(schema, root);

  if (resolved.examples?.length) return resolved.examples[0];
  const resolvedType = schemaType(resolved);
  const nullableCollectionDefault =
    resolved.default === null && (resolvedType === "object" || resolvedType === "array");
  if (resolved.default !== undefined && !nullableCollectionDefault) return resolved.default;
  if (resolved.const !== undefined) return resolved.const;
  if (resolved.enum?.length) return resolved.enum[0];

  const repeatedReference = reference !== undefined && references.has(reference);
  if (depth >= RECURSION_DEPTH_MAX || repeatedReference) return terminalExample(resolved);

  const nextReferences = new Set(references);
  if (reference) nextReferences.add(reference);

  if (resolvedType === "object" || resolved.properties) {
    return Object.fromEntries(
      Object.entries(resolved.properties ?? {}).map(([name, property]) => [
        name,
        exampleFromSchema(property, root, depth + 1, nextReferences),
      ]),
    );
  }
  if (resolvedType === "array" || resolved.items) {
    return resolved.items
      ? [exampleFromSchema(resolved.items, root, depth + 1, nextReferences)]
      : [];
  }
  return terminalExample(resolved);
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
        <Popover.Positioner sideOffset={8} align="start" className="z-50">
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
  const [open, setOpen] = useState(false);
  const resolved = structuralSchema(schema, root);
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

function JsonShape({ label, schema }: { label: "input" | "output"; schema: JsonSchema }) {
  const value = exampleFromSchema(schema, schema);
  const resolved = structuralSchema(schema, schema);
  const entries = Array.isArray(value)
    ? value.map((item, index) => [String(index), item] as const)
    : Object.entries(value as Record<string, JsonValue>);
  const array = Array.isArray(value);

  return (
    <div className="min-w-0 border border-border bg-background">
      <div className="flex items-center justify-between border-b border-border px-3 py-2 font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground">
        <span>{label}</span>
        <span>json</span>
      </div>
      <div className="overflow-visible px-2 py-3 font-mono text-[0.75rem] sm:text-[0.8125rem]">
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
    </div>
  );
}

export function McpTool({ name }: { name: string }) {
  const tool = mcpDocument.tools.find((candidate) => candidate.name === name);
  if (!tool) throw new Error(`MCP schema has no tool named ${name}`);

  const since = tool.input_schema["rift:since"];
  return (
    <section
      id={`tool-${tool.name}`}
      className="not-prose my-8 scroll-mt-24 border-y border-border py-5"
    >
      <header className="mb-4">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="m-0 font-mono text-lg font-medium tracking-tight">{tool.name}</h3>
          <Badge
            variant="outline"
            className="font-mono text-[0.625rem] uppercase tracking-[0.12em]"
          >
            {since ? `Since ${since}` : "Planned"}
          </Badge>
        </div>
        <p className="mt-2 max-w-3xl whitespace-pre-line text-sm leading-relaxed text-muted-foreground">
          {tool.description}
        </p>
      </header>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]">
        <JsonShape label="input" schema={tool.input_schema} />
        <JsonShape label="output" schema={tool.output_schema} />
      </div>
    </section>
  );
}
