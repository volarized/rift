"use client";

import { Dialog } from "@base-ui/react/dialog";
import { Maximize2, X } from "lucide-react";
import { dereference, type JsonSchema, PayloadTree, schemaType } from "@/components/json-tree";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

export type McpToolDefinition = {
  description: string;
  input_schema: JsonSchema;
  name: string;
  output_schema: JsonSchema;
};

const RECURSION_DEPTH_MAX = 8;

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

export function McpTool({ tool }: { tool: McpToolDefinition }) {
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
