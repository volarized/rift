"use client";

import { ApiReferenceReact } from "@scalar/api-reference-react";
import { useTheme } from "next-themes";
import { useMemo } from "react";
import "@scalar/api-reference-react/style.css";

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

const RIFT_THEME = `
  .scalar-app, .scalar-app .dark-mode, .scalar-app .light-mode {
    --scalar-font: var(--font-inter), sans-serif;
    --scalar-font-code: var(--font-geist-mono), monospace;
    --scalar-radius: 0px;
    --scalar-radius-max: 0px;
    --scalar-background-1: var(--background);
    --scalar-background-2: var(--muted);
    --scalar-background-3: var(--border);
    --scalar-color-1: var(--foreground);
    --scalar-color-2: var(--muted-foreground);
    --scalar-color-3: var(--muted-foreground);
    --scalar-color-accent: var(--foreground);
    --scalar-background-accent: var(--muted);
    --scalar-border-color: var(--border);
    --scalar-button-1: var(--foreground);
    --scalar-button-1-color: var(--background);
    --scalar-button-1-hover: var(--muted-foreground);
    --scalar-sidebar-background-1: var(--background);
    --scalar-sidebar-item-hover-background: var(--muted);
    --scalar-sidebar-item-active-background: var(--muted);
    --scalar-sidebar-color-1: var(--foreground);
    --scalar-sidebar-color-2: var(--muted-foreground);
    --scalar-sidebar-color-active: var(--foreground);
    --scalar-sidebar-border-color: var(--border);
  }
  .scalar-app section[role="group"]:has(> div [data-testid="auth-indicator"]) { display: none; }
  .scalar-app .scalar-reference-intro-server { display: none; }
  .scalar-app .client-libraries-list + div .selected-client { display: none; }
  .scalar-app .client-libraries-list { border-bottom: 1px solid var(--scalar-border-color); }
  .scalar-app [class*="rounded"] { border-radius: 0 !important; }
  .scalar-app .section-header,
  .scalar-app .sidebar-heading { font-family: var(--font-geist-mono), monospace; }
`;

export default function GlobalApi() {
  const { resolvedTheme } = useTheme();
  const configuration = useMemo(
    () => ({
      url: `${BASE}/global-api.openapi.json`,
      servers: [{ url: "https://api.volar.sh/rift/rest" }],
      theme: "none" as const,
      darkMode: resolvedTheme === "dark",
      hideDarkModeToggle: true,
      withDefaultFonts: false,
      customCss: RIFT_THEME,
      showDeveloperTools: "never" as const,
      agent: { disabled: true },
      mcp: { disabled: true },
      // Requests stay between the browser and the selected API.
      proxyUrl: "",
    }),
    [resolvedTheme],
  );

  return <ApiReferenceReact configuration={configuration} />;
}
