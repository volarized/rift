"use client";

import { useEffect, useState } from "react";
import { CheckIcon, CopyIcon } from "@phosphor-icons/react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/**
 * A shell line. A 1px-outlined region on the void: no fill, no radius, no
 * shadow. The prompt sigil is dim and unselectable so a copy or a drag-select
 * both yield the command alone.
 */
export function Command({
  children,
  prompt = "$",
  className,
}: {
  children: string;
  prompt?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1600);
    return () => clearTimeout(timer);
  }, [copied]);

  return (
    /* `min-w-0` so a long command scrolls inside the box rather than
       widening whatever grid track it was dropped into. */
    <div className={cn("group relative w-full min-w-0", className)}>
      <pre className="overflow-x-auto border border-border px-[18px] py-4 pr-12 text-left font-mono text-[13px] leading-relaxed">
        <span className="select-none text-muted-foreground">{prompt} </span>
        {children}
      </pre>
      <Button
        variant="ghost"
        size="icon-xs"
        aria-label={copied ? "Copied" : "Copy command"}
        onClick={() => {
          void navigator.clipboard.writeText(children).then(() => {
            setCopied(true);
          });
        }}
        className="absolute right-2 top-2 text-muted-foreground opacity-0 transition-opacity focus-visible:opacity-100 group-hover:opacity-100"
      >
        {copied ? <CheckIcon weight="bold" /> : <CopyIcon weight="regular" />}
      </Button>
    </div>
  );
}
