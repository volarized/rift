import {
  ArrowDownIcon,
  ArrowRightIcon,
  BracketsCurlyIcon,
  CpuIcon,
  FlowArrowIcon,
  GraphIcon,
  PlugsConnectedIcon,
  TreeStructureIcon,
} from "@phosphor-icons/react/ssr";
import Link from "fumadocs-core/link";

import { IconCard } from "@/components/icon-card";
import { Badge } from "@/components/ui/badge";

// Edge sources, in the order the vision page introduces them.
const sources = [
  {
    title: "Static analysis",
    href: "#static-analysis",
    icon: GraphIcon,
    description: "Joins syntax facts into edges that the index stores.",
    target: "Index",
    example: "",
  },
  {
    title: "Language engine",
    href: "#semantics",
    icon: CpuIcon,
    description: "Runs the language's own analysis inside Rift's process.",
    target: "Read",
    example: "ty",
  },
  {
    title: "LSP",
    href: "/docs/developer/configuration#languages-and-lsp",
    icon: PlugsConnectedIcon,
    description: "Asks a language server through the standard protocol.",
    target: "Read",
    example: "rust-analyzer",
  },
] as const;

// Symbols of the MCP guide's example project, in call order.
const symbols = [
  { name: "main", unit: "src/main.rs" },
  { name: "load_config", unit: "src/config.rs" },
  { name: "parse_config", unit: "src/config.rs" },
] as const;

// Desktop grids share one gap, so the connector lines up with the gaps between symbols.
const grid = "@min-[40rem]:grid-cols-3 @min-[40rem]:gap-x-12";
// Horizontal center of the first and second gap in a three-column grid with a 3rem gap.
const gapCenters = ["calc((100% - 6rem) / 3 + 1.5rem)", "calc((100% - 6rem) * 2 / 3 + 4.5rem)"];

export function SymbolEdges() {
  return (
    <figure
      className="not-prose @container my-8"
      aria-labelledby="symbols-facets-and-edges"
      aria-describedby="symbol-edges-description"
    >
      <p id="symbol-edges-description" className="sr-only">
        Static analysis, an embedded language engine such as ty, and a language server reached
        through LSP supply edges. In the example project, main calls load_config and load_config
        calls parse_config: each call is an edge, and the path of both edges is a flow. The
        load_config symbol is represented by one facet, its declaration in src/config.rs. Each card
        links to its documentation.
      </p>

      <section aria-label="Edge sources" className="@min-[40rem]:px-6">
        <ul className={`m-0 grid list-none gap-4 p-0 ${grid}`}>
          {sources.map((source) => (
            <li key={source.title} className="min-w-0">
              <IconCard
                href={source.href}
                icon={source.icon}
                title={source.title}
                className="flex h-full flex-col border-dashed [&>div:last-child]:flex [&>div:last-child]:flex-1 [&>div:last-child]:flex-col"
              >
                <p className="mt-3 mb-4 flex-1 leading-relaxed">{source.description}</p>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="outline" className="text-[10px] text-muted-foreground">
                    {source.target}
                  </Badge>
                  {source.example ? (
                    <span className="font-mono text-xs text-foreground">{source.example}</span>
                  ) : null}
                </div>
              </IconCard>
            </li>
          ))}
        </ul>
        <div aria-hidden="true" className="relative h-10 text-muted-foreground">
          <ArrowDownIcon
            size={20}
            weight="light"
            className="absolute top-2.5 left-1/2 -translate-x-1/2 @min-[40rem]:hidden"
          />
          <div className={`absolute inset-x-0 top-0 hidden h-5 ${grid} @min-[40rem]:grid`}>
            {sources.map((source) => (
              <div key={source.title} className="mx-auto border-l border-dashed border-current" />
            ))}
          </div>
          <div className="absolute top-5 right-[calc((100%-6rem)/6)] left-[calc((100%-6rem)/6)] hidden border-t border-dashed border-current @min-[40rem]:block" />
          {gapCenters.map((left) => (
            <div
              key={left}
              className="absolute top-5 bottom-0 hidden border-l border-dashed border-current @min-[40rem]:block"
              style={{ left }}
            />
          ))}
        </div>
      </section>

      <section
        aria-label="Flow"
        className="relative border border-dashed border-muted-foreground/60 p-4 pb-14 @min-[40rem]:p-6 @min-[40rem]:pb-14"
      >
        <ol className={`m-0 grid list-none gap-10 p-0 ${grid}`}>
          {symbols.map((symbol, index) => (
            <li key={symbol.name} className="relative min-w-0">
              <IconCard
                href="/docs/mcp#tool-get_symbol"
                icon={BracketsCurlyIcon}
                title={symbol.name}
                className="flex h-full flex-col [&>h3]:font-mono [&>div:last-child]:flex [&>div:last-child]:flex-1 [&>div:last-child]:flex-col"
              >
                <div className="mt-3 flex flex-1 flex-wrap items-end gap-2">
                  <Badge variant="outline" className="text-[10px] text-muted-foreground">
                    Symbol
                  </Badge>
                  <span className="font-mono text-xs text-foreground">{symbol.unit}</span>
                </div>
              </IconCard>
              {index < symbols.length - 1 ? <EdgeMarker /> : null}
              {index === 1 ? (
                <div
                  aria-hidden="true"
                  className="absolute top-full left-1/2 hidden h-14 border-l border-muted-foreground/60 @min-[40rem]:block"
                />
              ) : null}
            </li>
          ))}
        </ol>
        <Link
          href="/docs/developer/protocol/mcp#search-flows"
          className="absolute bottom-4 left-4 flex items-center gap-2 text-sm font-medium text-foreground hover:text-fd-primary @min-[40rem]:left-6"
        >
          <FlowArrowIcon size={20} weight="light" aria-hidden="true" />
          Flow
          <span className="font-mono text-xs font-normal text-muted-foreground">
            SERVICE :flows
          </span>
        </Link>
      </section>

      <div className="@min-[40rem]:px-6">
        <div aria-hidden="true" className={`relative h-10 text-muted-foreground ${grid}`}>
          <ArrowDownIcon
            size={20}
            weight="light"
            className="absolute top-2.5 left-1/2 -translate-x-1/2 @min-[40rem]:static @min-[40rem]:col-start-2 @min-[40rem]:mx-auto @min-[40rem]:mt-2.5 @min-[40rem]:translate-x-0"
          />
        </div>
        <div className={`grid ${grid}`}>
          <IconCard
            href="/docs/mcp#tool-facets"
            icon={TreeStructureIcon}
            title="Facet"
            className="flex h-full flex-col @min-[40rem]:col-start-2 [&>div:last-child]:flex [&>div:last-child]:flex-1 [&>div:last-child]:flex-col"
          >
            <p className="mt-3 mb-4 flex-1 leading-relaxed">
              One declaration: its location, source, and syntax structure.
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant="outline" className="text-[10px] text-muted-foreground">
                load_config
              </Badge>
              <span className="font-mono text-xs text-foreground">src/config.rs</span>
            </div>
          </IconCard>
        </div>
      </div>
    </figure>
  );
}

// An edge between two neighboring symbol cards: below the card on narrow screens, in the gap
// to its right on wide ones, where the connector from the edge sources ends.
function EdgeMarker() {
  return (
    <div
      aria-hidden="true"
      className="pointer-events-none absolute top-full left-1/2 flex h-10 -translate-x-1/2 items-center gap-1.5 text-muted-foreground @min-[40rem]:-top-6 @min-[40rem]:bottom-0 @min-[40rem]:left-full @min-[40rem]:h-auto @min-[40rem]:w-12 @min-[40rem]:translate-x-0 @min-[40rem]:flex-col @min-[40rem]:gap-1"
    >
      <div className="hidden w-0 flex-1 border-l border-dashed border-current @min-[40rem]:block" />
      <ArrowDownIcon size={20} weight="light" className="@min-[40rem]:hidden" />
      <ArrowRightIcon size={20} weight="light" className="hidden @min-[40rem]:block" />
      <span className="font-mono text-[10px]">edge</span>
      <div className="hidden flex-1 @min-[40rem]:block" />
    </div>
  );
}
