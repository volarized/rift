import type { ReactNode } from "react";
import { Definition } from "@/components/protocol/definition";
import { Prose } from "@/components/protocol/prose";
import { TypeLink } from "@/components/protocol/type-expr";
import {
  defNames,
  defNamesByFile,
  defs,
  documents,
  entryPointRows,
  entryPointSeams,
  entryPointsDescription,
  PROTOCOL_FILES,
  targetTiers,
} from "@/lib/protocol";

/**
 * The whole protocol reference, read straight from protocol/*.json.
 *
 * Definitions are grouped by the document that owns them and keep each
 * document's own `$defs` order. That grouping is the split itself rather than a
 * presentational choice: `core.json` is exactly the vocabulary both other
 * documents are defined in terms of, which is why it comes first.
 *
 * Headings carry explicit ids because the table of contents is built from the
 * same schemas rather than from this markup — see `protocolToc` in
 * src/lib/protocol.ts. The two are edited together; a heading added here needs
 * an entry there or it will not appear in the sidebar.
 */
export function ProtocolReference(): ReactNode {
  return (
    <>
      <p>
        The contract is three documents. <code>core.json</code> holds the vocabulary the other two
        are defined in terms of; <code>mcp.json</code> and <code>adapter.json</code> both reference
        it, and nothing references them back. Every type name below is a link, whichever document it
        lives in.
      </p>

      <h2 id="entry-points" className="scroll-m-20">
        Entry points
      </h2>
      {entryPointsDescription ? (
        <p>
          <Prose text={entryPointsDescription} />
        </p>
      ) : null}

      {entryPointSeams.map(({ file, name, seam }) => (
        <section key={name}>
          <h3 id={`entry-${name}`} className="scroll-m-20 font-mono">
            {name}
          </h3>
          <p className="text-fd-muted-foreground text-sm">
            declared in <code>{file}.json</code>
          </p>
          {seam.description ? (
            <p>
              <Prose text={seam.description} />
            </p>
          ) : null}
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr>
                  <th className="text-left">Member</th>
                  <th className="text-left">Role</th>
                  <th className="text-left">Type</th>
                </tr>
              </thead>
              <tbody>
                {entryPointRows(seam).map((row) => (
                  <tr key={`${row.member}.${row.role}.${row.type}`}>
                    <td>
                      <code className="text-[0.875em]">{row.member}</code>
                    </td>
                    <td>
                      {row.role === "—" ? "—" : <code className="text-[0.875em]">{row.role}</code>}
                    </td>
                    <td>
                      <TypeLink name={row.type} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      ))}

      {targetTiers ? (
        <>
          <h2 id="target-tiers" className="scroll-m-20">
            Target tiers
          </h2>
          <p>
            <Prose text={targetTiers.description} />
          </p>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr>
                  <th className="text-left">Entry</th>
                  <th className="text-left">Pointer</th>
                  <th className="text-left">Tier</th>
                </tr>
              </thead>
              <tbody>
                {targetTiers.rules.map((rule) => (
                  <tr key={`${rule.entry}${rule.pointer}`}>
                    <td>
                      <code className="text-[0.875em]">{rule.entry}</code>
                    </td>
                    <td>
                      <code className="text-[0.875em]">{rule.pointer}</code>
                    </td>
                    <td>
                      {defs[rule.tier] ? (
                        <TypeLink name={rule.tier} />
                      ) : (
                        <code className="text-[0.875em]">{rule.tier}</code>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      ) : null}

      {PROTOCOL_FILES.map((file) => (
        <section key={file}>
          <h2 id={`${file}-json`} className="scroll-m-20 font-mono">
            {file}.json
          </h2>
          <p>
            <Prose text={documents[file].description} />
          </p>
          <p className="text-fd-muted-foreground text-sm">
            {defNamesByFile[file].length} definitions · <code>{documents[file].$id}</code>
          </p>
          {defNamesByFile[file].map((name) => (
            <Definition key={name} name={name} />
          ))}
        </section>
      ))}

      <h2 id="index" className="scroll-m-20">
        Index
      </h2>
      <p>All {defNames.length} definitions, alphabetically, across the three documents.</p>
      <p className="text-sm">
        {[...defNames]
          .sort((a, b) => a.localeCompare(b))
          .map((name, index) => (
            <span key={name}>
              {index > 0 ? " · " : null}
              <TypeLink name={name} />
            </span>
          ))}
      </p>
    </>
  );
}
