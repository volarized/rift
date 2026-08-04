import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { Prose } from "@/components/protocol/prose";
import { Shape } from "@/components/protocol/shape";
import { mcpResources, mcpTools, referenceTypes } from "@/lib/protocol-surface";

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
        {mcpTools.length} tools. Each answers against the default branch unless you pass a{" "}
        <code>rev</code>, and each reports the revision it actually resolved to.
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
      <p>
        Resources are read by URI rather than called. A URI carries its own revision, so a link
        keeps meaning the same thing when it is handed to someone else.
      </p>

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

      <h2 id="type-reference" className="scroll-m-20">
        Type reference
      </h2>
      <p>
        The remaining {referenceTypes.mcp.length} types this document defines, in schema order.
        Types the tools and resources above already showed are anchored there instead.
      </p>
      {referenceTypes.mcp.map((name) => (
        <Definition key={name} name={name} />
      ))}
    </>
  );
}
