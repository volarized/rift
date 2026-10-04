import type { Icon } from "@phosphor-icons/react";
import { Card, type CardProps } from "fumadocs-ui/components/card";

import { cn } from "@/lib/utils";

type IconCardProps = Omit<CardProps, "icon" | "title"> & {
  icon: Icon;
  title: string;
};

// A square card whose icon sits on the title line.
export function IconCard({ icon: CardIcon, title, className, ...props }: IconCardProps) {
  return (
    <Card
      {...props}
      className={cn("min-w-0 rounded-none bg-background shadow-none", className)}
      title={
        <span className="flex items-start gap-2">
          <CardIcon size={20} weight="light" aria-hidden="true" className="shrink-0" />
          <span className="min-w-0 flex-1 leading-5">{title}</span>
        </span>
      }
    />
  );
}
