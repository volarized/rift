"use client";

import { CheckIcon, CopyIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** Selects and copies the command text without its prompt. */
export function Command({
  children,
  prompt = "$",
  copyLabel = "Copy command",
  className,
}: {
  children: string;
  prompt?: string;
  /** Accessible name of the copy button: what it copies. */
  copyLabel?: string;
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
      <pre className="overflow-x-auto border border-border bg-card py-3 pl-3 pr-12 text-left font-mono text-[13px] leading-relaxed transition-colors group-focus-within:border-primary-text/40 group-hover:border-primary-text/40">
        <span className="select-none text-primary-text">{prompt} </span>
        {children}
      </pre>
      <Button
        variant="outline"
        size="icon-sm"
        aria-label={copied ? "Copied" : copyLabel}
        onClick={() => {
          void navigator.clipboard.writeText(children).then(() => {
            setCopied(true);
          });
        }}
        className="absolute inset-y-0 right-2 my-auto text-muted-foreground"
      >
        {copied ? <CheckIcon weight="bold" /> : <CopyIcon weight="regular" />}
      </Button>
    </div>
  );
}
