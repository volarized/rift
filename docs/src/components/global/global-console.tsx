"use client";

import { useEffect, useState } from "react";

import { ApiReference } from "@/components/global/api-reference";
import { McpInspector } from "@/components/global/mcp-inspector";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

const TABS = [
  { id: "rest", label: "REST API" },
  { id: "mcp", label: "MCP" },
] as const;

type ConsoleTab = (typeof TABS)[number]["id"];

function tabFromHash(hash: string): ConsoleTab | null {
  const id = hash.replace(/^#/, "");
  return TABS.find((tab) => tab.id === id)?.id ?? null;
}

// The label style of the site; the active tab is marked by a hairline that
// sits on the list's own rule. Focus uses the 1px ring the buttons use.
const TRIGGER_CLASS =
  "h-11 flex-none px-3 first:-ml-3 font-mono text-[0.6875rem] font-normal uppercase tracking-[0.16em] " +
  "focus-visible:ring-1 focus-visible:outline-0 " +
  "group-data-horizontal/tabs:after:inset-x-3 group-data-horizontal/tabs:after:-bottom-px group-data-horizontal/tabs:after:h-px";

/**
 * The two halves of the console. The URL hash carries the active tab, so
 * `#rest` and `#mcp` are links, and a reload opens the same tab.
 */
export function GlobalConsole() {
  // `null` until the hash is read: the static HTML cannot know it, so the first
  // render shows the REST tab with nothing heavy mounted in it.
  const [hashTab, setHashTab] = useState<ConsoleTab | null>(null);
  // The REST reference stays mounted once opened, so a tab switch keeps the
  // request the visitor was editing.
  const [restOpened, setRestOpened] = useState(false);
  // The MCP console mounts when its tab first opens and stays mounted, so a tab
  // switch keeps the connection, the form and the history.
  const [mcpOpened, setMcpOpened] = useState(false);

  useEffect(() => {
    const read = () => {
      const next = tabFromHash(window.location.hash);
      setHashTab((current) => next ?? current ?? "rest");
    };
    read();
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, []);

  useEffect(() => {
    if (hashTab === "rest") setRestOpened(true);
    if (hashTab === "mcp") setMcpOpened(true);
  }, [hashTab]);

  const tab = hashTab ?? "rest";

  return (
    <Tabs
      value={tab}
      onValueChange={(value) => {
        const next = value as ConsoleTab;
        setHashTab(next);
        // Replace, not push: a tab switch is not a step the Back button undoes.
        window.history.replaceState(window.history.state, "", `#${next}`);
      }}
      className="gap-0"
    >
      <TabsList
        variant="line"
        aria-label="Console"
        className="w-full justify-start gap-1 border-b border-border p-0 group-data-horizontal/tabs:h-auto"
      >
        {TABS.map(({ id, label }) => (
          <TabsTrigger key={id} value={id} className={TRIGGER_CLASS}>
            {label}
          </TabsTrigger>
        ))}
      </TabsList>
      {/* Base UI hides a kept panel only after its animations end, and the
          loading label pulses without end, so the class hides it directly. */}
      <TabsContent
        value="rest"
        keepMounted
        className={cn("pt-6 text-sm", tab !== "rest" && "hidden")}
      >
        <ApiReference enabled={restOpened} />
      </TabsContent>
      <TabsContent
        value="mcp"
        keepMounted
        className={cn("pt-6 text-sm", tab !== "mcp" && "hidden")}
      >
        {mcpOpened ? <McpInspector /> : null}
      </TabsContent>
    </Tabs>
  );
}
