import {
  ArrowBendUpLeftIcon,
  BracketsCurlyIcon,
  ClockCounterClockwiseIcon,
  FileCodeIcon,
  FileTextIcon,
  GraphIcon,
  LinkIcon,
  PackageIcon,
  PlugsConnectedIcon,
} from "@phosphor-icons/react/ssr";
import type { ReactNode } from "react";

import { FlatDiagram } from "@/components/flat-diagram";
import { Badge } from "@/components/ui/badge";

const CHART = `
  flowchart BT
    subgraph surface["MCP surface"]
      search["search<br/>Discovery + traversal"]
      get_symbol["get_symbol<br/>Source + history"]
    end

    subgraph derived["Derived results"]
      semantic_graph["Semantic graph<br/>Symbol relationships"]
      name_bindings["Name bindings<br/>References to declarations"]
      references["Engine references<br/>Current read"]
      declarations["Declarations<br/>Project + packages"]
      timeline["Symbol history<br/>Revision changes"]
    end

    subgraph facts["Fact providers and source resolvers"]
      binding["Binding provider<br/>Scope + import resolution"]
      engines["Language engines<br/>Name / type resolution"]
      syntax["Syntax<br/>tree-sitter"]
      packages["Dependencies<br/>Package source"]
      history["Git history<br/>Git + tree-sitter"]
    end

    binding -- "resolve names across files" --> name_bindings
    name_bindings -- "index resolved references" --> semantic_graph
    engines -. "incoming references" .-> references
    syntax -- "declarations" --> declarations
    packages -- "public declarations" --> declarations
    history -- "compare revisions" --> timeline
    semantic_graph -- "relationship traversal" --> search
    references -. "merge callers" .-> search
    declarations -- "rank matches" --> search
    declarations -- "look up symbol" --> get_symbol
    timeline -- "include history" --> get_symbol
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
  search: <Logo name="mcp" />,
  get_symbol: <Logo name="mcp" />,
  declarations: <FileCodeIcon size={24} />,
  semantic_graph: <GraphIcon size={24} />,
  name_bindings: <LinkIcon size={24} />,
  references: <ArrowBendUpLeftIcon size={24} />,
  timeline: <ClockCounterClockwiseIcon size={24} />,
  syntax: <BracketsCurlyIcon size={24} />,
  packages: <PackageIcon size={24} />,
  binding: <LinkIcon size={24} />,
  engines: <PlugsConnectedIcon size={24} />,
  history: <Logo name="git" />,
};

export function CodebaseIntelligence() {
  return (
    <section aria-label="Codebase intelligence" className="not-prose my-10">
      <div className="flex flex-wrap justify-between gap-3 border-b border-border pb-4 font-mono text-xs text-muted-foreground">
        <span>Facts flow upward into MCP reads</span>
        <span>Dashed: on request</span>
      </div>
      <FlatDiagram
        chart={CHART}
        alt="At the top, search consumes declarations, indexed relationships, and engine references; get_symbol consumes declarations and symbol history. In the derived section, name bindings connect references to declarations and supply the semantic graph. Below them, the binding provider resolves scopes and imports across files, syntax and dependencies supply declarations, language engines supply incoming references on request, and Git revisions supply compared symbol states."
        variant="soft"
        icons={ICONS}
        metrics={{
          label: 15,
          note: 13,
          icon: 24,
          gutter: 12,
          margin: 24,
          rankSep: 56,
          nodeSep: 32,
          minDepth: 88,
        }}
        className="mx-auto my-8"
      />
      <p className="border-t border-border pt-4 text-xs text-muted-foreground">
        Select the diagram to zoom. Engine references join the current read; the index retains
        relationships resolved by name binding.
      </p>
    </section>
  );
}

const LANGUAGES = [
  {
    name: "Rust",
    logos: ["rust"],
    facts: ["Syntax", "Rift name binding", "LSP"],
    description:
      "tree-sitter extracts declarations and binding facts in one parse. Name binding resolves references across modules; a configured rust-analyzer supplies incoming references on request.",
    dependencies: "Cargo: public declarations from .rs source.",
  },
  {
    name: "Python",
    logos: ["python"],
    facts: ["Syntax", "Embedded ty"],
    description:
      "tree-sitter extracts declarations. The embedded ty engine supplies references through the same LSP session contract used by external language engines.",
    dependencies: "uv: public names from .pyi stubs, or .py source when stubs are absent.",
  },
  {
    name: "JavaScript / TypeScript",
    logos: ["javascript", "typescript"],
    facts: ["Syntax", "Configured LSP"],
    description:
      "Separate syntax providers parse JavaScript, TypeScript, and TSX. Each language identity selects its engine; TypeScript and TSX can share a named typescript-language-server process.",
    dependencies: "npm / Bun: public declarations from TypeScript .d.ts files.",
  },
  {
    name: "Markdown / JSON / YAML / TOML",
    logos: [],
    facts: ["Syntax"],
    description:
      "Syntax providers extract document structure and declarations, so documentation and configuration can participate in structured reads alongside program source.",
    dependencies:
      "Project source: syntax nodes and byte ranges identify the text each read returns.",
  },
];

export function CodebaseLanguages() {
  return (
    <div className="not-prose my-8 grid gap-4 lg:grid-cols-2">
      {LANGUAGES.map((language) => (
        <section key={language.name} className="rounded-xl border border-border bg-muted/30 p-6">
          <div className="mb-5 flex items-center gap-3">
            {language.logos.length ? (
              <div className="flex shrink-0 gap-2">
                {language.logos.map((logo) => (
                  <Logo key={logo} name={logo} />
                ))}
              </div>
            ) : (
              <FileTextIcon size={24} className="shrink-0" aria-hidden="true" />
            )}
            <h3 className="text-sm font-medium">{language.name}</h3>
          </div>
          <div className="mb-4 flex flex-wrap gap-2">
            {language.facts.map((fact) => (
              <Badge key={fact} variant="outline" className="rounded-md font-mono text-[10px]">
                {fact}
              </Badge>
            ))}
          </div>
          <p className="text-sm leading-relaxed text-muted-foreground">{language.description}</p>
          <p className="mt-5 border-t border-border pt-4 text-xs leading-relaxed text-muted-foreground">
            {language.dependencies}
          </p>
        </section>
      ))}
    </div>
  );
}
