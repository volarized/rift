import type { BaseLayoutProps } from "fumadocs-ui/layouts/shared";

import { ThemeTogglerButton } from "@/components/animate-ui/components/buttons/theme-toggler";
import { Logo } from "@/components/logo";

type LinkItems = NonNullable<BaseLayoutProps["links"]>;

const globalLink: LinkItems[number] = { text: "Global", url: "/global", active: "nested-url" };

/**
 * Link items of the docs layout.
 *
 * DocsLayout lists link items in the sidebar above the page tree, and takes the same set as
 * the home mobile menu (`on: "menu" | "all"`), so `on` cannot drop an item from the sidebar
 * alone. Install and Docs repeat page tree entries there; only the link that leaves the docs
 * stays.
 */
export const docsLinks: LinkItems = [globalLink];

/**
 * Shared layout configuration.
 *
 * Layouts can still be customised individually from:
 * Home Layout: app/(home)/layout.tsx
 * Docs Layout: app/docs/layout.tsx
 */
export function baseOptions(): BaseLayoutProps {
  return {
    githubUrl: "https://github.com/volarized/rift",
    nav: {
      title: (
        // Own the wordmark weight here: home and docs links inherit different weights.
        <span className="flex items-center justify-items-center gap-2 font-mono text-[15px] font-medium leading-none tracking-[0.04em]">
          <Logo stitches={40} strokeWidth={0.5} className="h-8 w-auto text-primary-text" />
          <span className="text-foreground">rift</span>
        </span>
      ),
      transparentMode: "top",
    },
    // Replaces fumadocs' built-in switch with the animate-ui one, which wipes
    // the new theme across the page via a View Transition.
    themeSwitch: {
      component: (
        <ThemeTogglerButton
          variant="outline"
          size="sm"
          direction="ltr"
          modes={["light", "dark", "system"]}
          className="text-muted-foreground"
        />
      ),
    },
    links: [
      // A shortcut into the docs section: Docs alone carries the active state there.
      { text: "Install", url: "/docs/installation", active: "none" },
      { text: "Docs", url: "/docs", active: "nested-url" },
      globalLink,
    ],
  };
}
