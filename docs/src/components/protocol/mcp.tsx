import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import { Shape } from "@/components/protocol/shape";
import { mcpResources, mcpTools } from "@/lib/protocol-surface";

/**
 * The MCP surface: what an agent can call, and what it gets back.
 *
 * Tools and resources come first because they are the whole point of the
 * document — a reader wants to know what `search` does before meeting
 * `SearchParams`. The type reference at the end picks up everything the surface
 * did not already show.
 */
export function McpReference(): ReactNode {
  return (
    <>
      <h2 id="tools" className="scroll-m-20">
        Tools
      </h2>
      <p>
        Tools work on the default branch unless you pass a <code>rev</code>, and each reports the
        revision it actually resolved to.
      </p>

      {mcpTools.map((tool) => (
        <section key={tool.name}>
          <h3 id={`tool-${tool.name}`} className="scroll-m-20 font-mono">
            {tool.name}
          </h3>
          {tool.description ? (
            <p>
              <Prose text={tool.description} />
            </p>
          ) : null}
          <Shape label="Input" name={tool.params} />
          <Shape label="Output" name={tool.result} />
        </section>
      ))}

      <h2 id="resources" className="scroll-m-20">
        Resources
      </h2>
      <p>Read a resource by URI. The URI carries its revision, so a copied link keeps its state.</p>

      {mcpResources.map((resource) => (
        <section key={resource.name}>
          <h3 id={`resource-${resource.name}`} className="scroll-m-20 font-mono">
            {resource.name}
          </h3>
          {resource.uriTemplate ? (
            <pre>
              <code>{resource.uriTemplate}</code>
            </pre>
          ) : null}
          {resource.description ? (
            <p>
              <Prose text={resource.description} />
            </p>
          ) : null}
          <Shape label="URI" name={resource.uri} />
        </section>
      ))}
    </>
  );
}
