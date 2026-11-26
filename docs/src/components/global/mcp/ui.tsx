"use client";

import { CheckIcon, CopyIcon } from "@phosphor-icons/react";
import { type ReactNode, useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** The label style of the site. */
export const LABEL = "font-mono text-[0.6875rem] uppercase tracking-[0.16em] text-muted-foreground";

/** Text of code and wire values. */
export const CODE = "font-mono text-[13px] leading-relaxed";

/**
 * A control on the void: no fill in either theme. Below `md` the text is 16px,
 * the size under which iOS zooms the page on focus.
 */
export const CONTROL =
  "bg-transparent font-mono text-[13px] max-md:h-10 max-md:text-base md:text-[13px] dark:bg-transparent";

/** A tab of a panel header: the label style, marked by a hairline on the panel's rule. */
export const PANEL_TAB =
  "h-10 flex-none px-2 font-mono text-[0.6875rem] font-normal uppercase tracking-[0.16em] " +
  "focus-visible:ring-1 focus-visible:outline-0 data-active:text-primary-text dark:data-active:text-primary-text after:bg-primary-text " +
  "group-data-horizontal/tabs:after:inset-x-2 group-data-horizontal/tabs:after:-bottom-px group-data-horizontal/tabs:after:h-px";

export const PANEL_TABS = "gap-1 p-0 group-data-horizontal/tabs:h-auto";

function useCopied(): [boolean, (text: string) => void] {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1600);
    return () => clearTimeout(timer);
  }, [copied]);

  const copy = (text: string) => {
    void navigator.clipboard.writeText(text).then(() => setCopied(true));
  };
  return [copied, copy];
}

type CopySource = string | (() => string);

function read(source: CopySource): string {
  return typeof source === "function" ? source() : source;
}

/** Icon button that copies `value`; `label` says what it copies. */
export function CopyButton({
  className,
  label,
  value,
}: {
  className?: string;
  label: string;
  value: CopySource;
}) {
  const [copied, copy] = useCopied();
  return (
    <Button
      type="button"
      variant="ghost"
      size="icon-sm"
      aria-label={copied ? "Copied" : label}
      title={label}
      onClick={() => copy(read(value))}
      className={cn("text-muted-foreground max-md:size-9", className)}
    >
      {copied ? <CheckIcon weight="bold" /> : <CopyIcon weight="light" />}
    </Button>
  );
}

/** Copy button with its label written out. The width holds while it reads "Copied". */
export function CopyTextButton({
  children,
  disabled,
  short,
  value,
}: {
  children: string;
  disabled?: boolean;
  /** Label shown in place of `children` on a phone. */
  short?: string;
  value: CopySource;
}) {
  const [copied, copy] = useCopied();
  return (
    <Button
      type="button"
      variant="outline"
      size="sm"
      disabled={disabled}
      aria-label={copied ? "Copied" : children}
      onClick={() => copy(read(value))}
      className="bg-transparent font-mono font-normal dark:bg-transparent"
    >
      {copied ? <CheckIcon weight="bold" /> : <CopyIcon weight="light" />}
      <span className="grid text-left">
        <span className={cn("col-start-1 row-start-1", copied && "invisible")}>
          {short ? (
            <>
              <span className="sm:hidden">{short}</span>
              <span className="max-sm:hidden">{children}</span>
            </>
          ) : (
            children
          )}
        </span>
        <span
          aria-hidden={!copied}
          className={cn("col-start-1 row-start-1", !copied && "invisible")}
        >
          Copied
        </span>
      </span>
    </Button>
  );
}

/** Header strip of a panel: a label, then whatever the panel puts beside it. */
export function PanelHeader({
  children,
  className,
  title,
  titleClassName,
}: {
  children?: ReactNode;
  className?: string;
  title: ReactNode;
  titleClassName?: string;
}) {
  return (
    <div
      className={cn(
        "flex h-10 shrink-0 items-center gap-3 border-b border-border pr-1.5 pl-3",
        className,
      )}
    >
      <h3 className={cn(LABEL, "shrink-0", titleClassName)}>{title}</h3>
      {children}
    </div>
  );
}

/** What a panel shows while it has nothing to show. */
export function EmptyState({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={cn("flex min-h-32 flex-1 items-center justify-center px-6 py-10", className)}>
      <p className={cn(LABEL, "max-w-80 text-center leading-relaxed")}>{children}</p>
    </div>
  );
}

/** A note under a raw view that was cut. */
export function CutNote({ cut }: { cut: number }) {
  if (cut === 0) return null;
  return (
    <p className={cn(LABEL, "border-t border-border px-3 py-2 normal-case tracking-normal")}>
      {cut.toLocaleString("en-US")} more characters are not shown. Copy to get all of it.
    </p>
  );
}
