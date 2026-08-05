import type { ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import {
  type ProtoEnum,
  type ProtoMessage,
  scipEnums,
  scipMessages,
  scipServices,
} from "@/lib/proto";
import { defs, homeOf } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";

const declared = new Set([
  ...scipMessages.map((message) => message.name),
  ...scipEnums.map((value) => value.name),
]);

function localName(name: string): string {
  if (name.startsWith("rift.core.") || name.startsWith("rift.scip.") || name.startsWith("scip.")) {
    return name.split(".").pop() ?? name;
  }
  return name.split(".")[0];
}

function resolveScip(name: string): string | null {
  const local = localName(name);
  if (declared.has(local)) return `#scip-${local}`;
  return homeOf[local] === "core" ? hrefFor(local) : null;
}

function resolveRift(name: string): string | null {
  const local = localName(name);
  if (local in defs && homeOf[local] === "core") return hrefFor(local);
  return declared.has(local) ? `#scip-${local}` : null;
}

/** Prose whose backticked core model names link to their generated definitions. */
export function RiftMapping({ text }: { text: string }): ReactNode {
  return <Prose text={text} resolve={resolveRift} />;
}

function Values({ value }: { value: ProtoEnum }): ReactNode {
  return (
    <ul>
      {value.values.map((member) => (
        <li key={`${member.name}-${member.number}`}>
          <code>{member.name}</code> = <code>{member.number}</code>
          {member.comment ? (
            <>
              {" — "}
              <Prose text={member.comment} resolve={resolveScip} />
            </>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

function MessageFields({ message }: { message: ProtoMessage }): ReactNode {
  return (
    <table>
      <thead>
        <tr>
          <th>Field</th>
          <th>Type</th>
          <th>Description</th>
        </tr>
      </thead>
      <tbody>
        {message.fields.map((field) => (
          <tr key={field.name}>
            <td>
              <code>{field.name}</code> = <code>{field.number}</code>
              {field.oneof ? <sup> {field.oneof}</sup> : null}
            </td>
            <td>
              {field.link && resolveScip(field.link) ? (
                <a href={resolveScip(field.link) ?? undefined}>
                  <code>
                    {field.map
                      ? "map "
                      : field.repeated
                        ? "repeated "
                        : field.optional
                          ? "optional "
                          : ""}
                    {field.type}
                  </code>
                </a>
              ) : (
                <code>
                  {field.map
                    ? "map "
                    : field.repeated
                      ? "repeated "
                      : field.optional
                        ? "optional "
                        : ""}
                  {field.type}
                </code>
              )}
            </td>
            <td>
              {field.comment ? <Prose text={field.comment} resolve={resolveScip} /> : "—"}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function MessageBlock({ message }: { message: ProtoMessage }): ReactNode {
  return (
    <section>
      <h3 id={`scip-${message.name}`} className="scroll-m-28 font-mono">
        {message.name}
      </h3>
      {message.comment ? (
        <p>
          <Prose text={message.comment} resolve={resolveScip} />
        </p>
      ) : null}
      <MessageFields message={message} />
      {message.enums.map((value) => (
        <div key={value.name}>
          <h4 className="font-mono">{`${message.name}.${value.name}`}</h4>
          {value.comment ? <Prose text={value.comment} resolve={resolveScip} /> : null}
          <Values value={value} />
        </div>
      ))}
    </section>
  );
}

/** The official `scip` package generated from the Pydantic protocol model. */
export function ScipReference(): ReactNode {
  return (
    <>
      {scipServices.flatMap((service) =>
        service.rpcs.map((rpc) => (
          <section key={`${service.name}.${rpc.name}`}>
            <h3 id={`scip-rpc-${rpc.name}`} className="scroll-m-28 font-mono">
              {`${service.name}.${rpc.name}`}
            </h3>
            <pre>
              <code>
                {`rpc ${rpc.name}(${rpc.requestStream ? "stream " : ""}${rpc.request})` +
                  ` returns (${rpc.responseStream ? "stream " : ""}${rpc.response})`}
              </code>
            </pre>
            {rpc.comment ? <Prose text={rpc.comment} resolve={resolveScip} /> : null}
          </section>
        )),
      )}
      {scipMessages.map((message) => (
        <MessageBlock key={message.fullName} message={message} />
      ))}
      {scipEnums.map((value) => (
        <section key={value.fullName}>
          <h3 id={`scip-${value.name}`} className="scroll-m-28 font-mono">
            {value.name}
          </h3>
          {value.comment ? <Prose text={value.comment} resolve={resolveScip} /> : null}
          <Values value={value} />
        </section>
      ))}
    </>
  );
}
