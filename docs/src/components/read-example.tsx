import { Tab } from "fumadocs-ui/components/tabs";
import { ExampleTabs } from "@/components/example-tabs";
import examples from "./read-examples.json";

/** Request and both response forms from one recorded tool call. */
export function ReadExample({ name }: { name: keyof typeof examples }) {
  const example = examples[name];
  return (
    <ExampleTabs items={["call", "text", "json"]}>
      <Tab value="call">
        <pre>
          <code>{JSON.stringify(example.arguments, null, 2)}</code>
        </pre>
      </Tab>
      <Tab value="text">
        <pre>
          <code>{example.text}</code>
        </pre>
      </Tab>
      <Tab value="json">
        <pre>
          <code>{JSON.stringify(example.json, null, 2)}</code>
        </pre>
      </Tab>
    </ExampleTabs>
  );
}
