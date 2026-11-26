import { CodeBlock, Pre } from "fumadocs-ui/components/codeblock";
import { Tab } from "fumadocs-ui/components/tabs";
import { ExampleTabs, type ExampleView } from "@/components/example-tabs";
import recorded from "./read-examples.json";
import generated from "./workspace-examples.generated.json";

const examples = { ...recorded, ...generated.queries, map: generated.map };

/** Request and both response forms from one MCP capture. */
export function ReadExample({ name }: { name: keyof typeof examples }) {
  const example: {
    arguments: object;
    text: string;
    json: unknown;
    query?: string;
  } = examples[name];
  const views: Record<ExampleView, string> = {
    query: example.query ?? "",
    call: JSON.stringify(example.arguments, null, 2),
    text: example.text,
    json: JSON.stringify(example.json, null, 2),
  };
  const items: ExampleView[] = example.query
    ? ["query", "call", "text", "json"]
    : ["call", "text", "json"];

  return (
    <ExampleTabs items={items}>
      {items.map((view) => (
        <Tab key={view} value={view}>
          <CodeBlock allowCopy={false} className="rounded-none border-0 bg-transparent">
            <Pre className="px-4">
              <code>{views[view]}</code>
            </Pre>
          </CodeBlock>
        </Tab>
      ))}
    </ExampleTabs>
  );
}
