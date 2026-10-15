"use client";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import {
  ErrorCode,
  McpError,
  type Resource,
  type ResourceTemplate,
  type Tool,
} from "@modelcontextprotocol/sdk/types.js";
import { ArrowRightIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";
import { McpCatalog, type McpSelection } from "@/components/mcp-catalog";
import { McpIcon } from "@/components/mcp-icon";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

const HOSTED_ENDPOINT = "https://api.volar.sh/rift/mcp/";
const FIELD =
  "w-full min-w-0 border border-border bg-background px-3 py-2 font-mono text-xs outline-none focus-visible:border-ring disabled:opacity-50";
const REQUEST_TIMEOUT = 30_000;

function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function object(text: string): Record<string, unknown> {
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Tool arguments must be a JSON object.");
  }
  return value as Record<string, unknown>;
}

export function McpInspector() {
  const [token, setToken] = useState("");
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [server, setServer] = useState("");
  const [tools, setTools] = useState<Tool[]>([]);
  const [resources, setResources] = useState<Resource[]>([]);
  const [templates, setTemplates] = useState<ResourceTemplate[]>([]);
  const [selection, setSelection] = useState<McpSelection>();
  const [parameters, setParameters] = useState("{}");
  const [uri, setUri] = useState("");
  const [response, setResponse] = useState<unknown>();
  const [error, setError] = useState("");
  const [elapsed, setElapsed] = useState<number>();
  const client = useRef<Client | null>(null);
  const pending = useRef<AbortController | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      pending.current?.abort();
      void client.current?.close();
      client.current = null;
    };
  }, []);

  async function connect() {
    setBusy(true);
    setError("");
    setResponse(undefined);
    setElapsed(undefined);
    const controller = new AbortController();
    pending.current = controller;
    let next: Client | null = null;
    try {
      const url = new URL(HOSTED_ENDPOINT);
      next = new Client({ name: "rift-docs", version: "1.0.0" });
      const transport = new StreamableHTTPClientTransport(url, {
        requestInit: {
          credentials: "omit",
          headers: token ? { Authorization: `Bearer ${token}` } : undefined,
        },
      });
      next.onclose = () => {
        if (mounted.current && client.current === next) {
          client.current = null;
          setConnected(false);
          setServer("");
          setTools([]);
          setResources([]);
          setTemplates([]);
          setSelection(undefined);
        }
      };
      await next.connect(transport, { signal: controller.signal, timeout: REQUEST_TIMEOUT });
      const options = { signal: controller.signal, timeout: REQUEST_TIMEOUT };
      const capabilities = next.getServerCapabilities();
      const tools: Tool[] = [];
      const resources: Resource[] = [];
      const templates: ResourceTemplate[] = [];
      if (capabilities?.tools) {
        let cursor: string | undefined;
        for (let page = 0; page < 20; page++) {
          const result = await next.listTools({ cursor }, options);
          tools.push(...result.tools);
          cursor = result.nextCursor;
          if (!cursor) break;
          if (page === 19) throw new Error("The tool listing exceeds 20 pages.");
        }
      }
      if (capabilities?.resources) {
        let cursor: string | undefined;
        for (let page = 0; page < 20; page++) {
          const result = await next.listResources({ cursor }, options);
          resources.push(...result.resources);
          cursor = result.nextCursor;
          if (!cursor) break;
          if (page === 19) throw new Error("The resource listing exceeds 20 pages.");
        }
        cursor = undefined;
        for (let page = 0; page < 20; page++) {
          try {
            const result = await next.listResourceTemplates({ cursor }, options);
            templates.push(...result.resourceTemplates);
            cursor = result.nextCursor;
            if (!cursor) break;
            if (page === 19) throw new Error("The resource template listing exceeds 20 pages.");
          } catch (cause) {
            if (page === 0 && cause instanceof McpError && cause.code === ErrorCode.MethodNotFound)
              break;
            throw cause;
          }
        }
      }
      if (!mounted.current) {
        await next.close();
        return;
      }
      client.current = next;
      setTools(tools);
      setResources(resources);
      setTemplates(templates);
      setSelection(undefined);
      setConnected(true);
      const info = next.getServerVersion();
      setServer(info ? `${info.name} ${info.version}` : "Connected");
      setResponse({ serverInfo: info, capabilities: next.getServerCapabilities() });
    } catch (cause) {
      await next?.close().catch(() => {});
      if (mounted.current) setError(message(cause));
    } finally {
      pending.current = null;
      if (mounted.current) setBusy(false);
    }
  }

  async function disconnect() {
    pending.current?.abort();
    const current = client.current;
    client.current = null;
    setConnected(false);
    setServer("");
    setTools([]);
    setResources([]);
    setTemplates([]);
    setSelection(undefined);
    setResponse(undefined);
    setElapsed(undefined);
    try {
      await current?.close();
    } catch (cause) {
      setError(message(cause));
    }
  }

  async function send() {
    const current = client.current;
    if (!current || !selection) return;
    setBusy(true);
    setError("");
    setResponse(undefined);
    const controller = new AbortController();
    pending.current = controller;
    const started = performance.now();
    try {
      const options = { signal: controller.signal, timeout: REQUEST_TIMEOUT };
      let result: unknown;
      if (selection.kind === "tool") {
        result = await current.callTool(
          { name: selection.name, arguments: object(parameters) },
          undefined,
          options,
        );
      } else {
        if (!uri.trim()) throw new Error("The resource URI is required.");
        if (selection.kind === "template" && /[{}]/.test(uri)) {
          throw new Error("Replace resource template parameters with values before reading.");
        }
        result = await current.readResource({ uri: uri.trim() }, options);
      }
      if (mounted.current) setResponse(result);
    } catch (cause) {
      if (mounted.current) setError(message(cause));
    } finally {
      pending.current = null;
      if (mounted.current) {
        setElapsed(Math.round(performance.now() - started));
        setBusy(false);
      }
    }
  }

  const selectedTool =
    selection?.kind === "tool" ? tools.find((tool) => tool.name === selection.name) : undefined;
  const selectedResource =
    selection?.kind === "resource"
      ? resources.find((resource) => resource.uri === selection.uri)
      : selection?.kind === "template"
        ? templates.find((template) => template.uriTemplate === selection.uriTemplate)
        : undefined;

  function select(next: McpSelection) {
    setSelection(next);
    setParameters("{}");
    setUri(next.kind === "resource" ? next.uri : next.kind === "template" ? next.uriTemplate : "");
    setResponse(undefined);
    setElapsed(undefined);
    setError("");
  }
  const toolError =
    response !== null &&
    response !== undefined &&
    typeof response === "object" &&
    "isError" in response &&
    response.isError === true;

  return (
    <section aria-label="MCP requests">
      <div className="border-b border-border px-5 py-4 text-sm leading-relaxed text-muted-foreground">
        <div className="mb-2 flex items-center gap-2 font-mono text-foreground">
          <McpIcon size={18} weight="light" aria-hidden="true" />
          MCP requests
        </div>
        <p>
          Select a tool to inspect its input schema and send a request, or select a resource to read
          it. The hosted MCP endpoint is not yet available.
        </p>
        <p className="mt-1">
          The server must allow this site's origin through CORS. Credentials stay in this tab.
        </p>
      </div>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          void connect();
        }}
        className="grid gap-4 border-b border-border p-5 md:grid-cols-[minmax(0,1fr)_minmax(12rem,0.5fr)_auto] md:items-end"
      >
        <div className="grid min-w-0 gap-2 text-xs text-muted-foreground">
          <span>Endpoint</span>
          <code className="break-all py-2 text-foreground">{HOSTED_ENDPOINT}</code>
        </div>
        <label className="grid gap-2 text-xs text-muted-foreground" htmlFor="mcp-token">
          Bearer token (optional)
          <input
            id="mcp-token"
            type="password"
            autoComplete="off"
            value={token}
            onChange={(event) => setToken(event.target.value)}
            disabled={connected || busy}
            className={FIELD}
          />
        </label>
        {connected ? (
          <Button
            type="button"
            variant="outline"
            disabled={busy}
            onClick={(event) => {
              event.preventDefault();
              void disconnect();
            }}
          >
            Disconnect
          </Button>
        ) : (
          <Button type="submit" variant="outline" disabled={busy}>
            Connect
          </Button>
        )}
      </form>
      <div
        className="flex min-h-11 flex-wrap items-center gap-3 border-b border-border px-5 py-2 text-xs text-muted-foreground"
        role="status"
      >
        <Badge variant="outline">{connected ? "Connected" : "Disconnected"}</Badge>
        <span>{server || "Connect to load tools and resources."}</span>
        {busy && <span>Request in progress.</span>}
      </div>
      <McpCatalog
        tools={tools}
        resources={resources}
        templates={templates}
        available={connected}
        busy={busy}
        selection={selection}
        onSelect={select}
      />
      <div className="grid min-w-0 lg:grid-cols-2">
        <div className="min-w-0 border-b border-border p-5 lg:border-r lg:border-b-0">
          <div className="mb-4 flex flex-wrap items-center justify-between gap-4">
            <h2 className="break-all font-mono text-sm">
              {selectedTool?.name ?? selectedResource?.title ?? selectedResource?.name ?? "Request"}
            </h2>
            {selection && (
              <Badge variant="outline">
                {selection.kind === "tool" ? "tools/call" : "resources/read"}
              </Badge>
            )}
          </div>
          {selectedTool && (
            <div className="mb-4 text-sm leading-relaxed text-muted-foreground">
              <p className="whitespace-pre-wrap">{selectedTool.description}</p>
              <details className="mt-3 border border-border p-3">
                <summary className="cursor-pointer font-mono text-xs">Input schema</summary>
                <pre className="mt-3 max-h-64 overflow-auto text-xs">
                  {JSON.stringify(selectedTool.inputSchema, null, 2)}
                </pre>
              </details>
            </div>
          )}
          {selectedResource && (
            <div className="mb-4 text-sm leading-relaxed text-muted-foreground">
              {selectedResource.description && <p>{selectedResource.description}</p>}
              {selectedResource.mimeType && (
                <Badge variant="outline" className="mt-2">
                  {selectedResource.mimeType}
                </Badge>
              )}
            </div>
          )}
          {selection?.kind === "tool" ? (
            <label className="grid gap-2 text-xs text-muted-foreground" htmlFor="mcp-parameters">
              Arguments
              <textarea
                id="mcp-parameters"
                value={parameters}
                onChange={(event) => setParameters(event.target.value)}
                spellCheck={false}
                disabled={busy}
                className={`${FIELD} min-h-48 resize-y leading-relaxed`}
              />
            </label>
          ) : selection ? (
            <label className="grid gap-2 text-xs text-muted-foreground" htmlFor="mcp-resource-uri">
              Resource URI
              <input
                id="mcp-resource-uri"
                value={uri}
                onChange={(event) => setUri(event.target.value)}
                readOnly={selection.kind === "resource"}
                spellCheck={false}
                disabled={busy}
                className={FIELD}
              />
              {selection.kind === "template" && (
                <span>Replace template parameters with the resource's values.</span>
              )}
            </label>
          ) : (
            <p className="text-sm leading-relaxed text-muted-foreground">
              Select a tool or resource from the lists above.
            </p>
          )}
          <div className="mt-4 flex gap-2">
            <Button disabled={!connected || !selection || busy} onClick={() => void send()}>
              <ArrowRightIcon size={16} weight="light" />
              {selection?.kind === "tool"
                ? "Call tool"
                : selection
                  ? "Read resource"
                  : "Send request"}
            </Button>
            {busy && (
              <Button variant="outline" onClick={() => pending.current?.abort()}>
                Cancel
              </Button>
            )}
          </div>
        </div>
        <div className="min-w-0 p-5">
          <div className="mb-4 flex items-center justify-between gap-4">
            <h2 className="font-mono text-sm">Response</h2>
            {elapsed !== undefined && (
              <span className="font-mono text-xs text-muted-foreground">{elapsed} ms</span>
            )}
          </div>
          {error && (
            <p
              role="alert"
              className="mb-4 border border-destructive/40 p-3 text-sm text-destructive"
            >
              {error}
            </p>
          )}
          {toolError && (
            <Badge variant="outline" className="mb-3">
              Tool error
            </Badge>
          )}
          <pre className="min-h-64 max-h-[40rem] overflow-auto border border-border p-4 font-mono text-xs leading-relaxed text-muted-foreground">
            {response === undefined
              ? "The server response appears here."
              : JSON.stringify(response, null, 2)}
          </pre>
        </div>
      </div>
    </section>
  );
}
