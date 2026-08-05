import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import {
  adapterOwned,
  adapterServices,
  type ProtoEnum,
  type ProtoField,
  type ProtoMessage,
  protoEnumOwner,
  protoMessages,
  protoSections,
  protoWrappers,
} from "@/lib/proto";
import { homeOf } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";

/**
 * Where a proto type name points, or null if nothing on either page holds it.
 *
 * Two destinations, because this page holds only half of what it renders. A
 * type the adapter file declares is on this page. A type generated from
 * `mcp.json` is documented once, on the reference page, and linking it here to
 * `#msg-…` would point at an anchor that does not exist. The plumbing wrappers
 * get no heading, so they get no link either, and a nested enum resolves to the
 * message it is declared in, which is the heading it is rendered under.
 */
function hrefForProtoType(name: string): string | null {
  if (protoWrappers.has(name)) return null;
  const owner = protoEnumOwner[name];
  if (owner) return `#msg-${owner}`;
  if (adapterOwned.has(name)) return `#msg-${name}`;
  return homeOf[name] === "core" ? hrefFor(name) : null;
}

function TypeRef({ name }: { name: string }): ReactNode {
  const href = hrefForProtoType(name);
  if (!href) return <code className="text-[0.875em]">{name}</code>;
  return (
    <a href={href} className="font-mono text-[0.875em] no-underline hover:underline">
      {name}
    </a>
  );
}

/**
 * A field's type, with the plumbing wrappers seen through. A `SymbolFacets`
 * field is a repeated `SymbolFacet` and reads better as one — and the wrapper
 * has no heading of its own to link to.
 */
function FieldType({ field }: { field: ProtoField }): ReactNode {
  const element = protoWrappers.has(field.type)
    ? messagesByName.get(field.type)?.fields[0].type
    : undefined;
  return (
    <>
      {field.repeated || element ? (
        <span className="text-fd-muted-foreground">repeated </span>
      ) : null}
      {field.optional ? <span className="text-fd-muted-foreground">optional </span> : null}
      <TypeRef name={element ?? field.type} />
    </>
  );
}

function Fields({ message }: { message: ProtoMessage }): ReactNode {
  if (message.fields.length === 0) return null;
  return (
    <div className="my-4 overflow-x-auto">
      <table className="my-0 w-full text-sm">
        <thead>
          <tr>
            <th className="text-left">Field</th>
            <th className="text-left">Type</th>
            <th className="text-left">Description</th>
          </tr>
        </thead>
        <tbody>
          {message.fields.map((field) => (
            <tr key={field.name} className="align-top">
              <td className="whitespace-nowrap">
                <code className="text-[0.875em]">{field.name}</code>
                {/* A oneof is the protobuf spelling of a tagged union: exactly
                    one of the fields carrying the same tag is set. */}
                {field.oneof ? (
                  <sup className="ml-1 text-fd-muted-foreground text-[0.7em] uppercase tracking-wide">
                    {field.oneof}
                  </sup>
                ) : null}
              </td>
              <td className="whitespace-nowrap">
                <FieldType field={field} />
              </td>
              <td>
                {field.comment ? (
                  <Prose text={field.comment} resolve={hrefForProtoType} />
                ) : (
                  <span className="text-fd-muted-foreground">—</span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

const messagesByName = new Map<string, ProtoMessage>(
  protoMessages.filter((message) => adapterOwned.has(message.name)).map((m) => [m.name, m]),
);
/** A section heading's anchor. */
export function sectionId(name: string): string {
  return `sec-${name.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;
}

/** An enum's values, described one by one where the file says what they mean. */
function EnumValues({ declared }: { declared: ProtoEnum }): ReactNode {
  if (!declared.values.some((value) => value.comment)) {
    return (
      <p>
        {declared.values.map((value, index) => (
          <span key={value.name}>
            {index > 0 ? " · " : null}
            <code className="text-[0.875em]">{value.name}</code>
          </span>
        ))}
      </p>
    );
  }

  return (
    <ul>
      {declared.values.map((value) => (
        <li key={value.name}>
          <code className="text-[0.875em]">{value.name}</code>
          {value.comment ? (
            <>
              {" — "}
              <Prose text={value.comment} resolve={hrefForProtoType} />
            </>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

function Declaration({ name }: { name: string }): ReactNode {
  const message = messagesByName.get(name);
  if (!message) return null;
  return (
    <section>
      <h3 id={`msg-${name}`} className="scroll-m-20 font-mono">
        {name}
      </h3>
      {message.comment ? (
        <p>
          <Prose text={message.comment} resolve={hrefForProtoType} />
        </p>
      ) : null}
      <Fields message={message} />
      {message.enums.map((declared) => (
        <div key={declared.name}>
          <h4 className="font-mono">{`${name}.${declared.name}`}</h4>
          {declared.comment ? (
            <p>
              <Prose text={declared.comment} resolve={hrefForProtoType} />
            </p>
          ) : null}
          <EnumValues declared={declared} />
        </div>
      ))}
    </section>
  );
}

/**
 * The adapter seam, as its service declares it.
 *
 * This page reads `adapter.proto` rather than a JSON Schema, because that is
 * what the seam is. Messages are grouped the way the file groups itself, so a
 * reader and an editor see the same shape.
 */
export function AdapterReference(): ReactNode {
  const service = adapterServices[0];

  return (
    <>
      <h2 id="service" className="scroll-m-20">
        Service
      </h2>
      <p>
        <code>Describe</code> runs at startup. <code>OpenWorkspace</code>, <code>Refresh</code>,{" "}
        <code>SyncVirtual</code>, and <code>CloseWorkspace</code> maintain compiler state.
      </p>
      <p>
        <code>Analyze</code>, <code>Match</code>, and <code>Actions</code> read that state.{" "}
        <code>Resolve</code> and <code>Format</code> return edits. <code>Validate</code> checks a
        candidate.
      </p>

      {(service?.rpcs ?? []).map((rpc) => (
        <section key={rpc.name}>
          <h3 id={`rpc-${rpc.name}`} className="scroll-m-20 font-mono">
            {rpc.name}
          </h3>
          <pre>
            <code>
              {`rpc ${rpc.name}(${rpc.requestStream ? "stream " : ""}${rpc.request})` +
                ` returns (${rpc.responseStream ? "stream " : ""}${rpc.response})`}
            </code>
          </pre>
          {rpc.comment ? (
            <p>
              <Prose text={rpc.comment} resolve={hrefForProtoType} />
            </p>
          ) : null}
        </section>
      ))}

      {protoSections.map((section) => (
        <div key={section.name}>
          <h2 id={sectionId(section.name)} className="scroll-m-20">
            {section.name}
          </h2>
          {section.types.map((name) => (
            <Declaration key={name} name={name} />
          ))}
        </div>
      ))}
    </>
  );
}
