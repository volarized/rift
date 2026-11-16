"use client";

import { BracketsCurlyIcon, PlugsConnectedIcon } from "@phosphor-icons/react";
import { type ReactNode, useEffect, useState } from "react";

import { ApiReference } from "@/components/global/api-reference";
import { McpInspector } from "@/components/global/mcp-inspector";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

const TABS = [
  { id: "api", label: "API", Icon: BracketsCurlyIcon },
  { id: "mcp", label: "MCP", Icon: PlugsConnectedIcon },
] as const;

type ConsoleTab = (typeof TABS)[number]["id"];

function tabFromHash(hash: string): ConsoleTab | null {
  const id = hash.replace(/^#/, "");
  return TABS.find((tab) => tab.id === id)?.id ?? null;
}

// One outlined control of two segments, in the label style of the site. The
// active segment is marked by a hairline that sits on the control's bottom
// rule. Focus uses the 1px ring the buttons use.
const TRIGGER_CLASS =
  "h-9 flex-none gap-2 px-3.5 font-mono text-[0.6875rem] font-normal uppercase tracking-[0.16em] " +
  "not-first:border-l-border focus-visible:ring-1 focus-visible:outline-0 " +
  "group-data-horizontal/tabs:after:-inset-x-px group-data-horizontal/tabs:after:-bottom-0.5 group-data-horizontal/tabs:after:h-px";

/**
 * The page header and the two halves of the console. The selector sits on the
 * title's line; `children` is the rest of the header, shown above either half.
 * The URL hash carries the active tab, so `#api` and `#mcp` are links, and a
 * reload opens the same tab.
 */
export function GlobalConsole({ title, children }: { title: string; children: ReactNode }) {
  // `null` until the hash is read: the static HTML cannot know it, so the first
  // render shows the API tab with nothing heavy mounted in it.
  const [hashTab, setHashTab] = useState<ConsoleTab | null>(null);
  // The API reference stays mounted once opened, so a tab switch keeps the
  // request the visitor was editing.
  const [apiOpened, setApiOpened] = useState(false);
  // The MCP console mounts when its tab first opens and stays mounted, so a tab
  // switch keeps the connection, the form and the history.
  const [mcpOpened, setMcpOpened] = useState(false);

  useEffect(() => {
    const read = () => {
      const next = tabFromHash(window.location.hash);
      setHashTab((current) => next ?? current ?? "api");
    };
    read();
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, []);

  useEffect(() => {
    if (hashTab === "api") setApiOpened(true);
    if (hashTab === "mcp") setMcpOpened(true);
  }, [hashTab]);

  const tab = hashTab ?? "api";

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
      <header className="mb-12 grid max-w-190 gap-5">
        <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
          <h1 className="font-mono text-3xl">{title}</h1>
          <TabsList
            variant="line"
            aria-label="Console"
            className="gap-0 border border-border p-0 group-data-horizontal/tabs:h-auto"
          >
            {TABS.map(({ id, label, Icon }) => (
              <TabsTrigger key={id} value={id} className={TRIGGER_CLASS}>
                <Icon aria-hidden="true" weight="light" className="size-3.5" />
                {label}
              </TabsTrigger>
            ))}
          </TabsList>
        </div>
        {children}
      </header>
      {/* Base UI hides a kept panel only after its animations end, and the
          loading label pulses without end, so the class hides it directly. */}
      <TabsContent value="api" keepMounted className={cn("text-sm", tab !== "api" && "hidden")}>
        <ApiReference enabled={apiOpened} />
      </TabsContent>
      <TabsContent value="mcp" keepMounted className={cn("text-sm", tab !== "mcp" && "hidden")}>
        {mcpOpened ? <McpInspector /> : null}
      </TabsContent>
    </Tabs>
  );
}
