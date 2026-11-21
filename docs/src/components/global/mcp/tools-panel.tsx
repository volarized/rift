"use client";

import { ArrowClockwiseIcon } from "@phosphor-icons/react";
import { type KeyboardEvent, useState } from "react";

import type { ToolInfo } from "@/components/global/mcp/types";
import { CONTROL, EmptyState, LABEL, PanelHeader } from "@/components/global/mcp/ui";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { cn } from "@/lib/utils";

export type ToolsState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; tools: ToolInfo[] }
  | { status: "failed"; message: string };

/** The filter shows once a list is long enough that scanning it takes effort. */
const FILTER_FROM = 6;

function firstLine(text: string | undefined): string {
  return text?.trim().split("\n", 1)[0] ?? "";
}

function matches(tool: ToolInfo, needle: string): boolean {
  return [tool.name, tool.title, tool.description].some((text) =>
    text?.toLowerCase().includes(needle),
  );
}

/** The tools `tools/list` returned. Arrow keys move the selection, Enter opens the form. */
export function ToolsPanel({
  className,
  onActivate,
  onRefresh,
  onSelect,
  selected,
  state,
}: {
  className?: string;
  onActivate: (name: string) => void;
  onRefresh: () => void;
  onSelect: (name: string) => void;
  selected?: string;
  state: ToolsState;
}) {
  const [filter, setFilter] = useState("");
  const tools = state.status === "ready" ? state.tools : [];
  const needle = filter.trim().toLowerCase();
  const visible = needle === "" ? tools : tools.filter((tool) => matches(tool, needle));
  // One stop in the tab order: the selected tool, or the first when it is filtered out.
  const tabStop = visible.some((tool) => tool.name === selected) ? selected : visible[0]?.name;

  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const target =
      event.key === "ArrowDown"
        ? index + 1
        : event.key === "ArrowUp"
          ? index - 1
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? visible.length - 1
              : null;
    if (target === null) return;
    event.preventDefault();
    const next = visible[Math.max(0, Math.min(visible.length - 1, target))];
    if (!next) return;
    onSelect(next.name);
    const list = event.currentTarget.closest("ul");
    list?.querySelector<HTMLElement>(`[data-tool="${CSS.escape(next.name)}"]`)?.focus();
  };

  return (
    <section aria-label="Tools" className={cn("flex min-h-0 min-w-0 flex-col", className)}>
      <PanelHeader
        title={
          <>
            Tools <span className="tabular-nums">{tools.length > 0 ? tools.length : ""}</span>
          </>
        }
      >
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="List the tools again"
          title="List the tools again"
          disabled={state.status === "idle" || state.status === "loading"}
          onClick={onRefresh}
          className="ml-auto text-muted-foreground max-md:size-9"
        >
          <ArrowClockwiseIcon weight="light" />
        </Button>
      </PanelHeader>
      {tools.length >= FILTER_FROM ? (
        <div className="shrink-0 border-b border-border p-2">
          <Input
            type="search"
            aria-label="Filter tools"
            placeholder="Filter tools"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            spellCheck={false}
            autoComplete="off"
            className={CONTROL}
          />
        </div>
      ) : null}
      <ScrollArea className="min-h-0 flex-1 max-lg:*:data-[slot=scroll-area-viewport]:max-h-72">
        {state.status === "idle" ? (
          <EmptyState>Connect to a server to list its tools</EmptyState>
        ) : state.status === "loading" ? (
          <div role="status" className="flex min-h-32 items-center justify-center px-6 py-10">
            <p className={cn(LABEL, "motion-safe:animate-pulse")}>Listing tools</p>
          </div>
        ) : state.status === "failed" ? (
          <div role="alert" className="grid justify-items-start gap-3 p-3">
            <p className="text-xs text-destructive">The server did not list its tools.</p>
            <p className="text-xs break-words">{state.message}</p>
            <Button type="button" variant="outline" size="sm" onClick={onRefresh}>
              Retry
            </Button>
          </div>
        ) : tools.length === 0 ? (
          <EmptyState>The server lists no tools</EmptyState>
        ) : visible.length === 0 ? (
          <EmptyState>No tool matches the filter</EmptyState>
        ) : (
          <ul>
            {visible.map((tool, index) => {
              const current = tool.name === selected;
              const summary = firstLine(tool.description);
              return (
                <li key={tool.name} className="border-b border-border">
                  <button
                    type="button"
                    data-tool={tool.name}
                    aria-current={current ? "true" : undefined}
                    tabIndex={tool.name === tabStop ? 0 : -1}
                    onClick={() => onSelect(tool.name)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") {
                        event.preventDefault();
                        onActivate(tool.name);
                      } else onKeyDown(event, index);
                    }}
                    className={cn(
                      "grid w-full cursor-pointer gap-0.5 border-l py-2 pr-3 pl-[calc(0.75rem-1px)] text-left outline-none focus-visible:ring-1 focus-visible:ring-ring focus-visible:ring-inset",
                      current
                        ? "border-l-foreground text-foreground"
                        : "border-l-transparent text-foreground/75 hover:text-foreground",
                    )}
                  >
                    <span className="truncate font-mono text-[13px]">{tool.name}</span>
                    {tool.title && tool.title !== tool.name ? (
                      <span className="truncate text-xs">{tool.title}</span>
                    ) : null}
                    {summary ? (
                      <span className="truncate text-xs text-muted-foreground">{summary}</span>
                    ) : null}
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </ScrollArea>
    </section>
  );
}
