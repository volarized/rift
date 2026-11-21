"use client";

import { type ComponentType, useCallback, useEffect, useState } from "react";

import { globalOpenApiUrl } from "@/components/global/constants";
import { Button } from "@/components/ui/button";

const LABEL = "font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground";

type OpenApiDocument = Record<string, unknown>;

type Loaded = {
  Reference: ComponentType<{ document: OpenApiDocument }>;
  document: OpenApiDocument;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The document without its security declarations. The contract lists an
 * optional bearer scheme; the page sends no credential, so the reference
 * renders no authentication form.
 */
function withoutSecurity(document: OpenApiDocument): OpenApiDocument {
  const { security: _security, ...rest } = document;
  if (isRecord(rest.components)) {
    const { securitySchemes: _schemes, ...components } = rest.components;
    rest.components = components;
  }
  if (isRecord(rest.paths)) {
    rest.paths = Object.fromEntries(
      Object.entries(rest.paths).map(([path, item]) => [
        path,
        isRecord(item)
          ? Object.fromEntries(
              Object.entries(item).map(([method, operation]) => {
                if (!isRecord(operation)) return [method, operation];
                const { security: _operationSecurity, ...kept } = operation;
                return [method, kept];
              }),
            )
          : item,
      ]),
    );
  }
  return rest;
}

async function fetchDocument(): Promise<OpenApiDocument> {
  const response = await fetch(globalOpenApiUrl());
  if (!response.ok) throw new Error(`OpenAPI document: HTTP ${response.status}`);
  const document: unknown = await response.json();
  if (!isRecord(document)) throw new Error("OpenAPI document: not an object");
  return withoutSecurity(document);
}

/**
 * API half of the console. Scalar is a browser-only bundle of several
 * megabytes, so its module is imported from an effect: the static export never
 * evaluates it, and a visitor who opens the MCP tab never downloads it.
 */
export function ApiReference({ enabled = true }: { enabled?: boolean }) {
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [failed, setFailed] = useState(false);

  const load = useCallback(() => {
    setFailed(false);
    Promise.all([import("@/components/global/scalar-reference"), fetchDocument()])
      .then(([module, document]) => setLoaded({ Reference: module.ScalarReference, document }))
      .catch(() => setFailed(true));
  }, []);

  useEffect(() => {
    if (enabled) load();
  }, [enabled, load]);

  return (
    <div className="grid">
      {/* The reserved height keeps the footer still while the bundle and the
          document arrive. */}
      <div className="min-h-[40rem]">
        {loaded ? (
          <loaded.Reference document={loaded.document} />
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
