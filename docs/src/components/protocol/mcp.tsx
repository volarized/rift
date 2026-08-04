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
        Five tools, and every one of them takes an explicit snapshot to work against. Nothing here reads
        "the current state of the repository" — you say which commit you mean, and you get the same
        answer every time you ask.
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
        Resources are read by URI rather than called. Each one names its leaf and its snapshot in the URI
        itself, which is what makes a link portable: a supervisor can hand a preview URI to a subagent and
        both resolve it to exactly the same bytes.
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
          <Shape label="Link" name={resource.link} />
        </section>
      ))}

      <h2 id="type-reference" className="scroll-m-20">
        Type reference
      </h2>
      <p>
        The remaining {referenceTypes.mcp.length} types this document defines, in schema order. Types the
        tools and resources above already showed are anchored there instead.
      </p>
      {referenceTypes.mcp.map((name) => (
        <Definition key={name} name={name} />
      ))}
    </>
  );
}
