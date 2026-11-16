"use client";

import { ArrowSquareOutIcon } from "@phosphor-icons/react";
import { type ComponentType, useCallback, useEffect, useState } from "react";

import {
  globalOpenApiUrl,
  RIFT_API_VERSION,
  RIFT_API_VERSION_HEADER,
} from "@/components/global/constants";
import { Button } from "@/components/ui/button";

const LABEL = "font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground";

type Loaded = { Reference: ComponentType };

/**
 * REST half of the console. Scalar is a browser-only bundle of several
 * megabytes, so its module is imported from an effect: the static export never
 * evaluates it, and a visitor who opens the MCP tab never downloads it.
 */
export function ApiReference({ enabled = true }: { enabled?: boolean }) {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [failed, setFailed] = useState(false);

  const load = useCallback(() => {
    setFailed(false);
    import("@/components/global/scalar-reference")
      .then((module) => setLoaded({ Reference: module.ScalarReference }))
      .catch(() => setFailed(true));
  }, []);

  useEffect(() => {
    if (enabled) load();
  }, [enabled, load]);

  return (
    <div className="grid gap-6">
      <div className="flex flex-wrap items-center justify-between gap-x-8 gap-y-3">
        <p className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <span className={LABEL}>Sent with every request</span>
          <code className="font-mono text-[13px]">
            {RIFT_API_VERSION_HEADER}: {RIFT_API_VERSION}
          </code>
        </p>
        <a
          href={globalOpenApiUrl()}
          target="_blank"
          rel="noreferrer noopener"
          className={`${LABEL} inline-flex min-h-6 items-center gap-1.5 underline underline-offset-4 transition-colors hover:text-foreground focus-visible:text-foreground`}
        >
          OpenAPI document
          <ArrowSquareOutIcon aria-hidden="true" size={12} />
        </a>
      </div>

      {/* The reserved height keeps the footer still while the bundle and the
          document arrive. */}
      <div className="min-h-[40rem]">
        {loaded ? (
          <loaded.Reference />
        ) : failed ? (
          <div
            role="alert"
            className="flex min-h-64 flex-col items-center justify-center gap-4 border border-border px-6 py-16"
          >
            <p className={LABEL}>The API reference did not load</p>
            <Button variant="outline" size="sm" onClick={load}>
              Retry
            </Button>
          </div>
        ) : (
          <div
            role="status"
            className="flex min-h-64 items-center justify-center border border-border px-6 py-16"
          >
            <p className={`${LABEL} motion-safe:animate-pulse`}>Loading the API reference</p>
          </div>
        )}
      </div>
    </div>
  );
}
