import { CodeBlock, Pre } from "fumadocs-ui/components/codeblock";
import { ExampleTabs } from "@/components/example-tabs";
import { TypingCode } from "@/components/typing-code";
import generated from "./workspace-examples.generated.json";

const examples = { ...generated.queries, map: generated.map };

function ExampleCode({ source, typing = false }: { source: string; typing?: boolean }) {
  return (
    <CodeBlock allowCopy={false} className="m-0 rounded-none border-0 bg-transparent">
      <Pre className="px-4">{typing ? <TypingCode source={source} /> : <code>{source}</code>}</Pre>
    </CodeBlock>
  );
}

/** Request and both response forms from one MCP capture. */
export function ReadExample({ name }: { name: keyof typeof examples }) {
  const example: {
    arguments: object;
    text: string;
    json: unknown;
  } = examples[name];
  const query = "query" in example.arguments ? example.arguments.query : undefined;

  return (
    <ExampleTabs
      sparql={typeof query === "string" && <ExampleCode source={query} />}
      mcp={<ExampleCode source={JSON.stringify(example.arguments, null, 2)} />}
      text={<ExampleCode source={example.text} typing />}
      json={<ExampleCode source={JSON.stringify(example.json, null, 2)} />}
    />
  );
}
