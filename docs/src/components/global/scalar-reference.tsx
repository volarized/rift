"use client";

import { type AnyApiReferenceConfiguration, ApiReferenceReact } from "@scalar/api-reference-react";
import { useTheme } from "next-themes";
import { type KeyboardEvent, useEffect, useMemo } from "react";

// Order matters: Scalar's stylesheet first, the rift theme over it.
import "@scalar/api-reference-react/style.css";
import "@/components/global/scalar-theme.css";

import {
  GLOBAL_REST_ENDPOINT,
  RIFT_API_VERSION,
  RIFT_API_VERSION_HEADER,
} from "@/components/global/constants";

/**
 * In the classic layout an operation row opens on a mouse click only: Enter or
 * Space on the focused row flips `aria-expanded` and renders nothing. This
 * turns the key press into the click the row answers, before Scalar sees it.
 */
function openOperationFromKeyboard(event: KeyboardEvent<HTMLDivElement>) {
  if (event.key !== "Enter" && event.key !== " ") return;
  const { target } = event;
  if (!(target instanceof HTMLElement) || !target.matches(".section-accordion-button")) return;
  event.preventDefault();
  event.stopPropagation();
  target.click();
}

/**
 * Scalar API Reference over the OpenAPI document `ApiReference` fetched. This
 * module is the lazy chunk `ApiReference` imports; nothing else may import it.
 */
export function ScalarReference({ document: content }: { document: Record<string, unknown> }) {
  const { resolvedTheme } = useTheme();
  const mode = resolvedTheme === "light" ? "light" : "dark";

  // The wrapper pushes every new object to `updateConfiguration`, so the
  // configuration changes identity only when the site theme does.
  const configuration = useMemo<AnyApiReferenceConfiguration>(
    () => ({
      content,
      // The document carries no `servers` on purpose; the endpoint comes from here.
      servers: [{ url: GLOBAL_REST_ENDPOINT, description: "Global API" }],
      // No `proxyUrl`: a request goes from the browser straight to the API.
      telemetry: false,
      persistAuth: false,

      // Seven operations: the accordion of `classic` lists them in one screen,
      // where `modern` renders every operation open, about 6400px of page.
      layout: "classic",
      theme: "none",
      withDefaultFonts: false,
      forceDarkModeState: mode,
      hideDarkModeToggle: true,

      // The page owns its URL hash (`#api`, `#mcp`) and the site owns Cmd/Ctrl+K.
      urlRouting: false,
      hideSearch: true,
      showSidebar: false,

      hideClientButton: true,
      showDeveloperTools: "never",
      documentDownloadType: "none",
      mcp: { disabled: true },
      agent: { disabled: true },

      hideModels: true,
      defaultOpenAllTags: true,
      orderSchemaPropertiesBy: "preserve",
      defaultHttpClient: { targetKey: "shell", clientKey: "curl" },

      onBeforeRequest: ({ requestBuilder }) => {
        // The document names the header in its description, not as a parameter,
        // so the request form has no field for it.
        const { headers } = requestBuilder as { headers?: Headers };
        headers?.set(RIFT_API_VERSION_HEADER, RIFT_API_VERSION);
      },
    }),
    [content, mode],
  );

  // Scalar marks <body> with `dark-mode` or `light-mode` when it mounts, which
  // sets `color-scheme` for the whole page. It does not move the mark when
  // `forceDarkModeState` changes and leaves it behind on unmount, so this
  // component keeps the mark on the site theme and removes it on the way out.
  useEffect(() => {
    const { classList } = document.body;
    classList.toggle("dark-mode", mode === "dark");
    classList.toggle("light-mode", mode === "light");
  }, [mode]);
  useEffect(() => () => document.body.classList.remove("light-mode", "dark-mode"), []);

  return (
    <div className="rift-scalar" onKeyDownCapture={openOperationFromKeyboard}>
      <ApiReferenceReact configuration={configuration} />
    </div>
  );
}
