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
import { Card } from "fumadocs-ui/components/card";

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
    title: "Symbol retrieval",
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
    position: "@min-[40rem]:col-start-3 @min-[40rem]:row-start-2",
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
    position: "@min-[40rem]:col-start-2 @min-[40rem]:row-start-2",
    direction: "left",
  },
  {
    title: "Update index",
    address: "Local index",
    href: "/docs/developer/architecture#index",
    description: "Rift detects file changes and updates project facts for the next read.",
    owner: "Rift",
    icon: ArrowsClockwiseIcon,
    position: "@min-[40rem]:col-start-1 @min-[40rem]:row-start-2",
    direction: "up",
  },
] as const;

const arrows = {
  right:
    "@min-[40rem]:top-1/2 @min-[40rem]:-right-[26px] @min-[40rem]:bottom-auto @min-[40rem]:left-auto @min-[40rem]:translate-x-0 @min-[40rem]:-translate-y-1/2 @min-[40rem]:-rotate-90",
  down: "",
  left: "@min-[40rem]:top-1/2 @min-[40rem]:bottom-auto @min-[40rem]:-left-[26px] @min-[40rem]:translate-x-0 @min-[40rem]:-translate-y-1/2 @min-[40rem]:rotate-90",
  up: "hidden @min-[40rem]:block @min-[40rem]:-top-[26px] @min-[40rem]:bottom-auto @min-[40rem]:rotate-180",
} as const;

export function AgentWorkflow() {
  return (
    <figure
      className="not-prose @container my-8"
      aria-labelledby="agent-workflow-caption"
      aria-describedby="agent-workflow-description"
    >
      <p id="agent-workflow-description" className="sr-only">
        Read the map, search, retrieve a symbol, inspect nodes, then edit with the agent's tools.
        Rift detects project file changes and updates the local index, closing the cycle back to the
        map and subsequent reads. Search and symbol retrieval also draw dependency facts from the
        global index. Each card links to its documentation.
      </p>
      <div className="mb-6 @min-[40rem]:mb-0 @min-[40rem]:grid @min-[40rem]:grid-cols-3 @min-[40rem]:gap-x-8">
        <aside
          aria-label="Dependency information source"
          className="@min-[40rem]:col-span-2 @min-[40rem]:col-start-2"
        >
          <Card
            title="Global index"
            href="/docs/developer/protocol/mcp#get_symbol-scope"
            icon={<GlobeHemisphereWestIcon aria-hidden="true" />}
            className="rounded-none border-dashed shadow-none"
          >
            Public declarations and docs for project dependencies, supplied to Search and Symbol
            retrieval through rift.
          </Card>
          <div
            aria-hidden="true"
            className="relative hidden h-12 text-muted-foreground @min-[40rem]:block"
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
      <div className="relative pl-6 @min-[40rem]:pl-0">
        <div
          aria-hidden="true"
          className="pointer-events-none absolute top-20 bottom-20 left-0 w-6 border-y border-l border-muted-foreground/60 @min-[40rem]:hidden"
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
        <ol className="m-0 grid list-none gap-8 p-0 @min-[40rem]:grid-cols-3">
          {steps.map((step) => (
            <li key={step.href} className={`relative min-w-0 ${step.position}`}>
              <Card
                href={step.href}
                className={`group flex h-full min-h-40 flex-col rounded-none bg-background p-4 shadow-none focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-ring [&>div:last-child]:flex [&>div:last-child]:flex-1 [&>div:last-child]:flex-col ${step.owner === "Agent" ? "border-dashed" : ""}`}
                title={
                  <span className="flex items-start gap-2">
                    <step.icon size={20} weight="light" aria-hidden="true" className="shrink-0" />
                    <span className="min-w-0 flex-1 leading-5">{step.title}</span>
                  </span>
                }
              >
                <p className="mt-3 mb-4 flex-1 leading-relaxed">{step.description}</p>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="outline" className="text-[10px] text-muted-foreground">
                    {step.owner}
                  </Badge>
                  <span className="font-mono text-xs text-foreground">{step.address}</span>
                </div>
              </Card>
              <ArrowDownIcon
                size={20}
                weight="light"
                aria-hidden="true"
                className={`pointer-events-none absolute -bottom-[26px] left-1/2 -translate-x-1/2 text-muted-foreground ${arrows[step.direction]}`}
              />
            </li>
          ))}
        </ol>
      </div>
    </figure>
  );
}
