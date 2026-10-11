import {
  BracketsCurlyIcon,
  ClockCounterClockwiseIcon,
  CpuIcon,
  FileTextIcon,
  GlobeHemisphereWestIcon,
  GraphIcon,
  HardDrivesIcon,
  MagnifyingGlassIcon,
  RankingIcon,
  TerminalIcon,
  TextTIcon,
} from "@phosphor-icons/react/ssr";
import type { ReactNode } from "react";
import { FlatDiagram } from "@/components/flat-diagram";
import { McpIcon } from "@/components/mcp-icon";
import { HuggingFace } from "@/components/ui/svgs/huggingFace";
import { Openai } from "@/components/ui/svgs/openai";

const CHART = `
  flowchart BT
    local_agent["Local agent"]
    subgraph surface["MCP entrypoint"]
      local_mcp["Local server<br/>Project reads"]
    end

    subgraph local["Local index"]
      local_source["Current project"]
      local_text["Text"]
      local_syntax["Syntax"]
      local_semantics["Semantics<br/>Symbol references on request"]
      local_documentation["Documentation"]
      local_context["Revision + origin"]
      local_fts["FTS corpus<br/>Names + signatures + docs + file content"]
      local_vector["Vector corpus<br/>Qualified name + declaration source"]
      local_embeddings["Embedding"]
      local_search["Hybrid search<br/>Identifier + BM25 + vector"]
      local_ranking["Ranking<br/>Weighted reciprocal rank fusion"]
    end


    global_index["Global Index"]

    local_source --> local_text --> local_syntax --> local_semantics
    local_syntax --> local_documentation
    local_source --> local_context
    local_documentation --> local_fts
    local_syntax --> local_vector
    local_vector --> local_embeddings
    local_fts --> local_search
    local_embeddings --> local_search
    local_semantics --> local_mcp
    local_search --> local_ranking
    local_ranking -->|local| local_mcp

    local_mcp -- "MCP · project reads" --> local_agent
    local_mcp -. "API · package context only" .-> global_index
`;

const ICONS: Record<string, ReactNode> = {
  local_mcp: <McpIcon size={24} aria-hidden="true" />,
  local_agent: <TerminalIcon size={24} weight="light" />,
  local_source: <HardDrivesIcon size={24} />,
  global_index: <GlobeHemisphereWestIcon size={24} />,
  local_text: <TextTIcon size={24} />,
  local_syntax: <BracketsCurlyIcon size={24} />,
  local_semantics: <GraphIcon size={24} />,
  local_documentation: <FileTextIcon size={24} />,
  local_context: <ClockCounterClockwiseIcon size={24} />,
  local_fts: <TextTIcon size={24} />,
  local_vector: <GraphIcon size={24} />,
  local_embeddings: <CpuIcon size={24} />,
  local_search: <MagnifyingGlassIcon size={24} />,
  local_ranking: <RankingIcon size={24} />,
};

export function CodebaseIntelligence() {
  return (
    <section aria-label="Codebase intelligence" className="not-prose my-10">
      <FlatDiagram
        title="Codebase intelligence"
        chart={CHART}
        alt="The local index applies text, syntax, semantics, documentation, revision, and origin to the current project. Hybrid local search indexes names, signatures, documentation, and file content for full-text search, and qualified names with declaration source for vector search. Identifier, BM25, and vector results enter weighted reciprocal rank fusion. Embeddings can come from Hugging Face, a workspace directory, or an OpenAI-compatible endpoint. A local agent reads project facts through the local server. The Global Index supplies public package facts through a separate API request carrying package context only. The local server exposes local, global, and all search scopes."
        variant="soft"
        cards={{
          local_agent: {
            icon: TerminalIcon,
            description: "The agent reads project facts through MCP.",
            address: "MCP",
            owner: "Agent",
          },
          global_index: { icon: GlobeHemisphereWestIcon, dashed: true },
        }}
        icons={ICONS}
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
