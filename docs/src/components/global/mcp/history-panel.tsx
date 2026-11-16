"use client";

import { ArrowDownIcon, ArrowUpIcon, CaretRightIcon } from "@phosphor-icons/react";
import { useEffect, useRef, useState } from "react";

import { formatClock, formatDuration, prettyJson, toCurl } from "@/components/global/mcp/format";
import type { HistoryEntry } from "@/components/global/mcp/types";
import {
  CODE,
  CopyTextButton,
  CutNote,
  EmptyState,
  LABEL,
  PanelHeader,
} from "@/components/global/mcp/ui";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { ScrollArea } from "@/components/ui/scroll-area";
import { cn } from "@/lib/utils";

// Direction, time, method, id, HTTP, duration. Time and id give way on a phone.
const COLUMNS =
  "grid grid-cols-[1rem_minmax(0,1fr)_4.5rem_4rem] items-center gap-x-3 px-3 " +
  "sm:grid-cols-[1rem_6.5rem_minmax(0,1fr)_minmax(0,9rem)_5rem_4.5rem]";

function methodText(entry: HistoryEntry): string {
  if (entry.kind === "transport") {
    return entry.httpMethod === "DELETE" ? "end session" : "request";
  }
  if (entry.kind === "body") return "response body";
  return entry.method ?? "response";
}

function kindText(entry: HistoryEntry): string | null {
  switch (entry.kind) {
    case "notification":
      return "notification";
    case "error":
      return "error";
    case "result":
      return "result";
    default:
      return null;
  }
}

function httpText(entry: HistoryEntry): string {
  if (entry.failure === "cancelled") return "cancelled";
  if (entry.failure === "no-response") return "no response";
  if (entry.direction === "sent") {
    return entry.status === undefined ? entry.httpMethod : `${entry.httpMethod} ${entry.status}`;
  }
  return String(entry.status ?? "");
}

function Row({ entry }: { entry: HistoryEntry }) {
  const [open, setOpen] = useState(false);
  const sent = entry.direction === "sent";
  const failed =
    entry.failure === "no-response" || (entry.status !== undefined && entry.status >= 400);
  const kind = kindText(entry);
  const detail = open ? prettyJson(entry.payload ?? null) : null;

  return (
    <Collapsible open={open} onOpenChange={setOpen} className="border-b border-border">
      <CollapsibleTrigger
        className={cn(
          COLUMNS,
          "group/row min-h-9 w-full max-md:min-h-11 cursor-pointer py-1.5 text-left font-mono text-[0.75rem] outline-none hover:text-foreground focus-visible:ring-1 focus-visible:ring-ring focus-visible:ring-inset",
          sent ? "text-foreground" : "text-foreground/75",
        )}
      >
        <span className="relative flex items-center text-muted-foreground">
          {sent ? (
            <ArrowUpIcon aria-hidden="true" weight="light" className="size-3.5" />
          ) : (
            <ArrowDownIcon aria-hidden="true" weight="light" className="size-3.5" />
          )}
          <span className="sr-only">{sent ? "Sent" : "Received"}</span>
        </span>
        <span className="tabular-nums text-muted-foreground max-sm:hidden">
          {formatClock(entry.at)}
        </span>
        <span className="flex min-w-0 items-center gap-2">
          <CaretRightIcon
            aria-hidden="true"
            weight="light"
            className="size-3 shrink-0 text-muted-foreground transition-transform group-data-panel-open/row:rotate-90 motion-reduce:transition-none"
          />
          <span className="truncate">{methodText(entry)}</span>
          {kind ? (
            <span
              className={cn(
                "shrink-0",
                entry.kind === "error" ? "text-destructive" : "text-muted-foreground max-sm:hidden",
              )}
            >
              {kind}
            </span>
          ) : null}
        </span>
        <span className="truncate text-muted-foreground max-sm:hidden">
          {entry.id === undefined || entry.id === null ? "" : `id ${entry.id}`}
        </span>
        <span className={cn("truncate tabular-nums", failed && "text-destructive")}>
          {httpText(entry)}
        </span>
        <span className="text-right tabular-nums text-muted-foreground">
          {entry.durationMs === undefined ? "" : formatDuration(entry.durationMs)}
        </span>
      </CollapsibleTrigger>
      <CollapsibleContent className="border-t border-border">
        {sent ? (
          <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-0.5 border-b border-border px-3 py-2 font-mono text-[0.75rem]">
            <dt className="text-muted-foreground">{entry.httpMethod}</dt>
            <dd className="break-all">{entry.url}</dd>
            {Object.entries(entry.headers ?? {}).map(([name, value]) => (
              <div key={name} className="contents">
                <dt className="text-muted-foreground">{name}</dt>
                <dd className="break-all">{value}</dd>
              </div>
            ))}
          </dl>
        ) : null}
        {entry.payload !== undefined && detail ? (
          <>
            <pre
              // biome-ignore lint/a11y/noNoninteractiveTabindex: a scrollable region must be reachable by keyboard
              tabIndex={0}
              className={cn(
                CODE,
                "max-h-80 overflow-auto px-3 py-2 break-words whitespace-pre-wrap outline-none focus-visible:ring-1 focus-visible:ring-ring focus-visible:ring-inset",
              )}
            >
              {detail.text}
            </pre>
            <CutNote cut={detail.cut} />
          </>
        ) : entry.text !== undefined ? (
          <pre
            className={cn(CODE, "max-h-80 overflow-auto px-3 py-2 break-words whitespace-pre-wrap")}
          >
            {entry.text.slice(0, 20_000)}
          </pre>
        ) : (
          <p className={cn(LABEL, "px-3 py-2")}>
            {entry.failure === "no-response"
              ? "No response arrived"
              : entry.failure === "cancelled"
                ? "Cancelled before a response arrived"
                : "No message body"}
          </p>
        )}
        <div className="flex flex-wrap gap-2 border-t border-border px-3 py-2">
          {entry.payload !== undefined ? (
            <CopyTextButton value={() => JSON.stringify(entry.payload, null, 2)}>
              Copy JSON
            </CopyTextButton>
          ) : null}
          {sent ? <CopyTextButton value={() => toCurl(entry)}>Copy as curl</CopyTextButton> : null}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}

/** Every message the page sent or received, oldest first. */
export function HistoryPanel({
  className,
  entries,
  onClear,
}: {
  className?: string;
  entries: HistoryEntry[];
  onClear: () => void;
}) {
  const root = useRef<HTMLDivElement>(null);
  // The list follows its newest row while the visitor has not scrolled away from it.
  const pinned = useRef(true);
  const count = entries.length;
  const lastRequest = entries.findLast(
    (entry) => entry.direction === "sent" && entry.payload !== undefined,
  );

  useEffect(() => {
    const viewport = root.current?.querySelector<HTMLElement>("[data-slot=scroll-area-viewport]");
    if (!viewport) return;
    const onScroll = () => {
      pinned.current = viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 24;
    };
    viewport.addEventListener("scroll", onScroll, { passive: true });
    return () => viewport.removeEventListener("scroll", onScroll);
  }, []);

  useEffect(() => {
    if (count === 0) pinned.current = true;
    const viewport = root.current?.querySelector<HTMLElement>("[data-slot=scroll-area-viewport]");
    if (viewport && pinned.current) viewport.scrollTop = viewport.scrollHeight;
  }, [count]);

  return (
    <section aria-label="Message history" className={cn("flex min-w-0 flex-col", className)}>
      <PanelHeader
        title={
          <>
            History <span className="tabular-nums">{count > 0 ? count : ""}</span>
          </>
        }
      >
        <div className="ml-auto flex items-center gap-2">
          <CopyTextButton
            short="Copy as curl"
            disabled={!lastRequest}
            value={() => (lastRequest ? toCurl(lastRequest) : "")}
          >
            Copy last request as curl
          </CopyTextButton>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={count === 0}
            onClick={onClear}
            className="bg-transparent font-mono font-normal dark:bg-transparent"
          >
            Clear
          </Button>
        </div>
      </PanelHeader>
      <div
        aria-hidden="true"
        className={cn(COLUMNS, LABEL, "h-8 shrink-0 border-b border-border text-[0.625rem]")}
      >
        <span />
        <span className="max-sm:hidden">Time</span>
        <span className="pl-5">Message</span>
        <span className="max-sm:hidden">Id</span>
        <span>HTTP</span>
        <span className="text-right">Duration</span>
      </div>
      <ScrollArea ref={root} className="h-80 *:data-[slot=scroll-area-viewport]:overscroll-contain">
        {count === 0 ? (
          <EmptyState className="h-80">
            Every JSON-RPC message this page sends or receives is listed here
          </EmptyState>
        ) : (
          <ol aria-label="Messages, oldest first">
            {entries.map((entry) => (
              <li key={entry.seq}>
                <Row entry={entry} />
              </li>
            ))}
          </ol>
        )}
      </ScrollArea>
    </section>
  );
}
