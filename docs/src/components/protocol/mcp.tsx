import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import { Shape } from "@/components/protocol/shape";
import { mcpResources, mcpToolGroups } from "@/lib/protocol-surface";

/**
 * The MCP surface: what an agent can call, and what it gets back.
 *
 * Tools and resources come first because they are the whole point of the
 * document — a reader wants to know what `search` does before meeting
 * `SearchParams`. The type reference at the end picks up everything the surface
 * did not already show.
 *
 * Tools are grouped by the family the schema puts them in, so a reader who came
 * for one operation lands among the ones it composes with.
 */
export function McpReference(): ReactNode {
  return (
    <>
      <h2 id="tools" className="scroll-m-28">
        Tools
      </h2>
      <p>
        Source-discovery tools read the current session unless you pass a <code>rev</code>, and each
        reports the snapshot it resolved to.
      </p>

      {mcpToolGroups.map((group) => (
        <section key={group.name}>
          <h3 id={`tools-${group.name}`} className="scroll-m-28">
            {group.title}
          </h3>
          <p>
            <Prose text={group.summary} />
          </p>

          {group.tools.map((tool) => (
            <section key={tool.name}>
              <h4 id={`tool-${tool.name}`} className="scroll-m-28 font-mono">
                {tool.name}
              </h4>
              {tool.description ? (
                <p>
                  <Prose text={tool.description} />
                </p>
              ) : null}
              <Shape label="Input" name={tool.params} />
              <Shape label="Output" name={tool.result} />
            </section>
          ))}
        </section>
      ))}

      <h2 id="resources" className="scroll-m-28">
        Resources
      </h2>
      <p>
        Read a resource by URI. An omitted <code>rev</code> selects the current session; a URI that
        carries one keeps the selected repository state.
      </p>

      {mcpResources.map((resource) => (
        <section key={resource.name}>
          <h3 id={`resource-${resource.name}`} className="scroll-m-28 font-mono">
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
