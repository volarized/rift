import { Badge } from "@/components/ui/badge";

interface VersionBadgeProps {
  version: `v${number}.${number}.${number}` | "planned";
}

export function VersionBadge({ version }: VersionBadgeProps) {
  return (
    <Badge variant="outline" className="mb-4">
      {version === "planned" ? "Planned" : `Since ${version}`}
    </Badge>
  );
}
