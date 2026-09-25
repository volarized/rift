import {
  BracketsCurlyIcon,
  CodeIcon,
  FileTextIcon,
  FoldersIcon,
  MapTrifoldIcon,
  PackageIcon,
  PlayIcon,
} from "@phosphor-icons/react/ssr";
import { Card } from "fumadocs-ui/components/card";
import { ServerCodeBlock } from "fumadocs-ui/components/codeblock.rsc";
import { Tab, Tabs, TabsList, TabsTrigger } from "fumadocs-ui/components/tabs";

import { Badge } from "@/components/ui/badge";

// Illustrative map for the guide's example project. Both views use these values.
const exampleMap = {
  revision: "3f9a1c2e",
  languages: [
    { language: "markdown", files: 1, symbols: 1 },
    { language: "rust", files: 3, symbols: 8 },
    { language: "toml", files: 2, symbols: 6 },
  ],
  modules: [{ path: "src", files: 3, symbols: 8 }],
  entry_points: ["rift://symbol/rust/src/main.rs/main"],
  docs: ["README.md"],
  packages: [
    {
      manager: "cargo",
      name: "tokio",
      requirement: "^1.53.1",
      availability: "canonical",
    },
  ],
  pagination: { page_index: 0, total_pages: 1 },
};

const languageNames: Record<string, string> = {
  markdown: "Markdown",
  rust: "Rust",
  toml: "TOML",
};

const cardClass = "min-w-0 rounded-none shadow-none";

export async function WorkspaceMapExample() {
  const json = await ServerCodeBlock({
    code: JSON.stringify(exampleMap, null, 2),
    lang: "json",
    codeblock: {
      title: "rift://map",
      className:
        "m-0 rounded-none border-0 shadow-none [&_pre]:w-full [&_pre]:break-words [&_pre]:whitespace-pre-wrap",
      viewportProps: { "aria-label": "Map JSON", className: "max-h-[32rem]" },
    },
  });

  return (
    <Tabs defaultValue="visualization" className="not-prose my-6 rounded-none">
      <TabsList aria-label="Example map view">
        <TabsTrigger value="visualization">
          <MapTrifoldIcon aria-hidden="true" />
          Visualization
        </TabsTrigger>
        <TabsTrigger value="json">
          <BracketsCurlyIcon aria-hidden="true" />
          JSON
        </TabsTrigger>
      </TabsList>
      <Tab value="visualization" className="@container rounded-none p-4 [&>figure:only-child]:m-0">
        <figure aria-label="Map of the example project">
          <figcaption className="flex flex-wrap items-center justify-between gap-3 border p-4">
            <span className="flex items-center gap-3">
              <MapTrifoldIcon size={24} weight="light" aria-hidden="true" />
              <span>
                <span className="block font-mono text-sm">rift://map</span>
                <span className="text-xs text-muted-foreground">Example project</span>
              </span>
            </span>
            <Badge variant="outline" className="font-mono text-xs text-muted-foreground">
              {exampleMap.revision}
            </Badge>
          </figcaption>
          <div aria-hidden="true" className="mx-auto h-6 w-px bg-border" />
          <div className="grid gap-3 @min-[32rem]:grid-cols-2">
            <Card
              title="Directories"
              icon={<FoldersIcon aria-hidden="true" />}
              className={cardClass}
            >
              {exampleMap.modules.map((module) => (
                <div key={module.path} className="mt-3 border-l pl-3">
                  <code className="text-foreground">{module.path}/</code>
                  <p className="mt-1 text-xs">
                    {module.files} {module.files === 1 ? "file" : "files"} · {module.symbols}{" "}
                    {module.symbols === 1 ? "symbol" : "symbols"}
                  </p>
                </div>
              ))}
            </Card>
            <Card title="Languages" icon={<CodeIcon aria-hidden="true" />} className={cardClass}>
              <dl className="mt-3 space-y-2">
                {exampleMap.languages.map((language) => (
                  <div
                    key={language.language}
                    className="flex flex-wrap justify-between gap-x-3 gap-y-1"
                  >
                    <dt className="text-foreground">{languageNames[language.language]}</dt>
                    <dd className="text-xs">
                      {language.files} {language.files === 1 ? "file" : "files"} ·{" "}
                      {language.symbols} {language.symbols === 1 ? "symbol" : "symbols"}
                    </dd>
                  </div>
                ))}
              </dl>
            </Card>
            <Card
              title="Documentation"
              icon={<FileTextIcon aria-hidden="true" />}
              className={cardClass}
            >
              {exampleMap.docs.map((path) => (
                <p key={path} className="mt-3 break-words font-mono text-foreground">
                  {path}
                </p>
              ))}
            </Card>
            <Card
              title="Dependencies"
              icon={<PackageIcon aria-hidden="true" />}
              className={cardClass}
            >
              {exampleMap.packages.map((pkg) => (
                <div key={pkg.name} className="mt-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <code className="text-foreground">{pkg.name}</code>
                    <Badge variant="outline" className="font-mono">
                      {pkg.manager}
                    </Badge>
                  </div>
                  <p className="mt-2 text-xs">
                    Version requirement: <span className="font-mono">{pkg.requirement}</span>
                  </p>
                </div>
              ))}
            </Card>
            <Card
              title="Entry points"
              icon={<PlayIcon aria-hidden="true" />}
              className={`${cardClass} @min-[32rem]:col-span-2`}
            >
              {exampleMap.entry_points.map((symbol) => (
                <p key={symbol} className="mt-3 break-all font-mono text-xs text-foreground">
                  {symbol}
                </p>
              ))}
            </Card>
          </div>
        </figure>
      </Tab>
      <Tab value="json" className="rounded-none p-0 [&>figure:only-child]:m-0">
        {json}
      </Tab>
    </Tabs>
  );
}
