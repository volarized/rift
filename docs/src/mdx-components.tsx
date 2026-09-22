import { Tab } from "fumadocs-ui/components/tabs";
import defaultMdxComponents from "fumadocs-ui/mdx";
import type { MDXComponents } from "mdx/types";
import { CliHelp } from "@/components/cli-help";
import { McpTool } from "@/components/mcp-tool";
import { PlatformTabs } from "@/components/platform-tabs";
import { VersionBadge } from "@/components/version-badge";

// The overrides merge through `Object.assign` rather than a spread.
// `MDXComponents` carries an index signature over every JSX intrinsic
// element, `@react-three/fiber` augments that set with three.js elements
// whose props resolve to `never`, and spreading the parameter into an object
// literal makes TypeScript re-check those members against the signature,
// where `Component<never>` is not assignable. `Object.assign` copies the same
// own properties, last argument winning, without that re-check.
export function getMDXComponents(components?: MDXComponents): MDXComponents {
  return Object.assign(
    {
      ...defaultMdxComponents,
      CliHelp,
      McpTool,
      PlatformTabs,
      Tab,
      VersionBadge,
    },
    components,
  );
}
