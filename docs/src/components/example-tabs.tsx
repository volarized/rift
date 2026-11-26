"use client";

import { type ReactNode, useState } from "react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

const tabStyle =
  "h-full border-0 border-b border-transparent bg-transparent px-0 py-2 text-sm after:hidden data-active:border-fd-primary data-active:bg-transparent data-active:text-fd-primary dark:data-active:border-fd-primary dark:data-active:bg-transparent";
const panel = "bg-fd-background text-[0.9375rem] outline-none";
const formatLabels = { sparql: "SPARQL", mcp: "MCP", map: "Map", text: "Text", json: "JSON" };
type Format = keyof typeof formatLabels;
type ResponseFormat = "map" | "text" | "json";

function FormatSwitch<T extends Format>({
  label,
  options,
  value,
  onChange,
}: {
  label: string;
  options: readonly T[];
  value: T;
  onChange: (value: T) => void;
}) {
  return (
    <fieldset
      aria-label={label}
      className="ms-auto inline-flex shrink-0 items-center gap-px border border-fd-border bg-fd-background p-px"
    >
      {options.map((format) => (
        <button
          type="button"
          key={format}
          aria-pressed={value === format}
          onClick={() => onChange(format)}
          className="whitespace-nowrap px-1.5 py-1 font-mono text-xs leading-none text-fd-muted-foreground uppercase hover:text-fd-foreground focus-visible:outline-1 focus-visible:outline-fd-primary aria-pressed:bg-fd-secondary aria-pressed:text-fd-primary sm:px-2"
        >
          {formatLabels[format]}
        </button>
      ))}
    </fieldset>
  );
}

export function ExampleTabs({
  sparql,
  mcp,
  text,
  json,
  visualization,
}: {
  sparql?: ReactNode;
  mcp: ReactNode;
  text: ReactNode;
  json: ReactNode;
  visualization?: ReactNode;
}) {
  const [section, setSection] = useState<"request" | "response">(
    visualization ? "response" : "request",
  );
  const [requestFormat, setRequestFormat] = useState<"sparql" | "mcp">(sparql ? "sparql" : "mcp");
  const [responseFormat, setResponseFormat] = useState<ResponseFormat>(
    visualization ? "map" : "text",
  );
  const responses = { map: visualization, text, json };

  return (
    <Tabs
      value={section}
      onValueChange={setSection}
      className="not-prose my-4 gap-0 overflow-hidden border border-fd-border"
    >
      <div className="flex min-h-10 flex-wrap items-center gap-x-2 gap-y-1 border-b border-fd-border bg-fd-secondary px-2 sm:gap-x-3 sm:px-3">
        <TabsList aria-label="Example view" className="min-h-10 gap-3 bg-transparent p-0">
          <TabsTrigger value="request" className={tabStyle}>
            Request
          </TabsTrigger>
          <TabsTrigger value="response" className={tabStyle}>
            Response
          </TabsTrigger>
        </TabsList>

        {section === "request" && sparql && (
          <FormatSwitch
            label="Request format"
            options={["sparql", "mcp"]}
            value={requestFormat}
            onChange={setRequestFormat}
          />
        )}
        {section === "response" && (
          <FormatSwitch
            label="Response format"
            options={visualization ? ["map", "text", "json"] : ["text", "json"]}
            value={responseFormat}
            onChange={setResponseFormat}
          />
        )}
      </div>

      <TabsContent value="request" className={panel}>
        {requestFormat === "sparql" ? sparql : mcp}
      </TabsContent>
      <TabsContent value="response" className={panel}>
        {responses[responseFormat]}
      </TabsContent>
    </Tabs>
  );
}
