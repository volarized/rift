"use client";

import { Banner } from "fumadocs-ui/components/banner";
import { usePathname } from "next/navigation";
import { DRAFT, LATEST_VERSION } from "@/lib/versions";

const VERSION_SEGMENT = /^\/docs\/(\d{4}-\d{2}-\d{2}|draft)(\/|$)/;

/**
 * Tells a reader when the page they are on is not the newest released
 * version — MCP's version warning, resolved at build time rather than by
 * probing a redirect, because the version list is baked into the static
 * export that carries this component.
 */
export function VersionBanner() {
  const pathname = usePathname();
  const match = VERSION_SEGMENT.exec(pathname ?? "");
  if (!match) return null;
  const version = match[1];

  if (version === DRAFT) {
    // Before the first cut the draft is the only tree, and a banner over
    // every page announces nothing a reader can act on.
    if (!LATEST_VERSION) return null;
    return (
      <Banner id="draft-version" variant="normal">
        You are viewing a draft of a not-yet-finalized version.{" "}
        <a href={`/docs/${LATEST_VERSION}`} className="underline">
          View the latest version ({LATEST_VERSION})
        </a>
      </Banner>
    );
  }

  if (LATEST_VERSION && version !== LATEST_VERSION) {
    return (
      <Banner id={`old-version-${version}`} variant="normal">
        You are viewing an older version ({version}) of the documentation.{" "}
        <a href={`/docs/${LATEST_VERSION}`} className="underline">
          View the latest version ({LATEST_VERSION})
        </a>
      </Banner>
    );
  }
  return null;
}
