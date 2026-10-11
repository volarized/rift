"use client";

import type { Resource, ResourceTemplate, Tool } from "@modelcontextprotocol/sdk/types.js";
import { BracketsCurlyIcon, TextAlignLeftIcon } from "@phosphor-icons/react";
import { Tab, Tabs, TabsList, TabsTrigger } from "fumadocs-ui/components/tabs";
import { useEffect, useState } from "react";
import { McpCatalog, type McpSelection } from "@/components/mcp-catalog";
import { McpIcon } from "@/components/mcp-icon";
import { Badge } from "@/components/ui/badge";
import examplesJson from "@/lib/local-mcp-examples.json";
import specJson from "../../public/mcp.json";

type Schema = Tool["inputSchema"] & { examples: Record<string, unknown>[] };
type SpecTool = {
  name: string;
  description: string;
  input_schema: Schema;
  output_schema: Schema;
};
type ToolExample = { content: { type: "text"; text: string }[] };
type ResourceExample = { contents: { uri: string; mimeType: string; text: string }[] };
type ExampleResource = Resource & { examples: ResourceExample[] };

const spec = specJson as unknown as { tools: SpecTool[] };
const examples = examplesJson as unknown as {
  tools: Record<string, ToolExample[]>;
  resources: ExampleResource[];
  resourceTemplates: ResourceTemplate[];
};
const tools: Tool[] = spec.tools.map((tool) => ({
  name: tool.name,
  description: tool.description,
  inputSchema: tool.input_schema,
  outputSchema: tool.output_schema,
}));
const CODE =
  "m-0 max-h-96 overflow-auto border border-border bg-background p-4 font-mono text-xs leading-relaxed text-muted-foreground";
const FIELD =
  "min-w-0 border border-border bg-background px-3 py-2 font-mono text-xs outline-none focus-visible:border-ring";

// Existing links select the tool or resource that owns the replaced section.
const anchors: Record<string, { selection: McpSelection; request?: number }> = {
  search: { selection: { kind: "tool", name: "search" } },
  "tool-search": { selection: { kind: "tool", name: "search" } },
  "search-fulltext": { selection: { kind: "tool", name: "search" } },
  "search-pattern": { selection: { kind: "tool", name: "search" }, request: 5 },
  "search-traversal": { selection: { kind: "tool", name: "search" }, request: 2 },
  "search-engine-references": { selection: { kind: "tool", name: "search" }, request: 2 },
  "search-change": { selection: { kind: "tool", name: "search" }, request: 7 },
  "search-commits": { selection: { kind: "tool", name: "search" }, request: 8 },
  "search-scope": { selection: { kind: "tool", name: "search" }, request: 4 },
  "requested-packages": { selection: { kind: "tool", name: "search" }, request: 6 },
  "symbol-retrieval": { selection: { kind: "tool", name: "get_symbol" } },
  "tool-get_symbol": { selection: { kind: "tool", name: "get_symbol" } },
  "get_symbol-scope": { selection: { kind: "tool", name: "get_symbol" }, request: 2 },
  nodes: { selection: { kind: "tool", name: "nodes" } },
  "tool-nodes": { selection: { kind: "tool", name: "nodes" } },
  reads: { selection: { kind: "tool", name: "search" } },
  "result-content": { selection: { kind: "tool", name: "search" } },
  "revision-reads": { selection: { kind: "tool", name: "get_symbol" } },
  "resource-map": { selection: { kind: "resource", uri: "rift://map" } },
  resources: { selection: { kind: "resource", uri: "rift://workspace" } },
  "resource-contents": { selection: { kind: "resource", uri: "rift://workspace" } },
  "resource-workspace": { selection: { kind: "resource", uri: "rift://workspace" } },
  errors: { selection: { kind: "tool", name: "search" } },
  "tool-execution-failures": { selection: { kind: "tool", name: "search" } },
  "protocol-failures": { selection: { kind: "tool", name: "search" } },
};

function Json({ value, label }: { value: unknown; label: string }) {
  return (
    <section aria-label={label}>
      <pre className={CODE}>{JSON.stringify(value, null, 2)}</pre>
    </section>
  );
}

export function LocalMcpReference() {
  const [selection, setSelection] = useState<McpSelection>({ kind: "tool", name: "search" });
  const [requestIndex, setRequestIndex] = useState(0);
  const [responseIndex, setResponseIndex] = useState(0);

  useEffect(() => {
    function selectAnchor() {
      const entry = anchors[window.location.hash.slice(1)];
      if (!entry) return;
      setSelection(entry.selection);
      setRequestIndex(entry.request ?? 0);
      setResponseIndex(0);
    }
    selectAnchor();
    window.addEventListener("hashchange", selectAnchor);
    return () => window.removeEventListener("hashchange", selectAnchor);
  }, []);

  function select(next: McpSelection) {
    setSelection(next);
    setRequestIndex(0);
    setResponseIndex(0);
  }

  const tool =
    selection.kind === "tool" ? spec.tools.find((tool) => tool.name === selection.name) : undefined;
  const resource =
    selection.kind === "resource"
      ? examples.resources.find((resource) => resource.uri === selection.uri)
      : undefined;
  const workspaceTemplate = examples.resourceTemplates.find(
    (template) => template.uriTemplate === "rift://workspace{?page_index}",
  );
  const workspace = resource?.uri === "rift://workspace";
  const resourceUri =
    workspace && requestIndex === 1 ? "rift://workspace?page_index=0" : resource?.uri;
  const request = tool?.input_schema.examples[requestIndex];
  const result:
    | (ToolExample & { structuredContent: Record<string, unknown> })
    | ResourceExample
    | undefined = tool
    ? {
        ...examples.tools[tool.name][responseIndex],
        structuredContent: tool.output_schema.examples[responseIndex],
      }
    : resource && {
        contents: resource.examples[0].contents.map((content) => ({
          ...content,
          uri: resourceUri ?? resource.uri,
        })),
      };

  return (
    <section aria-label="Local MCP demo" className="not-prose my-6 min-w-0 border border-border">
      {Object.keys(anchors).map((anchor) => (
        <span key={anchor} id={anchor} className="scroll-mt-24" />
      ))}
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border px-5 py-4">
        <span className="flex items-center gap-2 font-mono text-sm">
          <McpIcon size={18} weight="light" aria-hidden="true" />
          Local MCP
          <Badge variant="outline">demo</Badge>
        </span>
        <a
          href={`${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/mcp.json`}
          className="text-xs text-muted-foreground underline underline-offset-4"
        >
          MCP JSON schema
        </a>
      </div>
      <McpCatalog
        tools={tools}
        resources={examples.resources}
        templates={[]}
        available
        busy={false}
        selection={selection}
        onSelect={select}
      />
      <div className="grid min-w-0 xl:grid-cols-2">
        <section
          aria-label="Example request"
          className="min-w-0 border-b border-border p-5 xl:border-r xl:border-b-0"
        >
          <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
            <h2 className="break-all font-mono text-sm">{tool?.name ?? resource?.title}</h2>
            {tool ? (
              <Badge variant="outline">tools/call</Badge>
            ) : workspace ? (
              <select
                aria-label="Workspace URI example"
                value={requestIndex}
                onChange={(event) => setRequestIndex(Number(event.target.value))}
                className={FIELD}
              >
                <option value={0}>{resource?.uri}</option>
                <option value={1}>rift://workspace?page_index=0</option>
              </select>
            ) : (
              <Badge variant="outline" className="font-mono">
                {resourceUri}
              </Badge>
            )}
          </div>
          {tool ? (
            <details className="mb-4 text-xs leading-relaxed text-muted-foreground">
              <summary className="cursor-pointer">Tool description</summary>
              <p className="mt-3 whitespace-pre-wrap">{tool.description}</p>
            </details>
          ) : (
            <p className="mb-4 text-xs leading-relaxed text-muted-foreground">
              {workspace && requestIndex === 1
                ? workspaceTemplate?.description
                : resource?.description}
            </p>
          )}
          {tool ? (
            <>
              <label
                className="mb-3 flex flex-wrap items-center justify-between gap-3 text-xs text-muted-foreground"
                htmlFor="local-mcp-request-example"
              >
                Request example
                <select
                  id="local-mcp-request-example"
                  value={requestIndex}
                  onChange={(event) => setRequestIndex(Number(event.target.value))}
                  className={FIELD}
                >
                  {tool.input_schema.examples.map((example, index) => (
                    <option key={JSON.stringify(example)} value={index}>
                      Example {index + 1}
                    </option>
                  ))}
                </select>
              </label>
              <Json label="Request parameters" value={{ name: tool.name, arguments: request }} />
              <p className="mt-3 text-xs leading-relaxed text-muted-foreground">
                Request and response examples are listed independently in the exported schema.
              </p>
              <details className="mt-4 border border-border p-3">
                <summary className="cursor-pointer font-mono text-xs">Input schema</summary>
                <div className="mt-3">
                  <Json label="Input schema JSON" value={tool.input_schema} />
                </div>
              </details>
              <details className="mt-3 border border-border p-3">
                <summary className="cursor-pointer font-mono text-xs">Output schema</summary>
                <div className="mt-3">
                  <Json label="Output schema JSON" value={tool.output_schema} />
                </div>
              </details>
            </>
          ) : null}
        </section>
        <section aria-label="Example response" className="min-w-0 p-5">
          <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
            <h2 className="font-mono text-sm">Example response</h2>
            {tool && tool.output_schema.examples.length > 1 && (
              <select
                aria-label="Response example"
                value={responseIndex}
                onChange={(event) => setResponseIndex(Number(event.target.value))}
                className={FIELD}
              >
                {tool.output_schema.examples.map((example, index) => (
                  <option key={JSON.stringify(example)} value={index}>
                    Example {index + 1}
                  </option>
                ))}
              </select>
            )}
          </div>
          {tool && result && "content" in result ? (
            <Tabs
              key={tool.name}
              defaultValue="content"
              className="my-0 rounded-none bg-background [&_pre]:border-0"
            >
              <TabsList aria-label="Response format" className="border-b border-border">
                <TabsTrigger value="content" className="font-mono text-xs">
                  <TextAlignLeftIcon size={16} weight="light" aria-hidden="true" />
                  content
                </TabsTrigger>
                <TabsTrigger value="structured" className="font-mono text-xs">
                  <BracketsCurlyIcon size={16} weight="light" aria-hidden="true" />
                  structuredContent
                </TabsTrigger>
              </TabsList>
              <Tab value="content" className="rounded-none p-0">
                <section aria-label="content compact text">
                  <pre className={`${CODE} whitespace-pre-wrap break-words`}>
                    {result.content.map((block) => block.text).join("\n")}
                  </pre>
                </section>
              </Tab>
              <Tab value="structured" className="rounded-none p-0">
                <Json label="structuredContent JSON" value={result.structuredContent} />
              </Tab>
            </Tabs>
          ) : result && "contents" in result ? (
            <>
              <h3 className="mb-2 font-mono text-xs">contents</h3>
              <p className="mb-3 text-xs leading-relaxed text-muted-foreground">
                Resource reads return content entries selected by MIME type.
              </p>
              {result.contents.map((content) => (
                <div key={content.mimeType} className="mb-4">
                  <Badge variant="outline" className="mb-2">
                    {content.mimeType}
                  </Badge>
                  <section aria-label={`${content.mimeType} resource content`}>
                    <pre className={`${CODE} whitespace-pre-wrap break-words`}>{content.text}</pre>
                  </section>
                </div>
              ))}
            </>
          ) : null}
          <details className="mt-4 border border-border p-3">
            <summary className="cursor-pointer font-mono text-xs">MCP result</summary>
            <div className="mt-3">
              <Json label="MCP result JSON" value={result} />
            </div>
          </details>
        </section>
      </div>
    </section>
  );
}
