import { Badge } from "@/components/ui/badge";

import { cn } from "@/lib/utils";

interface VersionBadgeProps {
  version: `v${number}.${number}.${number}` | "planned";
  languages?: string[];
  inline?: boolean;
}

export function VersionBadge({ version, languages, inline }: VersionBadgeProps) {
  const status = version === "planned" ? "Planned" : `Since ${version}`;
  const label = languages?.length ? `${status} · ${languages.join(", ")}` : status;
  return (
    <Badge variant="outline" className={cn(inline ? "mx-1 align-[-0.2em]" : "mb-4")}>
      {label}
    </Badge>
  );
}
