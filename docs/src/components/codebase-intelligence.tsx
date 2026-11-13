import {
  AsteriskIcon,
  BracketsCurlyIcon,
  ClockCounterClockwiseIcon,
  CpuIcon,
  FileTextIcon,
  GlobeHemisphereWestIcon,
  GraphIcon,
  HardDrivesIcon,
  MagnifyingGlassIcon,
  PlugsConnectedIcon,
  RankingIcon,
  TextTIcon,
} from "@phosphor-icons/react/ssr";
import type { ReactNode } from "react";

import { FlatDiagram } from "@/components/flat-diagram";
import { Badge } from "@/components/ui/badge";
import { HuggingFace } from "@/components/ui/svgs/huggingFace";
import { Openai } from "@/components/ui/svgs/openai";

const CHART = `
  flowchart BT
    subgraph surface["MCP entrypoints"]
      local_mcp["Local server<br/>Project reads"]
      global_mcp["Global server<br/>Direct MCP"]
    end

    subgraph local["Local index"]
      local_source["Current project"]
      local_text["Text"]
      local_syntax["Syntax"]
      local_static["Static analysis<br/>Resolved calls"]
      local_semantics["Semantics<br/>LSP and in-process language engines"]
      local_documentation["Documentation"]
      local_context["Revision + origin"]
      local_fts["FTS corpus<br/>Names + signatures + docs + file content"]
      local_vector["Vector corpus<br/>Qualified name + declaration source"]
      local_embeddings["Embedding"]
      local_search["Hybrid search<br/>Identifier + BM25 + vector"]
      local_ranking["Ranking<br/>Weighted reciprocal rank fusion"]
    end

    subgraph global["Global index"]
      global_source["OSS package versions"]
      global_text["Text"]
      global_syntax["Syntax"]
      global_static["Static analysis<br/>Resolved calls"]
      global_semantics["Semantics<br/>Language engines"]
      global_documentation["Documentation"]
      global_context["Revision + origin"]
      global_search["FTS + vector<br/>(hybrid)"]
      global_ranking["Ranking"]
    end

    local_source --> local_text --> local_syntax --> local_static
    local_syntax --> local_semantics
    local_syntax --> local_documentation
    local_source --> local_context
    local_documentation --> local_fts
    local_syntax --> local_vector
    local_vector --> local_embeddings
    local_fts --> local_search
    local_embeddings --> local_search
    local_static --> local_mcp
    local_semantics --> local_mcp
    local_search --> local_ranking
    local_ranking -->|local| local_mcp

    global_source --> global_text --> global_syntax --> global_static
    global_syntax --> global_semantics
    global_syntax --> global_documentation
    global_source --> global_context
    global_text --> global_search
    global_static --> global_search
    global_semantics --> global_search
    global_documentation --> global_search
    global_context --> global_search
    global_search --> global_ranking
    global_ranking --> global_mcp
    global_ranking -. "API · global / all · project package context" .-> local_mcp
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
  local_static: <GraphIcon size={24} />,
  global_static: <GraphIcon size={24} />,
  local_semantics: <GraphIcon size={24} />,
  global_semantics: <GraphIcon size={24} />,
  local_documentation: <FileTextIcon size={24} />,
  global_documentation: <FileTextIcon size={24} />,
  local_context: <ClockCounterClockwiseIcon size={24} />,
  global_context: <ClockCounterClockwiseIcon size={24} />,
  local_fts: <TextTIcon size={24} />,
  local_vector: <GraphIcon size={24} />,
  local_embeddings: <CpuIcon size={24} />,
  local_search: <MagnifyingGlassIcon size={24} />,
  global_search: <MagnifyingGlassIcon size={24} />,
  local_ranking: <RankingIcon size={24} />,
  global_ranking: <RankingIcon size={24} />,
};

export function CodebaseIntelligence() {
  return (
    <section aria-label="Codebase intelligence" className="not-prose my-10">
      <div className="border-b border-border pb-4 font-mono text-xs text-muted-foreground">
        <span>Overview</span>
      </div>
      <FlatDiagram
        chart={CHART}
        alt="Static analysis connects calls to declarations using syntax. Language servers and in-process engines supply semantic facts. The planned global index applies these layers to collected package versions. Local hybrid search combines identifier, BM25, and vector rankings through weighted reciprocal rank fusion."
        variant="soft"
        icons={ICONS}
        nodeBadges={{
          global_mcp: "Planned",
          global_static: "Planned",
          local_documentation: "Planned",
          global_documentation: "Planned",
          global_search: "Planned",
          global_ranking: "Planned",
        }}
        groupBadges={{ global: "Planned" }}
        connectorRoutes={{
          "global_ranking->global_mcp": { from: "top", to: "bottom" },
          "global_ranking->local_mcp": { from: "left", to: "right" },
        }}
        nodePanels={{
          local_embeddings: {
            title: "Embedding",
            layout: "column",
            minWidth: 460,
            minDepth: 220,
            options: [
              {
                label: "Local",
                note: "Hugging Face / workspace directory",
                icon: <HuggingFace width="100%" height="100%" aria-hidden="true" />,
                weight: 1.4,
              },
              {
                label: "OpenAI-compatible",
                badge: "Planned",
                icon: (
                  <Openai width="100%" height="100%" className="dark:invert" aria-hidden="true" />
                ),
              },
            ],
          },
        }}
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
    </section>
  );
}

const CAPABILITY_ICONS = {
  Text: <TextTIcon size={14} />,
  Syntax: <BracketsCurlyIcon size={14} />,
  "Static analysis": <GraphIcon size={14} />,
  "JavaScript static analysis": <GraphIcon size={14} />,
  LSP: <PlugsConnectedIcon size={14} />,
  "In-process engine": <CpuIcon size={14} />,
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
    local: ["Syntax", "Static analysis", "LSP"],
    global: ["Syntax", "Static analysis", "LSP"],
    description:
      "Tree-sitter extracts declarations. Static analysis resolves calls through supported module paths and bindings. A configured rust-analyzer supplies additional references.",
  },
  {
    name: "Python",
    logos: ["python"],
    local: ["Syntax", "In-process engine"],
    global: ["Syntax", "In-process engine"],
    description:
      "Tree-sitter extracts declarations. An in-process language engine supplies references.",
  },
  {
    name: "JavaScript / TypeScript",
    logos: ["javascript", "typescript"],
    local: ["Syntax", "JavaScript static analysis", "LSP"],
    global: ["Syntax", "JavaScript static analysis", "LSP"],
    description:
      "Separate syntax providers parse JavaScript, TypeScript, and TSX. Static analysis follows supported JavaScript imports to their declarations. TypeScript and TSX can share a named typescript-language-server process.",
  },
  {
    name: "HTML / CSS / Vue / Svelte / Angular",
    logos: [],
    local: ["Syntax", "LSP"],
    global: ["Syntax", "LSP"],
    description:
      "Syntax providers preserve templates, styles, and embedded JavaScript or TypeScript ranges. Angular template ownership follows component declarations or explicit configuration. Tailwind context publishes authored utilities and variants; dynamic expressions retain source facts and warnings. Compiler resolution remains separate.",
  },
  {
    name: "C / C++ / Cython",
    logos: [],
    local: ["Syntax", "LSP"],
    global: ["Syntax", "LSP"],
    description:
      "Syntax providers extract native declarations and include syntax. The .h extension selects C by default; C++ headers require explicit language selection. Cython keeps its own source identity for .pyx, .pxd, and .pxi files.",
  },
  {
    name: "Markdown / JSON / JSONC / YAML / TOML",
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
          className={`rounded-xl border border-border bg-muted/30 px-6 py-4 ${language.wide ? "lg:col-span-2" : ""}`}
        >
          <div className="mb-2 flex items-center gap-3">
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
          <div className="mb-3 grid gap-1.5">
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
