"use client";

import { CaretRightIcon, CircleIcon, CircleNotchIcon, WarningIcon } from "@phosphor-icons/react";

import type { SessionInfo } from "@/components/global/mcp/types";
import { LABEL } from "@/components/global/mcp/ui";
import { type JsonValue, PayloadTree } from "@/components/json-tree";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { cn } from "@/lib/utils";

export type Connection =
  | { status: "idle" }
  | { status: "connecting" }
  | { status: "connected"; info: SessionInfo }
  | { status: "failed"; message: string };

export type PingState =
  | { status: "idle" }
  | { status: "pending" }
  | { status: "done"; ok: boolean; text: string };

const OUTLINE = "font-mono font-normal";

function StatusGlyph({ status }: { status: Connection["status"] }) {
  const size = "size-3 shrink-0";
  switch (status) {
    case "connecting":
      return (
        <CircleNotchIcon
          aria-hidden="true"
          className={cn(size, "motion-safe:animate-spin motion-reduce:opacity-60")}
        />
      );
    case "connected":
      // A lit lamp: the dot stays, the halo behind it pulses unless motion is reduced.
      return (
        <span aria-hidden="true" className={cn(size, "relative grid place-items-center")}>
          <span className="absolute size-2 rounded-full bg-positive opacity-60 motion-safe:animate-ping" />
          <span className="relative size-2 rounded-full bg-positive" />
        </span>
      );
    case "failed":
      return (
        <WarningIcon aria-hidden="true" weight="light" className={cn(size, "text-destructive")} />
      );
    default:
      return <CircleIcon aria-hidden="true" weight="light" className={size} />;
  }
}

const STATUS_TEXT: Record<Connection["status"], string> = {
  idle: "Not connected",
  connecting: "Connecting",
  connected: "Connected",
  failed: "Could not connect",
};

/** How the two sides agreed on the protocol version, in the specification's words. */
function agreement(info: SessionInfo): string {
  return info.era === "modern" ? "per-request metadata" : "initialize handshake";
}

function Facts({ info }: { info: SessionInfo }) {
  const capabilities = Object.keys(info.capabilities ?? {});
  return (
    <p className="flex flex-wrap gap-x-4 gap-y-0.5 font-mono text-[0.75rem] text-foreground md:col-span-2">
      <span>
        <span className="text-muted-foreground">server </span>
        {info.server ? `${info.server.name} ${info.server.version}` : "not named"}
      </span>
      <span>
        <span className="text-muted-foreground">protocol </span>
        {info.protocolVersion ?? "not stated"}, {agreement(info)}
      </span>
      <span>
        <span className="text-muted-foreground">capabilities </span>
        {capabilities.length > 0 ? capabilities.join(", ") : "none"}
      </span>
    </p>
  );
}

function Details({ info }: { info: SessionInfo }) {
  return (
    <div className="grid gap-x-8 gap-y-4 border-t border-border px-3 py-3 md:grid-cols-2">
      <Facts info={info} />
      <div className="grid content-start gap-1.5">
        <h3 className={LABEL}>Instructions</h3>
        {info.instructions ? (
          <p className="max-h-48 overflow-auto text-xs leading-relaxed whitespace-pre-line">
            {info.instructions}
          </p>
        ) : (
          <p className="text-xs text-muted-foreground">The server sent no instructions.</p>
        )}
        {info.server?.title ? (
          <p className="text-xs text-muted-foreground">
            Title <span className="text-foreground">{info.server.title}</span>
          </p>
        ) : null}
        {info.sessionId ? (
          <p className="text-xs text-muted-foreground">
            Session <code className="font-mono break-all text-foreground">{info.sessionId}</code>
          </p>
        ) : null}
      </div>
      <div className="grid min-w-0 content-start gap-1.5">
        <h3 className={LABEL}>Capabilities</h3>
        <div className="max-h-48 overflow-auto">
          <PayloadTree
            className="font-mono text-[0.75rem] break-words"
            schema={{}}
            value={(info.capabilities ?? {}) as JsonValue}
          />
        </div>
      </div>
    </div>
  );
}

/**
 * The connection to the global MCP endpoint: its state and the controls that
 * open, probe and close it. What the server said about itself once the
 * connection opened sits under Details. The page header names the endpoint.
 */
export function ConnectionPanel({
  connection,
  onCancel,
  onConnect,
  onDisconnect,
  onPing,
  ping,
}: {
  connection: Connection;
  onCancel: () => void;
  onConnect: () => void;
  onDisconnect: () => void;
  onPing: () => void;
  ping: PingState;
}) {
  const { status } = connection;
  const info = status === "connected" ? connection.info : undefined;
  const pingLabel = info?.era === "modern" ? "Discover" : "Ping";
  const open = status === "idle" || status === "failed";

  return (
    <section aria-label="Connection" className="border-b border-border">
      <Collapsible>
        <div className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-2 px-3 py-2">
          <div aria-live="polite" className="grid min-w-0 flex-1 basis-80 gap-1">
            <p
              className={cn(
                LABEL,
                "flex items-center gap-2",
                status !== "idle" && "text-foreground",
              )}
            >
              <StatusGlyph status={status} />
              <span className={status === "failed" ? "text-destructive" : undefined}>
                {STATUS_TEXT[status]}
              </span>
            </p>
            {info ? null : (
              <p className="text-xs text-muted-foreground">
                {status === "failed" ? (
                  <span className="text-foreground">{connection.message}</span>
                ) : status === "connecting" ? (
                  "Waiting for the server to answer."
                ) : (
                  "The page sends no request until you connect again."
                )}
              </p>
            )}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span
              aria-live="polite"
              className={cn(
                "font-mono text-[0.75rem] tabular-nums text-muted-foreground",
                ping.status === "done" && !ping.ok && "text-destructive",
              )}
            >
              {ping.status === "done" ? ping.text : ping.status === "pending" ? "waiting" : ""}
            </span>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!info || ping.status === "pending"}
              onClick={onPing}
              className={OUTLINE}
            >
              {pingLabel}
            </Button>
            <CollapsibleTrigger
              disabled={!info}
              render={
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className={cn(OUTLINE, "group/details")}
                />
              }
            >
              <CaretRightIcon
                aria-hidden="true"
                weight="light"
                className="transition-transform group-data-panel-open/details:rotate-90 motion-reduce:transition-none"
              />
              Details
            </CollapsibleTrigger>
            <Button
              type="button"
              size="sm"
              variant={open ? "default" : "outline"}
              onClick={status === "connected" ? onDisconnect : open ? onConnect : onCancel}
              className={cn("min-w-28 font-mono", !open && OUTLINE)}
            >
              {status === "connected" ? "Disconnect" : open ? "Connect" : "Cancel"}
            </Button>
          </div>
        </div>
        <CollapsibleContent>{info ? <Details info={info} /> : null}</CollapsibleContent>
      </Collapsible>
    </section>
  );
}
