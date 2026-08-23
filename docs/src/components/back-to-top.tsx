"use client";

import { ArrowUpIcon } from "@phosphor-icons/react/dist/ssr";
import { useEffect, useState } from "react";

import { cn } from "@/lib/utils";

/**
 * Sits under the table of contents, in fumadocs' `tableOfContent.footer` slot.
 *
 * The reference page is 200 definitions long and the protocol pages are not
 * much shorter, so getting back to the top of one is otherwise a scroll with no
 * end in sight. fumadocs ships no such control, and the TOC's own links only go
 * to headings - the page title is not one.
 *
 * It appears once the page has moved a viewport's worth, because a button that
 * scrolls to the top while you are already at the top is a button that does
 * nothing.
 */
export function BackToTop() {
  const [shown, setShown] = useState(false);

  useEffect(() => {
    const onScroll = () => {
      const element = document.scrollingElement;
      if (element) setShown(element.scrollTop > element.clientHeight);
    };

    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  return (
    <button
      type="button"
      // Kept mounted rather than unmounted, so the TOC's height does not change
      // under the reader as they scroll past the threshold.
      aria-hidden={!shown}
      tabIndex={shown ? 0 : -1}
      onClick={() => window.scrollTo({ top: 0, behavior: "smooth" })}
      className={cn(
        "mt-2 flex items-center gap-2 py-1.5 text-fd-muted-foreground text-sm transition-[color,opacity] hover:text-fd-foreground",
        shown ? "opacity-100" : "pointer-events-none opacity-0",
      )}
    >
      <ArrowUpIcon size={14} weight="light" className="shrink-0" />
      Back to top
    </button>
  );
}
