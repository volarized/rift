"use client";

import type { Resource, ResourceTemplate, Tool } from "@modelcontextprotocol/sdk/types.js";
import { FilesIcon, MagnifyingGlassIcon, WrenchIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";

export type McpSelection =
  | { kind: "tool"; name: string }
  | { kind: "resource"; uri: string }
  | { kind: "template"; uriTemplate: string };

type Props = {
  tools: Tool[];
  resources: Resource[];
  templates: ResourceTemplate[];
  available: boolean;
  busy: boolean;
  selection?: McpSelection;
  onSelect: (selection: McpSelection) => void;
};

const ROW =
  "block w-full cursor-pointer border-t border-border px-5 py-3 text-left hover:bg-muted/40 focus-visible:outline-2 focus-visible:outline-ring disabled:cursor-default disabled:opacity-50 aria-pressed:bg-muted/60";

function descriptionPreview(description: string) {
  return description.length > 140 ? `${description.slice(0, 137).trimEnd()}...` : description;
}

export function McpCatalog({
  tools,
  resources,
  templates,
  available,
  busy,
  selection,
  onSelect,
}: Props) {
  const [query, setQuery] = useState("");
  const search = query.trim().toLowerCase();
  const matches = (...values: (string | undefined)[]) =>
    values.some((value) => value?.toLowerCase().includes(search));
  const visibleTools = tools.filter((tool) => matches(tool.name, tool.title, tool.description));
  const visibleResources = resources.filter((resource) =>
    matches(resource.name, resource.title, resource.uri, resource.description),
  );
  const visibleTemplates = templates.filter((template) =>
    matches(template.name, template.title, template.uriTemplate, template.description),
  );

  return (
    <section aria-label="MCP tools and resources" className="border-b border-border">
      <label className="flex items-center gap-2 border-b border-border px-5 py-3">
        <MagnifyingGlassIcon size={16} weight="light" aria-hidden="true" />
        <input
          type="search"
          aria-label="Search tools and resources"
          placeholder="Search tools and resources"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground focus-visible:ring-1 focus-visible:ring-ring"
        />
      </label>
      <div className="grid min-w-0 md:grid-cols-2">
        <section
          aria-label="Tools"
          className="min-w-0 border-b border-border md:border-r md:border-b-0"
        >
          <h2 className="flex items-center gap-2 px-5 py-3 font-mono text-sm">
            <WrenchIcon size={18} weight="light" aria-hidden="true" />
            Tools
            {available && <Badge variant="outline">{tools.length}</Badge>}
          </h2>
          <div className="max-h-80 overflow-auto">
            {visibleTools.map((tool) => (
              <button
                key={tool.name}
                type="button"
                aria-label={`Select tool ${tool.name}`}
                aria-pressed={selection?.kind === "tool" && selection.name === tool.name}
                disabled={busy}
                onClick={() => onSelect({ kind: "tool", name: tool.name })}
                className={ROW}
              >
                <span className="block break-all font-mono text-sm">{tool.name}</span>
                {tool.description && (
                  <span className="mt-1 block line-clamp-3 text-xs leading-relaxed text-muted-foreground">
                    {descriptionPreview(tool.description)}
                  </span>
                )}
              </button>
            ))}
            {visibleTools.length === 0 && (
              <p className="border-t border-border px-5 py-5 text-xs leading-relaxed text-muted-foreground">
                {available
                  ? tools.length > 0
                    ? "No matching tools."
                    : "The server exposes no tools."
                  : "The hosted tool list is not yet available. Connect to load tool names, descriptions, and input schemas from the server."}
              </p>
            )}
          </div>
        </section>
        <section aria-label="Resources" className="min-w-0">
          <h2 className="flex items-center gap-2 px-5 py-3 font-mono text-sm">
            <FilesIcon size={18} weight="light" aria-hidden="true" />
            Resources
            {available && <Badge variant="outline">{resources.length + templates.length}</Badge>}
          </h2>
          <div className="max-h-80 overflow-auto">
            {visibleResources.map((resource) => (
              <button
                key={resource.uri}
                type="button"
                aria-label={`Select resource ${resource.name}`}
                aria-pressed={selection?.kind === "resource" && selection.uri === resource.uri}
                disabled={busy}
                onClick={() => onSelect({ kind: "resource", uri: resource.uri })}
                className={ROW}
              >
                <span className="block break-all font-mono text-sm">
                  {resource.title ?? resource.name}
                </span>
                <span className="mt-1 block break-all font-mono text-xs text-muted-foreground">
                  {resource.uri}
                </span>
                {resource.description && (
                  <span className="mt-1 block line-clamp-3 text-xs leading-relaxed text-muted-foreground">
                    {descriptionPreview(resource.description)}
                  </span>
                )}
              </button>
            ))}
            {visibleTemplates.map((template) => (
              <button
                key={template.uriTemplate}
                type="button"
                aria-label={`Select resource template ${template.name}`}
                aria-pressed={
                  selection?.kind === "template" && selection.uriTemplate === template.uriTemplate
                }
                disabled={busy}
                onClick={() => onSelect({ kind: "template", uriTemplate: template.uriTemplate })}
                className={ROW}
              >
                <span className="flex flex-wrap items-center gap-2">
                  <span className="break-all font-mono text-sm">
                    {template.title ?? template.name}
                  </span>
                  <Badge variant="outline">Template</Badge>
                </span>
                <span className="mt-1 block break-all font-mono text-xs text-muted-foreground">
                  {template.uriTemplate}
                </span>
                {template.description && (
                  <span className="mt-1 block line-clamp-3 text-xs leading-relaxed text-muted-foreground">
                    {descriptionPreview(template.description)}
                  </span>
                )}
              </button>
            ))}
            {visibleResources.length + visibleTemplates.length === 0 && (
              <p className="border-t border-border px-5 py-5 text-xs leading-relaxed text-muted-foreground">
                {available
                  ? resources.length + templates.length > 0
                    ? "No matching resources."
                    : "The server exposes no resources."
                  : "The hosted resource list is not yet available. Connect to load resource names, descriptions, and URIs from the server."}
              </p>
            )}
          </div>
        </section>
      </div>
    </section>
  );
}
