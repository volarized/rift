import { Tab } from "fumadocs-ui/components/tabs";
import defaultMdxComponents from "fumadocs-ui/mdx";
import type { MDXComponents } from "mdx/types";
import { McpTool } from "@/components/mcp-tool";
import { PlatformTabs } from "@/components/platform-tabs";
import { VersionBadge } from "@/components/version-badge";

// use this function to get MDX components, you will need it for rendering MDX
export function getMDXComponents(components?: MDXComponents): MDXComponents {
  return {
    ...defaultMdxComponents,
    McpTool,
    PlatformTabs,
    Tab,
    VersionBadge,
    ...components,
  };
}
