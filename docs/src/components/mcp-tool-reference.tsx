import { McpTool, type McpToolDefinition } from "@/components/mcp-tool";
import mcpDocument from "../../public/mcp.json";

export function McpToolReference({ name }: { name: string }) {
  const tool = (mcpDocument.tools as unknown as McpToolDefinition[]).find(
    (candidate) => candidate.name === name,
  );
  if (!tool) throw new Error(`MCP schema has no tool named ${name}`);

  return <McpTool tool={tool} />;
}
