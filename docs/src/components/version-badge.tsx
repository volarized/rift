import { Badge } from "@/components/ui/badge";

interface VersionBadgeProps {
  version: `v${number}.${number}.${number}`;
}

export function VersionBadge({ version }: VersionBadgeProps) {
  return (
    <Badge variant="outline" className="mb-4">
      Since {version}
    </Badge>
  );
}
