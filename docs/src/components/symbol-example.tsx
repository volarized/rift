import { ServerCodeBlock } from "fumadocs-ui/components/codeblock.rsc";
import { Tab, Tabs } from "fumadocs-ui/components/tabs";

import mcpDocument from "../../public/mcp.json";

export async function SymbolExample() {
  const example = mcpDocument.tools.find((tool) => tool.name === "get_symbol")?.output_schema
    .examples?.[0];
  if (!example || !("hits" in example)) {
    throw new Error("The get_symbol schema must include its response example.");
  }

  // The authored example includes history; the guide's default request asks for source only.
  const response = {
    ...example,
    hits: example.hits.map((hit) =>
      Object.fromEntries(Object.entries(hit).filter(([field]) => field !== "history")),
    ),
  };
  const examples = [
    { tab: "MCP call", title: "get_symbol", value: { name: "load_config" } },
    { tab: "JSON response", title: "get_symbol response", value: response },
  ];
  const blocks = await Promise.all(
    examples.map(async (example) => ({
      tab: example.tab,
      content: await ServerCodeBlock({
        code: JSON.stringify(example.value, null, 2),
        lang: "json",
        codeblock: {
          title: example.title,
          className:
            "m-0 rounded-none border-0 shadow-none [&_pre]:w-full [&_pre]:break-words [&_pre]:whitespace-pre-wrap",
          viewportProps: { "aria-label": example.title, className: "max-h-[32rem]" },
        },
      }),
    })),
  );

  return (
    <Tabs items={examples.map((example) => example.tab)} className="not-prose rounded-none">
      {blocks.map((block) => (
        <Tab
          key={block.tab}
          value={block.tab}
          className="rounded-none p-0 [&>figure:only-child]:m-0"
        >
          {block.content}
        </Tab>
      ))}
    </Tabs>
  );
}
