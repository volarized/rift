import {
  Client,
  type FetchLike,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";

import { describeFailure } from "@/components/global/mcp/format";
import type {
  CallOutcome,
  Failure,
  HistoryEntry,
  Recorder,
  Session,
  SessionInfo,
  ToolInfo,
  ToolResult,
} from "@/components/global/mcp/types";

/**
 * The MCP client of the console: the official TypeScript SDK over Streamable
 * HTTP. The SDK owns the protocol (version negotiation, the `initialize`
 * handshake, request metadata headers, SSE). This module hands it a `fetch`
 * that records every HTTP exchange, so the history shows the messages as they
 * crossed the wire and not as the SDK parsed them.
 */

const CLIENT_INFO = { name: "rift-docs-console", version: "1.0.0" };
export const TOKEN_PLACEHOLDER = "<token>";

export class SessionError extends Error {
  readonly failure: Failure;

  constructor(failure: Failure) {
    super(failure.reason);
    this.name = "SessionError";
    this.failure = failure;
  }
}

type RpcId = string | number;

/** State the recording `fetch` shares with the session that owns it. */
type Wire = {
  /** Method of every request sent, by id, to label the response that answers it. */
  methods: Map<RpcId, string>;
  /** Requests in flight, each abortable on its own. */
  inflight: Map<AbortController, string | undefined>;
  /** Id of the last `tools/call` sent, and the response that carried it. */
  lastCallId?: RpcId;
  lastCallResult?: unknown;
  /** The last request that got no HTTP response at all. */
  unreachable?: string;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRpcId(value: unknown): value is RpcId {
  return typeof value === "string" || typeof value === "number";
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

type Described = Pick<HistoryEntry, "kind" | "method" | "id">;

function describeSent(message: unknown): Described {
  if (!isRecord(message) || typeof message.method !== "string") return { kind: "transport" };
  return isRpcId(message.id)
    ? { kind: "request", method: message.method, id: message.id }
    : { kind: "notification", method: message.method };
}

function describeReceived(message: unknown, wire: Wire): Described {
  if (!isRecord(message) || message.jsonrpc !== "2.0") return { kind: "body" };
  if (typeof message.method === "string") {
    return isRpcId(message.id)
      ? { kind: "request", method: message.method, id: message.id }
      : { kind: "notification", method: message.method };
  }
  const id = isRpcId(message.id) ? message.id : null;
  const method = id === null ? undefined : wire.methods.get(id);
  return { kind: "error" in message ? "error" : "result", method, id };
}

function recordedHeaders(init: RequestInit | undefined): Record<string, string> {
  const headers: Record<string, string> = {};
  new Headers(init?.headers).forEach((value, name) => {
    headers[name] = name === "authorization" ? `Bearer ${TOKEN_PLACEHOLDER}` : value;
  });
  return headers;
}

/** Calls `onData` with the `data` of every event of an SSE body. */
async function readEventStream(
  stream: ReadableStream<Uint8Array>,
  onData: (data: string) => void,
): Promise<void> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer = (buffer + decoder.decode(value, { stream: true })).replace(/\r\n?/g, "\n");
      let boundary = buffer.indexOf("\n\n");
      while (boundary !== -1) {
        const data = buffer
          .slice(0, boundary)
          .split("\n")
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).replace(/^ /, ""))
          .join("\n");
        if (data !== "") onData(data);
        buffer = buffer.slice(boundary + 2);
        boundary = buffer.indexOf("\n\n");
      }
    }
  } catch {
    // The stream was aborted: what arrived is already recorded.
  }
}

function recordingFetch(recorder: Recorder, wire: Wire): FetchLike {
  return async (input, init) => {
    const url = String(input);
    const httpMethod = (init?.method ?? "GET").toUpperCase();
    // After `initialize` the SDK offers to open a standalone stream for
    // messages the server starts on its own. The console reads none, so the
    // offer is declined here, as a server without that stream declines it, and
    // no request is sent.
    if (httpMethod === "GET") return new Response(null, { status: 405 });
    const body = typeof init?.body === "string" ? init.body : undefined;
    const message = body === undefined ? undefined : parseJson(body);
    const sent = describeSent(message);
    if (sent.kind === "request" && isRpcId(sent.id) && sent.method) {
      wire.methods.set(sent.id, sent.method);
      if (sent.method === "tools/call") {
        wire.lastCallId = sent.id;
        wire.lastCallResult = undefined;
      }
    }

    // The request gets a controller of its own, so a cancelled call closes its
    // HTTP stream whatever the protocol revision does about cancellation.
    const controller = new AbortController();
    const outer = init?.signal;
    if (outer?.aborted) controller.abort(outer.reason);
    else outer?.addEventListener("abort", () => controller.abort(outer.reason), { once: true });
    wire.inflight.set(controller, sent.method);

    const started = performance.now();
    const elapsed = () => Math.round(performance.now() - started);
    const seq = recorder.add({
      direction: "sent",
      ...sent,
      httpMethod,
      url,
      headers: recordedHeaders(init),
      payload: message,
    });

    let response: Response;
    try {
      response = await fetch(input, { ...init, signal: controller.signal });
    } catch (error) {
      wire.inflight.delete(controller);
      const cancelled = controller.signal.aborted;
      if (!cancelled) wire.unreachable = url;
      recorder.update(seq, {
        failure: cancelled ? "cancelled" : "no-response",
        durationMs: elapsed(),
      });
      throw error;
    }

    wire.unreachable = undefined;
    const { status, statusText } = response;
    recorder.update(seq, { status, statusText, durationMs: elapsed() });

    const receive = (text: string) => {
      const payload = parseJson(text);
      const received = describeReceived(payload, wire);
      if (received.kind === "result" && received.id === wire.lastCallId && isRecord(payload)) {
        wire.lastCallResult = payload.result;
      }
      recorder.add({
        direction: "received",
        ...received,
        httpMethod,
        url,
        status,
        statusText,
        durationMs: elapsed(),
        ...(received.kind === "body" && payload === undefined ? { text } : { payload }),
      });
    };

    const contentType = response.headers.get("content-type") ?? "";
    if (response.body && contentType.includes("text/event-stream")) {
      const [forClient, forHistory] = response.body.tee();
      void readEventStream(forHistory, receive).finally(() => wire.inflight.delete(controller));
      return new Response(forClient, { status, statusText, headers: response.headers });
    }

    let text = "";
    try {
      text = await response.text();
    } catch {
      // Aborted while the body arrived: the row keeps its status.
    }
    wire.inflight.delete(controller);
    if (text !== "") receive(text);
    return new Response(text === "" ? null : text, {
      status,
      statusText,
      headers: response.headers,
    });
  };
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Names what went wrong from the last exchange first, the thrown error second. */
function failureOf(error: unknown, wire: Wire): Failure {
  if (wire.unreachable) return { reason: "unreachable", url: wire.unreachable };
  if (isRecord(error)) {
    const { code, data, message } = error as { code?: unknown; data?: unknown; message?: unknown };
    if (isRecord(data) && typeof data.status === "number") {
      return {
        reason: "http",
        status: data.status,
        statusText: typeof data.statusText === "string" ? data.statusText : "",
      };
    }
    if (typeof code === "number") {
      return { reason: "protocol", code, message: typeof message === "string" ? message : "" };
    }
  }
  return { reason: "other", message: errorMessage(error) };
}

async function connect(
  mode: "auto" | "legacy",
  endpoint: URL,
  token: string,
  fetchLike: FetchLike,
  signal: AbortSignal,
) {
  const transport = new StreamableHTTPClientTransport(endpoint, {
    fetch: fetchLike,
    requestInit: token === "" ? undefined : { headers: { Authorization: `Bearer ${token}` } },
  });
  const client = new Client(CLIENT_INFO, { versionNegotiation: { mode } });
  try {
    await client.connect(transport, { signal });
  } catch (error) {
    await client.close().catch(() => undefined);
    throw error;
  }
  return { client, transport };
}

/**
 * Opens a connection. The first attempt probes with `server/discover`, the
 * request a 2026-07-28 server answers, and falls back to `initialize` when the
 * server answers it as an earlier revision would. A probe that gets no
 * response is followed by a plain `initialize` as well: a server whose CORS
 * policy does not list the `Mcp-Method` header fails the probe in the browser
 * without being down.
 */
export async function openSession({
  endpoint,
  recorder,
  signal,
  token,
}: {
  endpoint: URL;
  recorder: Recorder;
  signal: AbortSignal;
  token: string;
}): Promise<Session> {
  const wire: Wire = { methods: new Map(), inflight: new Map() };
  const fetchLike = recordingFetch(recorder, wire);
  const abortAll = () => {
    for (const controller of wire.inflight.keys()) controller.abort();
  };
  signal.addEventListener("abort", abortAll, { once: true });

  let connection: Awaited<ReturnType<typeof connect>>;
  try {
    try {
      connection = await connect("auto", endpoint, token, fetchLike, signal);
    } catch (error) {
      // In a browser the SDK reads a probe without a response as an earlier
      // revision and has sent `initialize` itself by now; elsewhere it stops
      // at the probe, and the plain handshake is still untried.
      const handshakeTried = [...wire.methods.values()].includes("initialize");
      if (signal.aborted || !wire.unreachable || handshakeTried) throw error;
      wire.unreachable = undefined;
      connection = await connect("legacy", endpoint, token, fetchLike, signal);
    }
  } catch (error) {
    throw new SessionError(failureOf(error, wire));
  } finally {
    signal.removeEventListener("abort", abortAll);
  }

  const { client, transport } = connection;
  const era = client.getProtocolEra() === "modern" ? "modern" : "legacy";
  const server = client.getServerVersion();
  const info: SessionInfo = {
    era,
    protocolVersion: client.getNegotiatedProtocolVersion(),
    server: server && { name: server.name, title: server.title, version: server.version },
    capabilities: client.getServerCapabilities() as Record<string, unknown> | undefined,
    instructions: client.getInstructions(),
    sessionId: transport.sessionId,
  };

  return {
    info,

    async listTools() {
      wire.unreachable = undefined;
      try {
        const { tools } = await client.listTools(undefined, { cacheMode: "refresh" });
        return tools as ToolInfo[];
      } catch (error) {
        throw new SessionError(failureOf(error, wire));
      }
    },

    async callTool(name, args, callSignal): Promise<CallOutcome> {
      const abortCalls = () => {
        for (const [controller, method] of wire.inflight) {
          if (method === "tools/call") controller.abort();
        }
      };
      callSignal.addEventListener("abort", abortCalls, { once: true });
      wire.lastCallId = undefined;
      wire.lastCallResult = undefined;
      wire.unreachable = undefined;
      // The result as the server sent it, where the SDK returns a parsed copy.
      const wireResult = () =>
        isRecord(wire.lastCallResult) ? (wire.lastCallResult as ToolResult) : undefined;
      try {
        const result = await client.callTool({ name, arguments: args }, { signal: callSignal });
        return { kind: "result", result: wireResult() ?? (result as ToolResult) };
      } catch (error) {
        if (callSignal.aborted) return { kind: "cancelled" };
        // A result arrived and the SDK refused it, as when `structuredContent`
        // does not match the tool's `outputSchema`.
        const refused = wireResult();
        if (refused) return { kind: "failed", message: errorMessage(error), result: refused };
        const failure = failureOf(error, wire);
        if (failure.reason === "protocol") {
          const data = isRecord(error) ? (error as { data?: unknown }).data : undefined;
          return { kind: "protocol-error", code: failure.code, message: failure.message, data };
        }
        return { kind: "failed", message: describeFailure(failure) };
      } finally {
        callSignal.removeEventListener("abort", abortCalls);
      }
    },

    async ping() {
      wire.unreachable = undefined;
      try {
        if (era === "modern") await client.discover();
        else await client.ping();
      } catch (error) {
        throw new SessionError(failureOf(error, wire));
      }
    },

    async close() {
      abortAll();
      if (transport.sessionId) await transport.terminateSession().catch(() => undefined);
      await client.close().catch(() => undefined);
    },
  };
}
