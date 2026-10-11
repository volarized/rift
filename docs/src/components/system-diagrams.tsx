import {
  BracketsCurlyIcon,
  DatabaseIcon,
  FileTextIcon,
  FolderIcon,
  GlobeHemisphereWestIcon,
  HardDrivesIcon,
  TerminalIcon,
  TreeStructureIcon,
} from "@phosphor-icons/react/ssr";
import { FlatDiagram } from "@/components/flat-diagram";
import { McpIcon } from "@/components/mcp-icon";

export function ArchitectureDiagram() {
  return (
    <FlatDiagram
      title="Architectural Overview"
      alt="An agent starts rift mcp, which forwards requests to one local rift server. The server coordinates source discovery and providers, builds the local index, and requests package facts from the Global Index."
      chart={`flowchart TB
        agent["Agent"] --> mcp["rift mcp"] --> server["rift server"]
        server --> sources["Source resolvers"]
        server --> providers["Providers"]
        sources --> index["Local index"]
        providers --> index
        server -.-> global["Global Index"]
      `}
      metrics={{ rankSep: 40, nodeSep: 32 }}
      cards={{
        agent: {
          icon: TerminalIcon,
          description: "The agent sends MCP requests.",
          address: "MCP",
          owner: "Agent",
        },
        mcp: {
          icon: McpIcon,
          description: "Forward requests to the workspace's local server.",
          address: "rift mcp",
          owner: "Rift",
        },
        server: {
          icon: HardDrivesIcon,
          description: "Share project state across requests and agents.",
          address: "rift server",
          owner: "Rift",
        },
        sources: {
          icon: FolderIcon,
          description: "Identify project files and the project's dependency context.",
          address: "Workspace",
          owner: "Rift",
        },
        providers: {
          icon: TreeStructureIcon,
          description: "Analyze source into declarations and syntax facts.",
          address: "Source",
          owner: "Rift",
        },
        index: {
          icon: BracketsCurlyIcon,
          description: "Search project facts and retrieve declarations.",
          address: "Local index",
          owner: "Rift",
        },
        global: { icon: GlobeHemisphereWestIcon, dashed: true },
      }}
    />
  );
}

export function ImplementationDiagram() {
  return (
    <FlatDiagram
      title="Implementation Overview"
      alt="rift mcp uses rmcp to reach the Axum server on Tokio. Providers publish normalized facts into the in-memory index. Toasty writes lexical and vector data to SQLite; rusqlite writes server logs. Package reads reach the Global Index."
      chart={`flowchart TB
        mcp["rift mcp"] --> server["rift server"]
        server --> providers["Providers"]
        providers --> memory["In-memory index"]
        memory --> index["Lexical index"]
        memory --> vectors["Vector index"]
        server --> logs["Server logs"]
        server -.-> global["Global Index"]
      `}
      metrics={{ rankSep: 40, nodeSep: 32 }}
      cards={{
        mcp: {
          icon: McpIcon,
          description: "rmcp forwards MCP requests over Streamable HTTP.",
          address: "rift mcp",
          owner: "Rift",
        },
        server: {
          icon: HardDrivesIcon,
          description: "Axum serves requests on the Tokio runtime.",
          address: "/api/mcp",
          owner: "Rift",
        },
        providers: {
          icon: TreeStructureIcon,
          description: "tree-sitter parses source; normalization assembles the SymbolRecord graph.",
          address: "Providers",
          owner: "Rift",
        },
        memory: {
          icon: BracketsCurlyIcon,
          description: "Publish readable symbols and syntax nodes for project reads.",
          address: "Local index",
          owner: "Rift",
        },
        index: {
          icon: DatabaseIcon,
          description: "Toasty writes lexical search data to SQLite.",
          address: ".rift/index",
          owner: "Rift",
        },
        vectors: {
          icon: DatabaseIcon,
          description: "Toasty writes semantic vectors to SQLite.",
          address: ".rift/vectors",
          owner: "Rift",
        },
        logs: {
          icon: FileTextIcon,
          description: "rusqlite writes the server's recorded diagnostics.",
          address: ".rift/metrics",
          owner: "Rift",
        },
        global: { icon: GlobeHemisphereWestIcon, dashed: true },
      }}
    />
  );
}
