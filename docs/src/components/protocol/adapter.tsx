import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import {
  adapterOwned,
  type ProtoMessage,
  protoEnums,
  protoMessages,
  protoServices,
} from "@/lib/proto";

/** A proto type name links to wherever that type is rendered on this page. */
function TypeRef({ name, link }: { name: string; link?: string }): ReactNode {
  if (!link) return <code className="text-[0.875em]">{name}</code>;
  return (
    <a href={`#msg-${link}`} className="font-mono text-[0.875em] no-underline hover:underline">
      {name}
    </a>
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
                {field.repeated ? (
                  <span className="text-fd-muted-foreground">repeated </span>
                ) : null}
                {field.optional ? (
                  <span className="text-fd-muted-foreground">optional </span>
                ) : null}
                <TypeRef name={field.type} link={field.link} />
              </td>
              <td>
                {field.comment ? (
                  <Prose text={field.comment} />
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

/**
 * The adapter seam, as its service declares it.
 *
 * This page reads `adapter.proto` rather than a JSON Schema, because that is
 * what the seam is. Only the messages the adapter file declares are printed
 * here — the model messages it carries are generated from `core.json` and
 * documented once, in the reference.
 */
export function AdapterReference(): ReactNode {
  const service = protoServices[0];
  const owned = protoMessages.filter((message) => adapterOwned.has(message.name));
  const ownedEnums = protoEnums.filter((declared) => adapterOwned.has(declared.name));

  return (
    <>
      <h2 id="service" className="scroll-m-20">
        Service
      </h2>
      <p>
        Eight calls. There is no envelope, no request id, no operation name and no cancel call: gRPC
        is the framing, a stream is ordered, and a cancelled context ends the work.
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
              <Prose text={rpc.comment} />
            </p>
          ) : null}
        </section>
      ))}

      <h2 id="messages" className="scroll-m-20">
        Messages
      </h2>
      <p>
        What the seam itself defines. Everything it carries about code — <code>Symbol</code>,{" "}
        <code>Leaf</code>, <code>File</code>, <code>Snapshot</code> — is generated from the model
        and described in the reference.
      </p>

      {owned.map((message) => (
        <section key={message.name}>
          <h3 id={`msg-${message.name}`} className="scroll-m-20 font-mono">
            {message.name}
          </h3>
          {message.comment ? (
            <p>
              <Prose text={message.comment} />
            </p>
          ) : null}
          <Fields message={message} />
        </section>
      ))}

      {ownedEnums.map((declared) => (
        <section key={declared.name}>
          <h3 id={`msg-${declared.name}`} className="scroll-m-20 font-mono">
            {declared.name}
          </h3>
          {declared.comment ? (
            <p>
              <Prose text={declared.comment} />
            </p>
          ) : null}
          <p>
            {declared.values.map((value, index) => (
              <span key={value.name}>
                {index > 0 ? " · " : null}
                <code className="text-[0.875em]">{value.name}</code>
              </span>
            ))}
          </p>
        </section>
      ))}
    </>
  );
}
