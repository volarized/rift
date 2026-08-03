import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

import { Logo } from '@/components/logo';

/**
 * Shared layout configuration.
 *
 * Layouts can still be customised individually from:
 * Home Layout: app/(home)/layout.tsx
 * Docs Layout: app/docs/layout.tsx
 */
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        // Baseline-aligned, so the mark's apex sits on the wordmark's baseline
        // and its top edge on the cap line — the two read as one object rather
        // than an icon parked next to some text. The stroke is heavy because it
        // is in viewBox units: at this size 1 unit is ~0.13px, so the hairline
        // that suits the hero disappears here.
        <span className="flex items-center justify-items-center gap-2 font-mono text-[15px] leading-none tracking-[0.04em]">
          <Logo stitches={40} strokeWidth={0.5} className="h-8 w-auto" />
          <span className="text-foreground">rift</span>
        </span>
      ),
      transparentMode: 'top',
    },
    // see https://fumadocs.dev/docs/ui/navigation/links
    links: [
      {
        text: 'Documentation',
        url: '/docs',
        active: 'nested-url',
      },
    ],
  };
}
