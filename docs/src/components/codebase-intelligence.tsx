import {
  AsteriskIcon,
  BracketsCurlyIcon,
  ClockCounterClockwiseIcon,
  CpuIcon,
  DatabaseIcon,
  FileTextIcon,
  GlobeHemisphereWestIcon,
  GraphIcon,
  HardDrivesIcon,
  PlugsConnectedIcon,
  TextTIcon,
} from "@phosphor-icons/react/ssr";
import type { ReactNode } from "react";

import { FlatDiagram } from "@/components/flat-diagram";
import { Badge } from "@/components/ui/badge";

const CHART = `
  flowchart BT
    subgraph surface["MCP entrypoints"]
      local_mcp["Local server<br/>Project reads"]
      global_mcp["Global server<br/>Direct MCP · Planned"]
    end

    subgraph local["Local index"]
      local_source["Current project"]
      local_text["Text"]
      local_syntax["Syntax"]
      local_semantics["Semantics<br/>Symbol references on request"]
      local_documentation["Documentation<br/>Planned"]
      local_context["Revision + origin"]
      local_facts["Project facts"]
    end

    subgraph global["Global index · Planned"]
      global_source["OSS package versions"]
      global_text["Text"]
      global_syntax["Syntax"]
      global_semantics["Semantics<br/>Package relationships"]
      global_documentation["Documentation<br/>Planned"]
      global_context["Revision + origin"]
      global_facts["OSS package facts"]
    end

    local_source --> local_text --> local_syntax --> local_semantics
    local_syntax --> local_documentation
    local_source --> local_context
    local_text --> local_facts
    local_semantics --> local_facts
    local_documentation --> local_facts
    local_context --> local_facts
    local_facts --> local_mcp

    global_source --> global_text --> global_syntax --> global_semantics
    global_syntax --> global_documentation
    global_source --> global_context
    global_text --> global_facts
    global_semantics --> global_facts
    global_documentation --> global_facts
    global_context --> global_facts
    global_facts --> global_mcp
    global_facts -. "API · project package context" .-> local_mcp
`;

function Logo({ name, size = 24 }: { name: string; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true">
      <image
        href={`${process.env.NEXT_PUBLIC_BASE_PATH ?? ""}/icons/svgl/${name}.svg`}
        width="24"
        height="24"
        className={
          name === "javascript" || name === "typescript" ? "grayscale" : "brightness-0 dark:invert"
        }
      />
    </svg>
  );
}

const ICONS: Record<string, ReactNode> = {
  local_mcp: <Logo name="mcp" />,
  global_mcp: <Logo name="mcp" />,
  local_source: <HardDrivesIcon size={24} />,
  global_source: <GlobeHemisphereWestIcon size={24} />,
  local_text: <TextTIcon size={24} />,
  global_text: <TextTIcon size={24} />,
  local_syntax: <BracketsCurlyIcon size={24} />,
  global_syntax: <BracketsCurlyIcon size={24} />,
  local_semantics: <GraphIcon size={24} />,
  global_semantics: <GraphIcon size={24} />,
  local_documentation: <FileTextIcon size={24} />,
  global_documentation: <FileTextIcon size={24} />,
  local_context: <ClockCounterClockwiseIcon size={24} />,
  global_context: <ClockCounterClockwiseIcon size={24} />,
  local_facts: <DatabaseIcon size={24} />,
  global_facts: <DatabaseIcon size={24} />,
};

export function CodebaseIntelligence() {
  return (
    <section aria-label="Codebase intelligence" className="not-prose my-10">
      <div className="flex flex-wrap justify-between gap-3 border-b border-border pb-4 font-mono text-xs text-muted-foreground">
        <span>Same intelligence layers, different source</span>
        <span>Dashed: global API</span>
      </div>
      <FlatDiagram
        chart={CHART}
        alt="The local index applies text, syntax, semantics, planned documentation, revision, and origin to the current project, then serves those facts through the local MCP server. The planned global index applies the same layers to OSS package versions, stores package relationships, serves direct MCP reads, and supplies package facts to the local server through an API carrying project package context."
        variant="soft"
        icons={ICONS}
        metrics={{
          label: 15,
          note: 13,
          icon: 24,
          gutter: 12,
          margin: 24,
          rankSep: 48,
          nodeSep: 28,
          minDepth: 88,
        }}
        className="mx-auto my-8"
      />
      <p className="border-t border-border pt-4 text-xs text-muted-foreground">
        Select the diagram to zoom. Both indexes use the same intelligence layers. The local server
        reads project facts directly and requests relevant OSS package facts from the global index.
      </p>
    </section>
  );
}

const CAPABILITY_ICONS = {
  Text: <TextTIcon size={14} />,
  Syntax: <BracketsCurlyIcon size={14} />,
  LSP: <PlugsConnectedIcon size={14} />,
  ty: <CpuIcon size={14} />,
} satisfies Record<string, ReactNode>;

type Capability = keyof typeof CAPABILITY_ICONS;

interface LanguageSummary {
  name: string;
  logos: string[];
  icon?: ReactNode;
  local: Capability[];
  global?: Capability[];
  description: string;
  wide?: boolean;
}

const LANGUAGES: LanguageSummary[] = [
  {
    name: "Any language",
    logos: [],
    icon: <AsteriskIcon size={24} />,
    local: ["Text", "LSP"],
    description:
      "Every visible UTF-8 file can participate in local text search. Any configured language identity can start an LSP process, but structured declarations and relationship traversal still require a shipped syntax provider.",
    wide: true,
  },
  {
    name: "Rust",
    logos: ["rust"],
    local: ["Syntax", "LSP"],
    global: ["Syntax", "LSP"],
    description:
      "tree-sitter extracts declarations. A configured rust-analyzer supplies incoming references on request.",
  },
  {
    name: "Python",
    logos: ["python"],
    local: ["Syntax", "ty"],
    global: ["Syntax", "ty"],
    description:
      "tree-sitter extracts declarations. The embedded ty engine supplies references through the same LSP session contract used by external language engines.",
  },
  {
    name: "JavaScript / TypeScript",
    logos: ["javascript", "typescript"],
    local: ["Syntax", "LSP"],
    global: ["Syntax", "LSP"],
    description:
      "Separate syntax providers parse JavaScript, TypeScript, and TSX. Each language identity selects its engine; TypeScript and TSX can share a named typescript-language-server process.",
  },
  {
    name: "Markdown / JSON / YAML / TOML",
    logos: [],
    local: ["Syntax"],
    global: ["Syntax"],
    description:
      "Syntax providers extract document structure and declarations, so documentation and configuration can participate in structured reads alongside program source.",
  },
];

function CapabilityTags({ capabilities }: { capabilities: Capability[] }) {
  return (
    <div className="flex flex-wrap gap-2">
      {capabilities.map((capability) => (
        <Badge
          key={capability}
          variant="outline"
          className="gap-1.5 rounded-md font-mono text-[10px]"
        >
          {CAPABILITY_ICONS[capability]}
          {capability}
        </Badge>
      ))}
    </div>
  );
}

export function CodebaseLanguages() {
  return (
    <div className="not-prose my-8 grid gap-4 lg:grid-cols-2">
      {LANGUAGES.map((language) => (
        <section
          key={language.name}
          className={`rounded-xl border border-border bg-muted/30 p-6 ${language.wide ? "lg:col-span-2" : ""}`}
        >
          <div className="mb-5 flex items-center gap-3">
            {language.logos.length ? (
              <div className="flex shrink-0 gap-2">
                {language.logos.map((logo) => (
                  <Logo key={logo} name={logo} />
                ))}
              </div>
            ) : (
              <span className="shrink-0" aria-hidden="true">
                {language.icon ?? <FileTextIcon size={24} />}
              </span>
            )}
            <h3 className="text-sm font-medium">{language.name}</h3>
          </div>
          <div className="mb-5 grid gap-3">
            <div className="flex flex-wrap items-center gap-3">
              <div className="flex min-w-20 items-center gap-2 font-mono text-[10px] uppercase tracking-wide text-muted-foreground">
                <HardDrivesIcon size={14} />
                Local:
              </div>
              <CapabilityTags capabilities={language.local} />
            </div>
            {language.global ? (
              <div className="flex flex-wrap items-center gap-3">
                <div className="flex min-w-20 items-center gap-2 font-mono text-[10px] uppercase tracking-wide text-muted-foreground">
                  <GlobeHemisphereWestIcon size={14} />
                  Global:
                </div>
                <CapabilityTags capabilities={language.global} />
                <Badge variant="secondary" className="rounded-sm px-1.5 py-0 text-[9px]">
                  Planned
                </Badge>
              </div>
            ) : null}
          </div>
          <p className="text-sm leading-relaxed text-muted-foreground">{language.description}</p>
        </section>
      ))}
    </div>
  );
}
