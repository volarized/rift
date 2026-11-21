"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { GLOBAL_MCP_ENDPOINT } from "@/components/global/constants";
import {
  type Connection,
  ConnectionPanel,
  type PingState,
} from "@/components/global/mcp/connection-panel";
import { GLOBAL_EXAMPLES } from "@/components/global/mcp/examples";
import {
  clientConfiguration,
  describeFailure,
  failureFrom,
  formatDuration,
} from "@/components/global/mcp/format";
import { HistoryPanel } from "@/components/global/mcp/history-panel";
import { type Editor, EMPTY_EDITOR, RequestPanel } from "@/components/global/mcp/request-panel";
import { type CallState, ResultPanel } from "@/components/global/mcp/result-view";
import type { Arguments } from "@/components/global/mcp/schema-form";
import { fieldId } from "@/components/global/mcp/tool-form";
import { ToolsPanel, type ToolsState } from "@/components/global/mcp/tools-panel";
import type { HistoryEntry, Recorder, Session } from "@/components/global/mcp/types";
import { CODE, CopyButton, LABEL } from "@/components/global/mcp/ui";

/** Rows the history keeps; older rows leave as newer ones arrive. */
const HISTORY_ROWS_MAX = 500;

const CLIENT_CONFIGURATION = clientConfiguration("rift-global", GLOBAL_MCP_ENDPOINT);

/**
 * Tool the console selects after the first listing. The server lists `feedback`
 * first, and its example would send a report to the intake.
 */
const FIRST_TOOL = "search";

/**
 * A console for the global MCP server: connect over Streamable HTTP, list the
 * tools, call one, and read every message that crossed the wire. All state
 * lives in this component and nothing is stored. The connection opens when the
 * console mounts, which is when its tab first shows; after a disconnect the
 * page sends nothing until the visitor connects again.
 */
export default function Inspector() {
  const [connection, setConnection] = useState<Connection>({ status: "idle" });
  const [ping, setPing] = useState<PingState>({ status: "idle" });
  const [tools, setTools] = useState<ToolsState>({ status: "idle" });
  const [selected, setSelected] = useState<string>();
  const [editors, setEditors] = useState<Record<string, Editor>>({});
  const [call, setCall] = useState<CallState>({ status: "idle" });
  const [history, setHistory] = useState<HistoryEntry[]>([]);

  const session = useRef<Session | null>(null);
  // Bumped whenever the connection changes hands, so an answer that arrives
  // for an earlier connection is dropped.
  const generation = useRef(0);
  const connecting = useRef<AbortController | null>(null);
  const calling = useRef<AbortController | null>(null);
  const sequence = useRef(0);

  const recorder = useMemo<Recorder>(
    () => ({
      add(entry) {
        sequence.current += 1;
        const row = { ...entry, seq: sequence.current, at: Date.now() };
        setHistory((rows) => [...rows, row].slice(-HISTORY_ROWS_MAX));
        return row.seq;
      },
      update(seq, patch) {
        setHistory((rows) => rows.map((row) => (row.seq === seq ? { ...row, ...patch } : row)));
      },
    }),
    [],
  );

  const listTools = useCallback(async (active: Session, owner: number) => {
    setTools({ status: "loading" });
    try {
      const listed = await active.listTools();
      if (generation.current !== owner) return;
      setTools({ status: "ready", tools: listed });
      setSelected((current) =>
        listed.some((tool) => tool.name === current)
          ? current
          : (listed.find((tool) => tool.name === FIRST_TOOL) ?? listed[0])?.name,
      );
    } catch (error) {
      if (generation.current !== owner) return;
      setTools({ status: "failed", message: describeFailure(failureFrom(error)) });
    }
  }, []);

  const release = useCallback(() => {
    generation.current += 1;
    connecting.current?.abort();
    calling.current?.abort();
    calling.current = null;
    const active = session.current;
    session.current = null;
    if (active) void active.close();
  }, []);

  // Leaving the page closes the connection.
  useEffect(() => release, [release]);

  const connect = async () => {
    release();
    const owner = generation.current;
    const controller = new AbortController();
    connecting.current = controller;
    setConnection({ status: "connecting" });
    setPing({ status: "idle" });
    setTools({ status: "idle" });
    setCall({ status: "idle" });
    try {
      // The SDK loads with the first connection, not with the page.
      const { openSession } = await import("@/components/global/mcp/session");
      const opened = await openSession({
        endpoint: new URL(GLOBAL_MCP_ENDPOINT),
        recorder,
        signal: controller.signal,
      });
      if (generation.current !== owner) {
        void opened.close();
        return;
      }
      session.current = opened;
      setConnection({ status: "connected", info: opened.info });
      void listTools(opened, owner);
    } catch (error) {
      if (generation.current !== owner) return;
      setConnection({ status: "failed", message: describeFailure(failureFrom(error)) });
    }
  };

  const disconnect = () => {
    release();
    setConnection({ status: "idle" });
    setPing({ status: "idle" });
    setTools({ status: "idle" });
    setCall((current) => (current.status === "pending" ? { status: "idle" } : current));
  };

  const cancelConnect = useRef(disconnect);
  cancelConnect.current = disconnect;

  // Declared after the effect that releases on unmount, so a development
  // remount closes the first attempt before the second one opens.
  const connectOnMount = useRef(connect);
  connectOnMount.current = connect;
  useEffect(() => {
    void connectOnMount.current();
  }, []);

  const runPing = async () => {
    const active = session.current;
    if (!active) return;
    const owner = generation.current;
    const name = active.info.era === "modern" ? "server/discover" : "ping";
    const started = performance.now();
    setPing({ status: "pending" });
    try {
      await active.ping();
      if (generation.current !== owner) return;
      const took = formatDuration(performance.now() - started);
      setPing({ status: "done", ok: true, text: `${name} answered in ${took}` });
    } catch (error) {
      if (generation.current !== owner) return;
      setPing({ status: "done", ok: false, text: describeFailure(failureFrom(error)) });
    }
  };

  const runCall = async (args: Arguments) => {
    const active = session.current;
    if (!active) {
      // The connection was closed under the form; the page says so.
      disconnect();
      return;
    }
    if (!selected || call.status === "pending") return;
    const owner = generation.current;
    const controller = new AbortController();
    calling.current = controller;
    const startedAt = performance.now();
    setCall({ status: "pending", tool: selected, startedAt });
    const outcome = await active.callTool(selected, args, controller.signal);
    if (calling.current === controller) calling.current = null;
    if (generation.current !== owner) return;
    setCall({
      status: "done",
      tool: selected,
      outcome,
      durationMs: performance.now() - startedAt,
      finishedAt: performance.now(),
    });
  };

  // Escape cancels the call in flight, or the connection attempt, wherever
  // focus is: a button that starts either one is disabled while it runs.
  const busy = call.status === "pending" || connection.status === "connecting";
  useEffect(() => {
    if (!busy) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      if (calling.current) calling.current.abort();
      else cancelConnect.current();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [busy]);

  const tool =
    tools.status === "ready" ? tools.tools.find((item) => item.name === selected) : undefined;
  const editor = (selected && editors[selected]) || EMPTY_EDITOR;

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-10">
      <div className="min-w-0 border border-border">
        <h2 className="sr-only">MCP console</h2>
        <ConnectionPanel
          connection={connection}
          ping={ping}
          onConnect={() => void connect()}
          onCancel={disconnect}
          onDisconnect={disconnect}
          onPing={() => void runPing()}
        />
        <div className="grid grid-cols-[minmax(0,1fr)] lg:h-[40rem] lg:grid-cols-[17rem_minmax(0,1fr)]">
          <ToolsPanel
            className="max-lg:border-b lg:border-r border-border"
            state={tools}
            selected={selected}
            onSelect={setSelected}
            onActivate={(name) => {
              setSelected(name);
              // The form of the tool renders first; then focus moves into it.
              requestAnimationFrame(() => {
                const form = document.getElementById("mcp-call");
                form
                  ?.querySelector<HTMLElement>(`[id^="${CSS.escape(fieldId(name, ""))}"]`)
                  ?.focus();
              });
            }}
            onRefresh={() => {
              if (session.current) void listTools(session.current, generation.current);
            }}
          />
          <div className="grid min-h-0 min-w-0 grid-cols-[minmax(0,1fr)] lg:grid-rows-2 xl:grid-cols-2 xl:grid-rows-1">
            <div id="mcp-call" className="flex min-h-0 min-w-0 flex-col">
              <RequestPanel
                className="flex-1 border-border max-xl:border-b xl:border-r"
                tool={tool}
                editor={editor}
                examples={selected ? GLOBAL_EXAMPLES[selected] : undefined}
                pendingSince={call.status === "pending" ? call.startedAt : undefined}
                onEditorChange={(next) => {
                  if (selected) setEditors((current) => ({ ...current, [selected]: next }));
                }}
                onCall={(args) => void runCall(args)}
                onCancel={() => calling.current?.abort()}
              />
            </div>
            <ResultPanel call={call} tools={tools.status === "ready" ? tools.tools : []} />
          </div>
        </div>
        <HistoryPanel
          className="border-t border-border"
          entries={history}
          onClear={() => setHistory([])}
        />
      </div>

      <section aria-labelledby="mcp-client-configuration" className="grid max-w-190 gap-4">
        <h2 id="mcp-client-configuration" className={LABEL}>
          Client configuration
        </h2>
        <p className="text-sm text-muted-foreground">
          To call this server from an MCP client, add it to the client's{" "}
          <code className="font-mono text-[13px] text-foreground">mcpServers</code> configuration:
        </p>
        <div className="group relative min-w-0">
          <pre className={`${CODE} overflow-x-auto border border-border px-[18px] py-4 pr-12`}>
            {CLIENT_CONFIGURATION}
          </pre>
          <CopyButton
            label="Copy the client configuration"
            value={CLIENT_CONFIGURATION}
            className="absolute top-2 right-2"
          />
        </div>
      </section>
    </div>
  );
}
