"use client";

import { Tab, Tabs } from "fumadocs-ui/components/tabs";
import dynamic from "next/dynamic";
import { McpInspector } from "@/components/mcp-inspector";

const GlobalApi = dynamic(() => import("@/components/global-api"), {
  ssr: false,
  loading: () => <p className="p-5 text-sm text-muted-foreground">Loading API schema.</p>,
});

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

export function GlobalIndex() {
  return (
    <Tabs items={["API", "MCP"]} className="global-index-tabs">
      <Tab value="API" className="p-0">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-border px-4 py-3 text-xs text-muted-foreground">
          <code>https://api.volar.sh/rift/rest</code>
          <a href={`${BASE}/global-api.openapi.json`} className="underline underline-offset-4">
            OpenAPI schema
          </a>
        </div>
        <p className="border-b border-border px-4 py-3 text-sm text-muted-foreground">
          Browser requests require the API to allow this site's origin through CORS. The hosted API
          currently accepts requests from command-line clients.
        </p>
        <div className="rift-api min-w-0">
          <GlobalApi />
        </div>
      </Tab>
      <Tab value="MCP" className="p-0">
        <McpInspector />
      </Tab>
    </Tabs>
  );
}
