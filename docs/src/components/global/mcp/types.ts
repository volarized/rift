import type { JsonSchema } from "@/components/json-tree";

/** One tool as `tools/list` returns it; the fields the console reads. */
export type ToolInfo = {
  name: string;
  title?: string;
  description?: string;
  inputSchema: JsonSchema;
  outputSchema?: JsonSchema;
};

/** What the server said about itself when the connection opened. */
export type SessionInfo = {
  /** `modern`: per-request metadata (2026-07-28 and later). `legacy`: `initialize` handshake. */
  era: "modern" | "legacy";
  protocolVersion?: string;
  server?: { name: string; title?: string; version: string };
  capabilities?: Record<string, unknown>;
  instructions?: string;
  sessionId?: string;
};

/** One block of a tool result's `content`. */
export type ContentBlock = { type: string; text?: string; [key: string]: unknown };

/** The `result` of `tools/call`. */
export type ToolResult = {
  content?: ContentBlock[];
  structuredContent?: unknown;
  isError?: boolean;
  [key: string]: unknown;
};

/** How one `tools/call` ended. */
export type CallOutcome =
  | { kind: "result"; result: ToolResult }
  | { kind: "protocol-error"; code: number; message: string; data?: unknown }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string; result?: ToolResult };

/** Why a connection attempt, a listing or a ping did not complete. */
export type Failure =
  | { reason: "unreachable"; url: string }
  | { reason: "http"; status: number; statusText: string }
  | { reason: "protocol"; code: number; message: string }
  | { reason: "other"; message: string };

/** One row of the message history: a message this page sent or received. */
export type HistoryEntry = {
  seq: number;
  /** Wall-clock time of the row, in epoch milliseconds. */
  at: number;
  direction: "sent" | "received";
  /**
   * `request`, `notification`, `result` and `error` are JSON-RPC messages.
   * `transport` is an HTTP request with no message, `body` a response body that
   * is not a JSON-RPC message.
   */
  kind: "request" | "notification" | "result" | "error" | "transport" | "body";
  /** The message's method, or the method of the request a response answers. */
  method?: string;
  id?: string | number | null;
  httpMethod: string;
  url: string;
  /** Request headers of a sent row. */
  headers?: Record<string, string>;
  status?: number;
  statusText?: string;
  /** Sent row: time to the response headers. Received row: time since the request left. */
  durationMs?: number;
  failure?: "no-response" | "cancelled";
  /** The JSON-RPC message. */
  payload?: unknown;
  /** A response body that is not JSON. */
  text?: string;
};

export type Recorder = {
  add: (entry: Omit<HistoryEntry, "seq" | "at">) => number;
  update: (seq: number, patch: Partial<HistoryEntry>) => void;
};

export type Session = {
  info: SessionInfo;
  listTools: () => Promise<ToolInfo[]>;
  callTool: (
    name: string,
    args: Record<string, unknown>,
    signal: AbortSignal,
  ) => Promise<CallOutcome>;
  /** `ping` on a legacy connection, `server/discover` on a modern one. */
  ping: () => Promise<void>;
  close: () => Promise<void>;
};
