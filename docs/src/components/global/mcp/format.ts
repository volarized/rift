import type { Failure, HistoryEntry } from "@/components/global/mcp/types";

/** One sentence for a failure, for a status line. */
export function describeFailure(failure: Failure): string {
  switch (failure.reason) {
    case "unreachable":
      return `No response from ${failure.url}. The server is unreachable from this browser, or it does not allow cross-origin requests from this page.`;
    case "http":
      return `The server answered HTTP ${failure.status}${failure.statusText ? ` ${failure.statusText}` : ""}.`;
    case "protocol":
      return `The server answered JSON-RPC error ${failure.code}: ${failure.message}`;
    case "other":
      return failure.message;
  }
}

/** The failure a thrown value carries, or its message when it carries none. */
export function failureFrom(error: unknown): Failure {
  if (typeof error === "object" && error !== null && "failure" in error) {
    return (error as { failure: Failure }).failure;
  }
  return { reason: "other", message: error instanceof Error ? error.message : String(error) };
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

export function formatClock(at: number): string {
  const date = new Date(at);
  const part = (value: number, width = 2) => String(value).padStart(width, "0");
  return `${part(date.getHours())}:${part(date.getMinutes())}:${part(date.getSeconds())}.${part(date.getMilliseconds(), 3)}`;
}

/** Text of a JSON value for a raw view, cut at `limit` characters. */
export function prettyJson(value: unknown, limit = 200_000): { text: string; cut: number } {
  const text = JSON.stringify(value, null, 2) ?? "";
  return text.length > limit
    ? { text: text.slice(0, limit), cut: text.length - limit }
    : { text, cut: 0 };
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

/** The HTTP request of a sent row as a curl command. */
export function toCurl(entry: HistoryEntry): string {
  const lines = [`curl -sS -X ${entry.httpMethod} ${shellQuote(entry.url)}`];
  for (const [name, value] of Object.entries(entry.headers ?? {})) {
    lines.push(`  -H ${shellQuote(`${name}: ${value}`)}`);
  }
  if (entry.payload !== undefined) lines.push(`  -d ${shellQuote(JSON.stringify(entry.payload))}`);
  return lines.join(" \\\n");
}

/** The `mcpServers` entry an MCP client reads to reach `url`. */
export function clientConfiguration(name: string, url: string): string {
  return JSON.stringify({ mcpServers: { [name]: { url } } }, null, 2);
}
