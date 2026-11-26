"use client";

import { useEffect, useState } from "react";

import { formatDuration, prettyJson } from "@/components/global/mcp/format";
import type {
  CallOutcome,
  ContentBlock,
  ToolInfo,
  ToolResult,
} from "@/components/global/mcp/types";
import {
  CODE,
  CopyButton,
  CutNote,
  EmptyState,
  LABEL,
  PANEL_TAB,
  PANEL_TABS,
  PanelHeader,
} from "@/components/global/mcp/ui";
import { type JsonValue, PayloadTree } from "@/components/json-tree";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

export type CallState =
  | { status: "idle" }
  | { status: "pending"; tool: string; startedAt: number }
  | {
      status: "done";
      tool: string;
      outcome: CallOutcome;
      durationMs: number;
      /** Identity of the call: a new result starts with fresh tree state. */
      finishedAt: number;
    };

const VIEWS = [
  { id: "content", label: "Content" },
  { id: "structured", label: "Structured" },
  { id: "raw", label: "Raw" },
] as const;

type View = (typeof VIEWS)[number]["id"];

const TREE = "px-3 py-3 font-mono text-[0.75rem] break-words sm:text-[0.8125rem]";
const TEXT_LIMIT = 200_000;

/** Time since `startedAt`, re-read ten times a second while a call is pending. */
export function Elapsed({ startedAt }: { startedAt: number }) {
  const [now, setNow] = useState(() => performance.now());

  useEffect(() => {
    const timer = setInterval(() => setNow(performance.now()), 100);
    return () => clearInterval(timer);
  }, []);

  return <span className="tabular-nums">{formatDuration(Math.max(0, now - startedAt))}</span>;
}

function resultOf(outcome: CallOutcome): ToolResult | undefined {
  return outcome.kind === "result" || outcome.kind === "failed" ? outcome.result : undefined;
}

/** What the Raw view shows and the copy button copies for it. */
function rawOf(outcome: CallOutcome): unknown {
  if (outcome.kind === "protocol-error") {
    const { code, message, data } = outcome;
    return data === undefined ? { code, message } : { code, message, data };
  }
  return resultOf(outcome);
}

function statusOf(outcome: CallOutcome): { text: string; error: boolean } {
  switch (outcome.kind) {
    case "result":
      return outcome.result.isError
        ? { text: "Tool error", error: true }
        : { text: "Result", error: false };
    case "protocol-error":
      return { text: `JSON-RPC error ${outcome.code}`, error: true };
    case "cancelled":
      return { text: "Cancelled", error: false };
    case "failed":
      return { text: "Failed", error: true };
  }
}

function Block({ block, index }: { block: ContentBlock; index: number }) {
  const text = typeof block.text === "string" ? block.text : undefined;
  return (
    <div className="border-b border-border last:border-b-0">
      <div className="flex h-8 items-center justify-between pr-1.5 pl-3">
        <span className={LABEL}>
          {index + 1}. {block.type}
        </span>
        <CopyButton
          label={`Copy content block ${index + 1}`}
          value={() => text ?? JSON.stringify(block, null, 2)}
        />
      </div>
      {block.type === "text" && text !== undefined ? (
        <>
          <pre className={cn(CODE, "px-3 pb-3 break-words whitespace-pre-wrap")}>
            {text.slice(0, TEXT_LIMIT)}
          </pre>
          <CutNote cut={Math.max(0, text.length - TEXT_LIMIT)} />
        </>
      ) : (
        <PayloadTree
          className={cn(TREE, "pt-0")}
          schema={{}}
          value={block as JsonValue}
          openDepth={2}
          entryLimit={100}
        />
      )}
    </div>
  );
}

function Body({ outcome, tool, view }: { outcome: CallOutcome; tool?: ToolInfo; view: View }) {
  if (outcome.kind === "cancelled") {
    return <EmptyState>The call was cancelled before a result arrived</EmptyState>;
  }
  const result = resultOf(outcome);
  const message =
    outcome.kind === "protocol-error" || outcome.kind === "failed" ? (
      <p role="alert" className="border-b border-border px-3 py-3 text-xs text-destructive">
        {outcome.message}
      </p>
    ) : null;

  if (view === "raw") {
    const raw = rawOf(outcome);
    if (raw === undefined) return message ?? <EmptyState>No result arrived</EmptyState>;
    const { text, cut } = prettyJson(raw);
    return (
      <>
        {message}
        <pre className={cn(CODE, "px-3 py-3 break-words whitespace-pre-wrap")}>{text}</pre>
        <CutNote cut={cut} />
      </>
    );
  }

  if (view === "structured") {
    return (
      <>
        {message}
        {result?.structuredContent === undefined ? (
          <EmptyState>
            {result ? "The result carries no structuredContent" : "No result arrived"}
          </EmptyState>
        ) : (
          <PayloadTree
            className={TREE}
            schema={tool?.outputSchema ?? {}}
            value={result.structuredContent as JsonValue}
            openDepth={2}
            entryLimit={100}
          />
        )}
      </>
    );
  }

  const blocks = Array.isArray(result?.content) ? result.content : [];
  return (
    <>
      {message}
      {blocks.length === 0 ? (
        <EmptyState>
          {result ? "The result carries no content blocks" : "No result arrived"}
        </EmptyState>
      ) : (
        blocks.map((block, index) => (
          // Blocks have no identity of their own, and a result never reorders.
          // biome-ignore lint/suspicious/noArrayIndexKey: position is the identity
          <Block key={index} block={block} index={index} />
        ))
      )}
    </>
  );
}

/** Result of the last `tools/call`: its content blocks, `structuredContent`, and raw JSON. */
export function ResultPanel({
  call,
  className,
  tools,
}: {
  call: CallState;
  className?: string;
  tools: ToolInfo[];
}) {
  const [view, setView] = useState<View>("content");
  const outcome = call.status === "done" ? call.outcome : undefined;
  const status = outcome ? statusOf(outcome) : undefined;
  const tool = call.status === "idle" ? undefined : tools.find((item) => item.name === call.tool);

  return (
    <section aria-label="Result" className={cn("flex min-h-0 min-w-0 flex-col", className)}>
      <Tabs value={view} onValueChange={(value) => setView(value as View)} className="gap-0">
        {/* On a phone the three views take the row, and they name the panel. */}
        <PanelHeader title="Result" titleClassName="max-sm:sr-only">
          <TabsList
            variant="line"
            aria-label="Result view"
            className={cn(PANEL_TABS, "max-sm:-ml-2 sm:ml-auto")}
          >
            {VIEWS.map(({ id, label }) => (
              <TabsTrigger key={id} value={id} className={PANEL_TAB}>
                {label}
              </TabsTrigger>
            ))}
          </TabsList>
          <CopyButton
            label="Copy the result as JSON"
            value={() => (outcome ? (JSON.stringify(rawOf(outcome), null, 2) ?? "") : "")}
            className={cn(
              "max-sm:ml-auto",
              !outcome || rawOf(outcome) === undefined ? "invisible" : undefined,
            )}
          />
        </PanelHeader>
      </Tabs>
      <p
        aria-live="polite"
        className={cn(
          LABEL,
          "flex h-8 shrink-0 items-center gap-2 border-b border-border px-3 normal-case tracking-normal",
        )}
      >
        {call.status === "idle" ? <span>No call yet</span> : null}
        {call.status === "pending" ? (
          <>
            <span className="text-foreground motion-safe:animate-pulse">Calling {call.tool}</span>
            <Elapsed startedAt={call.startedAt} />
          </>
        ) : null}
        {call.status === "done" && status ? (
          <>
            <span className={status.error ? "text-destructive" : "text-foreground"}>
              {status.text}
            </span>
            <span>{call.tool}</span>
            <span className="tabular-nums">{formatDuration(call.durationMs)}</span>
            {outcome?.kind === "result" && outcome.result.isError ? (
              <code className="font-mono">isError: true</code>
            ) : null}
          </>
        ) : null}
      </p>
      <ScrollArea className="min-h-0 flex-1 max-lg:max-h-[28rem]">
        {outcome ? (
          <Body
            key={call.status === "done" ? call.finishedAt : 0}
            outcome={outcome}
            tool={tool}
            view={view}
          />
        ) : (
          <EmptyState>
            {call.status === "pending"
              ? "Waiting for the server"
              : "The result of a call shows here"}
          </EmptyState>
        )}
      </ScrollArea>
    </section>
  );
}
