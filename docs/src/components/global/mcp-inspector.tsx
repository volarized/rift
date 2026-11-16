"use client";

import { type ComponentType, useCallback, useEffect, useState } from "react";

import { Button } from "@/components/ui/button";

const LABEL = "font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground";

type Loaded = { Inspector: ComponentType };

/**
 * MCP half of the console. The console is imported from an effect, so the
 * static export never evaluates it and a visitor who stays on the REST tab
 * never downloads it. The MCP client it uses loads later still, with the first
 * connection.
 */
export function McpInspector() {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [failed, setFailed] = useState(false);

  const load = useCallback(() => {
    setFailed(false);
    import("@/components/global/mcp/inspector")
      .then((module) => setLoaded({ Inspector: module.default }))
      .catch(() => setFailed(true));
  }, []);

  useEffect(load, [load]);

  if (loaded) return <loaded.Inspector />;

  // The reserved height keeps the footer still while the console arrives.
  return (
    <div className="min-h-[40rem]">
      {failed ? (
        <div
          role="alert"
          className="flex min-h-64 flex-col items-center justify-center gap-4 border border-border px-6 py-16"
        >
          <p className={LABEL}>The MCP console did not load</p>
          <Button variant="outline" size="sm" onClick={load}>
            Retry
          </Button>
        </div>
      ) : (
        <div
          role="status"
          className="flex min-h-64 items-center justify-center border border-border px-6 py-16"
        >
          <p className={`${LABEL} motion-safe:animate-pulse`}>Loading the MCP console</p>
        </div>
      )}
    </div>
  );
}
