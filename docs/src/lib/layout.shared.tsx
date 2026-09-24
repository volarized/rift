import type { BaseLayoutProps } from "fumadocs-ui/layouts/shared";

import { ThemeTogglerButton } from "@/components/animate-ui/components/buttons/theme-toggler";
import { Logo } from "@/components/logo";

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
          <Logo stitches={40} strokeWidth={0.5} className="h-8 w-auto" />
          <span className="text-foreground">rift</span>
        </span>
      ),
      transparentMode: "top",
    },
    // Replaces fumadocs' built-in switch with the animate-ui one, which wipes
    // the new theme across the page via a View Transition instead of swapping
    // it instantly. Ghost variant so it reads as a bare icon in the nav.
    themeSwitch: {
      component: (
        <ThemeTogglerButton
          variant="ghost"
          size="sm"
          direction="ltr"
          modes={["light", "dark", "system"]}
          className="text-muted-foreground hover:text-foreground"
        />
      ),
    },
    // see https://fumadocs.dev/docs/ui/navigation/links
    // links: [
    //   {
    //     text: 'Documentation',
    //     url: '/docs',
    //     active: 'nested-url',
    //   },
    // ],
  };
}
