import {
  BracketsCurlyIcon,
  PlugsConnectedIcon,
  TextAlignLeftIcon,
} from "@phosphor-icons/react/ssr";
import { Tabs, TabsList, TabsTrigger } from "fumadocs-ui/components/tabs";
import type { ReactNode } from "react";

// The views one example can show, with the title and icon of each tab.
const views = {
  call: { label: "MCP call", icon: PlugsConnectedIcon },
  text: { label: "Text response", icon: TextAlignLeftIcon },
  json: { label: "JSON response", icon: BracketsCurlyIcon },
} as const;

export type ExampleView = keyof typeof views;

// The tab title of one view: its icon, then its label.
export function ExampleTabTrigger({ view }: { view: ExampleView }) {
  const { label, icon: ViewIcon } = views[view];
  return (
    <TabsTrigger value={view}>
      <ViewIcon aria-hidden="true" />
      {label}
    </TabsTrigger>
  );
}

// One example as tabs. Each child is a `<Tab value="...">` named after a view in `items`.
export function ExampleTabs({ items, children }: { items: ExampleView[]; children: ReactNode }) {
  return (
    <Tabs defaultValue={items[0]} className="rounded-none">
      <TabsList aria-label="Example view">
        {items.map((view) => (
          <ExampleTabTrigger key={view} view={view} />
        ))}
      </TabsList>
      {children}
    </Tabs>
  );
}
