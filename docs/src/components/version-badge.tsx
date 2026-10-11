import { Badge } from "@/components/ui/badge";

import { cn } from "@/lib/utils";

interface VersionBadgeProps {
  version: "planned";
  languages?: string[];
  inline?: boolean;
}

export function VersionBadge({ languages, inline }: VersionBadgeProps) {
  const status = "Planned";
  const label = languages?.length ? `${status} · ${languages.join(", ")}` : status;
  return (
    <Badge variant="outline" className={cn(inline ? "mx-1 align-[-0.2em]" : "mb-4")}>
      {label}
    </Badge>
  );
}
