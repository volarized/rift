import {
  ArrowDownIcon,
  ArrowRightIcon,
  ArrowsClockwiseIcon,
  ArrowUpIcon,
  BracketsCurlyIcon,
  GlobeHemisphereWestIcon,
  MagnifyingGlassIcon,
  MapTrifoldIcon,
  PencilSimpleIcon,
  TreeStructureIcon,
} from "@phosphor-icons/react/ssr";

import { IconCard } from "@/components/icon-card";
import { Badge } from "@/components/ui/badge";

const steps = [
  {
    title: "Map",
    address: "rift://map",
    href: "#resource-map",
    description: "Understand the project structure and dependencies.",
    owner: "Rift",
    icon: MapTrifoldIcon,
    position: "",
    direction: "right",
  },
  {
    title: "Search",
    address: "search",
    href: "#tool-search",
    description: "Find code and documentation relevant to the task.",
    owner: "Rift",
    icon: MagnifyingGlassIcon,
    position: "",
    direction: "right",
  },
  {
    title: "Symbols",
    address: "get_symbol",
    href: "#tool-get_symbol",
    description: "Read the declaration, source, and documentation.",
    owner: "Rift",
    icon: BracketsCurlyIcon,
    position: "",
    direction: "down",
  },
  {
    title: "Nodes",
    address: "nodes",
    href: "#tool-nodes",
    description: "Inspect syntax and choose the range to edit.",
    owner: "Rift",
    icon: TreeStructureIcon,
    position: "@min-[32rem]:col-start-3 @min-[32rem]:row-start-2",
    direction: "left",
  },
  {
    title: "Edit",
    address: "Agent tools",
    href: "#edit-and-index-updates",
    description:
      "Apply changes to project files. This action happens outside of rift with standard agent tooling.",
    owner: "Agent",
    icon: PencilSimpleIcon,
    position: "@min-[32rem]:col-start-2 @min-[32rem]:row-start-2",
    direction: "left",
  },
  {
    title: "Update index",
    address: "Local index",
    href: "/docs/developer/architecture#index",
    description: "Rift detects file changes and updates project facts for the next read.",
    owner: "Rift",
    icon: ArrowsClockwiseIcon,
    position: "@min-[32rem]:col-start-1 @min-[32rem]:row-start-2",
    direction: "up",
  },
] as const;

const arrows = {
  right:
    "@min-[32rem]:top-1/2 @min-[32rem]:-right-[26px] @min-[32rem]:bottom-auto @min-[32rem]:left-auto @min-[32rem]:translate-x-0 @min-[32rem]:-translate-y-1/2 @min-[32rem]:-rotate-90",
  down: "",
  left: "@min-[32rem]:top-1/2 @min-[32rem]:bottom-auto @min-[32rem]:-left-[26px] @min-[32rem]:translate-x-0 @min-[32rem]:-translate-y-1/2 @min-[32rem]:rotate-90",
  up: "hidden @min-[32rem]:block @min-[32rem]:-top-[26px] @min-[32rem]:bottom-auto @min-[32rem]:rotate-180",
} as const;

export function AgentWorkflow() {
  return (
    <figure
      className="not-prose @container my-6 @min-[32rem]:my-8"
      aria-labelledby="agent-workflow-caption"
      aria-describedby="agent-workflow-description"
    >
      <p id="agent-workflow-description" className="sr-only">
        Read the map, search, retrieve a symbol, inspect nodes, then edit with the agent's tools.
        Rift detects project file changes and updates the local index, closing the cycle back to the
        map and subsequent reads. Search and Symbols also draw dependency facts from the global
        index. Each card links to its documentation.
      </p>
      <div className="mb-4 @min-[32rem]:mb-0 @min-[32rem]:grid @min-[32rem]:grid-cols-3 @min-[32rem]:gap-x-8">
        <aside
          aria-label="Dependency information source"
          className="@min-[32rem]:col-span-2 @min-[32rem]:col-start-2"
        >
          <IconCard
            title="Global Index"
            href="/global-index"
            icon={GlobeHemisphereWestIcon}
            className="border-dashed p-3 @min-[32rem]:p-4"
          >
            Public declarations and docs for project dependencies, supplied to Search and Symbols
            through rift.
          </IconCard>
          <div
            aria-hidden="true"
            className="relative hidden h-12 text-muted-foreground @min-[32rem]:block"
          >
            <div className="absolute top-0 left-1/2 h-6 border-l border-dashed border-current" />
            <div className="absolute top-6 right-[calc((100%-2rem)/4)] left-[calc((100%-2rem)/4)] border-t border-dashed border-current" />
            <div className="absolute inset-x-0 top-6 grid grid-cols-2 gap-8">
              <ArrowDownIcon size={24} weight="light" className="mx-auto" />
              <ArrowDownIcon size={24} weight="light" className="mx-auto" />
            </div>
          </div>
        </aside>
      </div>
      <div className="relative pl-4 @min-[32rem]:pl-0">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute top-12 bottom-12 left-0 w-4 border-y border-l border-muted-foreground/60 @min-[32rem]:hidden"
        >
          <ArrowRightIcon
            size={18}
            weight="light"
            className="absolute -top-[9px] right-0 text-muted-foreground"
          />
          <ArrowUpIcon
            size={18}
            weight="light"
            className="absolute top-1/2 -left-[9px] bg-background text-muted-foreground"
          />
        </div>
        <ol className="m-0 grid list-none gap-6 p-0 @min-[32rem]:grid-cols-3 @min-[32rem]:gap-8">
          {steps.map((step) => (
            <li key={step.href} className={`relative min-w-0 ${step.position}`}>
              <IconCard
                href={step.href}
                icon={step.icon}
                title={step.title}
                className={`group p-3 focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-ring @min-[32rem]:flex @min-[32rem]:h-full @min-[32rem]:min-h-40 @min-[32rem]:flex-col @min-[32rem]:p-4 @min-[32rem]:[&>div:last-child]:flex @min-[32rem]:[&>div:last-child]:flex-1 @min-[32rem]:[&>div:last-child]:flex-col ${step.owner === "Agent" ? "border-dashed" : ""}`}
              >
                <p className="mt-2 mb-3 leading-relaxed @min-[32rem]:mt-3 @min-[32rem]:mb-4 @min-[32rem]:flex-1">
                  {step.description}
                </p>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="outline" className="text-[10px] text-muted-foreground">
                    {step.owner}
                  </Badge>
                  <span className="font-mono text-xs text-foreground">{step.address}</span>
                </div>
              </IconCard>
              <ArrowDownIcon
                size={20}
                weight="light"
                aria-hidden="true"
                className={`pointer-events-none absolute -bottom-[22px] left-1/2 -translate-x-1/2 text-muted-foreground @min-[32rem]:-bottom-[26px] ${arrows[step.direction]}`}
              />
            </li>
          ))}
        </ol>
      </div>
    </figure>
  );
}
