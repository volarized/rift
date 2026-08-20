import { Badge } from "@/components/ui/badge";

interface VersionBadgeProps {
  version: `v${number}.${number}.${number}` | "planned";
  languages?: string[];
}

export function VersionBadge({ version, languages }: VersionBadgeProps) {
  const status = version === "planned" ? "Planned" : `Since ${version}`;
  const label = languages?.length ? `${status} · ${languages.join(", ")}` : status;
  return (
    <Badge variant="outline" className="mb-4">
      {label}
    </Badge>
  );
}
